#![forbid(unsafe_code)]
//! # candyriya-cache
//!
//! The proxy's single caching surface. Everything the proxy caches —
//! buffer pools, cache-like byte entries, plugin scratch space — goes
//! through this crate, which is a thin, opinionated wrapper over
//! [cachegirl](https://github.com/bejiihiu/cachegirl). No hand-rolled
//! TTL maps exist anywhere in the proxy; cachegirl owns the registry.
//!
//! ## Layout
//!
//! One [`TempManager`] rooted at the proxy's data directory backs two
//! families of namespaces, mirroring the data-dir layout (`<root>` is
//! the directory passed to [`CacheManager::new`]):
//!
//! | namespace         | who                                   |
//! |-------------------|---------------------------------------|
//! | `cache/candyriya` | core (kernel) cache                   |
//! | `temp/candyriya`  | core scratch space                    |
//! | `cache/<plugin>`  | plugin cache                          |
//! | `temp/<plugin>`   | plugin scratch space                  |
//!
//! `<plugin>` is a validated plugin id (`[a-z0-9_.-]{1,32}`, no reserved
//! device names) — see [`validate_plugin_id`]. One namespace per plugin
//! per family, so quotas and cleanup passes are scoped per plugin.
//!
//! Note that cachegirl nests everything under a per-run instance
//! directory: on disk entries live at
//! `<root>/<instance-id>/cache/<plugin>/…`. That level is cachegirl's
//! crash-recovery mechanism (lock files, stale instance reclamation) and
//! is not configurable — do not build paths by hand, use
//! [`Namespace::name`](cachegirl::Namespace::name) instead.
//!
//! ## Quotas
//!
//! [`set_quota_bytes`](CacheManager::set_quota_bytes) and
//! [`set_quota_resources`](CacheManager::set_quota_resources) merge into
//! an existing namespace quota — setting one dimension does not reset
//! the other. Enforcement happens atomically at creation time and
//! surfaces as [`CachegirlError::Quota`] (`SizeExceeded` /
//! `CountExceeded`).
//!
//! ## Buffer pool
//!
//! [`buffer`](CacheManager::buffer) hands out pooled, reusable buffers
//! for hot paths (packet copying, serialization scratch). The pool is
//! deliberately **not** quota-accounted — cachegirl's quotas are built
//! on registry records and pooled buffers are not registered resources.
//! If you need accounted, policy-managed buffers, use
//! [`Namespace::create_buffer`] instead of the pool.
//!
//! ## Shutdown
//!
//! [`shutdown`](CacheManager::shutdown) is idempotent. With the default
//! configuration it waits for no leases and then, because cache and temp
//! data are disposable by design, removes **this instance's** directory
//! (`<root>/<instance-id>`) via cachegirl's own
//! `remove_instance_dir_on_shutdown` mechanism, which is confined to the
//! canonicalized root. The proxy's user data (config, logs, locales,
//! plugins) lives outside the root's instance directories and is never
//! touched; opt out of the deletion with
//! [`CacheManagerBuilder::remove_instance_dir_on_shutdown`].
//!
//! ## Scheduling cleanup
//!
//! cachegirl never sweeps on its own; a pass happens when you call it.
//! Do not spawn ad-hoc timers here — the proxy routes all periodic work
//! through `candyriya-scheduler`'s TaskService, which this crate does
//! not depend on. The wiring (done by the CLI, shown for context):
//!
//! ```text
//! task_service.every(Duration::from_secs(30), "cachegirl.sweep", {
//!     let cache = cache.clone();
//!     move || cache.cleanup_expired_all()
//! });
//! ```
//!
//! Rules of thumb: sweep `cache/*` every 30–60 s, sweep `temp/*` hotter
//! if scratch churns fast. A namespace-scoped pass
//! (`cleanup_expired(CleanupScope::Namespace("temp/some-plugin".into()))`)
//! is the right tool when one plugin needs aggressive reclamation
//! without touching anyone else's. Sweep frequency bounds garbage
//! growth, not correctness — quotas are enforced at creation regardless.
//! If you never call cleanup at all, resources only disappear through
//! explicit release or owning handles dropping.
//!
//! ## Wiring quotas (CLI)
//!
//! The CLI reads per-plugin quotas from its config and applies them
//! once at startup, before any plugin runs:
//!
//! ```
//! use candyriya_cache::CacheManager;
//!
//! # fn main() -> candyriya_cache::Result<()> {
//! # let dir = std::env::temp_dir().join(format!("candyriya-cache-quotas-{}", std::process::id()));
//! // let cache = CacheManager::new(&data_dir)?;
//! let cache = CacheManager::new(&dir)?;
//!
//! // core namespaces get sane fixed budgets (64 MiB each, here)
//! let core_cache = cache.core_cache()?;
//! let core_temp = cache.core_temp()?;
//! cache.set_quota_bytes(&core_cache, 64 * 1024 * 1024);
//! cache.set_quota_bytes(&core_temp, 64 * 1024 * 1024);
//!
//! // per plugin, straight out of the parsed config
//! let plugin = cache.plugin_cache("example.plugin")?;
//! cache.set_quota_bytes(&plugin, 8 * 1024 * 1024);
//! cache.set_quota_resources(&plugin, 1024);
//! assert_eq!(plugin.quota().max_bytes, Some(8 * 1024 * 1024));
//!
//! cache.shutdown()?;
//! let _ = std::fs::remove_dir_all(&dir);
//! # Ok(())
//! # }
//! ```
//!
//! ## Example
//!
//! ```
//! use std::time::Duration;
//! use candyriya_cache::{CacheManager, cache_entry};
//!
//! # fn main() -> candyriya_cache::Result<()> {
//! let dir = std::env::temp_dir().join(format!("candyriya-cache-doc-{}", std::process::id()));
//! let cache = CacheManager::new(&dir)?;
//!
//! let core = cache.core_cache()?;
//! cache_entry::insert(&core, "example", b"hello".to_vec(), Duration::from_secs(60))?;
//! let entry = cache_entry::get(&core, "example")?.expect("just inserted");
//! assert_eq!(entry.value, b"hello");
//! cache_entry::remove(&core, "example")?;
//!
//! let mut buf = cache.buffer(4096);
//! buf.extend_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]);
//! assert_eq!(buf.len(), 4);
//!
//! cache.shutdown()?;
//! let _ = std::fs::remove_dir_all(&dir);
//! # Ok(())
//! # }
//! ```

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

