//! The effects seam between this crate and the proxy.
//!
//! The commands hold no sockets, sessions or scheduler handles of their own;
//! everything that crosses the boundary comes through here, as two traits
//! with one shared implementation in the composition root:
//!
//! - [`OutputSender`] is the single output path. Every line a command wants
//!   the invoker to see goes through [`OutputSender::send`]. Lines are
//!   MiniMessage strings — [`escape_minimessage`](crate::escape_minimessage)
//!   belongs on every interpolated argument before it lands in a line.
//! - [`ProxyEffects`] is the cross-cutting sink the proxy implements: output
//!   (inherited from the [`OutputSender`] supertrait) plus player moves
//!   ([`ProxyEffects::switch`]).
//!
//! `switch` is reported back as a [`SwitchResult`], so a caller never
//! learns a route moved before the proxy confirms it — lines the invoker
//! sees are always the post-move truth.
//!
//! [`ShutdownHandle`] is the one-shot flag behind `/candyriya end`; it is
//! not part of [`ProxyEffects`] because it is a signal, not an effect a
//! command performs on the world.

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::source::{CommandSource, PlayerRef};

/// Where command output lines go.
///
/// `line` is MiniMessage text (the i18n values are MiniMessage). The
/// implementation decides how to render it for the recipient: ANSI for the
/// console, chat components for a player. Called synchronously from the
/// command handler — keep it cheap and non-blocking.
pub trait OutputSender: Send + Sync {
    /// Emit one MiniMessage line to the invoker.
    fn send(&self, source: &CommandSource, line: &str);
}

/// A request to move one player to a server. The proxy applies it through
/// the routing layer; the command does not and cannot switch anything
/// itself. Bulk moves are issued one request per player.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SwitchRequest {
    /// The player to move.
    pub player: PlayerRef,
    /// The server to switch to (already validated against the directory).
    pub server: String,
}

impl SwitchRequest {
    /// Creates a switch request from its parts.
    pub fn new(player: PlayerRef, server: impl Into<String>) -> Self {
        Self {
            player,
            server: server.into(),
        }
    }
}

/// What happened to a one-shot switch request.
///
/// The effects implementation validates against the live runtime (the roster
/// the proxy and the server configuration may have moved on since the
/// command's pre-check), so every failure mode is reported, never panicked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SwitchResult {
    /// The player is now routed to the requested backend (a no-op re-switch
    /// to the current backend also reports this).
    Switched {
        /// The player that was moved.
        player: String,
        /// The backend the player was moved to.
        server: String,
    },
    /// The player is not connected (any more).
    PlayerNotFound {
        /// The player name the caller asked about.
        player: String,
    },
    /// The server is not configured (any more).
    ServerNotFound {
        /// The server name the caller asked about.
        server: String,
    },
    /// The switch failed; `reason` is opaque, render it escaped.
    Failed {
        /// The player that was supposed to move.
        player: String,
        /// The requested backend.
        server: String,
        /// Why the move failed. Never MiniMessage; treat as plain text.
        reason: String,
    },
}

/// Side effects a command triggers. Implemented by the proxy.
///
/// Output delivery is inherited from [`OutputSender`]: implementors get a
/// single output path instead of two half-overlapping ones, and the invoker
/// sees exactly what the proxy chose to render. `switch` hands a validated
/// player/server pair over to the routing layer; the command layer never
/// applies routes by itself.
pub trait ProxyEffects: OutputSender {
    /// Applies a server switch to the given player.
    ///
    /// `server` is the configured name exactly as spelled in the config
    /// (the command resolved it before calling). The implementation must be
    /// total: no panics, no blocking on the caller's thread indefinitely —
    /// enqueue the transfer and report.
    fn switch(&self, request: SwitchRequest) -> SwitchResult;
}

/// Shared one-way shutdown flag.
///
/// `/candyriya end` flips it exactly once; the composition root (cli)
/// watches the same flag — the cli attaches it to the scheduler drain —
/// and initiates the real graceful shutdown. This crate never initiates a
/// shutdown itself, it cannot: it does not own the runtime. Cloned handles
/// share one flag.
#[derive(Clone, Default)]
pub struct ShutdownHandle {
    requested: Arc<AtomicBool>,
}

impl ShutdownHandle {
    /// A fresh flag in the not-requested state.
    pub fn new() -> Self {
        Self::default()
    }

    /// Flips the flag. Returns `true` when this call performed the flip,
    /// `false` when a shutdown was already requested (the first call wins).
    pub fn trigger(&self) -> bool {
        !self.requested.swap(true, Ordering::SeqCst)
    }

    /// Whether a shutdown has been requested.
    pub fn is_requested(&self) -> bool {
        self.requested.load(Ordering::SeqCst)
    }
}

impl fmt::Debug for ShutdownHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ShutdownHandle")
            .field("requested", &self.is_requested())
            .finish()
    }
}
