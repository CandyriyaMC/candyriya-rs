//! Forced-hosts routing: map the host the client connected to onto a fixed
//! set of backends.
//!
//! # Matching semantics
//!
//! A pattern in the forced-hosts table is one of the following:
//!
//! * `*` — matches **any** host. This is the catch-all and the least
//!   specific pattern;
//! * `example.com` — an **exact** match. Only a host that normalizes to the
//!   same labels matches;
//! * `*.example.com` — a **single-label wildcard**. The leading `*` covers
//!   exactly one DNS label. `a.example.com` matches, `a.b.example.com` does
//!   **not** (that is two labels in place of the `*`), and `example.com`
//!   does not match either (the `*` requires the label to exist).
//!
//! Comparison rules:
//!
//! 1. Both patterns and the observed host are **normalized** before
//!    matching: lowercased and with all trailing dots stripped (so
//!    `Example.COM.` and `example.com` are the same host).
//! 2. Specificity is ranked by kind: **exact** (no wildcard) beats
//!    **wildcard**, which beats `*`. Note this ranks by *wildcard kind*,
//!    not merely wildcard count: a single-label wildcard outranks the
//!    catch-all even though both contain one `*`.
//! 3. Same-rank patterns that both match a host are necessarily identical;
//!    those are rejected at construction time. Any remaining tie is broken
//!    by **earlier declaration wins** (the table keeps the declaration
//!    order, matching how a config file reads top to bottom).
//! 4. Matching is case-insensitive (full Unicode lowercasing is applied);
//!    no IDNA/punycode conversion is done — patterns and hosts are compared
//!    as raw lowercased UTF-8 strings.
//!
//! Patterns that use `*` anywhere other than as the whole first label
//! (`a.*.com`, `*.*`, `ex*mple.com`) are rejected at construction time, as
//! are empty labels (`a..com`), whitespace and control characters. The same
//! normalization is applied to the observed host; a host that fails
//! normalization never matches anything.

use std::fmt;

use crate::error::RouteError;

/// A parsed, normalized forced-host pattern.
///
/// Specificity is decided purely by the pattern kind: exact carries no
/// wildcard at all, a wildcard carries exactly one covering one label, and
/// the catch-all carries one covering any number of labels. Two patterns of
/// the same kind that could both match one host are the same pattern (and
/// rejected as duplicates), so kind plus declaration order decide every
/// contest.
#[derive(Debug, Clone, PartialEq, Eq)]
enum PatternKind {
    /// `*` — matches everything.
    MatchAll,
    /// `example.com` — matches exactly these labels.
    Exact(Vec<String>),
    /// `*.example.com` — matches exactly one label followed by these labels.
    Wildcard(Vec<String>),
}

impl PatternKind {
    fn rank(&self) -> u8 {
        match self {
            Self::MatchAll => 0,
            Self::Wildcard(_) => 1,
            Self::Exact(_) => 2,
        }
    }
}

/// One entry in the forced-hosts table: a pattern plus the backends it
/// routes to, in the order they should be tried.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForcedHost {
    pattern: String,
    kind: PatternKind,
    backends: Vec<String>,
}

impl ForcedHost {
    /// The normalized pattern, e.g. `*.example.com`.
    pub fn pattern(&self) -> &str {
        &self.pattern
    }

    /// The backends this pattern routes to, in declaration order.
    pub fn backends(&self) -> &[String] {
        &self.backends
    }
}

/// The forced-hosts table: the host the client connected to mapped onto a
/// fixed, ordered set of backends.
///
/// The table keeps the declaration order of the configuration file; when two
/// entries have identical specificity the earlier one wins. Entries are
/// validated at construction time, so a broken configuration fails fast at
/// startup instead of silently not routing players.
///
/// Use [`ForcedHosts::resolve`] to look up the backends for a host.
#[derive(Debug, Default, Clone)]
pub struct ForcedHosts {
    entries: Vec<ForcedHost>,
}

impl ForcedHosts {
    /// Builds the table from `(pattern, backends)` pairs, preserving the
    /// declaration order.
    ///
    /// # Errors
    ///
    /// * [`RouteError::InvalidPattern`] — a pattern is empty, malformed, or
    ///   uses `*` illegally;
    /// * [`RouteError::EmptyForcedBackends`] — a pattern is paired with an
    ///   empty backend list;
    /// * [`RouteError::DuplicatePattern`] — two patterns normalize to the
    ///   same thing.
    pub fn new(
        entries: impl IntoIterator<Item = (String, Vec<String>)>,
    ) -> Result<Self, RouteError> {
        let mut table = Vec::new();
        for (raw_pattern, backends) in entries {
            if backends.is_empty() {
                // normalize first so the error message shows what the operator
                // actually wrote, trimmed and case-folded
                let pattern = normalize_host(&raw_pattern).unwrap_or_else(|| raw_pattern.clone());
                return Err(RouteError::EmptyForcedBackends(pattern));
            }
            let (pattern, kind) = parse_pattern(&raw_pattern)?;
            if table
                .iter()
                .any(|entry: &ForcedHost| entry.pattern == pattern)
            {
                return Err(RouteError::DuplicatePattern(pattern));
            }
            table.push(ForcedHost {
                pattern,
                kind,
                backends,
            });
        }
        Ok(Self { entries: table })
    }