pub mod cache_entry;
mod error;
mod plugin_id;

pub use error::{Error, Result};

// Selective re-exports: the handles and value types callers actually
// name. cachegirl's manager-wide internals (registries, leases,
// recovery policy) stay out so this crate stays the opinionated surface.
pub use cachegirl::Error as CachegirlError;
pub use cachegirl::resource::CacheEntry;
pub use cachegirl::{
    BufferPool, CacheEntryResource, CleanupPolicy, CleanupReport, CleanupScope, CreateOptions,
    Namespace, NamespaceQuota, PooledBuffer, QuotaError, RegistryError, ResourceId, ResourceMeta,
    TempManager, Ttl,
};

pub use plugin_id::validate_plugin_id;

/// The core namespace suffix; cache and temp families both use it.
const CORE_NAMESPACE: &str = "candyriya";

/// Root of every cache/temp operation in the proxy.
///
/// Owns one cachegirl [`TempManager`] (rooted at the proxy data
/// directory) and one [`BufferPool`]. Cheap to wrap in an `Arc` and hand
/// to consumers; `&CacheManager` access is fine because every method
/// here takes shared references.
pub struct CacheManager {
    manager: TempManager,
    pool: BufferPool,
}

impl CacheManager {
    /// Build a manager with the defaults: random instance id, no
    /// cleanup sweep on shutdown, instance directory removed on shutdown
    /// (cache and temp are disposable), zero wait for leases.
    ///
    /// This is what the proxy wants; use [`CacheManager::builder`] to
    /// override any of it.
    pub fn new(root: impl Into<PathBuf>) -> Result<Self> {
        Self::builder().root(root).build()
    }

