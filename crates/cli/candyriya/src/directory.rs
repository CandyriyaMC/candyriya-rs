//! [`NetworkDirectory`] implementation for the live proxy.
//!
//! The command layer never touches proxy internals directly, so it asks this
//! directory for a consistent view at invocation time, computed from:
//!
//! - the live [`ConfigStore`] (re-read per call, so `/candyriya reload`
//!   is visible immediately) for the configured backends and their
//!   `try`-chain priority,
//! - the shared proxy state for live connection counters,
//! - the routing registry (when wired) for per-backend health.
//!
//! There is no backend query channel for player counts, so per-backend
//! player numbers are counted from the live session roster (players whose
//! current backend is that one) rather than invented.

use std::sync::Arc;
use std::time::Instant;

use candyriya_command::{
    BackendSnapshot, BackendState as CmdBackendState, ConnectionStats, NetworkDirectory,
    OnlinePlayer, ProxySnapshot, ServerInfo,
};
use candyriya_config::ConfigStore;
use candyriya_proxy::ProxyState;

/// Read-only live view over the proxy for the command layer.
pub struct CliDirectory {
    config: Arc<ConfigStore>,
    state: Arc<ProxyState>,
    config_name: String,
    log_level: String,
    started: Instant,
}

impl std::fmt::Debug for CliDirectory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CliDirectory")
            .field("config", &self.config_name)
            .finish_non_exhaustive()
    }
}

impl CliDirectory {
    /// Build the view from the shared proxy state and the live config store.
    #[must_use]
    pub fn new(
        config: Arc<ConfigStore>,
        state: Arc<ProxyState>,
        config_name: String,
        log_level: String,
        started: Instant,
    ) -> Self {
        Self {
            config,
            state,
            config_name,
            log_level,
            started,
        }
    }
}

impl CliDirectory {
    /// The live roster as command-layer players, sorted by name.
    fn roster_players(&self) -> Vec<OnlinePlayer> {
        self.state
            .roster
            .snapshot()
            .into_iter()
            .map(|entry| OnlinePlayer {
                uuid: entry.uuid,
                name: entry.name,
                server: entry.server,
            })
            .collect()
    }
}

fn route_state(state: Option<candyriya_route::BackendState>) -> CmdBackendState {
    match state {
        Some(candyriya_route::BackendState::Online) => CmdBackendState::Online,
        Some(candyriya_route::BackendState::Offline) => CmdBackendState::Offline,
        _ => CmdBackendState::Unknown,
    }
}

impl NetworkDirectory for CliDirectory {
    fn servers(&self) -> Vec<ServerInfo> {
        // the store hands out a point-in-time snapshot; reload swaps it
        // atomically, so this always reflects the latest committed config
        let config = self.config.get();
        let mut servers: Vec<ServerInfo> = config
            .servers
            .try_servers
            .iter()
            .enumerate()
            .map(|(index, name)| ServerInfo::new(name.clone(), index))
            .collect();
        // servers configured but not present in `try`: listed after the chain,
        // alphabetically, so the output is deterministic
        let mut rest: Vec<&String> = config
            .servers
            .servers
            .keys()
            .filter(|name| !config.servers.try_servers.contains(name))
            .collect();
        rest.sort();
        let base = config.servers.try_servers.len();
        servers.extend(
            rest.into_iter()
                .enumerate()
                .map(|(offset, name)| ServerInfo::new(name.clone(), base + offset)),
        );
        servers
    }

    fn online_players(&self) -> Vec<OnlinePlayer> {
        self.roster_players()
    }

    fn snapshot(&self) -> ProxySnapshot {
        let players = self.roster_players();
        let config = self.config.get();
        let mut backends: Vec<BackendSnapshot> = config
            .servers
            .servers
            .iter()
            .map(|(name, address)| {
                let state = self.state.router().and_then(|router| router.state(name));
                BackendSnapshot {
                    name: name.clone(),
                    address: address.clone(),
                    state: route_state(state),
                    players: players
                        .iter()
                        .filter(|player| player.server.as_deref() == Some(name.as_str()))
                        .count()
                        .try_into()
                        .unwrap_or(u32::MAX),
                    // the backend's own player cap is not observable from
                    // configuration; 0 means "unknown" here
                    max_players: 0,
                    ping: None,
                }
            })
            .collect();
        backends.sort_by(|a, b| a.name.cmp(&b.name));

        ProxySnapshot {
            version: env!("CARGO_PKG_VERSION").to_string(),
            uptime: self.started.elapsed(),
            config_name: self.config_name.clone(),
            effective_log_level: self.log_level.clone(),
            connections: ConnectionStats {
                active: self.state.limiter.current_connections(),
                accepted: self
                    .state
                    .conn_counter
                    .load(std::sync::atomic::Ordering::Relaxed),
                rejected: self
                    .state
                    .connection_rejections
                    .load(std::sync::atomic::Ordering::Relaxed),
            },
            players,
            backends,
        }
    }
}
