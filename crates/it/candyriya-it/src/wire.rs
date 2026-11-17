//! Byte-level framing on a raw [`TcpStream`].
//!
//! Rather than `Framed`, the harness drives [`PacketCodec`] directly: one
//! `BytesMut` per connection, fed by `AsyncReadExt::read_buf`. This keeps the
//! harness honest about partial frames, mid-frame EOFs and exact byte counts,
//! with no dependency beyond tokio and candyriya-protocol.

use bytes::BytesMut;
use candyriya_protocol::{Packet, PacketCodec, ProtocolError};
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