    /// Start configuring a [`CacheManager`].
    pub fn builder() -> CacheManagerBuilder {
        CacheManagerBuilder::default()
    }

    /// The directory this manager is confined to.
    pub fn root(&self) -> &Path {
        self.manager.root()
    }

    /// This run's instance id (cachegirl prefixes everything with it).
    pub fn instance_id(&self) -> &str {
        self.manager.instance_id()
    }

    /// True after [`shutdown`](Self::shutdown) has run.
    pub fn is_closed(&self) -> bool {
        self.manager.is_closed()
    }

    /// Read-only access to the underlying manager.
    ///
    /// Escape hatch for the CLI and tests; prefer the helpers on this
    /// type, they encode the proxy's layout.
    pub fn manager(&self) -> &TempManager {
        &self.manager
    }

    /// Borrow the shared pooled-buffer arena.
    pub fn pool(&self) -> &BufferPool {
        &self.pool
    }

    /// Take a cleared buffer with at least `size` bytes of capacity.
    ///
    /// The buffer is recycled by the pool when dropped. Not
    /// quota-accounted — see the crate-level docs.
    pub fn buffer(&self, size: usize) -> PooledBuffer {
        self.pool.acquire_with_capacity(size)
    }

    /// Namespace for core (kernel) cache data: `cache/candyriya`.
    pub fn core_cache(&self) -> Result<Arc<Namespace>> {
        Ok(self.manager.namespace(&format!("cache/{CORE_NAMESPACE}"))?)
    }

    /// Namespace for core scratch space: `temp/candyriya`.
    pub fn core_temp(&self) -> Result<Arc<Namespace>> {
        Ok(self.manager.namespace(&format!("temp/{CORE_NAMESPACE}"))?)
    }

    /// Namespace for a plugin's cached data: `cache/<plugin-id>`.
    pub fn plugin_cache(&self, plugin_id: &str) -> Result<Arc<Namespace>> {
        validate_plugin_id(plugin_id)?;
        Ok(self.manager.namespace(&format!("cache/{plugin_id}"))?)
    }

    /// Namespace for a plugin's scratch space: `temp/<plugin-id>`.
    pub fn plugin_temp(&self, plugin_id: &str) -> Result<Arc<Namespace>> {
        validate_plugin_id(plugin_id)?;
        Ok(self.manager.namespace(&format!("temp/{plugin_id}"))?)
    }

    /// Set the byte quota of a namespace, keeping its resource-count
    /// quota (if any) untouched.
    ///
    /// `0` means "no bytes may be stored" — any non-empty payload is
    /// rejected with [`CachegirlError::Quota`]. Creation-time
    /// enforcement, atomically, on top of the manager-wide limits.
    pub fn set_quota_bytes(&self, namespace: &Namespace, bytes: u64) {
        let mut quota = namespace.quota();
        quota.max_bytes = Some(bytes);
        namespace.set_quota(quota);
    }

    /// Set the resource-count quota of a namespace, keeping its byte
    /// quota (if any) untouched.
    ///
    /// `0` means "no resources may be created".
    pub fn set_quota_resources(&self, namespace: &Namespace, resources: u64) {
        let mut quota = namespace.quota();
        quota.max_resources = Some(resources);
        namespace.set_quota(quota);
    }

    /// Run a cleanup pass over every namespace in the manager.
    ///
    /// This is the call the CLI registers with the scheduler (see the
    /// crate docs). Returns the pass report.
    pub fn cleanup_expired_all(&self) -> Result<CleanupReport> {
        Ok(self.manager.cleanup_expired()?)
    }

    /// Run a cleanup pass scoped to one scope.
    ///
    /// `CleanupScope::Manager` is exactly [`cleanup_expired_all`].
    /// `CleanupScope::Namespace(name)` sweeps that namespace and every
    /// namespace registered under it — e.g. `cache` would take the
    /// whole cache branch — and nothing else. Note the namespace is
    /// created if it does not exist yet (namespaces are lazy in
    /// cachegirl), so reusing this to "check" a name is not free.
    ///
    /// [`cleanup_expired_all`]: Self::cleanup_expired_all
    pub fn cleanup_expired(&self, scope: CleanupScope) -> Result<CleanupReport> {
        match scope {
            CleanupScope::Manager => self.cleanup_expired_all(),
            CleanupScope::Namespace(name) => {
                Ok(self.manager.namespace(&name)?.cleanup_expired()?)
            }
        }
    }

