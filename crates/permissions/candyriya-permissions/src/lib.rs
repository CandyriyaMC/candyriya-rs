//! # candyriya-permissions
//!
//! The proxy's own permission engine: validated nodes, contexts, an in-memory
//! grant provider, a node registry with defaults, and a resolver implementing
//! wildcard + allow/deny precedence.
//!
//! ## What this crate is
//!
//! - **Core API, not a library of policy.** It knows nodes, grants, contexts and
//!   the resolution algorithm. It does not know players, configs, databases or
//!   the network; those concerns live in provider crates built on top.
//! - **Provider-agnostic resolution.** [`PermissionResolver::resolve`] takes the
//!   subject and a *flat* grant list; assembling that list (subject grants ∪
//!   group grants) is the provider's job. [`InMemoryProvider`] ships here for
//!   tests and as the shape for the future config provider.
//! - **No `unsafe`, no I/O, no clock, no threads.** Resolution is a pure
//!   function apart from a shared, lock-protected default lookup in the
//!   registry.
//!
//! ## Matching rules (the crux — see [`PermissionResolver`])
//!
//! 1. A grant is applicable to node `N` iff `N` matches the grant's pattern
//!    (exact, or wildcard: `*` matches everything; `foo.*` matches `foo.bar`
//!    and `foo.bar.baz`, **not** `foo`) and every context on the grant is
//!    present with the same value on the subject (subset match).
//! 2. Applicable grants are ranked by **specificity**: the number of literal
//!    dot-separated segments (a trailing wildcard counts as zero). Highest
//!    specificity wins, regardless of allow/deny and of where the grant came
//!    from.
//! 3. Exact-length ties go to **deny**: an explicit deny at the winning length
//!    always wins over an allow at the same length.
//! 4. No applicable grant → the **registry default** for the node → if
//!    unregistered, **deny** (default-deny).
//!
//! ## Case sensitivity
//!
//! Nodes are validated to be lowercase ASCII at construction; matching is
//! case-sensitive. A node that differs by case cannot exist next to its lowercase
//! twin because construction rejects it.
//!
//! ## Group nesting
//!
//! This crate resolves **only the flat list it is given**. Nested group-in-group
//! membership must be flattened by the provider before resolving; see
//! [`InMemoryProvider::effective_grants`] for the one-level semantics.
//!
//! No resolver-side caching lives here on purpose: caches belong to the proxy
//! layer, which owns the cache infrastructure.
#![forbid(unsafe_code)]

mod context;
mod grant;
mod node;
mod provider;
mod registry;
mod resolver;

pub use context::{
    MAX_CONTEXT_NAME_LEN, MAX_CONTEXT_VALUE_LEN, PermissionContext, PermissionContextError,
    Subject, SubjectKind,
};
pub use grant::{Grant, PermissionValue};
pub use node::{MAX_NODE_LEN, PermissionNode, PermissionNodeError};
pub use provider::InMemoryProvider;
pub use registry::{PermissionRegistry, RegisteredNode, RegistryError};
pub use resolver::{BestMatch, PermissionResolver, Resolution};
