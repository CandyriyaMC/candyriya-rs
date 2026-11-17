//! Human-friendly duration parsing for config values.
//!
//! `[plugins] max-call-timeout = "5s"` is a string on disk; it decodes into
//! a real [`Duration`] here so the rest of the codebase never re-parses it.
//! The grammar is deliberately strict: a non-negative integer followed by
//! exactly one unit — `ms`, `s`, `m` or `h` — and nothing else. `"5.5s"`,
//! `"5 s"`, `"+5s"` and unit-less `"5"` are all hard errors at decode time.

use std::time::Duration;

use serde::Deserialize;

/// Error produced when a duration string does not match the grammar.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "expected `<number><unit>` with unit `ms`, `s`, `m` or `h` (e.g. `250ms`, `5s`, `2m`, `1h`), got `{0}`"
)]
pub struct DurationParseError(String);

/// Parses `"250ms" | "5s" | "2m" | "1h"` into a [`Duration`].
///
/// Note that `"0s"` parses successfully to [`Duration::ZERO`]; whether a zero
/// is meaningful is a schema decision made at validation time, not here.
pub(crate) fn parse_duration(text: &str) -> Result<Duration, DurationParseError> {
    // longest unit first so "ms" is not read as unit "s" with a trailing digit
    let (digits, multiplier_ms): (&str, u64) = match text
        .strip_suffix("ms")
        .map(|n| (n, 1))
        .or_else(|| text.strip_suffix('s').map(|n| (n, 1_000)))
        .or_else(|| text.strip_suffix('m').map(|n| (n, 60_000)))
        .or_else(|| text.strip_suffix('h').map(|n| (n, 3_600_000)))
    {
        Some(pair) => pair,
        None => return Err(DurationParseError(text.to_owned())),
    };

    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return Err(DurationParseError(text.to_owned()));
    }
    let value: u64 = digits
        .parse()
        .map_err(|_| DurationParseError(text.to_owned()))?;
    let millis = value
        .checked_mul(multiplier_ms)
        .ok_or_else(|| DurationParseError(text.to_owned()))?;
    Ok(Duration::from_millis(millis))
}

/// Serde deserializer for duration strings: decode must never panic and a
/// bad value must surface as a decode error (with key path), not a panic.
pub(crate) fn deserialize_duration<'de, D>(deserializer: D) -> Result<Duration, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let text = String::deserialize(deserializer)?;
    parse_duration(&text).map_err(serde::de::Error::custom)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_all_units() {
        assert_eq!(parse_duration("250ms").unwrap(), Duration::from_millis(250));
        assert_eq!(parse_duration("5s").unwrap(), Duration::from_secs(5));
        assert_eq!(parse_duration("2m").unwrap(), Duration::from_secs(120));
        assert_eq!(parse_duration("1h").unwrap(), Duration::from_secs(3600));
        assert_eq!(parse_duration("0s").unwrap(), Duration::ZERO);
        assert_eq!(
            parse_duration("1000000ms").unwrap(),
            Duration::from_secs(1000)
        );
    }

    #[test]
    fn rejects_garbage() {
        for bad in [
            "",
            "s",
            "ms",
            "5",
            "5x",
            "5 s",
            " 5s",
            "+5s",
            "-5s",
            "5.5s",
            "1ss",
            "s5",
            "huge huge",
        ] {
            assert!(parse_duration(bad).is_err(), "must reject `{bad}`");
        }
        // overflow of the millisecond multiplication must error, not wrap
        assert!(parse_duration("99999999999999999999h").is_err());
    }

    #[test]
    fn leading_zeros_are_accepted() {
        assert_eq!(parse_duration("05s").unwrap(), Duration::from_secs(5));
    }
}
