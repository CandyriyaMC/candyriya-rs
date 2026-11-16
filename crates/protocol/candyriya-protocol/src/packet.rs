//! Framed packets on the wire and the [`PacketCodec`] that (de)serializes them.
//!
//! Without compression enabled, every packet on the stream is:
//!
//! 1. **Length** — a [`VarInt`](crate::decode_varint), the number of bytes of
//!    `Packet ID + Data` that follow (the length field itself is not counted);
//! 2. **Packet ID** — a [`VarInt`](crate::decode_varint);
//! 3. **Data** — an opaque byte array, its meaning depends on the connection
//!    state and the packet id.
//!
//! The vanilla implementation refuses to put more than `2^21 - 1` (2097151)
//! bytes into an uncompressed packet — the maximum a 3-byte `VarInt` length
//! field can express — and additionally constrains the length *field* to at
//! most 3 bytes even when the value would fit. This crate deliberately picks
//! a slightly wider hard limit of [`MAX_PACKET_SIZE`] (2 MiB) checked against
//! the decoded value; packets larger than that are rejected with
//! [`ProtocolError::PacketTooLarge`].
//!
//! See <https://minecraft.wiki/w/Java_Edition_protocol/Packets#Packet_format>.

use bytes::{Buf, Bytes, BytesMut};
use tokio_util::codec::{Decoder, Encoder};

use crate::error::ProtocolError;
use crate::varint::{scan_varint, varint_len, write_varint};

/// Maximum size of a single packet frame, in bytes.
///
/// This is the limit the decoder enforces on the decoded value of the frame
/// length prefix and that the encoder enforces on frames it produces.
/// The wiki caps uncompressed packets at `2^21 - 1` bytes; 2 MiB (one byte
/// beyond the largest 3-byte `VarInt`) keeps the limit trivially expressible
/// as a `usize` while still bounding memory for hostile peers.
pub const MAX_PACKET_SIZE: usize = 2 * 1024 * 1024;

/// A decoded packet.
///
/// `id` is the raw packet id as it appears on the wire; interpreting it is
/// the caller's job (its meaning depends on the connection state and the
/// protocol version). `payload` holds exactly the data that followed the id.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Packet {
    /// Raw packet id.
    pub id: i32,
    /// Packet data, excluding the packet id.
    pub payload: Bytes,
}

/// A stateless length-prefixed frame codec for the Minecraft wire format.
///
/// `decode` consumes a frame only when the buffer holds the complete frame;
/// otherwise the buffer is left untouched and `Ok(None)` is returned, so
/// partial frames can be fed across multiple polls. `decode_eof` inherits
/// the default behaviour (an error if bytes are left unconsumed at end of
/// stream).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct PacketCodec;

impl PacketCodec {
    /// Creates a new codec.
    pub fn new() -> Self {
        Self
    }
}

impl Decoder for PacketCodec {
    type Item = Packet;
    type Error = ProtocolError;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Packet>, ProtocolError> {
        let Some((length, header_len)) = scan_varint(src)? else {
            // might be an incomplete length prefix; wait for more bytes
            src.reserve(5);
            return Ok(None);
        };
        // a negative declared length wraps around to something gigantic; reject it
        let length = usize::try_from(length).map_err(|_| ProtocolError::PacketTooLarge)?;
        if length > MAX_PACKET_SIZE {
            return Err(ProtocolError::PacketTooLarge);
        }
        let frame_len = header_len
            .checked_add(length)
            .ok_or(ProtocolError::PacketTooLarge)?;
        if src.len() < frame_len {
            src.reserve(frame_len - src.len());
            return Ok(None);
        }
        src.advance(header_len);
        // split_to(length) hands out a zero-copy view; the payload below is a
        // further refcounted slice of the same allocation
        let frame = src.split_to(length).freeze();
        let id_len = match scan_varint(&frame[..])? {
            Some((id, id_len)) => (id, id_len),
            // the declared length covers the id, so an unterminated id VarInt
            // inside the frame is a truncation as far as the frame is concerned
            None => return Err(ProtocolError::Truncated),
        };
        let (id, id_len) = id_len;
        Ok(Some(Packet {
            id,
            payload: frame.slice(id_len..),
        }))
    }
}

