//! Minecraft's length-prefixed UTF-8 strings.
//!
//! Strings are a `VarInt` byte count followed by that many bytes of regular
//! UTF-8 (not Java's modified UTF-8). The character limit is expressed in
//! UTF-16 code units: a scalar value above U+FFFF counts as two units. Both
//! limits are checked on read and on write:
//!
//! * at most `max_len` UTF-16 code units,
//! * at most `max_len * 3` bytes of encoded UTF-8.
//!
//! The default cap is 32767 units, the largest value the format allows;
//! the wire limit of `n * 3` bytes for `n` code units is exactly why three
//! bytes per unit are legal (3-byte UTF-8 sequences are used by CJK, and
//! 32767-unit strings in 3-byte characters reach 98301 bytes).
//!
//! See <https://minecraft.wiki/w/Java_Edition_protocol/Data_types#String>.

use bytes::{Buf, BufMut};

use crate::error::ProtocolError;
use crate::varint::{decode_varint, write_varint};

/// Default maximum string length, in UTF-16 code units.
///
/// This is the largest value the format permits at all (the wiki caps any
/// string at 32767 units); individual fields in the protocol often use a
/// tighter limit, which callers select via the `max_len` parameter.
pub const DEFAULT_MAX_STRING_LENGTH: usize = 32767;

fn utf16_len(value: &str) -> usize {
    value.chars().map(char::len_utf16).sum()
}

/// Reads a length-prefixed UTF-8 string from `src`, advancing it past the bytes consumed.
///
/// Returns [`ProtocolError::Truncated`] if `src` ends before the declared
/// payload, [`ProtocolError::StringTooLong`] if the declared byte length is
/// negative or larger than `max_len * 3` bytes, or if the decoded string
/// uses more than `max_len` UTF-16 code units, [`ProtocolError::InvalidUtf8`]
/// if the payload is not valid UTF-8, or [`ProtocolError::VarIntTooBig`] /
/// [`ProtocolError::Truncated`] if the length prefix itself is malformed.
pub fn read_string(src: &mut impl Buf, max_len: usize) -> Result<String, ProtocolError> {
    let byte_len = decode_varint(src)?;
    // a length prefix is never negative; a negative varint here is garbage
    if byte_len < 0 {
        return Err(ProtocolError::StringTooLong);
    }
    let byte_len = byte_len as usize;
    let max_bytes = max_len.saturating_mul(3);
    if byte_len > max_bytes {
        return Err(ProtocolError::StringTooLong);
    }
    if src.remaining() < byte_len {
        return Err(ProtocolError::Truncated);
    }
    let payload = src.copy_to_bytes(byte_len);
    let value = String::from_utf8(payload.to_vec()).map_err(|_| ProtocolError::InvalidUtf8)?;
    if utf16_len(&value) > max_len {
        return Err(ProtocolError::StringTooLong);
    }
    Ok(value)
}