    /// Number of patterns in the table.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Returns `true` when the table is empty (every host falls back to the
    /// try list).
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Iterates the table in declaration order.
    pub fn iter(&self) -> impl Iterator<Item = &ForcedHost> + '_ {
        self.entries.iter()
    }

    /// Resolves `host` to the backends it may be routed to, applying the
    /// matching rules documented at the module level.
    ///
    /// The host is normalized the same way patterns are (lowercase,
    /// trailing dots stripped). A host that fails normalization, or one that
    /// matches no pattern, yields `None`.
    pub fn resolve<'host>(&'host self, host: &str) -> Option<&'host [String]> {
        let normalized = normalize_host(host)?;
        let labels = split_labels(&normalized)?;
        let mut best: Option<&ForcedHost> = None;
        for entry in &self.entries {
            let matches = match &entry.kind {
                PatternKind::MatchAll => true,
                PatternKind::Exact(expected) => labels == expected[..],
                PatternKind::Wildcard(suffix) => {
                    labels.len() == suffix.len() + 1 && labels[1..] == suffix[..]
                }
            };
            if !matches {
                continue;
            }
            best = match best {
                None => Some(entry),
                Some(current) => {
                    if entry.kind.rank() > current.kind.rank() {
                        Some(entry)
                    } else {
                        // equal rank keeps the earlier declaration
                        Some(current)
                    }
                }
            };
        }
        best.map(|entry| entry.backends.as_slice())
    }
}

impl fmt::Display for ForcedHosts {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, entry) in self.entries.iter().enumerate() {
            if index > 0 {
                write!(f, ", ")?;
            }
            write!(f, "{} -> [{}]", entry.pattern, entry.backends.join(", "))?;
        }
        Ok(())
    }
}

/// Normalizes a host-like string: trims surrounding whitespace, lowercases,
/// and strips all trailing dots. Returns `None` when nothing usable remains.
fn normalize_host(host: &str) -> Option<String> {
    let trimmed = host.trim();
    if trimmed.is_empty() {
        return None;
    }
    let without_dots = trimmed.trim_end_matches('.');
    if without_dots.is_empty() {
        return None;
    }
    Some(without_dots.to_lowercase())
}

/// Splits an already-normalized host into labels, or `None` if any label is
/// empty or contains illegal characters.
fn split_labels(host: &str) -> Option<Vec<String>> {
    let mut labels = Vec::new();
    for label in host.split('.') {
        if !is_valid_label(label) {
            return None;
        }
        labels.push(label.to_owned());
    }
    Some(labels)
}

/// A DNS-ish label: letters, digits, `-` and `_`. Empty labels and anything
/// else (whitespace, control chars, `/`, `@`, `:` …) are rejected. We do not
/// enforce the leading/trailing hyphen rule — forced hosts occasionally
/// carry underscores and other odd-but-harmless characters.
fn is_valid_label(label: &str) -> bool {
    !label.is_empty()
        && label
            .chars()
            .all(|ch| ch.is_alphanumeric() || ch == '-' || ch == '_')
}

/// Parses a raw pattern into its normalized string form and its kind.
fn parse_pattern(raw: &str) -> Result<(String, PatternKind), RouteError> {
    let invalid = |reason: &'static str| RouteError::InvalidPattern {
        pattern: raw.to_owned(),
        reason,
    };

    let normalized = normalize_host(raw).ok_or_else(|| invalid("pattern is empty"))?;
    if normalized == "*" {
        return Ok((normalized, PatternKind::MatchAll));
    }

    let labels: Vec<&str> = normalized.split('.').collect();
    for label in &labels {
        if label.is_empty() {
            return Err(invalid("empty label in pattern"));
        }
        if label.contains('*') && *label != "*" {
            return Err(invalid("'*' must cover the entire label"));
        }
        if !label.contains('*') && !is_valid_label(label) {
            return Err(invalid("label contains illegal characters"));
        }
    }

    match labels.as_slice() {
        [first, rest @ ..] if *first == "*" => {
            if rest.is_empty() {
                // "*" alone was handled above; this is ".*" after normalization
                return Err(invalid("wildcard needs a suffix after it"));
            }
            if rest.contains(&"*") {
                // e.g. "*.*" — a suffix label may not be a wildcard
                return Err(invalid("'*' is only allowed as the entire first label"));
            }
            let suffix: Vec<String> = rest.iter().map(|label| (*label).to_owned()).collect();
            Ok((normalized, PatternKind::Wildcard(suffix)))
        }
        _ => {
            if labels.contains(&"*") {
                // a "*" label somewhere other than the front, e.g. a.*.com or *.*
                return Err(invalid("'*' is only allowed as the entire first label"));
            }
            let exact: Vec<String> = labels.iter().map(|label| (*label).to_owned()).collect();
            Ok((normalized, PatternKind::Exact(exact)))
        }
    }
}
