//! Byte-safe truncation for UTF-8 strings.
//!
//! Used for log lines, kick reasons and any other place where a human-readable
//! string has to fit into a fixed byte budget without splitting a multi-byte
//! character.

/// Clamp `s` to at most `max_bytes` bytes, cutting at a UTF-8 character boundary.
///
/// The result is always a prefix of `s` whose byte length is `<= max_bytes`.
/// If `max_bytes` lands inside a multi-byte character the character is dropped
/// entirely; `str::is_char_boundary` is used to find the largest valid cut
/// point at or below `max_bytes`.
///
/// If `s` is already `<= max_bytes` bytes long it is returned unchanged.
///
/// # Examples
///
/// ```
/// use candyriya_utils::clamp_str;
///
/// assert_eq!(clamp_str("hello", 10), "hello");
/// assert_eq!(clamp_str("hello world", 5), "hello");
/// // multi-byte characters are never split
/// assert_eq!(clamp_str("héllo", 2), "h"); // 'é' is 2 bytes, 1 byte is not a boundary
/// assert_eq!(clamp_str("héllo", 3), "hé");
/// assert_eq!(clamp_str("anything", 0), "");
/// ```
#[must_use]
pub fn clamp_str(s: &str, max_bytes: usize) -> &str {
    truncate_to_char_boundary(s, max_bytes)
}

/// Truncate `s` to at most `max` bytes at a UTF-8 character boundary.
///
/// Semantically identical to [`clamp_str`]; kept as a more descriptive name
/// for call sites that think in terms of truncation.
///
/// # Examples
///
/// ```
/// use candyriya_utils::truncate_to_char_boundary;
///
/// let s = "привет candyriya"; // Cyrillic is 2 bytes per char
/// let truncated = truncate_to_char_boundary(s, 7);
/// assert_eq!(truncated, "при"); // 7 bytes = 3 full chars + 1 stray byte, dropped
/// ```
#[must_use]
pub fn truncate_to_char_boundary(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_string_returned_unchanged() {
        assert_eq!(clamp_str("abc", 3), "abc");
        assert_eq!(clamp_str("abc", 100), "abc");
    }

    #[test]
    fn empty_input_and_zero_max() {
        assert_eq!(clamp_str("", 0), "");
        assert_eq!(clamp_str("abc", 0), "");
    }

    #[test]
    fn ascii_truncation_is_exact() {
        assert_eq!(clamp_str("hello world", 5), "hello");
        assert_eq!(truncate_to_char_boundary("hello world", 11), "hello world");
    }

    #[test]
    fn cut_at_exact_boundary_of_multibyte_char() {
        // 'é' is 0xC3 0xA9 (2 bytes): cutting at exactly 3 keeps it
        assert_eq!(clamp_str("héllo", 3), "hé");
        // '€' is 0xE2 0x82 0xAC (3 bytes)
        assert_eq!(clamp_str("a€b", 4), "a€");
        // '𝄞' (U+1D11E) is 4 bytes
        assert_eq!(clamp_str("𝄞x", 4), "𝄞");
    }

    #[test]
    fn cut_inside_multibyte_char_drops_it() {
        // one byte into 'é'
        assert_eq!(clamp_str("héllo", 2), "h");
        // two bytes into '€'
        assert_eq!(clamp_str("a€b", 3), "a");
        // three bytes into '𝄞'
        assert_eq!(clamp_str("𝄞x", 3), "");
        // first byte of the string is multi-byte, max cuts inside it
        assert_eq!(clamp_str("é", 1), "");
    }

    #[test]
    fn multibyte_run_truncation() {
        let s = "привет"; // 6 chars * 2 bytes = 12 bytes
        assert_eq!(clamp_str(s, 12), "привет");
        assert_eq!(clamp_str(s, 11), "приве"); // odd cut inside 'т', backs off to 10
        assert_eq!(clamp_str(s, 10), "приве");
        assert_eq!(clamp_str(s, 9), "прив"); // inside 'е', backs off to 8
        assert_eq!(clamp_str(s, 1), "");
    }

    #[test]
    fn max_larger_than_len_is_identity() {
        let s = "a€b𝄞"; // 1 + 3 + 1 + 4 = 9 bytes
        assert_eq!(clamp_str(s, 9), s);
        assert_eq!(truncate_to_char_boundary(s, usize::MAX), s);
    }
}
