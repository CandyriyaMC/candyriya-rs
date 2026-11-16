//! Lowercase hex encoding/decoding helpers.
//!
//! Used for hashing and id representations where the exact textual form matters
//! (logs, cache keys, debug output). Decoding accepts both cases; encoding is
//! always lowercase.

/// Errors from [`hex_decode`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HexDecodeError {
    /// The input length is odd; hex stores two nibbles per byte.
    #[error("hex string has odd length ({0})")]
    OddLength(usize),
    /// A byte was not a hex digit.
    #[error("invalid hex character {0:?} at position {1}")]
    InvalidChar(char, usize),
}

const HEX_TABLE: &[u8; 16] = b"0123456789abcdef";

/// Encode `bytes` as a lowercase hex string.
///
/// # Examples
///
/// ```
/// use candyriya_utils::hex_lower;
///
/// assert_eq!(hex_lower(&[0x00, 0xff, 0x1a]), "00ff1a");
/// assert_eq!(hex_lower(&[]), "");
/// ```
#[must_use]
pub fn hex_lower(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        out.push(HEX_TABLE[(byte >> 4) as usize] as char);
        out.push(HEX_TABLE[(byte & 0x0f) as usize] as char);
    }
    out
}

/// Decode a hex string into bytes.
///
/// Both uppercase and lowercase digits are accepted; no `0x` prefix and no
/// whitespace are allowed.
///
/// # Examples
///
/// ```
/// use candyriya_utils::{HexDecodeError, hex_decode};
///
/// assert_eq!(hex_decode("00ff1a").unwrap(), vec![0x00, 0xff, 0x1a]);
/// assert_eq!(hex_decode("00FF1A").unwrap(), vec![0x00, 0xff, 0x1a]);
/// assert_eq!(hex_decode("abc"), Err(HexDecodeError::OddLength(3)));
/// assert_eq!(hex_decode("zz"), Err(HexDecodeError::InvalidChar('z', 0)));
/// ```
pub fn hex_decode(s: &str) -> Result<Vec<u8>, HexDecodeError> {
    if !s.len().is_multiple_of(2) {
        return Err(HexDecodeError::OddLength(s.len()));
    }

    fn nibble(byte: u8, pos: usize) -> Result<u8, HexDecodeError> {
        match byte {
            b'0'..=b'9' => Ok(byte - b'0'),
            b'a'..=b'f' => Ok(byte - b'a' + 10),
            b'A'..=b'F' => Ok(byte - b'A' + 10),
            _ => Err(HexDecodeError::InvalidChar(byte as char, pos)),
        }
    }

    // the input is checked to be ASCII below; walk it as bytes for positions
    if !s.is_ascii() {
        // find the first non-ASCII char to report a stable position
        let pos = s.find(|c: char| !c.is_ascii()).unwrap_or(s.len());
        let ch = s[pos..].chars().next().unwrap_or('\u{fffd}');
        return Err(HexDecodeError::InvalidChar(ch, pos));
    }

    let bytes = s.as_bytes();
    let (chunks, _) = bytes.as_chunks::<2>();
    let mut out = Vec::with_capacity(chunks.len());
    for (index, [first, second]) in chunks.iter().enumerate() {
        let hi = nibble(*first, index * 2)?;
        let lo = nibble(*second, index * 2 + 1)?;
        out.push((hi << 4) | lo);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_lower_empty() {
        assert_eq!(hex_lower(&[]), "");
    }

    #[test]
    fn hex_lower_all_bytes() {
        let bytes: Vec<u8> = (0..=255u8).collect();
        let hex = hex_lower(&bytes);
        assert_eq!(hex.len(), 512);
        assert!(
            hex.bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        );
        assert_eq!(&hex[..6], "000102");
        assert_eq!(&hex[506..], "fdfeff");
    }

    #[test]
    fn roundtrip_lowercase() {
        let bytes: Vec<u8> = (0..=255u8).collect();
        assert_eq!(hex_decode(&hex_lower(&bytes)).unwrap(), bytes);
    }

    #[test]
    fn decode_accepts_uppercase_and_mixed() {
        assert_eq!(
            hex_decode("DEADBEEF").unwrap(),
            vec![0xde, 0xad, 0xbe, 0xef]
        );
        assert_eq!(
            hex_decode("DeadBeef").unwrap(),
            vec![0xde, 0xad, 0xbe, 0xef]
        );
    }

    #[test]
    fn decode_empty_string() {
        assert_eq!(hex_decode("").unwrap(), Vec::<u8>::new());
    }

    #[test]
    fn decode_odd_length() {
        assert_eq!(hex_decode("0"), Err(HexDecodeError::OddLength(1)));
        assert_eq!(hex_decode("abc"), Err(HexDecodeError::OddLength(3)));
    }

    #[test]
    fn decode_invalid_char_reports_position() {
        assert_eq!(hex_decode("0g"), Err(HexDecodeError::InvalidChar('g', 1)));
        assert_eq!(hex_decode("g0"), Err(HexDecodeError::InvalidChar('g', 0)));
        assert_eq!(hex_decode("ff0g"), Err(HexDecodeError::InvalidChar('g', 3)));
        // no prefixes allowed
        assert_eq!(hex_decode("0x00"), Err(HexDecodeError::InvalidChar('x', 1)));
    }

    #[test]
    fn decode_non_ascii_reports_char() {
        let err = hex_decode("éé").unwrap_err();
        match err {
            HexDecodeError::InvalidChar(ch, _) => assert_eq!(ch, 'é'),
            other => panic!("expected InvalidChar, got {other:?}"),
        }
    }
}
