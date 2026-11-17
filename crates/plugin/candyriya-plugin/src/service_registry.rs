//! A process-wide, type-safe service registry.
//!
//! The registry exists for one thing today: services owned by plugins. A
//! plugin that publishes a service registers it under a key and tags it with
//! its own id; when that plugin unloads or hot-reloads, its entries are
//! revoked in bulk ([`ServiceRegistry::remove_services_owned_by`]). Keys are
//! dotted, lowercase, namespaced strings; the registry itself does not
//! enforce the shape, misuse shows up as a lookup miss.
//!
//! Cross-subsystem services used to live here too; they are dissolved into
//! supervised units in the supervision tree, and host code reaches them
//! over the capability IPC plane instead. What remains here is the
//! plugin-owned revocation machinery — unchanged in shape and semantics.
//!
//! # Semantics
//!
//! - Registering a key that is already taken replaces the old value and
//!   logs a warning — re-registration is how the composition root refreshes
//!   a service, not an error.
//! - [`ServiceRegistry::get_service`] returns `None` when the key is absent
//!   or the stored value is not a `T`.
//! - [`ServiceRegistry::remove_service`] only removes the entry when it
//!   downcasts to `T`; a type mismatch leaves the entry alone and returns
//!   `None`. A removed service is not recoverable through this registry.
//! - [`ServiceRegistry::clear`] drops every entry.
//!
//! # Plugin-owned services
//!
//! Services may be tagged with the id of the plugin that registered them
//! ([`ServiceRegistry::register_owned_service`]). When that plugin is
//! unloaded or hot-reloaded, [`ServiceRegistry::remove_services_owned_by`]
//! revokes exactly those entries — a dead plugin must not leave live
//! handles in the registry. Core services registered with
//! [`ServiceRegistry::register_service`] are never affected.
//!
//! The registry is cheap to clone (an `Arc` clone); every clone refers to
//! the same underlying map. Build one at composition time and hand clones
//! to whoever needs it.

use std::any::Any;
use std::sync::Arc;

use dashmap::DashMap;
use dashmap::mapref::entry::Entry;

struct ServiceEntry {
    value: Arc<dyn Any + Send + Sync>,
    owner: Option<String>,
}

/// A concurrent, type-safe registry of long-lived proxy services.
#[derive(Clone, Default)]
pub struct ServiceRegistry {
    inner: Arc<DashMap<String, ServiceEntry>>,
}

impl std::fmt::Debug for ServiceRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServiceRegistry")
            .field("entries", &self.inner.len())
            .finish()
    }
}

impl ServiceRegistry {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// The number of entries currently in the registry.
    pub fn len(&self) -> usize {
        self.inner.len()
    }

    /// Whether the registry holds no entries at all.
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    /// Register `value` under `key`, replacing any existing entry.
    ///
    /// Ownership is tagged to no plugin; core services use this. Replacing
    /// an existing entry logs a `warn` — that is almost always the
    /// composition root refreshing a service, not a bug.
    pub fn register_service(&self, key: String, value: Arc<dyn Any + Send + Sync>) {
        self.register(key, value, None)
    }

    /// Register `value` under `key`, tagged as owned by plugin `owner`.
    ///
    /// Plugin-owned services are revoked in bulk by
    /// [`ServiceRegistry::remove_services_owned_by`] when the owning plugin
    /// is unloaded or hot-reloaded. Replacing an existing entry behaves
    /// exactly like [`ServiceRegistry::register_service`].
    pub fn register_owned_service(
        &self,
        key: String,
        value: Arc<dyn Any + Send + Sync>,
        owner: &str,
    ) {
        self.register(key, value, Some(owner.to_owned()))
    }

    fn register(&self, key: String, value: Arc<dyn Any + Send + Sync>, owner: Option<String>) {
        if let Some(previous) = self
            .inner
            .insert(key.clone(), ServiceEntry { value, owner })
        {
            tracing::warn!(
                service = %key,
                previous_owner = ?previous.owner,
                "replaced an already-registered service",
            );
        }
    }

    /// Look up the service registered under `key`, downcast to `T`.
    ///
    /// Returns `None` when the key is absent or the stored value is not a
    /// `T`. The returned handle keeps the service alive independently of
    /// the registry.
    pub fn get_service<T: Any + Send + Sync>(&self, key: &str) -> Option<Arc<T>> {
        let entry = self.inner.get(key)?;
        Arc::<dyn Any + Send + Sync>::clone(&entry.value)
            .downcast()
            .ok()
    }

    /// Remove the service registered under `key`, downcast to `T`.
    ///
    /// The entry is only removed when it downcasts to `T`; on a type
    /// mismatch the entry is left untouched and `None` is returned.
    pub fn remove_service<T: Any + Send + Sync>(&self, key: &str) -> Option<Arc<T>> {
        match self.inner.entry(key.to_string()) {
            Entry::Occupied(occupied) => {
                occupied.get().value.downcast_ref::<T>()?;
                let (_, entry) = occupied.remove_entry();
                Arc::downcast::<T>(entry.value).ok()
            }
            Entry::Vacant(_) => None,
        }
    }

    /// Remove every service tagged with `owner`. Returns how many were
    /// removed. Unowned services are never touched.
    pub fn remove_services_owned_by(&self, owner: &str) -> usize {
        let keys: Vec<String> = self
            .inner
            .iter()
            .filter(|entry| entry.owner.as_deref() == Some(owner))
            .map(|entry| entry.key().clone())
            .collect();
        let mut removed = 0;
        for key in keys {
            if self.inner.remove(&key).is_some() {
                removed += 1;
            }
        }
        removed
    }

    /// Drop every entry from the registry.
    pub fn clear(&self) {
        self.inner.clear();
    }
}
