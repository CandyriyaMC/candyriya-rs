//! UUID helpers for the harness.
//!
//! Minecraft carries UUIDs as two big-endian 64-bit halves (see
//! [`candyriya_protocol::write_uuid`]). The helpers in this module format and
//! parse the canonical hyphenated text form, and derive a *harness-stable*
//! identifier from a player name.
//!
//! [`uuid_from_name`] is deliberately **not** the vanilla offline-mode UUID
//! (that is an MD5 of `OfflinePlayer:<name>`, which would pull an MD5
//! implementation into the harness for no reason). It is deterministic and
//! non-zero, which is all the login flow needs; the integration tests assert
//! exactly that.

use thiserror::Error;

/// Error returned when parsing a canonical hyphenated UUID.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum UuidParseError {
    /// The string did not have the `8-4-4-4-12` hyphenated layout.
    #[error("not a canonical hyphenated uuid (expected 8-4-4-4-12 hex groups)")]
    BadLayout,

    /// A hex group contained a non-hex character or overflowed.
    #[error("invalid hex digit in uuid")]
    BadHex,
}

/// FNV-1a 64-bit offset basis.
const FNV_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
/// FNV-1a 64-bit prime.
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// One FNV-1a pass over `domain`, a NUL separator and `name`.
fn fnv1a(domain: &str, name: &str) -> u64 {
    let mut hash = FNV_OFFSET_BASIS;
    for byte in domain
        .as_bytes()
        .iter()
        .chain([0x00].iter())
        .chain(name.as_bytes().iter())
    {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

/// Derive a stable, deterministic `(msb, lsb)` UUID pair from a player name.
///
/// Two independent FNV-1a passes with different domain separators feed the
/// two halves; the version nibble of the most significant half is forced to
/// `0b0011` (v3-style) and the two top bits of the least significant half to
/// `0b10` (variant 1), so the pair inspects as a valid UUID everywhere. The
/// version bits also guarantee the pair is never all-zero.
pub fn uuid_from_name(name: &str) -> (i64, i64) {
    let mut msb = fnv1a("candyriya-uuid-msb", name);
    // version nibble: top 4 bits of byte 6, i.e. bits 48..52
    msb &= !(0x0f_u64 << 48);
    msb |= 0x03_u64 << 48;

    let mut lsb = fnv1a("candyriya-uuid-lsb", name);
    // variant bits: top 2 bits of the most significant byte of the lsb half
    lsb &= !(0xc0_u64 << 56);
    lsb |= 0x80_u64 << 56;

    (msb as i64, lsb as i64)
}

/// Format a `(msb, lsb)` pair as the canonical hyphenated lowercase UUID.
pub fn format_uuid((msb, lsb): (i64, i64)) -> String {
    let m = msb as u64;
    let l = lsb as u64;
    format!(
        "{:08x}-{:04x}-{:04x}-{:04x}-{:012x}",
        (m >> 32) as u32,
        ((m >> 16) & 0xffff) as u16,
        (m & 0xffff) as u16,
        ((l >> 48) & 0xffff) as u16,
        l & 0x0000_ffff_ffff_ffff,
    )
}

/// Parse a canonical hyphenated UUID back into its `(msb, lsb)` halves.
///
/// The inverse of [`format_uuid`]; rejects anything that is not exactly
/// 36 ASCII characters in the `8-4-4-4-12` layout with lowercase or
/// uppercase hex digits.
pub fn parse_uuid(value: &str) -> Result<(i64, i64), UuidParseError> {
    let bytes = value.as_bytes();
    if bytes.len() != 36 {
        return Err(UuidParseError::BadLayout);
    }
    for hyphen in [8, 13, 18, 23] {
        if bytes[hyphen] != b'-' {
            return Err(UuidParseError::BadLayout);
        }
    }

    let group = |range: std::ops::Range<usize>| -> Result<u64, UuidParseError> {
        let mut acc: u64 = 0;
        for &byte in &bytes[range] {
            let digit = match byte {
                b'0'..=b'9' => u64::from(byte - b'0'),
                b'a'..=b'f' => u64::from(byte - b'a') + 10,
                b'A'..=b'F' => u64::from(byte - b'A') + 10,
                _ => return Err(UuidParseError::BadHex),
            };
            acc = (acc << 4) | digit;
        }
        Ok(acc)
    };

    let g1 = group(0..8)?;
    let g2 = group(9..13)?;
    let g3 = group(14..18)?;
    let g4 = group(19..23)?;
    let g5 = group(24..36)?;

    let msb = (g1 << 32) | (g2 << 16) | g3;
    let lsb = (g4 << 48) | g5;
    Ok((msb as i64, lsb as i64))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uuid_from_name_is_deterministic_and_non_zero() {
        for name in ["Steve", "Alex", "ItHarness", "Пестр", ""] {
            let first = uuid_from_name(name);
            assert_eq!(first, uuid_from_name(name));
            assert_ne!(first, (0, 0));
        }
        // distinct names get distinct ids
        assert_ne!(uuid_from_name("a"), uuid_from_name("b"));
    }

    #[test]
    fn uuid_from_name_carries_version_and_variant_bits() {
        let (msb, lsb) = uuid_from_name("Steve");
        // version nibble (v3) in the top 4 bits of byte 6
        assert_eq!(((msb as u64) >> 48) & 0x0f, 0x03);
        // variant bits 0b10 in the top 2 bits of the lsb half
        assert_eq!(((lsb as u64) >> 62) & 0x03, 0b10);
    }

    #[test]
    fn format_parse_roundtrip() {
        for name in ["Steve", "Alex", "ItHarness", "Пестр", ""] {
            let pair = uuid_from_name(name);
            let text = format_uuid(pair);
            assert_eq!(text.len(), 36);
            assert_eq!(parse_uuid(&text).unwrap(), pair);
        }
        // the zero pair formats and parses back too
        assert_eq!(
            parse_uuid("00000000-0000-0000-0000-000000000000").unwrap(),
            (0, 0)
        );
    }

    #[test]
    fn parse_rejects_garbage() {
        assert_eq!(parse_uuid(""), Err(UuidParseError::BadLayout));
        assert_eq!(
            parse_uuid("00000000-0000-0000-0000-00000000000"),
            Err(UuidParseError::BadLayout)
        );
        assert_eq!(
            parse_uuid("0000000x-0000-0000-0000-000000000000"),
            Err(UuidParseError::BadHex)
        );
        assert_eq!(
            parse_uuid("00000000_0000-0000-0000-000000000000"),
            Err(UuidParseError::BadLayout)
        );
    }
}
