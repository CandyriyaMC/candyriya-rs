//! Contexts, subjects and subject kinds.
//!
//! A [`PermissionContext`] is a name/value pair narrowing *where* a grant applies
//! (for example `server=lobby`). Context names are restricted to
//! `[a-z0-9_-]{1,32}` and values to at most [`MAX_CONTEXT_VALUE_LEN`] printable
//! ASCII characters (`0x20..=0x7E`); matching compares both parts as exact,
//! case-sensitive strings.
//!
//! A [`Subject`] is whatever a permission decision is about: the console, a
//! player, a group or anything else. The subject carries its flat list of
//! held groups (`groups`) and the contexts of the current moment
//! (`contexts`). Group nesting is **not** resolved by this crate — providers
//! that hand out nested memberships must flatten them before the resolver sees
//! the list, and even then the resolver works on the flat list it was given.
//!
//! Contexts are stored in a [`BTreeMap`], so iteration and equality are
//! deterministic regardless of insertion order.

use std::collections::BTreeMap;
use std::fmt;

use thiserror::Error;

/// Maximum length of a context name, in bytes.
pub const MAX_CONTEXT_NAME_LEN: usize = 32;

/// Maximum length of a context value, in bytes.
pub const MAX_CONTEXT_VALUE_LEN: usize = 128;

/// Errors returned when parsing a [`PermissionContext`].
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum PermissionContextError {
    /// The context name was empty.
    #[error("context name must not be empty")]
    EmptyName,
    /// The context name was longer than [`MAX_CONTEXT_NAME_LEN`] bytes.
    #[error("context name is {actual} bytes long, maximum is {max}")]
    NameTooLong {
        /// Maximum allowed length.
        max: usize,
        /// Actual length.
        actual: usize,
    },
    /// The context name contained a character outside `[a-z0-9_-]`.
    #[error("context name contains invalid character {0:?} (allowed: [a-z0-9_-])")]
    InvalidNameChar(char),
    /// The context value exceeded [`MAX_CONTEXT_VALUE_LEN`] bytes.
    #[error("context value is {actual} bytes long, maximum is {max}")]
    ValueTooLong {
        /// Maximum allowed length.
        max: usize,
        /// Actual length.
        actual: usize,
    },
    /// The context value contained a non-printable character.
    #[error("context value contains non-printable character {0:?} (allowed: ASCII 0x20..=0x7E)")]
    InvalidValueChar(char),
}

/// A validated context key/value pair, e.g. `server=lobby`.
///
/// Empty values are allowed; everything else is validated. Equality and hashing
/// compare both parts exactly (case-sensitive).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PermissionContext {
    name: String,
    value: String,
}

impl PermissionContext {
    /// Parses and validates a context from `name` and `value`.
    pub fn new(name: &str, value: &str) -> Result<Self, PermissionContextError> {
        if name.is_empty() {
            return Err(PermissionContextError::EmptyName);
        }
        if name.len() > MAX_CONTEXT_NAME_LEN {
            return Err(PermissionContextError::NameTooLong {
                max: MAX_CONTEXT_NAME_LEN,
                actual: name.len(),
            });
        }
        if let Some(ch) = name
            .chars()
            .find(|ch| !matches!(ch, 'a'..='z' | '0'..='9' | '_' | '-'))
        {
            return Err(PermissionContextError::InvalidNameChar(ch));
        }
        if value.len() > MAX_CONTEXT_VALUE_LEN {
            return Err(PermissionContextError::ValueTooLong {
                max: MAX_CONTEXT_VALUE_LEN,
                actual: value.len(),
            });
        }
        if let Some(ch) = value.chars().find(|ch| !matches!(ch, ' ' | '!'..='~')) {
            return Err(PermissionContextError::InvalidValueChar(ch));
        }
        Ok(Self {
            name: name.to_owned(),
            value: value.to_owned(),
        })
    }

    /// The context name, e.g. `server`.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The context value, e.g. `lobby`.
    pub fn value(&self) -> &str {
        &self.value
    }
}

impl fmt::Display for PermissionContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}={}", self.name, self.value)
    }
}

/// Kind of a permission subject.
///
/// Used as part of the provider key together with the subject id. `Other` exists
/// for integrations that have their own subject categories; the payload is
/// opaque to the engine.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SubjectKind {
    /// The proxy console itself.
    Console,
    /// A connected player (identified by the proxy's canonical player id/uuid string).
    Player,
    /// A permission group.
    Group,
    /// Anything else; the payload is treated as an opaque category name.
    Other(String),
}

/// A permission subject.
///
/// `id` is opaque to the engine (for players, the proxy uses the uuid string).
/// `groups` is the **flat** list of group ids the subject currently holds; this
/// crate never expands nested groups — flattening is the provider's concern.
/// `contexts` are the subject's current contexts; a grant applies only if every
/// context on the grant is present here with the same value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Subject {
    /// The kind of subject.
    pub kind: SubjectKind,
    /// The subject id; opaque to the engine.
    pub id: String,
    /// Flat list of group ids the subject holds.
    pub groups: Vec<String>,
    /// The subject's current contexts.
    pub contexts: BTreeMap<String, String>,
}

impl Subject {
    /// Creates a subject with no groups and no contexts.
    pub fn new(kind: SubjectKind, id: impl Into<String>) -> Self {
        Self {
            kind,
            id: id.into(),
            groups: Vec::new(),
            contexts: BTreeMap::new(),
        }
    }

    /// Adds a group to the flat list, returning `self` for chaining.
    pub fn with_group(mut self, group: impl Into<String>) -> Self {
        self.groups.push(group.into());
        self
    }

    /// Adds several groups to the flat list, returning `self` for chaining.
    pub fn with_groups<I, S>(mut self, groups: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.groups.extend(groups.into_iter().map(Into::into));
        self
    }

    /// Sets a context on the subject, returning `self` for chaining.
    ///
    /// The name and value are inserted as given. Because contexts only ever act
    /// as equality keys against validated grant contexts, an out-of-grammar value
    /// here can only fail to match — it can never widen access.
    pub fn with_context(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.contexts.insert(name.into(), value.into());
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_valid_contexts() {
        for (name, value) in [
            ("server", "lobby"),
            ("world", "world_nether"),
            ("a", ""),
            ("a-b_c", "x y!~"),
        ] {
            assert!(
                PermissionContext::new(name, value).is_ok(),
                "expected valid: {name}={value}"
            );
        }
    }

    #[test]
    fn rejects_invalid_contexts() {
        assert!(PermissionContext::new("", "v").is_err());
        assert!(PermissionContext::new(&"n".repeat(33), "v").is_err());
        assert!(PermissionContext::new("Server", "v").is_err());
        assert!(PermissionContext::new("my server", "v").is_err());
        assert!(PermissionContext::new("s'rver", "v").is_err());
        assert!(PermissionContext::new("server", &"v".repeat(129)).is_err());
        assert!(PermissionContext::new("server", "lob\tby").is_err());
        assert!(PermissionContext::new("server", "lobby\n").is_err());
        assert!(PermissionContext::new("server", "x§").is_err());
    }

    #[test]
    fn subject_builder() {
        let subject = Subject::new(SubjectKind::Player, "uuid-1")
            .with_group("admins")
            .with_groups(["mods", "devs"])
            .with_context("server", "lobby");
        assert_eq!(subject.groups, ["admins", "mods", "devs"]);
        assert_eq!(
            subject.contexts.get("server").map(String::as_str),
            Some("lobby")
        );
    }
}
