//! Permission nodes: validated newtype with wildcard semantics.
//!
//! A [`PermissionNode`] is a dot-separated pattern such as `candyriya.command.kick`.
//! Nodes are **lowercase ASCII only**; matching is **case-sensitive** (a node can
//! never contain uppercase characters, so two nodes can only differ by case if one
//! of them is invalid and rejected at construction time).
//!
//! Valid nodes:
//!
//! - `foo`, `foo.bar`, `foo.bar.baz`, `my-node_1.part-2`
//! - `*` — the whole-string wildcard, matches **everything**
//! - `foo.*`, `foo.bar.*` — trailing wildcards, the `*` must be a full trailing
//!   segment separated by a literal dot
//!
//! Rejected: empty nodes, nodes longer than [`MAX_NODE_LEN`] bytes, characters
//! outside `[a-z0-9._*-]`, leading/trailing/double dots (empty segments), a `*`
//! anywhere but as the whole string or as a trailing `.*` segment (`foo.*.bar`,
//! `*.foo`, `f*o`, `**` are all invalid).
//!
//! Wildcard matching rules (the crux — read carefully):
//!
//! - `*` matches **any** node, including a bare `foo`.
//! - `foo.*` matches `foo.bar` and `foo.bar.baz`, but **not** `foo` itself.
//!   The literal prefix `foo.` must be present and at least one further segment
//!   must follow it (dot-boundary prefix match only).
//! - There is no single-segment wildcard support: `*.foo`, `foo.*.baz` and `f*o`
//!   are rejected at construction time.
//! - `foo.*` must have the literal dot before the star; `foo*` is rejected.
//!
//! Specificity (used for precedence, see [`crate::PermissionResolver`]) is the
//! number of **literal** dot-separated segments; a trailing wildcard counts as
//! zero. So `*` → 0, `foo.*` → 1, `foo.bar.*` → 2, `foo.bar` → 2.

use std::fmt;

use thiserror::Error;

/// Maximum allowed length of a permission node, in bytes.
pub const MAX_NODE_LEN: usize = 128;

/// Errors returned when parsing a [`PermissionNode`].
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum PermissionNodeError {
    /// The node was empty.
    #[error("permission node must not be empty")]
    Empty,
    /// The node exceeded [`MAX_NODE_LEN`] bytes.
    #[error("permission node is {actual} bytes long, maximum is {max}")]
    TooLong {
        /// Maximum allowed length.
        max: usize,
        /// Actual length.
        actual: usize,
    },
    /// The node contained a character outside `[a-z0-9._*-]`.
    #[error("permission node contains invalid character {0:?} (allowed: [a-z0-9._*-])")]
    InvalidChar(char),
    /// A dot produced an empty segment: leading dot, trailing dot or `..`.
    #[error("permission node has an empty segment (leading, trailing or double dot)")]
    EmptySegment,
    /// A `*` appeared in a position other than the whole string or a trailing `.*` segment.
    #[error("wildcard '*' is only allowed as the whole pattern or as a trailing '.*' segment")]
    MisplacedWildcard,
}

/// A validated permission node, e.g. `candyriya.command.kick` or `candyriya.command.*`.
///
/// Construct with [`PermissionNode::new`]. The wrapped string is guaranteed to be
/// non-empty, lowercase ASCII, at most [`MAX_NODE_LEN`] bytes, and free of empty
/// segments and misplaced wildcards.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PermissionNode(String);

impl PermissionNode {
    /// Parses and validates `raw` into a permission node.
    ///
    /// See the module docs for the full grammar.
    pub fn new(raw: &str) -> Result<Self, PermissionNodeError> {
        if raw.is_empty() {
            return Err(PermissionNodeError::Empty);
        }
        if raw.len() > MAX_NODE_LEN {
            return Err(PermissionNodeError::TooLong {
                max: MAX_NODE_LEN,
                actual: raw.len(),
            });
        }
        if let Some(ch) = raw
            .chars()
            .find(|ch| !matches!(ch, 'a'..='z' | '0'..='9' | '.' | '_' | '-' | '*'))
        {
            return Err(PermissionNodeError::InvalidChar(ch));
        }

        let count = raw.split('.').count();
        for (index, segment) in raw.split('.').enumerate() {
            let is_last = index + 1 == count;
            // The only legal `*` positions are the whole string ("*") and a
            // trailing `.*` segment, both of which are a single last segment of `*`.
            if is_last && segment == "*" {
                continue;
            }
            if segment.is_empty() {
                return Err(PermissionNodeError::EmptySegment);
            }
            if segment.contains('*') {
                return Err(PermissionNodeError::MisplacedWildcard);
            }
            // `.` was consumed by the split, `*` handled above; remaining chars
            // are guaranteed [a-z0-9_-] by the earlier check.
        }

        Ok(Self(raw.to_owned()))
    }

