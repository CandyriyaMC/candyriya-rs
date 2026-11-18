//! Shared state injected at command construction.
//!
//! Commands in this crate hold no connection state of their own; everything
//! they need arrives as handles at construction time:
//!
//! - the [`Locales`] pair, always from `candyriya-i18n` (never a hardcoded
//!   string outside this crate's MiniMessage locale files);
//! - a read-only view over the proxy's configured servers, online players,
//!   status snapshot and supervision tree ([`NetworkDirectory`]);
//! - the effects sink ([`ProxyEffects`](crate::ProxyEffects)), which doubles
//!   as the single output path
//!   ([`OutputSender`](crate::OutputSender)).
//!
//! The proxy implements both traits. Tests provide deterministic fixtures, so
//! these commands never talk to a network.

use std::sync::Arc;

use uuid::Uuid;

use crate::effects::ProxyEffects;
use crate::locales::Locales;
use crate::snapshot::ProxySnapshot;

/// A snapshot of one configured backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerInfo {
    /// The configured server name (as in `[servers]`).
    pub name: String,
    /// Index into the `try` chain (0 = first target on a fresh connect).
    ///
    /// Used for `/glist`'s priority coloring; the proxy's routing registry
    /// knows the order and reports it here.
    pub priority: usize,
}

impl ServerInfo {
    /// Creates a server snapshot from its parts.
    pub fn new(name: impl Into<String>, priority: usize) -> Self {
        Self {
            name: name.into(),
            priority,
        }
    }
}

/// A snapshot of one online player.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OnlinePlayer {
    /// The player's unique id.
    pub uuid: Uuid,
    /// The player's name, as registered by the session layer (not validated
    /// here).
    pub name: String,
    /// The backend the player is currently on, if any.
    ///
    /// `None` while the player is between servers; such players count
    /// towards the network total but have no per-backend line of their own.
    pub server: Option<String>,
}

impl OnlinePlayer {
    /// Creates a player snapshot from its parts.
    pub fn new(uuid: Uuid, name: impl Into<String>, server: Option<String>) -> Self {
        Self {
            uuid,
            name: name.into(),
            server,
        }
    }
}

/// Read-only view over the proxy's routing registry and session state.
///
/// Implemented by the proxy; the command layer never touches config or the
/// session map directly. Implementations must be cheap to call (a `Vec`
/// clone of a small snapshot is fine) and must not block on the network —
/// return the most recent known state, stale beats nothing.
pub trait NetworkDirectory: Send + Sync {
    /// Every configured server, with its `try`-chain priority (0 = first).
    ///
    /// The returned vector's order is not significant; the implementation may
    /// be emitted in any order and the callers sort by `priority`.
    fn servers(&self) -> Vec<ServerInfo>;

    /// Every player currently on the network, with their current server.
    fn online_players(&self) -> Vec<OnlinePlayer>;

    /// A consistent status view taken at invocation time.
    ///
    /// Carries the `/candyriya info` and `/candyriya servers` payload:
    /// version, uptime, configured log level, connection counters and the
    /// per-backend table. Cheap for the same reason the other two methods
    /// are cheap; never fresher than the last health check.
    fn snapshot(&self) -> ProxySnapshot;

    /// A consistent view of the supervision tree for `/candyriya ps` and
    /// `/candyriya tree`.
    ///
    /// Carries every unit in the registry plus the per-connection counters,
    /// ascending by pid, and the tree's own zombie gauges so the summary
    /// line reports exactly what the tree tracks. Cheap for the same reason
    /// the other methods are cheap: a clone of small vectors, never a
    /// round trip anywhere.
    fn ps_snapshot(&self) -> crate::PsSnapshot;
}

/// Everything a command implementation needs, built once and cloned into it.
///
/// [`CommandState::effects`] is the single sink for both output
/// ([`OutputSender::send`](crate::OutputSender::send)) and cross-cutting
/// actions ([`ProxyEffects::switch`](crate::ProxyEffects::switch)).
#[derive(Clone)]
pub struct CommandState {
    /// The locale tables, bound per outbound line (embedded defaults or
    /// overrides, whatever the composition root built).
    pub locales: Locales,
    /// Read-only view over servers, online players and the status snapshot.
    pub directory: Arc<dyn NetworkDirectory>,
    /// Side effects the commands trigger — output and player moves.
    pub effects: Arc<dyn ProxyEffects>,
}
