//! [`NetworkDirectory`] implementation for the live proxy.
//!
//! The command layer never touches proxy internals directly, so it asks this
//! directory for a consistent view at invocation time, computed from:
//!
//! - the `config` unit in the supervision tree, reached with a blocking
//!   IPC hand-off — the command trait is synchronous end to end and runs
//!   on the blocking console thread, so the read is driven on the calling
//!   thread rather than awaited (threading a runtime through brigadier was
//!   judged worse than the hand-off; see [`CliDirectory::live_config`]);
//! - the shared proxy state for live connection counters,
//! - the `route` unit (through the same snapshot IPC the proxy uses) for
//!   per-backend health.
//!
//! The command trait is deliberately infallible, so a total configuration
//! outage cannot be reported as an error: when the fetch fails, the boot
//! configuration is served and the failure is logged loudly — a stale but
//! honest view, never a fabricated empty table. The normal path never
//! serves a stale snapshot: it always asks the unit.
//!
//! There is no backend query channel for player counts, so per-backend
//! player numbers are counted from the live session roster (players whose
//! current backend is that one) rather than invented.

use std::sync::Arc;
use std::time::Instant;

use tracing::error;

use candyriya_command::{
    BackendSnapshot, BackendState as CmdBackendState, ConnectionStats, NetworkDirectory,
    OnlinePlayer, ProxySnapshot, PsConn, PsExitStatus, PsSnapshot, PsUnit, PsUnitKind, PsUnitState,
    ServerInfo,
};
use candyriya_config::Config;
use candyriya_init::{ExitStatus, Init, Pid, UnitKind, UnitState};
use candyriya_proxy::ProxyState;

/// The handles a live directory needs to talk to the supervision tree.
///
/// Grouped so the constructor stays readable; all parts are clones of
/// things the composition root already owns.
pub struct TreeLinks {
    /// The runtime the blocking console thread hands off to for one
    /// invocation. [`tokio::runtime::Handle::block_on`] is only used from
    /// that blocking thread — never from an async context, where it panics.
    pub runtime: tokio::runtime::Handle,
    /// The supervision tree (cloned; cheap).
    pub init: Init,
    /// The `config` unit's pid. Calls attribute to the root; the console
    /// is a unit of its own but its task-local does not reach the blocking
    /// thread, and the root is an ancestor of everything.
    pub config_pid: Pid,
    /// The configuration this process booted with. Served only when the
    /// unit cannot produce a snapshot (see the module docs); never served
    /// alongside a healthy unit.
    pub boot_fallback: Arc<Config>,
}

