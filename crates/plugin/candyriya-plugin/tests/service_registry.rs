//! Service registry behaviour: register/get/remove/clear, type safety,
//! plugin-owned service revocation.

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use candyriya_plugin::ServiceRegistry;

#[derive(Debug, PartialEq, Eq)]
struct RouteHandle {
    name: String,
}

#[test]
fn register_get_roundtrip_returns_the_same_arc() {
    let registry = ServiceRegistry::new();
    let handle = Arc::new(RouteHandle {
        name: "minecraft.route".to_owned(),
    });

    registry.register_service("minecraft.route".to_owned(), handle.clone());

    let got = registry
        .get_service::<RouteHandle>("minecraft.route")
        .expect("service is present");
    assert!(Arc::ptr_eq(&handle, &got));
    assert_eq!(got.name, "minecraft.route");
}

#[test]
fn get_missing_service_returns_none() {
    let registry = ServiceRegistry::new();
    assert!(
        registry
            .get_service::<RouteHandle>("minecraft.route")
            .is_none()
    );
}

#[test]
fn get_with_wrong_type_returns_none() {
    let registry = ServiceRegistry::new();
    registry.register_service(
        "player.store".to_owned(),
        Arc::new(RouteHandle { name: "x".into() }),
    );

    // stored type is RouteHandle, asked for u64 — no downcast, no service
    assert!(registry.get_service::<u64>("player.store").is_none());
}

#[test]
fn remove_service_returns_some_once_then_none() {
    let registry = ServiceRegistry::new();
    let handle = Arc::new(RouteHandle {
        name: "permissions.resolver".to_owned(),
    });
    registry.register_service("permissions.resolver".to_owned(), handle.clone());

    let removed = registry
        .remove_service::<RouteHandle>("permissions.resolver")
        .expect("first remove yields the service");
    assert!(Arc::ptr_eq(&handle, &removed));

    assert!(
        registry
            .get_service::<RouteHandle>("permissions.resolver")
            .is_none()
    );
    // remove unloaded service returns None
    assert!(
        registry
            .remove_service::<RouteHandle>("permissions.resolver")
            .is_none()
    );
}

#[test]
fn remove_with_wrong_type_leaves_the_entry_alone() {
    let registry = ServiceRegistry::new();
    registry.register_service(
        "config.manager".to_owned(),
        Arc::new(RouteHandle { name: "x".into() }),
    );

    // wrong T: entry must survive — a failed downcast is not a removal
    assert!(registry.remove_service::<u64>("config.manager").is_none());
    assert!(
        registry
            .get_service::<RouteHandle>("config.manager")
            .is_some()
    );

    // right T: removal works
    assert!(
        registry
            .remove_service::<RouteHandle>("config.manager")
            .is_some()
    );
    assert!(
        registry
            .get_service::<RouteHandle>("config.manager")
            .is_none()
    );
}

#[test]
fn clear_drops_every_entry() {
    let registry = ServiceRegistry::new();
    registry.register_service("a".to_owned(), Arc::new(1u64));
    registry.register_service("b".to_owned(), Arc::new(RouteHandle { name: "y".into() }));
    assert_eq!(registry.len(), 2);

    registry.clear();

    assert!(registry.is_empty());
    assert!(registry.get_service::<u64>("a").is_none());
    assert!(registry.get_service::<RouteHandle>("b").is_none());
}

#[test]
fn clones_share_the_same_registry() {
    let registry = ServiceRegistry::new();
    let clone = registry.clone();

    clone.register_service("scheduler.service".to_owned(), Arc::new(99u64));

    assert_eq!(
        registry.get_service::<u64>("scheduler.service"),
        Some(Arc::new(99))
    );
}

#[test]
fn replacing_a_key_returns_the_new_value() {
    let registry = ServiceRegistry::new();
    registry.register_service("forwarding.config".to_owned(), Arc::new("old"));
    registry.register_service("forwarding.config".to_owned(), Arc::new("new"));

    assert_eq!(
        registry
            .get_service::<&'static str>("forwarding.config")
            .as_deref(),
        Some(&"new")
    );
}

#[test]
fn plugin_owned_services_are_revoked_in_bulk() {
    let registry = ServiceRegistry::new();
    registry.register_owned_service("mine.a".to_owned(), Arc::new("a"), "plugin-x");
    registry.register_owned_service("mine.b".to_owned(), Arc::new("b"), "plugin-x");
    registry.register_owned_service("mine.c".to_owned(), Arc::new("c"), "plugin-y");
    registry.register_service("core.shim".to_owned(), Arc::new("core"));

    let removed = registry.remove_services_owned_by("plugin-x");

    assert_eq!(removed, 2);
    assert!(registry.get_service::<&'static str>("mine.a").is_none());
    assert!(registry.get_service::<&'static str>("mine.b").is_none());
    assert_eq!(
        registry.get_service::<&'static str>("mine.c").as_deref(),
        Some(&"c"),
        "services owned by a different plugin stay"
    );
    assert_eq!(
        registry.get_service::<&'static str>("core.shim").as_deref(),
        Some(&"core"),
        "core services are never revoked"
    );
}

#[test]
fn clearing_an_owner_without_entries_is_a_noop() {
    let registry = ServiceRegistry::new();
    registry.register_service("core.shim".to_owned(), Arc::new("core"));

    assert_eq!(registry.remove_services_owned_by("no-such-plugin"), 0);
    assert_eq!(registry.len(), 1);
}

#[test]
fn atomic_counter_service_survives_lookups() {
    let registry = ServiceRegistry::new();
    let counter = Arc::new(AtomicU64::new(0));
    registry.register_service("player.store".to_owned(), counter.clone());

    for _ in 0..10 {
        if let Some(counter) = registry.get_service::<AtomicU64>("player.store") {
            counter.fetch_add(1, Ordering::Relaxed);
        }
    }

    assert_eq!(counter.load(Ordering::Relaxed), 10);
}