impl Encoder<Packet> for PacketCodec {
    type Error = ProtocolError;

    fn encode(&mut self, item: Packet, dst: &mut BytesMut) -> Result<(), ProtocolError> {
        let id_len = varint_len(item.id);
        let frame_len = id_len
            .checked_add(item.payload.len())
            .ok_or(ProtocolError::PacketTooLarge)?;
        if frame_len > MAX_PACKET_SIZE {
            return Err(ProtocolError::PacketTooLarge);
        }
        let prefix_len = varint_len(frame_len as i32);
        dst.reserve(prefix_len + frame_len);
        write_varint(frame_len as i32, dst);
        write_varint(item.id, dst);
        dst.extend_from_slice(&item.payload);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_ID: i32 = 0x00;

    fn encode(packet: Packet) -> BytesMut {
        let mut codec = PacketCodec::new();
        let mut buf = BytesMut::new();
        tokio_util::codec::Encoder::encode(&mut codec, packet, &mut buf).unwrap();
        buf
    }

    fn decode(bytes: &[u8]) -> Result<Option<Packet>, ProtocolError> {
        let mut codec = PacketCodec::new();
        let mut buf = BytesMut::from(bytes);
        tokio_util::codec::Decoder::decode(&mut codec, &mut buf)
    }

    #[test]
    fn encode_length_prefix_counts_id_and_payload() {
        let packet = Packet {
            id: TEST_ID,
            payload: Bytes::from_static(&[1, 2, 3]),
        };
        let buf = encode(packet);
        // length = 1 byte id + 3 bytes payload
        assert_eq!(&buf[..], &[0x04, 0x00, 0x01, 0x02, 0x03]);
    }

    #[test]
    fn encode_negative_id_takes_five_bytes() {
        let packet = Packet {
            id: -1,
            payload: Bytes::new(),
        };
        let buf = encode(packet);
        assert_eq!(&buf[..], &[0x05, 0xFF, 0xFF, 0xFF, 0xFF, 0x0F]);
    }

    #[test]
    fn roundtrip_zero_payload() {
        let packet = Packet {
            id: 0x2A,
            payload: Bytes::new(),
        };
        let buf = encode(packet.clone());
        assert_eq!(decode(&buf).unwrap(), Some(packet));
    }

    #[test]
    fn roundtrip_with_large_payload() {
        let payload = Bytes::from(vec![0xAB; 100_000]);
        let packet = Packet { id: 0x42, payload };
        let buf = encode(packet.clone());
        assert_eq!(decode(&buf).unwrap(), Some(packet));
    }

    #[test]
    fn decode_accepts_frame_split_across_calls() {
        let packet = Packet {
            id: 0x00,
            payload: Bytes::from_static(&[0xDE, 0xAD, 0xBE, 0xEF, 0x1, 0x2, 0x3]),
        };
        let frame = encode(packet.clone());

        let mut codec = PacketCodec::new();
        let mut buf = BytesMut::new();
        let mut decoded = None;
        // feed the frame 3 bytes at a time; nothing may decode early
        for chunk in frame.chunks(3) {
            buf.extend_from_slice(chunk);
            for _ in 0..2 {
                match tokio_util::codec::Decoder::decode(&mut codec, &mut buf).unwrap() {
                    Some(packet) => {
                        assert!(decoded.is_none(), "decoded twice");
                        decoded = Some(packet);
                    }
                    None => break,
                }
            }
        }
        assert_eq!(decoded, Some(packet));
        assert_eq!(buf.len(), 0);
    }

    #[test]
    fn decode_byte_at_a_time() {
        let packet = Packet {
            id: 0x7F, // id varint wide enough that a byte lands mid-way
            payload: Bytes::from_static(&[0x11; 16]),
        };
        let frame = encode(packet.clone());

        let mut codec = PacketCodec::new();
        let mut buf = BytesMut::new();
        let mut decoded = None;
        for byte in frame {
            buf.extend_from_slice(&[byte]);
            if let Some(packet) = tokio_util::codec::Decoder::decode(&mut codec, &mut buf).unwrap()
            {
                decoded = Some(packet);
            }
        }
        assert_eq!(decoded, Some(packet));
    }

    #[test]
    fn decode_two_frames_in_one_buffer() {
        let first = Packet {
            id: 1,
            payload: Bytes::from_static(&[0xAA]),
        };
        let second = Packet {
            id: 2,
            payload: Bytes::from_static(&[0xBB, 0xCC]),
        };
        let mut buf = encode(first.clone());
        buf.extend_from_slice(&encode(second.clone()));

        let mut codec = PacketCodec::new();
        assert_eq!(
            tokio_util::codec::Decoder::decode(&mut codec, &mut buf).unwrap(),
            Some(first)
        );
        assert_eq!(
            tokio_util::codec::Decoder::decode(&mut codec, &mut buf).unwrap(),
            Some(second)
        );
        assert_eq!(
            tokio_util::codec::Decoder::decode(&mut codec, &mut buf).unwrap(),
            None
        );
    }

    #[test]
    fn oversize_length_prefix_is_rejected() {
        // 0x0FFFFFF as a varint declares a frame beyond MAX_PACKET_SIZE
        let raw = [0xFF, 0xFF, 0xFF, 0x0F];
        assert!(matches!(decode(&raw), Err(ProtocolError::PacketTooLarge)));
        // i32::MIN as a varint would wrap around to a gigantic unsigned length
        let raw = [0x80, 0x80, 0x80, 0x80, 0x08];
        assert!(matches!(decode(&raw), Err(ProtocolError::PacketTooLarge)));
    }

    #[test]
    fn encode_rejects_oversize_frame() {
        let packet = Packet {
            id: 0,
            payload: Bytes::from(vec![0x00; MAX_PACKET_SIZE]), // id byte makes it exceed
        };
        let mut codec = PacketCodec::new();
        let mut buf = BytesMut::new();
        assert!(matches!(
            tokio_util::codec::Encoder::encode(&mut codec, packet, &mut buf),
            Err(ProtocolError::PacketTooLarge)
        ));
    }

    #[test]
    fn zero_length_frame_is_truncated() {
        // a frame with declared length 0 carries no packet id at all
        assert!(matches!(decode(&[0x00]), Err(ProtocolError::Truncated)));
    }

    #[test]
    fn unterminated_id_inside_frame_is_truncated() {
        // length 1 claiming a 1-byte id, but that byte has the continuation bit set
        assert!(matches!(
            decode(&[0x01, 0x80]),
            Err(ProtocolError::Truncated)
        ));
    }

    #[test]
    fn frame_with_varint_id_split_at_boundary() {
        // negative id takes 5 bytes; a split exactly at the id/payload boundary must work
        let packet = Packet {
            id: -1,
            payload: Bytes::from_static(&[0x42]),
        };
        let frame = encode(packet.clone());
        assert_eq!(
            tokio_util::codec::Decoder::decode(&mut PacketCodec::new(), &mut frame.clone())
                .unwrap(),
            Some(packet.clone())
        );
        // split inside the id varint
        let mut partial = BytesMut::from(&frame[..3]);
        assert_eq!(
            tokio_util::codec::Decoder::decode(&mut PacketCodec::new(), &mut partial).unwrap(),
            None
        );
        partial.extend_from_slice(&frame[3..]);
        assert_eq!(
            tokio_util::codec::Decoder::decode(&mut PacketCodec::new(), &mut partial).unwrap(),
            Some(packet)
        );
    }
}
