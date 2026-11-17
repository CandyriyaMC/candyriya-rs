//! Byte-level framing for one half of a login-session link.
//!
//! A Minecraft link in the login state is: `[AES-CFB8 [compressed [packet]]]`
//! — the cipher is the outermost transform and the compression only applies
//! after a Set Compression packet has been seen. This module owns that
//! composition for *both* directions of *both* links (client ↔ proxy and
//! proxy ↔ backend); everything above it speaks in [`Packet`]s only.
//!
//! Both transforms are optional and start disabled; [`Wire::enable_cipher`]
//! and [`Wire::enable_compression`] switch them on mid-session, exactly the
//! way the wire protocol does. Reads are **byte-wise**: the login phase is
//! short and the frames are tiny, and byte reads keep the framing exact —
//! there is no over-read into the play-state bytes after the connection
//! switches state. Every buffered byte counts toward a hard 2 MiB cap, so a
//! hostile peer cannot grow memory by dripping bytes.
//!
//! Framing-boundary detection scans only the VarInt frame-length prefix
//! (at most 5 bytes) before deciding to decode, so the cost stays linear
//! even against a peer that dribbles a large frame one byte at a time.

use bytes::{Bytes, BytesMut};
use candyriya_compression::Compression;
use candyriya_encryption::AesCfb8;
use candyriya_protocol::{MAX_PACKET_SIZE, Packet, PacketCodec, ProtocolError};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_util::codec::Decoder;

use crate::ProxyError;

/// Hard cap on the buffered wire bytes of one frame, prefix included.
const MAX_WIRE: usize = MAX_PACKET_SIZE + 5;

/// The per-link framing transforms and the read buffer.
///
/// Cheap to create (`Wire::default()`); the cipher and compressor are only
/// allocated when a Set Compression / the encryption handshake switches
/// them on.
#[derive(Default)]
pub(crate) struct Wire {
    cipher: Option<AesCfb8>,
    compression: Option<Compression>,
    buf: BytesMut,
}

