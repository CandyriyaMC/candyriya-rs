//! A string newtype that is guaranteed to contain at least one non-whitespace
//! character.
//!
//! Used for names and identifiers passed around the proxy (server names,
//! backend ids, display labels) where an empty or all-whitespace value is
//! always a bug upstream. ASCII is deliberately *not* enforced: Minecraft
//! names and messages are arbitrary Unicode.

use std::borrow::Borrow;
use std::fmt;
use std::ops::Deref;
use std::str::FromStr;

/// A non-empty, non-whitespace-only string.
///
/// Construct via [`NonEmptyString::new`]; the only invariant enforced is that
/// at least one character in the string is not whitespace according to
/// [`char::is_whitespace`]. The original string is stored as-is (not trimmed).
///
/// ```
/// use candyriya_utils::NonEmptyString;
///
/// assert!(NonEmptyString::new("lobby").is_some());
/// assert_eq!(NonEmptyString::new("").map(|s| s.as_str().to_owned()), None);
/// assert!(NonEmptyString::new("   ").is_none());
/// assert_eq!(NonEmptyString::new("  lobby  ").unwrap().as_str(), "  lobby  ");
/// ```
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NonEmptyString(String);

impl NonEmptyString {
    /// Create a new `NonEmptyString`.
    ///
    /// Returns `None` if the string is empty or contains only whitespace.
    #[must_use]
    pub fn new(s: impl Into<String>) -> Option<Self> {
        let s = s.into();
        if s.chars().any(|c| !c.is_whitespace()) {
            Some(Self(s))
        } else {
            None
        }
    }

    /// Borrow the inner string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Unwrap into the inner `String`.
    #[must_use]
    pub fn into_string(self) -> String {
        self.0
    }
}

impl Deref for NonEmptyString {
    type Target = str;

    fn deref(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for NonEmptyString {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl Borrow<str> for NonEmptyString {
    fn borrow(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for NonEmptyString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for NonEmptyString {
    type Err = NonEmptyStringError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::new(s).ok_or(NonEmptyStringError)
    }
}

/// Error returned when parsing an empty or all-whitespace string as
/// [`NonEmptyString`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("string is empty or contains only whitespace")]
pub struct NonEmptyStringError;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_empty() {
        assert_eq!(NonEmptyString::new(""), None);
    }

    #[test]
    fn rejects_whitespace_only() {
        assert_eq!(NonEmptyString::new(" "), None);
        assert_eq!(NonEmptyString::new("   \t\r\n"), None);
        assert_eq!(NonEmptyString::new("\u{00a0}"), None); // nbsp
    }

    #[test]
    fn accepts_non_whitespace() {
        assert_eq!(NonEmptyString::new("a").unwrap().as_str(), "a");
        assert_eq!(
            NonEmptyString::new("  lobby  ").unwrap().as_str(),
            "  lobby  "
        );
        // unicode non-whitespace counts
        assert!(NonEmptyString::new("привет").is_some());
        assert!(NonEmptyString::new(" привет ").is_some());
    }

    #[test]
    fn string_and_str_inputs() {
        assert_eq!(
            NonEmptyString::new(String::from("x")).unwrap().as_str(),
            "x"
        );
        assert_eq!(NonEmptyString::new("y").unwrap().as_str(), "y");
    }

    #[test]
    fn from_str_matches_new() {
        assert!("ok".parse::<NonEmptyString>().is_ok());
        assert_eq!("".parse::<NonEmptyString>(), Err(NonEmptyStringError));
        assert_eq!("   ".parse::<NonEmptyString>(), Err(NonEmptyStringError));
    }

    #[test]
    fn ordering_and_eq() {
        let a = NonEmptyString::new("abc").unwrap();
        let b = NonEmptyString::new("abc").unwrap();
        let c = NonEmptyString::new("abd").unwrap();
        assert_eq!(a, b);
        assert!(a < c);
    }

    #[test]
    fn display_and_deref() {
        let s = NonEmptyString::new("lobby").unwrap();
        assert_eq!(s.to_string(), "lobby");
        assert_eq!(s.len(), 5); // deref to str
        let inner: String = s.into_string();
        assert_eq!(inner, "lobby");
    }
}