/// Read-only live view over the proxy for the command layer.
pub struct CliDirectory {
    state: Arc<ProxyState>,
    links: TreeLinks,
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
    /// Build the view from the shared proxy state and the tree handles.
    #[must_use]
    pub fn new(
        state: Arc<ProxyState>,
        links: TreeLinks,
        config_name: String,
        log_level: String,
        started: Instant,
    ) -> Self {
        Self {
            state,
            links,
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

    /// The current configuration, fetched through the tree.
    ///
    /// The fetch parks the calling (blocking console) thread for the
    /// duration of one round trip to the config unit — the same future an
    /// async caller awaits, driven inline. On any failure the boot
    /// configuration is served and the failure is logged loudly: the
    /// command layer cannot return an error, and boot values — while
    /// possibly stale — are real configuration, unlike a fabricated empty
    /// table.
    fn live_config(&self) -> Arc<Config> {
        match self
            .links
            .runtime
            .block_on(candyriya_proxy::config_unit::config(
                &self.links.init,
                Pid::INIT,
                self.links.config_pid,
            )) {
            Ok(snapshot) => snapshot,
            Err(error) => {
                error!(%error, "configuration unit unavailable for command output; serving the boot configuration");
                Arc::clone(&self.links.boot_fallback)
            }
        }
    }

    /// The routing snapshot from the `route` unit, as name → state.
    ///
    /// Same hand-off as [`CliDirectory::live_config`]. On any failure every
    /// backend reads as `Unknown` — the command layer cannot return an
    /// error, and an honest "unknown" is not a fabricated health verdict.
    fn live_backend_states(&self) -> Vec<candyriya_route::BackendSnapshot> {
        match self
            .links
            .runtime
            .block_on(self.state.route(candyriya_route::RouteMsg::Snapshot))
        {
            Ok(candyriya_route::RouteResp::Snapshot(snapshot)) => snapshot,
            Ok(other) => {
                error!(reply = ?other, "unexpected route unit reply to a Snapshot call; backend states reported as unknown");
                Vec::new()
            }
            Err(error) => {
                error!(%error, "route unit unavailable for command output; backend states reported as unknown");
                Vec::new()
            }
        }
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
        let config = self.live_config();
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
        let config = self.live_config();
        let health = self.live_backend_states();
        let mut backends: Vec<BackendSnapshot> = config
            .servers
            .servers
            .iter()
            .map(|(name, address)| {
                let state = health
                    .iter()
                    .find(|entry| entry.name() == name.as_str())
                    .map(candyriya_route::BackendSnapshot::state);
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

    fn ps_snapshot(&self) -> PsSnapshot {
        // reads straight off the registry; no unit calls, no IPC — the
        // operator view is allowed to look at the kernel's books directly
        let units = self.links.init.units().into_iter().map(unit_info).collect();
        let conns = self
            .links
            .init
            .connections()
            .into_iter()
            .map(|counters| PsConn {
                pid: counters.pid.as_u64(),
                bytes_rx: counters.bytes_rx,
                bytes_tx: counters.bytes_tx,
                connected_at: counters.connected_at,
                last_activity: counters.last_activity,
            })
            .collect();
        let snapshot = self.links.init.snapshot();
        PsSnapshot {
            units,
            conns,
            zombies: snapshot.zombies as u64,
            stuck_zombies: snapshot.stuck_zombies as u64,
        }
    }
}

fn ps_kind(kind: UnitKind) -> PsUnitKind {
    match kind {
        UnitKind::Oneshot => PsUnitKind::Oneshot,
        UnitKind::LongLived => PsUnitKind::LongLived,
        UnitKind::Every => PsUnitKind::Every,
        UnitKind::Connection => PsUnitKind::Connection,
        UnitKind::Plugin => PsUnitKind::Plugin,
    }
}

fn ps_state(state: UnitState) -> PsUnitState {
    match state {
        UnitState::Running => PsUnitState::Running,
        UnitState::Zombie => PsUnitState::Zombie,
        UnitState::Restarting => PsUnitState::Restarting,
        UnitState::FailedTerminal => PsUnitState::FailedTerminal,
    }
}

fn ps_exit(status: &ExitStatus) -> PsExitStatus {
    match status {
        ExitStatus::Success => PsExitStatus::Success,
        ExitStatus::Failure(message) => PsExitStatus::Failure(message.clone()),
        ExitStatus::Panicked => PsExitStatus::Panicked,
        ExitStatus::Cancelled => PsExitStatus::Cancelled,
        ExitStatus::TimedOut => PsExitStatus::TimedOut,
    }
}

fn unit_info(unit: candyriya_init::UnitInfo) -> PsUnit {
    PsUnit {
        pid: unit.pid.as_u64(),
        // Pid::NONE (the root's missing parent) renders as 0; the table's
        // "0 means none" convention lives in one place
        ppid: if unit.ppid == Pid::NONE {
            0
        } else {
            unit.ppid.as_u64()
        },
        name: unit.name,
        kind: ps_kind(unit.kind),
        state: ps_state(unit.state),
        parked: unit.parked,
        started_at: unit.started_at,
        restart_count: unit.restart_count,
        last_exit: unit.last_exit.as_ref().map(ps_exit),
        exited_at: unit.exited_at,
    }
}
