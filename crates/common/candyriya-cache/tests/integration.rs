//! Integration tests for candyriya-cache.
//!
//! Every test gets its own dedicated root under the OS temp dir and
//! deletes it on drop, so no instance directories or cache data are
//! left behind.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use cachegirl::{Error as CachegirlError, QuotaError, RegistryError};

use candyriya_cache::{CacheManager, CleanupScope, Error, cache_entry, validate_plugin_id};

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// A unique temp root that removes itself (and everything under it) on
/// drop, even if the test panics.
struct TestRoot(PathBuf);

impl TestRoot {
    fn new() -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "candyriya-cache-test-{}-{nanos:x}-{seq}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("test root must be creatable");
        Self(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn core_namespaces_work_under_one_root() {
    let root = TestRoot::new();
    let cache = CacheManager::new(root.path()).expect("manager builds");

    let core_cache = cache.core_cache().expect("core cache ns");
    let core_temp = cache.core_temp().expect("core temp ns");
    assert_eq!(core_cache.name(), "cache/candyriya");
    assert_eq!(core_temp.name(), "temp/candyriya");
    // same call, same namespace object
    let again = cache.core_cache().expect("same ns again");
    assert!(std::sync::Arc::ptr_eq(&core_cache, &again));

    cache_entry::insert(
        &core_cache,
        "shared",
        b"cache-payload".to_vec(),
        Duration::from_secs(60),
    )
    .expect("insert into cache ns");
    cache_entry::insert(
        &core_temp,
        "shared",
        b"temp-payload".to_vec(),
        Duration::from_secs(60),
    )
    .expect("insert into temp ns");

    assert_eq!(
        cache_entry::get(&core_cache, "shared")
            .unwrap()
            .unwrap()
            .value,
        b"cache-payload"
    );
    assert_eq!(
        cache_entry::get(&core_temp, "shared")
            .unwrap()
            .unwrap()
            .value,
        b"temp-payload"
    );

    cache.shutdown().expect("shutdown");
    assert!(
        root.path()
            .read_dir()
            .expect("root readable")
            .next()
            .is_none()
    );
}

#[test]
fn plugin_namespaces_are_isolated() {
    let root = TestRoot::new();
    let cache = CacheManager::new(root.path()).expect("manager builds");

    let plugin_a = cache.plugin_cache("plugin-a").expect("plugin a ns");
    let plugin_b = cache.plugin_cache("plugin-b").expect("plugin b ns");
    assert_eq!(plugin_a.name(), "cache/plugin-a");
    assert_eq!(plugin_b.name(), "cache/plugin-b");

    let resource = cache_entry::insert(
        &plugin_a,
        "secret",
        b"hidden".to_vec(),
        Duration::from_secs(60),
    )
    .expect("insert into a");

    // plugin b cannot see plugin a's entry…
    assert!(cache_entry::get(&plugin_b, "secret").unwrap().is_none());

    // …and cannot release plugin a's resource either:
    // cachegirl hides other namespaces' ids behind NotFound
    let err = plugin_b.release(resource.id()).unwrap_err();
    assert!(
        matches!(err, CachegirlError::Registry(RegistryError::NotFound(_))),
        "expected NotFound, got {err:?}"
    );

    // …and b can still store its own entry under the same key
    cache_entry::insert(
        &plugin_b,
        "secret",
        b"b-local".to_vec(),
        Duration::from_secs(60),
    )
    .expect("same key in another namespace is fine");

    cache.shutdown().expect("shutdown");
}

#[test]
fn quota_bytes_are_enforced_at_creation() {
    let root = TestRoot::new();
    let cache = CacheManager::new(root.path()).expect("manager builds");

    let ns = cache.plugin_cache("quota-bytes").expect("ns");
    cache.set_quota_bytes(&ns, 8 * 1024);
    assert_eq!(ns.quota().max_bytes, Some(8 * 1024));

    // 7 KB fits
    cache_entry::insert(&ns, "a", vec![0u8; 7 * 1024], Duration::from_secs(60))
        .expect("7 KB fits under 8 KB");
    // exactly 8 KB in total still fits (boundary is allowed)
    cache_entry::insert(&ns, "b", vec![0u8; 1024], Duration::from_secs(60))
        .expect("exactly 8 KB fits");

    // one more byte overflows
    let err = match cache_entry::insert(&ns, "c", vec![0u8; 1], Duration::from_secs(60)) {
        Ok(_) => panic!("the byte past the quota must be rejected"),
        Err(err) => err,
    };
    match err {
        Error::Cachegirl(CachegirlError::Quota(QuotaError::SizeExceeded { limit, requested })) => {
            assert_eq!(limit, 8 * 1024);
            assert_eq!(requested, 8 * 1024 + 1);
        }
        other => panic!("expected SizeExceeded, got {other:?}"),
    }

    // the rejected insert left the accounting untouched
    let (bytes, count) = ns.usage().expect("usage");
    assert_eq!(bytes, 8 * 1024);
    assert_eq!(count, 2);

    cache.shutdown().expect("shutdown");
}

#[test]
fn quota_count_is_enforced_at_creation() {
    let root = TestRoot::new();
    let cache = CacheManager::new(root.path()).expect("manager builds");

    let ns = cache.plugin_cache("quota-count").expect("ns");
    cache.set_quota_resources(&ns, 2);
    // count helper keeps the byte quota untouched (and vice versa)
    assert_eq!(ns.quota().max_resources, Some(2));
    assert_eq!(ns.quota().max_bytes, None);

    cache_entry::insert(&ns, "a", b"x".to_vec(), Duration::from_secs(60)).expect("one");
    cache_entry::insert(&ns, "b", b"x".to_vec(), Duration::from_secs(60)).expect("two");
    let err = match cache_entry::insert(&ns, "c", b"x".to_vec(), Duration::from_secs(60)) {
        Ok(_) => panic!("third resource must be rejected"),
        Err(err) => err,
    };
    match err {
        Error::Cachegirl(CachegirlError::Quota(QuotaError::CountExceeded { limit })) => {
            assert_eq!(limit, 2);
        }
        other => panic!("expected CountExceeded, got {other:?}"),
    }

    cache.shutdown().expect("shutdown");
}

#[test]
fn buffer_pool_allocates_and_recycles() {
    let root = TestRoot::new();
    let cache = CacheManager::new(root.path()).expect("manager builds");

    let pointer_before;
    {
        let mut buf = cache.buffer(1024);
        assert!(buf.capacity() >= 1024);
        buf.extend_from_slice(&[0xAB; 512]);
        assert_eq!(buf.len(), 512);
        pointer_before = buf.as_slice().as_ptr();
    }
    // checked in on drop
    assert_eq!(cache.pool().pooled(), 1);

    {
        let buf = cache.buffer(1024);
        assert!(buf.capacity() >= 1024);
        assert!(buf.is_empty(), "returned buffer comes back cleared");
        // the pool handed out the very same allocation
        assert_eq!(buf.as_slice().as_ptr(), pointer_before);
    }

    cache.shutdown().expect("shutdown");
}

#[test]
fn temp_and_cache_namespaces_both_work() {
    let root = TestRoot::new();
    let cache = CacheManager::new(root.path()).expect("manager builds");

    let temp_plugin = cache.plugin_temp("scratchy").expect("plugin temp ns");
    let cache_plugin = cache.plugin_cache("scratchy").expect("plugin cache ns");
    assert_eq!(temp_plugin.name(), "temp/scratchy");
    assert_eq!(cache_plugin.name(), "cache/scratchy");

    cache_entry::insert(
        &temp_plugin,
        "blob",
        b"scratch".to_vec(),
        Duration::from_secs(60),
    )
    .expect("temp insert");
    cache_entry::insert(
        &cache_plugin,
        "blob",
        b"cached".to_vec(),
        Duration::from_secs(60),
    )
    .expect("cache insert");

    assert_eq!(
        cache_entry::get(&temp_plugin, "blob")
            .unwrap()
            .unwrap()
            .value,
        b"scratch"
    );
    assert_eq!(
        cache_entry::get(&cache_plugin, "blob")
            .unwrap()
            .unwrap()
            .value,
        b"cached"
    );

    cache.shutdown().expect("shutdown");
}

#[test]
fn cleanup_expired_removes_zero_ttl_entries() {
    let root = TestRoot::new();
    let cache = CacheManager::new(root.path()).expect("manager builds");

    let core_cache = cache.core_cache().expect("core cache ns");
    let core_temp = cache.core_temp().expect("core temp ns");

    // a zero TTL is expired the moment it is registered — no sleeping needed
    cache_entry::insert(&core_cache, "perishable", b"x".to_vec(), Duration::ZERO)
        .expect("insert perishable cache entry");
    cache_entry::insert(&core_temp, "perishable", b"x".to_vec(), Duration::ZERO)
        .expect("insert perishable temp entry");

    // scoped sweep touches only the cache branch
    let report = cache
        .cleanup_expired(CleanupScope::Namespace("cache/candyriya".to_string()))
        .expect("scoped cleanup");
    assert_eq!(report.removed, 1);
    assert!(
        cache_entry::get(&core_cache, "perishable")
            .unwrap()
            .is_none()
    );
    assert!(
        cache_entry::get(&core_temp, "perishable")
            .unwrap()
            .is_some(),
        "scoped sweep must not touch the temp branch"
    );

    // manager-wide sweep reclaims the rest
    let report = cache.cleanup_expired_all().expect("manager cleanup");
    assert_eq!(report.removed, 1);
    assert!(
        cache_entry::get(&core_temp, "perishable")
            .unwrap()
            .is_none()
    );

    cache.shutdown().expect("shutdown");
}

#[test]
fn shutdown_is_idempotent() {
    let root = TestRoot::new();
    let cache = CacheManager::new(root.path()).expect("manager builds");

    cache.core_cache().expect("materialize a namespace");
    let first = cache.shutdown().expect("first shutdown");
    let second = cache.shutdown().expect("second shutdown is a calm no-op");
    assert_eq!(second.scanned, 0);
    assert!(cache.is_closed());
    // creating namespaces after shutdown fails
    assert!(cache.core_cache().is_err());
    // reports still came back fine
    let _ = first;
}

#[test]
fn shutdown_removes_the_instance_directory() {
    let root = TestRoot::new();
    let cache = CacheManager::new(root.path()).expect("manager builds");

    let instance = cache.instance_id().to_string();
    cache.core_cache().expect("materialize namespaces");
    assert!(root.path().join(&instance).exists());

    cache.shutdown().expect("shutdown");
    assert!(
        !root.path().join(&instance).exists(),
        "instance directory must be gone after shutdown"
    );
    // the data root itself is kept — it may hold other instances' dirs
    assert!(root.path().exists());
}

#[test]
fn invalid_plugin_ids_are_rejected() {
    let root = TestRoot::new();
    let cache = CacheManager::new(root.path()).expect("manager builds");

    for id in ["", "Bad", "bad id", "a/b", "nul", "com1", &"x".repeat(33)] {
        let err = match cache.plugin_cache(id) {
            Ok(_) => panic!("{id} should be rejected as a plugin id"),
            Err(err) => err,
        };
        assert!(
            matches!(err, Error::InvalidPluginId { .. }),
            "{id} should be rejected as a plugin id, got {err:?}"
        );
        assert!(matches!(
            cache.plugin_temp(id),
            Err(Error::InvalidPluginId { .. })
        ));
    }

    // and the happy path keeps working afterwards
    assert!(cache.plugin_cache("ok-plugin.2").is_ok());

    assert!(validate_plugin_id("lobby").is_ok());

    cache.shutdown().expect("shutdown");
}

#[test]
fn cache_entry_remove_flow() {
    let root = TestRoot::new();
    let cache = CacheManager::new(root.path()).expect("manager builds");
    let ns = cache.core_cache().expect("core cache ns");

    cache_entry::insert(&ns, "doomed", b"bye".to_vec(), Duration::from_secs(60)).expect("insert");
    assert!(cache_entry::remove(&ns, "doomed").expect("remove"));
    assert!(cache_entry::get(&ns, "doomed").unwrap().is_none());
    // removing a missing key reports false, not an error
    assert!(!cache_entry::remove(&ns, "doomed").expect("second remove"));

    // duplicate keys are rejected
    cache_entry::insert(&ns, "dupe", b"one".to_vec(), Duration::from_secs(60))
        .expect("first insert");
    assert!(cache_entry::insert(&ns, "dupe", b"two".to_vec(), Duration::from_secs(60)).is_err());

    cache.shutdown().expect("shutdown");
}
