//! UUIDs in their Minecraft wire format.
//!
//! UUIDs are encoded as an unsigned 128-bit integer, transmitted as two
//! unsigned 64-bit longs, most significant first, each big-endian. This is
//! the same layout as Java's `UUID.getMostSignificantBits` /
//! `getLeastSignificantBits`.
//!
//! See <https://minecraft.wiki/w/Java_Edition_protocol/Data_types#UUID>.

use bytes::{Buf, BufMut};

use crate::error::ProtocolError;

/// Reads a UUID from `src` as its two 64-bit halves (most significant first),
/// advancing it past the 16 bytes consumed.
///
/// Returns [`ProtocolError::Truncated`] if `src` holds fewer than 16 bytes;
/// `src` is then left untouched.
pub fn read_uuid(src: &mut impl Buf) -> Result<(i64, i64), ProtocolError> {
    if src.remaining() < 16 {
        return Err(ProtocolError::Truncated);
    }
    Ok((src.get_i64(), src.get_i64()))
}

/// Writes a UUID to `dst` as its two 64-bit halves (most significant first),
/// big-endian on the wire.
pub fn write_uuid(msb: i64, lsb: i64, dst: &mut impl BufMut) {
    dst.put_i64(msb);
    dst.put_i64(lsb);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let mut buf = bytes::BytesMut::new();
        write_uuid(-0x1234_5678_9ABC_DEF0, 0x0F0E_0D0C_0B0A_0908, &mut buf);
        assert_eq!(buf.len(), 16);
        let mut slice: &[u8] = &buf;
        let (msb, lsb) = read_uuid(&mut slice).unwrap();
        assert_eq!(msb, -0x1234_5678_9ABC_DEF0);
        assert_eq!(lsb, 0x0F0E_0D0C_0B0A_0908);
        assert_eq!(slice.remaining(), 0);
    }

    #[test]
    fn zero_and_max_values() {
        for (msb, lsb) in [
            (0i64, 0i64),
            (i64::MAX, i64::MAX),
            (i64::MIN, i64::MIN),
            (-1, -1),
        ] {
            let mut buf = bytes::BytesMut::new();
            write_uuid(msb, lsb, &mut buf);
            let mut slice: &[u8] = &buf;
            assert_eq!(read_uuid(&mut slice).unwrap(), (msb, lsb));
        }
    }

    #[test]
    fn truncated_input_yields_truncated_and_does_not_consume() {
        let raw = [0x00; 15];
        let mut slice: &[u8] = &raw;
        assert!(matches!(
            read_uuid(&mut slice),
            Err(ProtocolError::Truncated)
        ));
        assert_eq!(slice.remaining(), 15);
    }
}
