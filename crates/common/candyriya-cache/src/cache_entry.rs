//! Cache-like storage: byte payloads keyed by a string, with a TTL.
//!
//! This is a thin, opinionated layer over cachegirl's cache entry
//! resources for data that looks like a cache — think "Mojang profile
//! payloads", not "player state". Entries are keyed per namespace;
//! keys are unique inside a namespace, inserting the same key twice is
//! rejected, and lookups refresh the entry's access time (so an idle
//! TTL only bites entries nobody reads).
//!
//! TTL modes follow cachegirl: [`insert`] gives a fixed TTL counted
//! from creation. For a sliding, read-resetting TTL use
//! [`insert_with_options`] with `CreateOptions::idle_ttl` instead.
//!
//! ```
//! use std::time::Duration;
//! use candyriya_cache::{CacheManager, cache_entry};
//!
//! # fn main() -> candyriya_cache::Result<()> {
//! let dir = std::env::temp_dir().join(format!("candyriya-cache-entry-{}", std::process::id()));
//! let cache = CacheManager::new(&dir)?;
//! let ns = cache.core_cache()?;
//!
//! cache_entry::insert(&ns, "greeting", b"hello".to_vec(), Duration::from_secs(60))?;
//! let entry = cache_entry::get(&ns, "greeting")?.expect("just inserted");
//! assert_eq!(entry.value, b"hello");
//!
//! assert!(cache_entry::remove(&ns, "greeting")?);
//! assert!(cache_entry::get(&ns, "greeting")?.is_none());
//!
//! cache.shutdown()?;
//! let _ = std::fs::remove_dir_all(&dir);
//! # Ok(())
//! # }
//! ```

use std::time::Duration;

use cachegirl::resource::CacheEntry;
use cachegirl::{CacheEntryResource, CreateOptions, Namespace, ResourceKind};

use crate::error::Result;

/// Store `value` under `key` with a fixed TTL counted from creation.
///
/// Fails with a quota error when the payload does not fit the
/// namespace's quota, and with a backend error when the key is already
/// taken — cachegirl keys are unique per namespace.
pub fn insert(
    namespace: &Namespace,
    key: &str,
    value: Vec<u8>,
    ttl: Duration,
) -> Result<CacheEntryResource> {
    Ok(namespace.create_cache_entry_with(key, value, CreateOptions::new().ttl(ttl))?)
}

/// Store `value` under `key` with explicit creation options.
///
/// This is the full-control sibling of [`insert`]: idle TTLs, custom
/// eviction priority, owner tags and free-form registry metadata all
/// go through here.
pub fn insert_with_options(
    namespace: &Namespace,
    key: &str,
    value: Vec<u8>,
    options: CreateOptions,
) -> Result<CacheEntryResource> {
    Ok(namespace.create_cache_entry_with(key, value, options)?)
}

/// Fetch the entry stored under `key`, refreshing its access time.
///
/// Returns `None` when the key does not exist or the entry was already
/// reclaimed by a cleanup pass.
pub fn get(namespace: &Namespace, key: &str) -> Result<Option<CacheEntry>> {
    Ok(namespace.cache_get(key)?)
}

/// Remove the entry stored under `key`.
///
/// Returns `true` when an entry was destroyed, `false` when the key was
/// not there or the entry is pinned by an active lease (it stays
/// registered and moves to `PendingCleanup`).
pub fn remove(namespace: &Namespace, key: &str) -> Result<bool> {
    let mut removed = false;
    // `resources()` is exact-match on this namespace, so we never touch
    // another plugin's entries while hunting for our key
    for meta in namespace.resources()? {
        let is_our_key = meta.kind == ResourceKind::Cache
            && meta.metadata.get("cache_key").map(String::as_str) == Some(key);
        if is_our_key && namespace.release(&meta.id)? {
            removed = true;
        }
    }
    Ok(removed)
}