    /// Shut the manager down.
    ///
    /// Idempotent; the first call does the work, later calls report an
    /// empty pass. Runs cachegirl's shutdown sequence: blocks new
    /// resource creation, waits for leases if configured, sweeps expired
    /// resources if configured, removes the instance lock file and —
    /// with the default `remove_instance_dir_on_shutdown(true)` —
    /// removes this instance's directory (`<root>/<instance-id>`),
    /// confined to the canonicalized root. Nothing outside the data
    /// root's instance directories is deleted.
    pub fn shutdown(&self) -> Result<CleanupReport> {
        Ok(self.manager.shutdown()?)
    }
}

/// Builder for [`CacheManager`]. Finish with [`CacheManagerBuilder::build`].
pub struct CacheManagerBuilder {
    root: Option<PathBuf>,
    instance_id: Option<String>,
    cleanup_on_shutdown: bool,
    remove_instance_dir_on_shutdown: bool,
    shutdown_wait_leases: Duration,
}

impl Default for CacheManagerBuilder {
    fn default() -> Self {
        Self {
            root: None,
            instance_id: None,
            cleanup_on_shutdown: false,
            remove_instance_dir_on_shutdown: true,
            shutdown_wait_leases: Duration::ZERO,
        }
    }
}

impl CacheManagerBuilder {
    /// Root directory for all cache/temp data (the proxy's data dir).
    /// Required.
    pub fn root(mut self, root: impl Into<PathBuf>) -> Self {
        self.root = Some(root.into());
        self
    }

    /// Explicit instance id; a random one is generated when omitted.
    pub fn instance_id(mut self, id: impl Into<String>) -> Self {
        self.instance_id = Some(id.into());
        self
    }

    /// Sweep expired resources during [`shutdown`](CacheManager::shutdown).
    ///
    /// Default: false. Note that with the default
    /// `remove_instance_dir_on_shutdown(true)` everything disposable is
    /// deleted wholesale at shutdown anyway; this flag is for setups
    /// that keep the instance directory and still want a final sweep.
    pub fn cleanup_on_shutdown(mut self, cleanup: bool) -> Self {
        self.cleanup_on_shutdown = cleanup;
        self
    }

    /// Remove this run's instance directory (`<root>/<instance-id>`)
    /// on shutdown.
    ///
    /// Default: true — cache and temp data are disposable by design,
    /// and this is what keeps per-run instance directories from
    /// accumulating under the data dir. cachegirl confines the removal
    /// to the canonicalized root (the root itself, symlinks to it, or
    /// paths resolving outside it are refused). Set to `false` to keep
    /// instance data across runs; instances from crashed runs can then
    /// be reclaimed by building the manager with cachegirl's
    /// `cleanup_on_start` instead.
    pub fn remove_instance_dir_on_shutdown(mut self, remove: bool) -> Self {
        self.remove_instance_dir_on_shutdown = remove;
        self
    }

    /// How long [`shutdown`](CacheManager::shutdown) waits for active
    /// leases before proceeding. Default: zero (never waits).
    pub fn shutdown_wait_leases(mut self, timeout: Duration) -> Self {
        self.shutdown_wait_leases = timeout;
        self
    }

    /// Validate and build the manager.
    pub fn build(self) -> Result<CacheManager> {
        let root = self.root.ok_or(Error::MissingRoot)?;

        let mut builder = TempManager::builder()
            .root(root)
            .cleanup_on_shutdown(self.cleanup_on_shutdown)
            .remove_instance_dir_on_shutdown(self.remove_instance_dir_on_shutdown)
            .shutdown_wait_leases(self.shutdown_wait_leases);
        if let Some(id) = self.instance_id {
            builder = builder.instance_id(id);
        }

        Ok(CacheManager {
            manager: builder.build()?,
            pool: BufferPool::new(),
        })
    }
}