/// Writes `value` to `dst` as a length-prefixed UTF-8 string.
///
/// Returns [`ProtocolError::StringTooLong`] if `value` uses more than
/// `max_len` UTF-16 code units. Note that because of the UTF-16 unit count,
/// a string whose scalar values all render as single units may still fail
/// even when its byte length is within `max_len * 3`.
pub fn write_string(
    dst: &mut impl BufMut,
    value: &str,
    max_len: usize,
) -> Result<(), ProtocolError> {
    let units = utf16_len(value);
    if units > max_len {
        return Err(ProtocolError::StringTooLong);
    }
    let bytes = value.as_bytes();
    write_varint(bytes.len() as i32, dst);
    dst.put_slice(bytes);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(value: &str, max_len: usize) -> String {
        let mut buf = bytes::BytesMut::new();
        write_string(&mut buf, value, max_len).unwrap();
        let mut slice: &[u8] = &buf;
        read_string(&mut slice, max_len).unwrap()
    }

    #[test]
    fn roundtrip_basic() {
        assert_eq!(roundtrip("", DEFAULT_MAX_STRING_LENGTH), "");
        assert_eq!(
            roundtrip("Hello, world!", DEFAULT_MAX_STRING_LENGTH),
            "Hello, world!"
        );
        assert_eq!(roundtrip("héllo €", DEFAULT_MAX_STRING_LENGTH), "héllo €");
    }

    #[test]
    fn multibyte_three_and_four_byte_sequences() {
        // 3-byte sequence, one UTF-16 code unit
        assert_eq!(roundtrip("€", 1), "€");
        // 4-byte sequence, two UTF-16 code units
        assert_eq!(roundtrip("𝄞", 2), "𝄞");
        assert!(matches!(
            write_string(&mut bytes::BytesMut::new(), "𝄞", 1),
            Err(ProtocolError::StringTooLong)
        ));
    }

    #[test]
    fn utf16_units_are_counted_not_bytes() {
        // 1 unit, 3 bytes: exactly one unit of cap used
        assert_eq!(roundtrip("€", 1), "€");
        // 2 units, 4 bytes: two units of cap used
        assert_eq!(roundtrip("𝄞", 2), "𝄞");
        // byte count fits the cap but the unit count does not
        assert!(matches!(
            write_string(&mut bytes::BytesMut::new(), "aabb", 3),
            Err(ProtocolError::StringTooLong)
        ));
    }

    #[test]
    fn exact_boundary_at_cap() {
        // cap of 5: exactly 5 units succeed
        assert_eq!(roundtrip("abcde", 5), "abcde");
        // 6 units fail on write at the boundary
        assert!(matches!(
            write_string(&mut bytes::BytesMut::new(), "abcdef", 5),
            Err(ProtocolError::StringTooLong)
        ));
    }

    #[test]
    fn read_rejects_over_cap_string() {
        let mut buf = bytes::BytesMut::new();
        write_string(&mut buf, "abcdef", 6).unwrap();
        let mut slice: &[u8] = &buf;
        assert!(matches!(
            read_string(&mut slice, 5),
            Err(ProtocolError::StringTooLong)
        ));
    }

    #[test]
    fn read_rejects_byte_length_beyond_cap() {
        // declared byte length larger than max_len * 3 is rejected before payload is touched
        let mut raw = bytes::BytesMut::new();
        write_varint(16, &mut raw); // 16 bytes declared, cap of 5 allows 15
        raw.extend_from_slice(b"0123456789abcdef");
        let mut slice: &[u8] = &raw;
        assert!(matches!(
            read_string(&mut slice, 5),
            Err(ProtocolError::StringTooLong)
        ));
    }

    #[test]
    fn read_rejects_negative_length_prefix() {
        let raw = [0xFF, 0xFF, 0xFF, 0xFF, 0x0F]; // -1 as a VarInt
        let mut slice: &[u8] = &raw;
        assert!(matches!(
            read_string(&mut slice, DEFAULT_MAX_STRING_LENGTH),
            Err(ProtocolError::StringTooLong)
        ));
    }

    #[test]
    fn read_rejects_invalid_utf8() {
        let mut raw = bytes::BytesMut::new();
        write_varint(2, &mut raw);
        raw.extend_from_slice(&[0xFF, 0xFE]);
        let mut slice: &[u8] = &raw;
        assert!(matches!(
            read_string(&mut slice, DEFAULT_MAX_STRING_LENGTH),
            Err(ProtocolError::InvalidUtf8)
        ));
    }

    #[test]
    fn read_rejects_truncated_payload() {
        let mut raw = bytes::BytesMut::new();
        write_varint(5, &mut raw);
        raw.extend_from_slice(b"ab"); // declared 5 bytes, only 2 present
        let mut slice: &[u8] = &raw;
        assert!(matches!(
            read_string(&mut slice, DEFAULT_MAX_STRING_LENGTH),
            Err(ProtocolError::Truncated)
        ));
    }

    #[test]
    fn write_at_exact_three_bytes_per_unit_boundary() {
        // 5 units of 3-byte characters is exactly at max_len = 5 and within 15 bytes
        let value: String = std::iter::repeat_n('€', 5).collect();
        let mut buf = bytes::BytesMut::new();
        write_string(&mut buf, &value, 5).unwrap();
        let mut slice: &[u8] = &buf;
        assert_eq!(read_string(&mut slice, 5).unwrap(), value);
    }
}
