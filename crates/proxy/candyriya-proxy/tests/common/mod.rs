//! Shared fixtures for the integration tests: wiring a live `config` unit
//! the way the composition root does, against the real supervision tree.
//! No fakes, no mocks — the unit under test is exactly the one production
//! spawns.
#![allow(dead_code)] // not every test binary uses every helper

use std::sync::Arc;

use candyriya_config::{Config, ConfigStore};
use candyriya_init::{Init, Pid};
use candyriya_scheduler::TaskService;

use candyriya_proxy::{ProxyState, config_spec};

/// Spawn a live config unit holding `config`; returns the tree handle and
/// the unit's pid.
fn spawn_config(config: Config) -> (Init, Pid) {
    let service = TaskService::builder()
        .handle(tokio::runtime::Handle::current())
        .blocking_threads(1)
        .build()
        .expect("task service builds");
    let init = Init::new(service);
    let store = Arc::new(ConfigStore::new(config));
    let pid = init
        .spawn_unit(Pid::INIT, config_spec(store))
        .expect("config unit spawns");
    (init, pid)
}

/// A state wired to a live config unit over a real tree.
pub fn state(config: Config) -> ProxyState {
    let (init, pid) = spawn_config(config);
    ProxyState::new(init, pid)
}

/// Like [`state`], but hands back the tree handle and the config pid so a
/// test can cancel the unit and exercise the callers' failure paths.
pub fn state_with(config: Config) -> (ProxyState, Init, Pid) {
    let (init, pid) = spawn_config(config);
    (ProxyState::new(init.clone(), pid), init, pid)
}

/// [`state`] with an explicit per-IP cap.
pub fn state_with_limits(config: Config, per_ip_max: u64) -> ProxyState {
    let (init, pid) = spawn_config(config);
    ProxyState::with_limits(init, pid, per_ip_max)
}
