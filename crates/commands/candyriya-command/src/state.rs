//! Shared state injected at command construction.
//!
//! Commands in this crate hold no connection state of their own; everything
//! they need arrives as handles at construction time:
//!
//! - the [`I18n`] tables, always from `candyriya-i18n` (never a hardcoded
//!   string outside this crate's MiniMessage locale files);
//! - a read-only view over the proxy's configured servers and online players
//!   ([`NetworkDirectory`]);
//! - the side effects commands trigger ([`ProxyEffects`]).
//!
//! The proxy implements both traits. Tests provide deterministic fixtures, so
//! these commands never talk to a network.

use std::sync::Arc;

use uuid::Uuid;

use crate::source::{CommandOrigin, PlayerRef};

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
/// clone of a small snapshot is fine) and must not block.
pub trait NetworkDirectory: Send + Sync {
    /// Every configured server, with its `try`-chain priority (0 = first).
    ///
    /// The returned vector's order is not significant; the implementation may
    /// be emitted in any order and the callers sort by `priority`.
    fn servers(&self) -> Vec<ServerInfo>;

    /// Every player currently on the network, with their current server.
    fn online_players(&self) -> Vec<OnlinePlayer>;
}

/// What a switch request targets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SwitchTarget {
    /// Exactly one player, resolved against the online list.
    One(PlayerRef),
    /// The literal `ALL` target: every player on the network.
    All,
}

/// A request to move player(s) to a server. The proxy applies it through the
/// routing layer; the command does not and cannot switch anything itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SwitchRequest {
    /// Who gets switched.
    pub target: SwitchTarget,
    /// The server to switch to (already validated against the directory).
    pub server: String,
}

impl SwitchRequest {
    /// Creates a switch request from its parts.
    pub fn new(target: SwitchTarget, server: impl Into<String>) -> Self {
        Self {
            target,
            server: server.into(),
        }
    }
}

/// Side effects a command triggers. Implemented by the proxy.
///
/// Messages are delivered verbatim to the given invoker. `message` is a
/// **MiniMessage string, unparsed**: implementors parse it with `lobsterchat`
/// at the boundary (see CONTRIBUTING — text never travels half-rendered).
/// Every interpolated argument must be pre-escaped with
/// [`escape_minimessage`](crate::escape_minimessage); this crate does not
/// escape for you.
///
/// `switch` hands a validated target/server pair over to the routing layer;
/// the command layer never applies routes by itself.
pub trait ProxyEffects: Send + Sync {
    /// Delivers a user-facing message to the invoking source.
    fn message(&self, origin: &CommandOrigin, message: String);

    /// Applies a server switch to the given target.
    fn switch(&self, request: SwitchRequest);
}

/// Everything a command implementation needs, built once and cloned into it.
#[derive(Clone)]
pub struct CommandState {
    /// The locale tables (embedded defaults, possibly with overrides).
    pub i18n: candyriya_i18n::I18n,
    /// Read-only view over servers and online players.
    pub directory: Arc<dyn NetworkDirectory>,
    /// Side effects the commands trigger.
    pub effects: Arc<dyn ProxyEffects>,
}
