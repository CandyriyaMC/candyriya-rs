//! Read-only snapshots of the live proxy the commands render.
//!
//! The composition root (the cli) implements
//! [`NetworkDirectory::snapshot`](crate::NetworkDirectory::snapshot) over the
//! real proxy state — backend registry, connection tracker, config — so the
//! commands never reach into proxy internals and never talk to the
//! network. Everything a command needs comes from one consistent
//! [`ProxySnapshot`] taken at invocation time.
//!
//! [`NetworkDirectory`]: crate::NetworkDirectory

use std::time::Duration;

/// Connection lifecycle counters reported by `/candyriya info`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ConnectionStats {
    /// Connections currently established.
    pub active: u64,
    /// Total connections accepted since startup.
    pub accepted: u64,
    /// Connections refused before the handshake completed (limits, rate caps).
    pub rejected: u64,
}

/// Health of one backend as of the last health check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BackendState {
    /// Never checked, or the check result is inconclusive.
    #[default]
    Unknown,
    /// Last check answered within the ping timeout.
    Online,
    /// Last check failed or timed out.
    Offline,
}

/// One backend as it appears in status output.
///
/// Richer than [`ServerInfo`](crate::ServerInfo) — this is the status view
/// (`/candyriya info` and `/candyriya servers`); `ServerInfo` carries only
/// the `try`-chain priority, which is what routing order and `/glist` need.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackendSnapshot {
    /// Configured server name (as in `[servers]`).
    pub name: String,
    /// Configured `host:port` address.
    pub address: String,
    /// Health at snapshot time.
    pub state: BackendState,
    /// Players currently routed to this backend.
    pub players: u32,
    /// Configured player cap of this backend.
    pub max_players: u32,
    /// Round trip of the last status ping; `None` when never measured.
    pub ping: Option<Duration>,
}

/// Everything the commands render, one consistent view per invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProxySnapshot {
    /// Proxy version (workspace version).
    pub version: String,
    /// How long the proxy has been up.
    pub uptime: Duration,
    /// Name of the configuration the proxy runs with (path or `embedded defaults`).
    pub config_name: String,
    /// Effective log level string (e.g. `info`, `debug`).
    pub effective_log_level: String,
    /// Connection lifecycle counters.
    pub connections: ConnectionStats,
    /// Players online at snapshot time. Mirrors [`NetworkDirectory::online_players`](crate::NetworkDirectory::online_players); carried here so one invocation sees one consistent view.
    pub players: Vec<crate::OnlinePlayer>,
    /// Configured backends in configuration order.
    pub backends: Vec<BackendSnapshot>,
}

impl ProxySnapshot {
    /// Case-insensitive player lookup by name.
    pub fn player(&self, name: &str) -> Option<&crate::OnlinePlayer> {
        self.players
            .iter()
            .find(|player| player.name.eq_ignore_ascii_case(name))
    }
}