    /// The raw, validated node text.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Number of literal (non-wildcard) dot-separated segments.
    ///
    /// A trailing `.*` segment contributes zero (only literal segments count),
    /// and the bare `*` pattern has specificity 0 as well. Used for resolution
    /// precedence: the pattern with the most literal segments wins.
    pub fn specificity(&self) -> usize {
        self.0.split('.').filter(|segment| *segment != "*").count()
    }

    /// Whether this node is the bare `*` wildcard.
    pub fn is_star(&self) -> bool {
        self.0 == "*"
    }

    /// Whether `self` (acting as a grant pattern) matches `node`.
    ///
    /// Rules:
    ///
    /// - `*` matches every node.
    /// - Otherwise an exact string match wins.
    /// - Otherwise a trailing-wildcard pattern `prefix.*` matches `node` iff
    ///   `node` starts with `prefix.` and has at least one (possibly nested)
    ///   segment after it. `foo.*` therefore matches `foo.bar` and
    ///   `foo.bar.baz`, never `foo`, and never `foo` with more/fewer segments'
    ///   fuzz — the boundary is a literal dot.
    ///
    /// This method does not care whether `node` is a concrete node; a wildcard
    /// query node is well-defined under the same rules but proxies should query
    /// with concrete nodes.
    pub(crate) fn matches(&self, node: &PermissionNode) -> bool {
        let pattern = self.as_str();
        let candidate = node.as_str();
        if pattern == "*" {
            return true;
        }
        if pattern == candidate {
            return true;
        }
        if let Some(prefix) = pattern.strip_suffix(".*") {
            return candidate.len() > prefix.len() + 1
                && candidate.starts_with(prefix)
                && candidate.as_bytes()[prefix.len()] == b'.';
        }
        false
    }
}

impl fmt::Display for PermissionNode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::str::FromStr for PermissionNode {
    type Err = PermissionNodeError;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        Self::new(raw)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_valid_nodes() {
        for raw in [
            "*",
            "foo",
            "foo.bar",
            "foo.bar.baz",
            "foo.*",
            "foo.bar.*",
            "my-node_1.part-2.x",
            "a",
        ] {
            assert!(PermissionNode::new(raw).is_ok(), "expected valid: {raw}");
        }
    }

    #[test]
    fn rejects_invalid_nodes() {
        let long = "a".repeat(MAX_NODE_LEN + 1);
        let cases: &[&str] = &[
            "",
            ".foo",
            "foo.",
            "foo..bar",
            "..",
            ".",
            "foo*bar",
            "foo*",
            "f*o",
            "*.foo",
            "foo.*.bar",
            "**",
            "foo.**",
            "foo bar",
            "Foo.bar",
            "FOO",
            "foo/bar",
            long.as_str(),
        ];
        for raw in cases {
            assert!(PermissionNode::new(raw).is_err(), "expected invalid: {raw}");
        }
    }

    #[test]
    fn accepts_max_length_node() {
        let raw = "a".repeat(MAX_NODE_LEN);
        assert!(PermissionNode::new(&raw).is_ok());
    }

    #[test]
    fn specificity_counts_literal_segments() {
        assert_eq!(PermissionNode::new("*").unwrap().specificity(), 0);
        assert_eq!(PermissionNode::new("foo.*").unwrap().specificity(), 1);
        assert_eq!(PermissionNode::new("foo.bar.*").unwrap().specificity(), 2);
        assert_eq!(PermissionNode::new("foo.bar").unwrap().specificity(), 2);
        assert_eq!(PermissionNode::new("foo.bar.baz").unwrap().specificity(), 3);
    }

    #[test]
    fn matching_rules() {
        let node = |s: &str| PermissionNode::new(s).unwrap();
        let query = |s: &str| PermissionNode::new(s).unwrap();

        assert!(node("*").matches(&query("foo.bar.baz")));
        assert!(node("*").matches(&query("foo")));
        assert!(node("foo.*").matches(&query("foo.bar")));
        assert!(node("foo.*").matches(&query("foo.bar.baz")));
        assert!(!node("foo.*").matches(&query("foo")));
        assert!(!node("foo.*").matches(&query("foobar")));
        assert!(!node("foo.*").matches(&query("foobar.baz")));
        assert!(node("foo.bar.*").matches(&query("foo.bar.baz")));
        assert!(node("foo.bar.*").matches(&query("foo.bar.baz.qux")));
        assert!(!node("foo.bar.*").matches(&query("foo.bar")));
        assert!(node("foo.bar").matches(&query("foo.bar")));
        assert!(!node("foo.bar").matches(&query("foo.bar.baz")));
        assert!(!node("foo").matches(&query("foo.bar")));
    }
}
