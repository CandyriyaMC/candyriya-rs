//! `VarInt` / `VarLong` variable-length integer coding.
//!
//! Minecraft encodes 32- and 64-bit signed integers as variable-length
//! values, similar to Protocol Buffers' varints with ZigZag stripped out:
//! the 7 least significant bits of each byte carry the value, the most
//! significant bit is a continuation flag, and the groups are written from
//! least to most significant (little-endian 7-bit groups, not little-endian
//! bytes). `VarInt`s are at most 5 bytes, `VarLong`s at most 10, and
//! unnecessarily long-but-legal encodings (e.g. `81 00` for `1`) are
//! allowed by the spec, so they are accepted on read.
//!
//! Values are two's complement signed integers; negative values therefore
//! always occupy the maximum number of bytes (e.g. `-1` as a `VarInt` is
//! `FF FF FF FF 0F`). Decoding truncates exactly the same way the reference
//! implementation does: bits beyond the target width are silently discarded.
//!
//! See <https://minecraft.wiki/w/Java_Edition_protocol/VarInt_and_VarLong>.

use bytes::{Buf, BufMut};

use crate::error::ProtocolError;

/// Scans `src` for a complete `VarInt` without consuming anything.
///
/// Returns `Ok(Some((value, bytes_read)))` on success, `Ok(None)` if `src`
/// holds only a prefix of the encoding, or `Err(ProtocolError::VarIntTooBig)`
/// if the encoding used more than 5 bytes.
pub(crate) fn scan_varint(src: &[u8]) -> Result<Option<(i32, usize)>, ProtocolError> {
    let mut value: u32 = 0;
    for position in (0..32).step_by(7) {
        let index = position / 7;
        let Some(&byte) = src.get(index) else {
            return Ok(None);
        };
        // the shift at position 28 only keeps the low 4 bits in the u32,
        // exactly like the reference implementation dropping the rest
        value |= ((byte & 0x7F) as u32) << position;
        if byte & 0x80 == 0 {
            return Ok(Some((value as i32, index + 1)));
        }
    }
    Err(ProtocolError::VarIntTooBig)
}

/// Scans `src` for a complete `VarLong` without consuming anything.
///
/// Returns `Ok(Some((value, bytes_read)))` on success, `Ok(None)` if `src`
/// holds only a prefix of the encoding, or `Err(ProtocolError::VarLongTooBig)`
/// if the encoding used more than 10 bytes.
pub(crate) fn scan_varlong(src: &[u8]) -> Result<Option<(i64, usize)>, ProtocolError> {
    let mut value: u64 = 0;
    for position in (0..64).step_by(7) {
        let index = position / 7;
        let Some(&byte) = src.get(index) else {
            return Ok(None);
        };
        // the shift at position 63 only keeps the low bit in the u64,
        // matching the truncation behaviour of the reference implementation
        value |= ((byte & 0x7F) as u64) << position;
        if byte & 0x80 == 0 {
            return Ok(Some((value as i64, index + 1)));
        }
    }
    Err(ProtocolError::VarLongTooBig)
}

/// Number of bytes `value` takes on the wire.
pub(crate) fn varint_len(value: i32) -> usize {
    let mut remaining = value as u32;
    let mut len = 1;
    while remaining >= 0x80 {
        remaining >>= 7;
        len += 1;
    }
    len
}

/// Reads a `VarInt` from `src`, advancing it past the bytes consumed.
///
/// Returns [`ProtocolError::Truncated`] if `src` does not hold a complete
/// encoding (`src` is then left untouched), or [`ProtocolError::VarIntTooBig`]
/// if the encoding used more than 5 bytes. Bits beyond 32 are silently
/// discarded, matching the reference implementation.
pub fn decode_varint(src: &mut impl Buf) -> Result<i32, ProtocolError> {
    match scan_varint(src.chunk())? {
        Some((value, len)) => {
            src.advance(len);
            Ok(value)
        }
        None => Err(ProtocolError::Truncated),
    }
}

