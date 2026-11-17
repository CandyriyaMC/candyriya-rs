//! The primitive field codecs of the `candyriya:main` protocol.
//!
//! These are the exact byte layouts of the protocol's common types; the
//! packet encoders in [`crate::ServerboundMessage`] and
//! [`crate::ClientboundMessage`] are built on them and nothing else. They
//! are public so tests and tooling can exercise the primitives directly,
//! and so the layout tables in [`crate::schema`] map one-to-one onto
//! functions.
//!
//! Everything here operates on an input-pointer (`&mut &[u8]`) for reads —
//! the slice is advanced past the bytes consumed — and on a `Vec<u8>` for
//! writes, which keeps the packet encoders allocation-simple.
//!
//! The `VarInt`, string and UUID layouts are inherited unchanged from
//! [`candyriya_protocol`]; this module only re-expresses them in terms of
//! [`CandyriyaMessageError`] and attaches
//! field names for errors. Nothing is reimplemented.

use bytes::Buf;
use uuid::Uuid;

use candyriya_protocol::ProtocolError;

use crate::error::CandyriyaMessageError;

/// Maps a [`ProtocolError`] onto [`CandyriyaMessageError`], attaching the
/// name of the field being read when the payload ran out.
fn map_proto(error: ProtocolError, field: &'static str) -> CandyriyaMessageError {
    match error {
        ProtocolError::Truncated => CandyriyaMessageError::UnexpectedEof { field },
        ProtocolError::VarIntTooBig | ProtocolError::VarLongTooBig => {
            CandyriyaMessageError::VarIntTooBig
        }
        ProtocolError::StringTooLong => CandyriyaMessageError::StringTooLong,
        ProtocolError::InvalidUtf8 => CandyriyaMessageError::InvalidUtf8,
        // ProtocolError is #[non_exhaustive]; the remaining variants never
        // arise from the primitives this crate uses (no I/O, no frame
        // codec), but a wildcard is required for exhaustiveness. They map
        // to the closest truncation-style failure instead of panicking.
        _ => CandyriyaMessageError::UnexpectedEof { field },
    }
}

/// Reads a `VarInt`, advancing `src` past the bytes consumed.
///
/// # Errors
///
/// - [`CandyriyaMessageError::UnexpectedEof`] if `src` ends before the
///   encoding is complete,
/// - [`CandyriyaMessageError::VarIntTooBig`] if the encoding uses more
///   than five bytes.
pub fn read_varint(src: &mut &[u8]) -> Result<i32, CandyriyaMessageError> {
    read_varint_at(src, "varint")
}

/// Writes `value` to `dst` as a `VarInt`. Negative values always take the
/// full five bytes, like the reference implementation.
pub fn write_varint(value: i32, dst: &mut Vec<u8>) {
    candyriya_protocol::write_varint(value, dst);
}

/// Reads a length-prefixed UTF-8 string, advancing `src` past the bytes consumed.
///
/// The length prefix is a `VarInt`; the payload is plain UTF-8 (not
/// CESU-8 / Java modified UTF-8). At most 32767 UTF-16 code units are
/// accepted, the largest value the Minecraft string format allows.
///
/// # Errors
///
/// - [`CandyriyaMessageError::UnexpectedEof`] if the payload ends before
///   the declared string bytes are present,
/// - [`CandyriyaMessageError::StringTooLong`] if the length prefix is
///   negative, or the byte length or the decoded code unit count exceeds
///   the 32767-unit cap,
/// - [`CandyriyaMessageError::InvalidUtf8`] if the payload is not valid UTF-8,
/// - [`CandyriyaMessageError::VarIntTooBig`] if the length prefix is malformed.
pub fn read_string(src: &mut &[u8]) -> Result<String, CandyriyaMessageError> {
    read_string_at(src, "string")
}

/// Writes `value` as a length-prefixed UTF-8 string.
///
/// # Errors
///
/// - [`CandyriyaMessageError::StringTooLong`] if `value` uses more than
///   32767 UTF-16 code units.
pub fn write_string(value: &str, dst: &mut Vec<u8>) -> Result<(), CandyriyaMessageError> {
    candyriya_protocol::write_string(dst, value, candyriya_protocol::DEFAULT_MAX_STRING_LENGTH)
        .map_err(|error| map_proto(error, "string"))
}

