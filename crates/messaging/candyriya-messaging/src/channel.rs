//! Channel identifiers for plugin-message channels.
//!
//! Two syntaxes are accepted, mirroring the ecosystems this proxy talks to:
//!
//! - **Modern** `namespace:key` keys, e.g. `bungeecord:main` or
//!   `candyriya:main`. Both segments must be non-empty and contain only
//!   `a-z`, `0-9`, `_`, `.`, `/` and `-`; the whole identifier is capped at
//!   [`MAX_CHANNEL_LEN`] bytes.
//! - **Legacy** single-part names, e.g. `BungeeCord`. No colon at all; the
//!   name may use ASCII letters in either case plus digits, `_`, `.` and
//!   `-`, so old mixed-case channel names keep working untouched.
//!
//! A single colon separates the two modern segments; identifiers with more
//! than one colon are rejected. Length is measured in bytes, which is safe:
//! every accepted character is ASCII, so bytes and characters coincide.

use std::fmt;
use std::str::FromStr;

use thiserror::Error;

/// Maximum total length of a channel identifier, in bytes.
pub const MAX_CHANNEL_LEN: usize = 64;

const MODERN_ALLOWED: &str = "a-z, 0-9, '_', '.', '/' and '-'";
const LEGACY_ALLOWED: &str = "ASCII letters, digits, '_', '.' and '-'";

/// Which part of the identifier an error refers to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelSegment {
    /// A legacy single-part name (no colon).
    Legacy,
    /// The namespace segment of a `namespace:key` identifier.
    Namespace,
    /// The key segment of a `namespace:key` identifier.
    Key,
}

impl ChannelSegment {
    fn as_str(self) -> &'static str {
        match self {
            ChannelSegment::Legacy => "legacy channel name",
            ChannelSegment::Namespace => "namespace segment",
            ChannelSegment::Key => "key segment",
        }
    }
}

/// Errors produced when validating a channel identifier.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ChannelIdentifierError {
    /// The identifier is empty.
    #[error("channel identifier is empty")]
    Empty,
    /// The identifier exceeds [`MAX_CHANNEL_LEN`] bytes.
    #[error("channel identifier is {len} bytes long, the maximum is {max} bytes")]
    TooLong {
        /// Measured length in bytes.
        len: usize,
        /// The maximum, 64.
        max: usize,
    },
    /// More than one colon was present.
    #[error(
        "channel identifier contains {count} colons; exactly one colon is allowed in the namespace:key form"
    )]
    MultipleColons {
        /// Number of colons found.
        count: usize,
    },
    /// The namespace segment is empty (identifier starts with a colon).
    #[error("channel identifier has an empty namespace segment")]
    EmptyNamespace,
    /// The key segment is empty (identifier ends with a colon).
    #[error("channel identifier has an empty key segment")]
    EmptyKey,
    /// A character outside the segment's allowed set was found.
    #[error("invalid character {character:?} in {segment}; allowed characters are {allowed}")]
    InvalidCharacter {
        /// Which part of the identifier failed.
        segment: ChannelSegment,
        /// The offending character.
        character: char,
        /// The allowed set for that part.
        allowed: &'static str,
    },
}

impl fmt::Display for ChannelSegment {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

fn is_modern_char(c: char) -> bool {
    matches!(c, 'a'..='z' | '0'..='9' | '_' | '.' | '/' | '-')
}

fn is_legacy_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-')
}

fn validate_modern(segment: &str, which: ChannelSegment) -> Result<(), ChannelIdentifierError> {
    for ch in segment.chars() {
        if !is_modern_char(ch) {
            return Err(ChannelIdentifierError::InvalidCharacter {
                segment: which,
                character: ch,
                allowed: MODERN_ALLOWED,
            });
        }
    }
    Ok(())
}

/// A validated plugin-message channel identifier.
///
/// Construct with [`ChannelIdentifier::new`] (or [`FromStr`]); validation
/// happens once, at the boundary, so downstream code can treat the value as
/// a well-formed channel name in both syntaxes.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ChannelIdentifier(String);

impl ChannelIdentifier {
    /// Validates `s` and returns a channel identifier.
    ///
    /// See the module docs for the exact rules.
    pub fn new(s: &str) -> Result<Self, ChannelIdentifierError> {
        if s.is_empty() {
            return Err(ChannelIdentifierError::Empty);
        }
        if s.len() > MAX_CHANNEL_LEN {
            return Err(ChannelIdentifierError::TooLong {
                len: s.len(),
                max: MAX_CHANNEL_LEN,
            });
        }
        let colon_count = s.bytes().filter(|&b| b == b':').count();
        match colon_count {
            0 => {
                for ch in s.chars() {
                    if !is_legacy_char(ch) {
                        return Err(ChannelIdentifierError::InvalidCharacter {
                            segment: ChannelSegment::Legacy,
                            character: ch,
                            allowed: LEGACY_ALLOWED,
                        });
                    }
                }
            }
            1 => {
                // exactly one colon: find returns the only valid separator index
                let idx = s.find(':').unwrap_or(0);
                let namespace = &s[..idx];
                let key = &s[idx + 1..];
                if namespace.is_empty() {
                    return Err(ChannelIdentifierError::EmptyNamespace);
                }
                if key.is_empty() {
                    return Err(ChannelIdentifierError::EmptyKey);
                }
                validate_modern(namespace, ChannelSegment::Namespace)?;
                validate_modern(key, ChannelSegment::Key)?;
            }
            count => {
                return Err(ChannelIdentifierError::MultipleColons { count });
            }
        }
        Ok(Self(s.to_owned()))
    }

    /// The identifier as written on the wire.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// `Some((namespace, key))` for modern `namespace:key` identifiers,
    /// `None` for legacy single-part names.
    pub fn parts(&self) -> Option<(&str, &str)> {
        self.0.split_once(':')
    }

    /// Whether this is a modern `namespace:key` identifier.
    pub fn is_modern(&self) -> bool {
        self.parts().is_some()
    }
}

impl fmt::Display for ChannelIdentifier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for ChannelIdentifier {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl FromStr for ChannelIdentifier {
    type Err = ChannelIdentifierError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::new(s)
    }
}
