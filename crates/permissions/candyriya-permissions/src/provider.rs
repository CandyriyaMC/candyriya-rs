//! In-memory grant provider.
//!
//! [`InMemoryProvider`] stores grants keyed by subject kind and id — one bucket
//! per subject or per group — exactly like the future config provider, but with
//! no persistence. It exists so tests and development harnesses can drive the
//! resolver without a backend.
//!
//! It is deliberately dumb about resolution semantics:
//!
//! - [`InMemoryProvider::grants_for`] returns the grants stored under that exact
//!   key (a subject's **own** grants, or a group's grants).
//! - [`InMemoryProvider::effective_grants`] unions the subject's own grants with
//!   the grants of every group in `subject.groups` — one flat level. Nested
//!   group-in-group membership is **not** expanded here; providers that model
//!   nesting must pre-flatten the group ids on the subject (or flatten in their
//!   own storage) before resolving. The resolver itself only ever sees the flat
//!   list it is handed.
//!
//! Grants are bucketed as a kind-keyed outer map and an id-keyed inner map so
//! reads never allocate; `impl Borrow<(SubjectKind, &str)>` does not exist for
//! tuple keys.

use std::collections::HashMap;

use crate::context::{Subject, SubjectKind};
use crate::grant::Grant;

/// Grant storage keyed by subject kind and id.
#[derive(Debug, Default)]
pub struct InMemoryProvider {
    grants: HashMap<SubjectKind, HashMap<String, Vec<Grant>>>,
}

impl InMemoryProvider {
    /// Creates an empty provider.
    pub fn new() -> Self {
        Self::default()
    }

    /// Stores `grant` for the given subject/group key.
    ///
    /// Only `kind` and `id` are used; the subject's groups and contexts are
    /// irrelevant when filing a grant.
    pub fn grant(&mut self, subject: &Subject, grant: Grant) {
        self.grants
            .entry(subject.kind.clone())
            .or_default()
            .entry(subject.id.clone())
            .or_default()
            .push(grant);
    }

    /// Stores `grant` under an explicit group key.
    ///
    /// Convenience for the common "permission group" case; equivalent to
    /// [`InMemoryProvider::grant`] with a group-kind subject.
    pub fn grant_to_group(&mut self, group: &str, grant: Grant) {
        self.grants
            .entry(SubjectKind::Group)
            .or_default()
            .entry(group.to_owned())
            .or_default()
            .push(grant);
    }

    /// Returns the grants stored under the subject's own `(kind, id)` key.
    ///
    /// Returns an empty slice when the key has no grants.
    pub fn grants_for<'a>(&'a self, subject: &'a Subject) -> &'a [Grant] {
        self.grants
            .get(&subject.kind)
            .and_then(|by_id| by_id.get(&subject.id))
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    /// Returns the grants stored for a group id, if any.
    pub fn grants_for_group<'a>(&'a self, group: &str) -> &'a [Grant] {
        self.grants
            .get(&SubjectKind::Group)
            .and_then(|by_id| by_id.get(group))
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    /// Computes the **flat effective grant list** for `subject`:
    /// its own grants, followed by the grants of each group in
    /// `subject.groups`, in the order the groups are listed.
    ///
    /// One level only — nested group nesting is the provider's concern and is
    /// intentionally not expanded here. The order of the returned list does not
    /// influence the resolution decision (precedence is specificity, then
    /// deny-wins at equal length); it only influences which grant is reported as
    /// [`crate::BestMatch`], ties being broken in favor of earlier entries.
    pub fn effective_grants(&self, subject: &Subject) -> Vec<Grant> {
        let mut effective = Vec::new();
        effective.extend_from_slice(self.grants_for(subject));
        for group in &subject.groups {
            effective.extend_from_slice(self.grants_for_group(group));
        }
        effective
    }

    /// Total number of granted entries across all keys.
    pub fn len(&self) -> usize {
        self.grants
            .values()
            .flat_map(HashMap::values)
            .map(Vec::len)
            .sum()
    }

    /// Whether the provider holds no grants at all.
    pub fn is_empty(&self) -> bool {
        self.grants
            .values()
            .flat_map(HashMap::values)
            .all(Vec::is_empty)
    }
}
