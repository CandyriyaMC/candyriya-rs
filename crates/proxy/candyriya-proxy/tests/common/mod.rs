//! Shared fixtures for the integration tests: wiring a live `config` and
//! `route` unit the way the composition root does, against the real supervision
//! tree. No fakes, no mocks — the units under test are exactly the ones
//! production spawns. The `config` unit owns the live configuration, the
//! `route` unit serves routing from the same shared store the reload path
//! swaps through; both are children of the root here, like in production.
#![allow(dead_code)] // not every test binary uses every helper

use std::sync::Arc;

use candyriya_config::{Config, ConfigStore};
use candyriya_init::{Init, Pid};
use candyriya_route::{RouterStore, route_spec, router_store};
use candyriya_scheduler::TaskService;

use candyriya_proxy::{ProxyState, config_spec};

/// Spawn a live `config` and `route` unit holding `config`, sharing one
/// router store with the caller; returns the tree handle, both unit pids and
/// the store.
fn spawn_units(config: Config) -> (Init, Pid, Pid, RouterStore) {
    let service = TaskService::builder()
        .handle(tokio::runtime::Handle::current())
        .blocking_threads(1)
        .build()
        .expect("task service builds");
    let init = Init::new(service);
    let store = Arc::new(ConfigStore::new(config));
    let config_pid = init
        .spawn_unit(Pid::INIT, config_spec(store))
        .expect("config unit spawns");
    let router_store = router_store(None);
    let route_pid = init
        .spawn_unit(Pid::INIT, route_spec(Arc::clone(&router_store)))
        .expect("route unit spawns");
    (init, config_pid, route_pid, router_store)
}

/// A state wired to a live `config` and `route` unit over a real tree.
pub fn state(config: Config) -> ProxyState {
    let (init, config_pid, route_pid, store) = spawn_units(config);
    ProxyState::new(init, config_pid, route_pid, store)
}

/// Like [`state`], but also hands back the tree handle and both unit pids so
/// a test can cancel them and exercise the callers' failure paths.
pub fn state_with(config: Config) -> (ProxyState, Init, Pid, Pid) {
    let (init, config_pid, route_pid, store) = spawn_units(config);
    (
        ProxyState::new(init.clone(), config_pid, route_pid, store),
        init,
        config_pid,
        route_pid,
    )
}

/// [`state`] with an explicit per-IP cap.
pub fn state_with_limits(config: Config, per_ip_max: u64) -> ProxyState {
    let (init, config_pid, route_pid, store) = spawn_units(config);
    ProxyState::with_limits(init, config_pid, route_pid, store, per_ip_max)
}