/// Reads a length-prefixed byte array, advancing `src` past the bytes consumed.
///
/// The length prefix is a `VarInt`; the declared length must fit the
/// remaining payload. Allocation happens only after the declaration is
/// checked against the bytes actually present.
///
/// # Errors
///
/// - [`CandyriyaMessageError::ByteArrayTooLarge`] if the length prefix is
///   negative,
/// - [`CandyriyaMessageError::UnexpectedEof`] if the declared length exceeds
///   the bytes remaining in `src` or the payload ends before the declared
///   bytes are present,
/// - [`CandyriyaMessageError::VarIntTooBig`] if the length prefix is malformed.
pub fn read_byte_array(src: &mut &[u8]) -> Result<Vec<u8>, CandyriyaMessageError> {
    let len = read_varint_at(src, "byte_array_len")?;
    if len < 0 {
        return Err(CandyriyaMessageError::ByteArrayTooLarge);
    }
    let len = len as usize;
    if len > src.remaining() {
        return Err(CandyriyaMessageError::UnexpectedEof {
            field: "byte_array",
        });
    }
    let bytes = src[..len].to_vec();
    *src = &src[len..];
    Ok(bytes)
}

/// Writes `value` as a length-prefixed byte array.
///
/// # Errors
///
/// - [`CandyriyaMessageError::ByteArrayTooLarge`] if `value` exceeds
///   `i32::MAX` bytes, which no packet payload (capped by
///   [`crate::MAX_PACKET_BYTES`]) can ever hold.
pub fn write_byte_array(value: &[u8], dst: &mut Vec<u8>) -> Result<(), CandyriyaMessageError> {
    if value.len() > i32::MAX as usize {
        return Err(CandyriyaMessageError::ByteArrayTooLarge);
    }
    candyriya_protocol::write_varint(value.len() as i32, dst);
    dst.extend_from_slice(value);
    Ok(())
}

/// Reads a boolean (`0x00` = false, `0x01` = true), advancing `src` by one byte.
///
/// # Errors
///
/// - [`CandyriyaMessageError::UnexpectedEof`] if `src` is empty,
/// - [`CandyriyaMessageError::InvalidBoolean`] if the byte is anything
///   other than `0x00` or `0x01`.
pub fn read_boolean(src: &mut &[u8]) -> Result<bool, CandyriyaMessageError> {
    read_boolean_at(src, "boolean")
}

/// Writes `value` as a single byte (`0x00` or `0x01`).
pub fn write_boolean(value: bool, dst: &mut Vec<u8>) {
    dst.push(u8::from(value));
}

/// Reads a UUID: 16 bytes, most significant bits first, each 64-bit half
/// big-endian — the same layout as two `INTEGER` longs on the wire.
///
/// # Errors
///
/// - [`CandyriyaMessageError::UnexpectedEof`] if fewer than 16 bytes remain.
pub fn read_uuid(src: &mut &[u8]) -> Result<Uuid, CandyriyaMessageError> {
    read_uuid_at(src, "uuid")
}

/// Writes `value` as its 16 big-endian bytes (MSB first), the exact same
/// layout as an `i64` most-significant-half followed by an `i64`
/// least-significant-half, each big-endian.
pub fn write_uuid(value: &Uuid, dst: &mut Vec<u8>) {
    dst.extend_from_slice(value.as_bytes());
}

pub(crate) fn read_varint_at(
    src: &mut &[u8],
    field: &'static str,
) -> Result<i32, CandyriyaMessageError> {
    candyriya_protocol::decode_varint(src).map_err(|error| map_proto(error, field))
}

pub(crate) fn read_string_at(
    src: &mut &[u8],
    field: &'static str,
) -> Result<String, CandyriyaMessageError> {
    candyriya_protocol::read_string(src, candyriya_protocol::DEFAULT_MAX_STRING_LENGTH)
        .map_err(|error| map_proto(error, field))
}

pub(crate) fn read_boolean_at(
    src: &mut &[u8],
    field: &'static str,
) -> Result<bool, CandyriyaMessageError> {
    let Some(&byte) = src.first() else {
        return Err(CandyriyaMessageError::UnexpectedEof { field });
    };
    match byte {
        0x00 => {
            *src = &src[1..];
            Ok(false)
        }
        0x01 => {
            *src = &src[1..];
            Ok(true)
        }
        other => Err(CandyriyaMessageError::InvalidBoolean(other)),
    }
}

pub(crate) fn read_uuid_at(
    src: &mut &[u8],
    field: &'static str,
) -> Result<Uuid, CandyriyaMessageError> {
    if src.remaining() < 16 {
        return Err(CandyriyaMessageError::UnexpectedEof { field });
    }
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&src[..16]);
    *src = &src[16..];
    Ok(Uuid::from_bytes(bytes))
}
