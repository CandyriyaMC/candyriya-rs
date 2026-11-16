//! Grants: the raw allow/deny decisions attached to subjects and groups.

use crate::context::PermissionContext;
use crate::node::PermissionNode;

/// The decision carried by a grant.
///
/// There is no "inherit" pseudo-value: a subject with no applicable grants falls
/// back to the registry default (and to deny when nothing is registered), see
/// [`crate::PermissionResolver`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PermissionValue {
    /// The grant allows access.
    Allow,
    /// The grant denies access.
    Deny,
}

impl PermissionValue {
    /// Whether this value allows access.
    pub fn is_allow(self) -> bool {
        matches!(self, Self::Allow)
    }
}

/// A single grant of a permission node to a subject or group.
///
/// A grant is **applicable** to node `N` iff:
///
/// 1. `N` is matched by [`Grant::node`] (exact match or wildcard match), and
/// 2. for every context on this grant, the subject has the same context name
///    with the same value (subset match). A grant with no contexts applies in
///    every context.
///
/// `source` identifies where the grant came from (a plugin id, a config file
/// path, `"manual"` ...); it is carried along for diagnostics and carries no
/// resolution semantics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grant {
    /// The node (or wildcard pattern) this grant applies to.
    pub node: PermissionNode,
    /// Whether the grant allows or denies.
    pub value: PermissionValue,
    /// Contexts the grant is limited to; empty means "everywhere".
    pub contexts: Vec<PermissionContext>,
    /// Where the grant came from; no resolution semantics.
    pub source: Option<String>,
}

impl Grant {
    /// Creates a grant that applies everywhere (no contexts).
    pub fn allow(node: PermissionNode) -> Self {
        Self {
            node,
            value: PermissionValue::Allow,
            contexts: Vec::new(),
            source: None,
        }
    }

    /// Creates a grant that applies everywhere (no contexts) and denies.
    pub fn deny(node: PermissionNode) -> Self {
        Self {
            node,
            value: PermissionValue::Deny,
            contexts: Vec::new(),
            source: None,
        }
    }

    /// Limits the grant to the given contexts, returning `self` for chaining.
    pub fn with_contexts(mut self, contexts: Vec<PermissionContext>) -> Self {
        self.contexts = contexts;
        self
    }

    /// Records where the grant came from, returning `self` for chaining.
    pub fn with_source(mut self, source: impl Into<String>) -> Self {
        self.source = Some(source.into());
        self
    }
}
