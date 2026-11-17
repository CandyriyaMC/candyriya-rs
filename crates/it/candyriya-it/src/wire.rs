//! Byte-level framing on a raw [`TcpStream`].
//!
//! Rather than `Framed`, the harness drives [`PacketCodec`] directly: one
//! `BytesMut` per connection, fed by `AsyncReadExt::read_buf`. This keeps the
//! harness honest about partial frames, mid-frame EOFs and exact byte counts,
//! with no dependency beyond tokio and candyriya-protocol.
//!
//! The same applies to compressed links: after a Set Compression packet,
//! every frame on the wire is `VarInt Packet Length | VarInt Data Length |
//! payload`, which is exactly what `candyriya-compression` produces and
//! consumes. The packet *body* (id VarInt + payload) is identical in both
//! modes; only the framing around it differs, so the body helpers are
//! shared.

use bytes::{Buf, Bytes, BytesMut};
use candyriya_compression::Compression;
use candyriya_protocol::{
    MAX_PACKET_SIZE, Packet, PacketCodec, ProtocolError, decode_varint, write_varint,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_util::codec::{Decoder, Encoder};

use crate::HarnessError;

/// Write one frame to the stream. The frame is encoded into a fresh buffer,
/// then written in full; encoding failures (oversize frame) abort the write.
pub(crate) async fn write_packet(
    stream: &mut TcpStream,
    packet: Packet,
) -> Result<(), HarnessError> {
    let mut codec = PacketCodec::new();
    let mut buf = BytesMut::new();
    codec.encode(packet, &mut buf)?;
    stream.write_all(&buf).await?;
    Ok(())
}

/// Read one frame from the stream.
///
/// Returns `Ok(None)` on a clean end-of-stream *between* frames. An EOF that
/// lands in the middle of a frame is [`ProtocolError::Truncated`]: both peers
/// of this harness always produce complete frames, so a partial frame can
/// only mean a broken peer, and reporting it beats waiting forever.
///
/// `buf` is reused across reads and only grows to the largest frame seen.
pub(crate) async fn read_packet(
    stream: &mut TcpStream,
    buf: &mut BytesMut,
) -> Result<Option<Packet>, HarnessError> {
    let mut codec = PacketCodec::new();
    loop {
        match codec.decode(buf)? {
            Some(packet) => return Ok(Some(packet)),
            None => {
                if stream.read_buf(buf).await? == 0 {
                    return if buf.is_empty() {
                        Ok(None)
                    } else {
                        Err(ProtocolError::Truncated.into())
                    };
                }
            }
        }
    }
}

/// Write one frame to a compressed link. The frame is encoded into a fresh
/// buffer, then written in full.
pub(crate) async fn write_packet_compressed(
    stream: &mut TcpStream,
    packet: Packet,
    compression: &Compression,
) -> Result<(), HarnessError> {
    let body = encode_body(&packet)?;
    let frame = compression.encode_frame(&body)?;
    stream.write_all(&frame).await?;
    Ok(())
}

/// Read one frame from a compressed link.
///
/// Returns `Ok(None)` on a clean end-of-stream *between* frames; a partial
/// frame at EOF is [`ProtocolError::Truncated`], exactly like the plain path.
pub(crate) async fn read_packet_compressed(
    stream: &mut TcpStream,
    buf: &mut BytesMut,
    compression: &Compression,
) -> Result<Option<Packet>, HarnessError> {
    loop {
        match compression.decode_frame(buf) {
            Ok((consumed, body)) => {
                buf.advance(consumed);
                return Ok(Some(decode_body(&body)?));
            }
            Err(candyriya_compression::CompressionError::Truncated) => {
                if stream.read_buf(buf).await? == 0 {
                    return if buf.is_empty() {
                        Ok(None)
                    } else {
                        Err(ProtocolError::Truncated.into())
                    };
                }
            }
            Err(error) => return Err(error.into()),
        }
    }
}

/// Encode the packet body: `VarInt id | payload`. The framing that wraps it
/// (plain or compressed) is the caller's business.
fn encode_body(packet: &Packet) -> Result<Vec<u8>, HarnessError> {
    let mut body = BytesMut::new();
    write_varint(packet.id, &mut body);
    body.extend_from_slice(&packet.payload);
    if body.len() > MAX_PACKET_SIZE {
        return Err(ProtocolError::PacketTooLarge.into());
    }
    Ok(body.to_vec())
}

/// Decode the packet body: `VarInt id | payload`. An empty body never reaches
/// here (the frame layer produced it, and frames with no id are a truncation
/// for our purposes).
fn decode_body(body: &[u8]) -> Result<Packet, HarnessError> {
    if body.is_empty() {
        return Err(ProtocolError::Truncated.into());
    }
    let mut cursor: &[u8] = body;
    let id = decode_varint(&mut cursor)?;
    Ok(Packet {
        id,
        payload: Bytes::copy_from_slice(cursor),
    })
}