/// Reads a `VarLong` from `src`, advancing it past the bytes consumed.
///
/// Returns [`ProtocolError::Truncated`] if `src` does not hold a complete
/// encoding (`src` is then left untouched), or [`ProtocolError::VarLongTooBig`]
/// if the encoding used more than 10 bytes. Bits beyond 64 are silently
/// discarded, matching the reference implementation.
pub fn decode_varlong(src: &mut impl Buf) -> Result<i64, ProtocolError> {
    match scan_varlong(src.chunk())? {
        Some((value, len)) => {
            src.advance(len);
            Ok(value)
        }
        None => Err(ProtocolError::Truncated),
    }
}

/// Writes `value` to `dst` as a `VarInt`.
///
/// Negative values are encoded as their full 32-bit two's complement
/// bit pattern, so they always take 5 bytes.
pub fn write_varint(value: i32, dst: &mut impl BufMut) {
    let mut remaining = value as u32;
    loop {
        let mut byte = (remaining & 0x7F) as u8;
        remaining >>= 7;
        if remaining != 0 {
            byte |= 0x80;
            dst.put_u8(byte);
        } else {
            dst.put_u8(byte);
            return;
        }
    }
}

/// Writes `value` to `dst` as a `VarLong`.
///
/// Negative values are encoded as their full 64-bit two's complement
/// bit pattern, so they always take 10 bytes.
pub fn write_varlong(value: i64, dst: &mut impl BufMut) {
    let mut remaining = value as u64;
    loop {
        let mut byte = (remaining & 0x7F) as u8;
        remaining >>= 7;
        if remaining != 0 {
            byte |= 0x80;
            dst.put_u8(byte);
        } else {
            dst.put_u8(byte);
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn varint_roundtrip(value: i32) -> Vec<u8> {
        let mut buf = bytes::BytesMut::new();
        write_varint(value, &mut buf);
        buf.to_vec()
    }

    fn varlong_roundtrip(value: i64) -> Vec<u8> {
        let mut buf = bytes::BytesMut::new();
        write_varlong(value, &mut buf);
        buf.to_vec()
    }

    fn decode(encoded: &[u8]) -> Result<i32, ProtocolError> {
        let mut slice = encoded;
        decode_varint(&mut slice)
    }

    fn decode_long(encoded: &[u8]) -> Result<i64, ProtocolError> {
        let mut slice = encoded;
        decode_varlong(&mut slice)
    }

    // golden vectors straight from the wiki
    #[test]
    fn varint_golden_encodings() {
        let cases: &[(i32, &[u8])] = &[
            (0, &[0x00]),
            (1, &[0x01]),
            (2, &[0x02]),
            (127, &[0x7F]),
            (128, &[0x80, 0x01]),
            (255, &[0xFF, 0x01]),
            (25565, &[0xDD, 0xC7, 0x01]),
            (2097151, &[0xFF, 0xFF, 0x7F]),
            (2147483647, &[0xFF, 0xFF, 0xFF, 0xFF, 0x07]),
            (-1, &[0xFF, 0xFF, 0xFF, 0xFF, 0x0F]),
            (-2147483648, &[0x80, 0x80, 0x80, 0x80, 0x08]),
        ];
        for (value, expected) in cases {
            assert_eq!(&varint_roundtrip(*value), expected, "encode {value}");
            assert_eq!(decode(expected).unwrap(), *value, "decode {value}");
        }
    }

    #[test]
    fn varlong_golden_encodings() {
        let cases: &[(i64, &[u8])] = &[
            (0, &[0x00]),
            (1, &[0x01]),
            (2, &[0x02]),
            (127, &[0x7F]),
            (128, &[0x80, 0x01]),
            (255, &[0xFF, 0x01]),
            (2147483647, &[0xFF, 0xFF, 0xFF, 0xFF, 0x07]),
            (
                9223372036854775807,
                &[0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x7F],
            ),
            (
                -1,
                &[0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x01],
            ),
            (
                -2147483648,
                &[0x80, 0x80, 0x80, 0x80, 0xF8, 0xFF, 0xFF, 0xFF, 0xFF, 0x01],
            ),
            (
                -9223372036854775808,
                &[0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x01],
            ),
        ];
        for (value, expected) in cases {
            assert_eq!(&varlong_roundtrip(*value), expected, "encode {value}");
            assert_eq!(decode_long(expected).unwrap(), *value, "decode {value}");
        }
    }

    #[test]
    fn varint_roundtrip_full_i32_range() {
        // dense coverage around the encoding width boundaries, positive and negative
        for boundary in [0i64, 128, 16384, 2097152, 268435456] {
            for value in (boundary - 2)..=(boundary + 2) {
                let encoded = varint_roundtrip(value as i32);
                assert_eq!(decode(&encoded).unwrap(), value as i32);
            }
            for value in (-boundary - 2)..=(-boundary + 2) {
                let encoded = varint_roundtrip(value as i32);
                assert_eq!(decode(&encoded).unwrap(), value as i32);
            }
        }
        // stride over the whole domain; an exhaustive 2^32 sweep would be
        // too slow in debug builds
        let stride = 4096i64;
        let mut value = i64::from(i32::MIN);
        while value <= i64::from(i32::MAX) {
            let encoded = varint_roundtrip(value as i32);
            assert_eq!(decode(&encoded).unwrap(), value as i32, "value {value}");
            value += stride;
        }
        assert_eq!(decode(&varint_roundtrip(i32::MAX)).unwrap(), i32::MAX);
        assert_eq!(decode(&varint_roundtrip(i32::MIN)).unwrap(), i32::MIN);
    }

    #[test]
    fn varint_roundtrip_negative_max_extremes() {
        for value in [i32::MIN, i32::MIN + 1, -1, -2, i32::MAX - 1, i32::MAX] {
            let encoded = varint_roundtrip(value);
            assert_eq!(decode(&encoded).unwrap(), value);
        }
        for value in [i64::MIN, i64::MIN + 1, -1, i64::MAX - 1, i64::MAX] {
            let encoded = varlong_roundtrip(value);
            assert_eq!(decode_long(&encoded).unwrap(), value);
        }
    }

    #[test]
    fn varint_unnecessarily_long_encoding_is_accepted() {
        // the wiki allows inputs like 81 00 for the value 1
        assert_eq!(decode(&[0x81, 0x00]).unwrap(), 1);
        assert_eq!(decode(&[0x80, 0x80, 0x80, 0x80, 0x00]).unwrap(), 0);
    }

    #[test]
    fn varint_rejects_sixth_byte() {
        // five continuation bytes and then another byte: too big
        assert!(matches!(
            decode(&[0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x01]),
            Err(ProtocolError::VarIntTooBig)
        ));
        // continuation bit never clears within five bytes
        assert!(matches!(
            decode(&[0xFF, 0xFF, 0xFF, 0xFF, 0x8F]),
            Err(ProtocolError::VarIntTooBig)
        ));
    }

    #[test]
    fn varlong_rejects_eleventh_byte() {
        assert!(matches!(
            decode_long(&[
                0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x01
            ]),
            Err(ProtocolError::VarLongTooBig)
        ));
        assert!(matches!(
            decode_long(&[0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x81]),
            Err(ProtocolError::VarLongTooBig)
        ));
    }

    #[test]
    fn truncated_input_yields_truncated() {
        assert!(matches!(decode(&[]), Err(ProtocolError::Truncated)));
        assert!(matches!(decode(&[0x80]), Err(ProtocolError::Truncated)));
        assert!(matches!(
            decode(&[0xFF, 0xFF]),
            Err(ProtocolError::Truncated)
        ));
        assert!(matches!(
            decode_long(&[0x80, 0x80]),
            Err(ProtocolError::Truncated)
        ));

        // the buffer must stay untouched on Truncated
        let mut slice: &[u8] = &[0x80];
        assert!(matches!(
            decode_varint(&mut slice),
            Err(ProtocolError::Truncated)
        ));
        assert_eq!(slice, &[0x80]);
    }

    #[test]
    fn fifth_byte_high_bits_are_truncated() {
        // bits beyond 32 are silently dropped, like the reference implementation
        assert_eq!(decode(&[0xFF, 0xFF, 0xFF, 0xFF, 0x7F]).unwrap(), -1);
        assert_eq!(decode(&[0xFF, 0xFF, 0xFF, 0xFF, 0x07]).unwrap(), i32::MAX);
    }
}
