//! Shared fixtures for the integration tests: wiring a live `config` and
//! `route` unit the way the composition root does, against the real supervision
//! tree. No fakes, no mocks — the units under test are exactly the ones
//! production spawns. The `config` unit owns the live configuration, the
//! `route` unit serves routing from the same shared store the reload path
//! swaps through; both are children of the root here, like in production.
//! A `players` group unit rides the same tree so connection units hang
//! where they hang in production — under it, never directly on the root.
#![allow(dead_code)] // not every test binary uses every helper

use std::path::Path;
use std::sync::Arc;

use candyriya_cache::CacheManager;
use candyriya_cache::cache_spec;
use candyriya_config::{Config, ConfigStore};
use candyriya_init::{ExitStatus, Init, Pid, RestartPolicy, UnitKind, UnitSpec};
use candyriya_route::{RouterStore, route_spec, router_store};
use candyriya_scheduler::TaskService;

use candyriya_proxy::{ProxyState, config_spec};

/// Spawn the `players` group unit parented to the root, holding the grants
/// connection units may inherit at spawn. The composition root spawns it at
/// boot with the same shape; tests pass the pids that exist in the fixture.
pub(crate) fn spawn_players(init: &Init, grants: &[Pid]) -> Pid {
    let mut spec = UnitSpec::new(
        "players",
        UnitKind::LongLived,
        RestartPolicy::Never,
        std::future::pending::<ExitStatus>,
    );
    for &grant in grants {
        spec = spec.with_grant(grant);
    }
    init.spawn_unit(Pid::INIT, spec)
        .expect("players unit spawns")
}

/// Spawn a live `config` and `route` unit holding `config`, sharing one
/// router store with the caller; returns the tree handle, both unit pids and
/// the store. The `players` group unit is spawned as well so connection
/// units spawn the way they do in production; its pid is returned too.
fn spawn_units(config: Config) -> (Init, Pid, Pid, Pid, RouterStore) {
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
    let players_pid = spawn_players(&init, &[config_pid, route_pid]);
    (init, config_pid, route_pid, players_pid, router_store)
}

/// A state wired to a live `config` and `route` unit over a real tree.
pub fn state(config: Config) -> ProxyState {
    let (init, config_pid, route_pid, players_pid, store) = spawn_units(config);
    ProxyState::new(init, config_pid, route_pid, players_pid, store)
}

/// Like [`state`], but also hands back the tree handle and both unit pids so
/// a test can cancel them and exercise the callers' failure paths.
pub fn state_with(config: Config) -> (ProxyState, Init, Pid, Pid) {
    let (init, config_pid, route_pid, players_pid, store) = spawn_units(config);
    (
        ProxyState::new(init.clone(), config_pid, route_pid, players_pid, store),
        init,
        config_pid,
        route_pid,
    )
}

/// Like [`state_with`], but also hands back the `players` group unit's pid
/// so a test can pin down which unit connection units hang under.
pub fn state_with_players(config: Config) -> (ProxyState, Init, Pid) {
    let (init, config_pid, route_pid, players_pid, store) = spawn_units(config);
    (
        ProxyState::new(init.clone(), config_pid, route_pid, players_pid, store),
        init,
        players_pid,
    )
}

/// [`state`] with an explicit per-IP cap.
pub fn state_with_limits(config: Config, per_ip_max: u64) -> ProxyState {
    let (init, config_pid, route_pid, players_pid, store) = spawn_units(config);
    ProxyState::with_limits(init, config_pid, route_pid, players_pid, store, per_ip_max)
}

/// A state wired to a live `config`, `route` and `cache` unit over one
/// tree — the way the composition root wires them — with a fresh cache
/// root under `cache_root`.
///
/// Returns the state, the tree handle, the `cache` unit's pid and the
/// manager so a test can both wire a `FaviconCache` to the unit and shut
/// the manager down at the end. The unit owns one clone of the manager,
/// exactly like production. The `players` group unit holds the `cache`
/// grant too, the same way it does in production.
pub fn state_with_cache(
    config: Config,
    cache_root: &Path,
) -> (ProxyState, Init, Pid, Arc<CacheManager>) {
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
    let manager = Arc::new(CacheManager::new(cache_root).expect("cache manager"));
    let cache_pid = init
        .spawn_unit(Pid::INIT, cache_spec(Arc::clone(&manager)))
        .expect("cache unit spawns");
    let players_pid = spawn_players(&init, &[config_pid, route_pid, cache_pid]);
    (
        ProxyState::new(
            init.clone(),
            config_pid,
            route_pid,
            players_pid,
            router_store,
        ),
        init,
        cache_pid,
        manager,
    )
}
