//! Shared process-wide handle to the embedded protocol registry.
//!
//! `Registry::embedded()` parses the compiled-in tables on every call,
//! which is far too much work for connection-hot paths; the snapshot is
//! immutable, so it is loaded once and shared by the whole process.
//! `None` is cached too — a broken embedded snapshot is a build bug and
//! failing consistently beats re-parsing on every connection.

use std::sync::OnceLock;

use candyriya_protocol_registry::Registry;

/// The embedded registry, or `None` if the snapshot is invalid.
pub(crate) fn embedded_registry() -> Option<&'static Registry> {
    static REGISTRY: OnceLock<Option<Registry>> = OnceLock::new();
    REGISTRY.get_or_init(|| Registry::embedded().ok()).as_ref()
}
