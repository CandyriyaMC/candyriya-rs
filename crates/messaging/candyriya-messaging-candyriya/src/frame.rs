//! Shared encode/decode head and tail for both packet families.
//!
//! The layout `VarInt protocol_version || VarInt packet_id || body` is the
//! same for every packet; only the body grammar differs per packet id.
//! Factoring the head and tail here keeps the two message enums free of
//! copy-pasted framing logic.

use bytes::Bytes;

use crate::direction::Direction;
use crate::error::CandyriyaMessageError;
use crate::{CANDYRIYA_PROTOCOL_VERSION, MAX_PACKET_BYTES};

/// Writes the `protocol_version || packet_id` prefix into a fresh buffer.
///
/// The direction check runs first: encoding a packet family with the wrong
/// direction is a programming error, not a wire error.
pub(crate) fn begin_packet(
    direction: Direction,
    expected: Direction,
    packet_id: i32,
) -> Result<Vec<u8>, CandyriyaMessageError> {
    if direction != expected {
        return Err(CandyriyaMessageError::DirectionMismatch {
            expected,
            found: direction,
        });
    }
    let mut buf = Vec::with_capacity(64);
    candyriya_protocol::write_varint(CANDYRIYA_PROTOCOL_VERSION, &mut buf);
    candyriya_protocol::write_varint(packet_id, &mut buf);
    Ok(buf)
}

/// Enforces [`MAX_PACKET_BYTES`] and freezes the buffer into [`Bytes`].
pub(crate) fn finish_packet(buf: Vec<u8>) -> Result<Bytes, CandyriyaMessageError> {
    if buf.len() > MAX_PACKET_BYTES {
        return Err(CandyriyaMessageError::PacketTooLarge {
            actual: buf.len(),
            max: MAX_PACKET_BYTES,
        });
    }
    Ok(Bytes::from(buf))
}

/// Checks the direction and the size cap, then reads the
/// `protocol_version || packet_id` prefix.
///
/// Returns `(packet_id, body_cursor)` where the cursor is the unconsumed
/// remainder of the payload. The size cap is enforced **before** any field
/// parser runs, so an oversized payload never reaches the field parsers.
/// An empty payload is rejected outright — there is nothing to dispatch on.
pub(crate) fn decode_head<'a>(
    direction: Direction,
    expected: Direction,
    bytes: &'a [u8],
) -> Result<(i32, &'a [u8]), CandyriyaMessageError> {
    if direction != expected {
        return Err(CandyriyaMessageError::DirectionMismatch {
            expected,
            found: direction,
        });
    }
    if bytes.is_empty() {
        return Err(CandyriyaMessageError::EmptyPayload);
    }
    if bytes.len() > MAX_PACKET_BYTES {
        return Err(CandyriyaMessageError::PacketTooLarge {
            actual: bytes.len(),
            max: MAX_PACKET_BYTES,
        });
    }
    let mut cursor: &'a [u8] = bytes;
    let version = crate::codec::read_varint_at(&mut cursor, "protocol_version")?;
    if version != CANDYRIYA_PROTOCOL_VERSION {
        return Err(CandyriyaMessageError::ProtocolVersionMismatch {
            expected: CANDYRIYA_PROTOCOL_VERSION,
            found: version,
        });
    }
    let packet_id = crate::codec::read_varint_at(&mut cursor, "packet_id")?;
    Ok((packet_id, cursor))
}

/// Rejects payloads with unconsumed bytes after the last field.
///
/// The packet grammar is fixed-width per packet id, so a well-formed
/// payload is exactly consumed; anything left over is corruption or
/// smuggling and is reported.
pub(crate) fn ensure_consumed(cursor: &[u8]) -> Result<(), CandyriyaMessageError> {
    if cursor.is_empty() {
        Ok(())
    } else {
        Err(CandyriyaMessageError::TrailingBytes {
            bytes: cursor.len(),
        })
    }
}