impl Wire {
    /// A clean-slate link: no cipher, no compression, empty buffer.
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Switch this link to the encrypted layout (key and IV are both the
    /// shared secret, CFB8 feedback runs continuously from this point on).
    pub(crate) fn enable_cipher(&mut self, cipher: AesCfb8) {
        self.cipher = Some(cipher);
    }

    /// Switch this link to the compressed layout with `threshold`.
    ///
    /// A negative threshold disables compression (the wiki's way of saying
    /// so out loud); otherwise frames at or above `threshold` bytes are
    /// zlib-compressed from the next packet on.
    pub(crate) fn enable_compression(&mut self, compression: Option<Compression>) {
        self.compression = compression;
    }

    /// Take the cipher out of this link, leaving `None` behind.
    ///
    /// Used by the play-state pump, which needs the CFB8 feedback state on
    /// paths that are independent of this struct. Splitting through
    /// [`AesCfb8::into_halves`] keeps the chain intact across the
    /// transition. Consuming the cipher also means the byte-level pump is
    /// the only writer of this link from now on — the frame-oriented API
    /// ([`read_packet`], [`write_packet`]) must not be used afterwards.
    pub(crate) fn take_cipher(&mut self) -> Option<AesCfb8> {
        self.cipher.take()
    }

    /// Whether the link was switched to the compressed layout.
    pub(crate) fn compression_enabled(&self) -> bool {
        self.compression.is_some()
    }
}

/// Length of the VarInt prefix at `buf[0]` or `None` when the prefix is not
/// complete yet. `Err` covers a hostile unterminated prefix.
fn scan_frame_length(buf: &[u8]) -> Result<Option<(usize, usize)>, ProxyError> {
    let mut value: u64 = 0;
    for (i, &byte) in buf.iter().take(5).enumerate() {
        value |= u64::from(byte & 0x7F) << (7 * i);
        if byte & 0x80 == 0 {
            // value > MAX_PACKET_SIZE is filtered by the caller's wire cap
            return Ok(Some((value as usize, i + 1)));
        }
    }
    if buf.len() >= 5 {
        return Err(ProxyError::BadPacket(
            "frame length prefix did not terminate within 5 bytes".to_owned(),
        ));
    }
    Ok(None)
}

/// Total wire bytes of the frame at the front of `buf`, or `None` when
/// more bytes are needed. Errors when the declared length exceeds the cap.
fn frame_wire_len(buf: &[u8]) -> Result<Option<usize>, ProxyError> {
    let Some((declared, header_len)) = scan_frame_length(buf)? else {
        return Ok(None);
    };
    if declared > MAX_PACKET_SIZE {
        return Err(ProxyError::BadPacket(
            "frame declares a length above the 2 MiB cap".to_owned(),
        ));
    }
    Ok(Some(header_len + declared))
}

/// Parse an unframed `packet id varint || payload` blob into a [`Packet`].
/// Used for the payload a Set Compression packet unlocks: the decompressor
/// hands us raw packet bytes, not a framed stream.
fn packet_from_unframed(bytes: &[u8]) -> Result<Packet, ProxyError> {
    let mut src = bytes;
    let id = candyriya_protocol::decode_varint(&mut src)
        .map_err(|error| ProxyError::BadPacket(error.to_string()))?;
    Ok(Packet {
        id,
        payload: Bytes::copy_from_slice(src),
    })
}

/// Attempt to decode exactly one frame from the front of the buffer.
///
/// Returns `Ok(Some(packet))` once the full frame has decoded,
/// `Ok(None)` when more bytes are needed.
fn take_frame(wire: &mut Wire) -> Result<Option<Packet>, ProxyError> {
    let Some(total) = frame_wire_len(&wire.buf)? else {
        return Ok(None);
    };
    // the prefix said how long the frame is; wait for the whole body
    if wire.buf.len() < total {
        return Ok(None);
    }
    let raw: BytesMut = wire.buf.split_to(total);
    match &wire.compression {
        Some(compression) => match compression.decode_frame(&raw) {
            Ok((consumed, payload)) => {
                if consumed != raw.len() {
                    return Err(ProxyError::BadPacket(
                        "compressed frame is not well formed".to_owned(),
                    ));
                }
                Ok(Some(packet_from_unframed(&payload)?))
            }
            Err(other) => Err(ProxyError::BadPacket(format!(
                "invalid compressed frame: {other}"
            ))),
        },
        None => {
            let mut raw = raw;
            match PacketCodec.decode(&mut raw) {
                Ok(Some(packet)) => Ok(Some(packet)),
                // the boundary check guarantees the whole frame was fed; a
                // decoder that still refuses it is a broken frame, not a
                // partial one
                Ok(None) | Err(ProtocolError::PacketTooLarge) => Err(ProxyError::BadPacket(
                    "frame declares a length above the 2 MiB cap".to_owned(),
                )),
                Err(other) => Err(ProxyError::BadPacket(other.to_string())),
            }
        }
    }
}

/// Read one packet from the stream, blocking until one decodes.
///
/// Byte-wise reads; exceeding the 2 MiB cap errors out as
/// [`ProxyError::BadPacket`], a clean EOF errors as [`ProxyError::BadPacket`]
/// too (a peer that vanishes mid-phase is not an answer).
pub(crate) async fn read_packet(
    stream: &mut TcpStream,
    wire: &mut Wire,
) -> Result<Packet, ProxyError> {
    let mut byte = [0_u8; 1];
    loop {
        if let Some(packet) = take_frame(wire)? {
            return Ok(packet);
        }
        if wire.buf.len() >= MAX_WIRE {
            return Err(ProxyError::BadPacket(
                "frame exceeded the 2 MiB wire cap".to_owned(),
            ));
        }
        match stream.read(&mut byte).await {
            Ok(0) => {
                return Err(ProxyError::BadPacket(
                    "connection closed mid-login before a complete frame".to_owned(),
                ));
            }
            Ok(_) => {}
            Err(error) => return Err(ProxyError::Io(error)),
        }
        if let Some(cipher) = &mut wire.cipher {
            cipher.decrypt_in_place(&mut byte);
        }
        wire.buf.extend_from_slice(&byte);
    }
}

/// Write one packet to the stream, atomically w.r.t. the transforms.
///
/// The frame is zlib-compressed first (when compression is on) and the
/// resulting bytes are encrypted afterwards (when the cipher is on) — the
/// same nesting the wire protocol defines. Compression operates on the
/// *unframed* `packet id || payload` bytes, exactly what
/// [`Compression::encode_frame`] expects.
pub(crate) async fn write_packet(
    stream: &mut TcpStream,
    wire: &mut Wire,
    packet: Packet,
) -> Result<(), ProxyError> {
    // both paths share the unframed body; only the framing differs
    let mut unframed = BytesMut::with_capacity(packet.payload.len() + 5);
    candyriya_protocol::write_varint(packet.id, &mut unframed);
    unframed.extend_from_slice(&packet.payload);

    let frame = match &wire.compression {
        Some(compression) => compression
            .encode_frame(&unframed)
            .map_err(|error| ProxyError::BadPacket(format!("compression failed: {error}")))?,
        None => {
            let mut out = BytesMut::with_capacity(unframed.len() + 5);
            candyriya_protocol::write_varint(unframed.len() as i32, &mut out);
            out.extend_from_slice(&unframed);
            out.to_vec()
        }
    };
    let mut frame = frame;
    if let Some(cipher) = &mut wire.cipher {
        cipher.encrypt_in_place(&mut frame);
    }
    stream.write_all(&frame).await.map_err(ProxyError::Io)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_complete_small_prefix() {
        assert_eq!(
            scan_frame_length(&[0x04, 0xAA, 0xBB]).unwrap(),
            Some((4, 1))
        );
        assert_eq!(scan_frame_length(&[0xFF, 0x01]).unwrap(), Some((255, 2)));
    }

    #[test]
    fn scan_incomplete_prefix_waits() {
        assert_eq!(scan_frame_length(&[0x80]).unwrap(), None);
        assert_eq!(scan_frame_length(&[0x80, 0x80]).unwrap(), None);
        assert_eq!(scan_frame_length(&[0xFF, 0xFF, 0xFF, 0xFF]).unwrap(), None);
    }

    #[test]
    fn scan_hostile_prefix_errors() {
        // five bytes all with the continuation bit set cannot be a VarInt
        assert!(scan_frame_length(&[0x80, 0x80, 0x80, 0x80, 0x80]).is_err());
    }

    #[test]
    fn frame_len_respects_the_cap() {
        // 0xFF 0xFF 0x7F declares 2097151 (the largest 3-byte VarInt VarInt), fine
        assert_eq!(
            frame_wire_len(&[0xFF, 0xFF, 0x7F]).unwrap(),
            Some(2097151 + 3)
        );
        // 0xFF 0xFF 0xFF 0x7F declares 268435455, way above MAX_PACKET_SIZE
        assert!(frame_wire_len(&[0xFF, 0xFF, 0xFF, 0x7F]).is_err());
    }

    #[test]
    fn roundtrip_uncompressed() {
        let mut sender = Wire::new();
        let mut receiver = Wire::new();
        let packet = Packet {
            id: 0x02,
            payload: Bytes::from_static(&[1, 2, 3, 4]),
        };
        let frame = encode_frame_for_test(&mut sender, packet.clone());
        receiver.buf.extend_from_slice(&frame);
        assert_eq!(take_frame(&mut receiver).unwrap(), Some(packet));
        assert_eq!(take_frame(&mut receiver).unwrap(), None);
    }

    #[test]
    fn roundtrip_compressed() {
        let mut sender = Wire::new();
        sender.enable_compression(Compression::new(0, -1).ok()); // compress everything
        let packet = Packet {
            id: 0x42,
            payload: Bytes::from_static(&[0xAB; 512]),
        };
        let frame = encode_frame_for_test(&mut sender, packet.clone());

        let mut receiver = Wire::new();
        receiver.enable_compression(Compression::new(0, -1).ok());
        receiver.buf.extend_from_slice(&frame);
        assert_eq!(take_frame(&mut receiver).unwrap(), Some(packet));
    }

    /// Encode through the same path write_packet uses, minus the socket.
    fn encode_frame_for_test(wire: &mut Wire, packet: Packet) -> Vec<u8> {
        let mut unframed = BytesMut::with_capacity(packet.payload.len() + 5);
        candyriya_protocol::write_varint(packet.id, &mut unframed);
        unframed.extend_from_slice(&packet.payload);
        match &wire.compression {
            Some(compression) => compression.encode_frame(&unframed).unwrap(),
            None => {
                let mut out = BytesMut::with_capacity(unframed.len() + 5);
                candyriya_protocol::write_varint(unframed.len() as i32, &mut out);
                out.extend_from_slice(&unframed);
                out.to_vec()
            }
        }
    }
}
