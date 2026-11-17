//! `candyriya-proxy`: the proxy core.
//!
//! This crate is being built in slices. This slice owns the **accept-loop
//! half** of the proxy:
//!
//! - [`ProxySocket`] — binding a TCP listener with the production socket
//!   options (`SO_REUSEADDR`, `SO_KEEPALIVE`, `TCP_NODELAY`, optionally
//!   `SO_REUSEPORT`), via `socket2`.
//! - [`accept_loop`] / [`serve`] — the accept task, registered with the
//!   [`candyriya_scheduler::TaskService`] (never an ad-hoc `tokio::spawn`),
//!   stopping cleanly on a shutdown `watch<bool>`.
//! - Per-connection accounting: a per-IP concurrent-connection cap and a
//!   global cap (a [`candyriya_ratelimit::ConnectionLimiter`] for the
//!   per-IP side plus a semaphore for the global side), applied before a
//!   connection is handed to the service.
//! - [`handle_connection`] — the per-connection task: reads the handshake,
//!   routes on its intent. A status-intent handshake is answered by
//!   [`handle_status`]; a login-intent handshake is handed to
//!   [`handle_login`]; other intents have no handler yet and the
//!   connection is closed.
//! - [`handle_login`] — the login slice: reads the Login Start, validates
//!   the username, resolves the profile (offline-mode UUID derivation, or
//!   the online-mode RSA/AES-CFB8 encryption handshake with Mojang
//!   verification), routes to a backend (forced hosts first, then the
//!   `try` chain with kick fallback), and completes the login exchange —
//!   Set Compression, modern `velocity:player_info` forwarding (BungeeGuard
//!   and legacy included) or Login Disconnect, forwarding the Login Success
//!   verbatim to the client.
//! - The **play-state hot path**: once the Login Success has been
//!   delivered, the proxy stops parsing packets and copies bytes
//!   bidirectionally between the two sockets (AES-128-CFB8 carried over
//!   from the login phase where online mode established it; zlib passes
//!   through untouched), bounded by an 8 KiB buffer per direction and a
//!   per-chunk write stall so a peer that stops reading is torn down
//!   instead of buffered forever.
//! - [`read_handshake`] — reading and validating the client's first frame:
//!   framing through [`candyriya_protocol::PacketCodec`], the frame-size
//!   cap, the packet id and intent checks, and the `server_address`
//!   validation. [`HandshakeRead`] is the incremental state machine behind
//!   it; [`handle_status`] is the status-intent handler it feeds.
//! - [`handle_status`] — the status-intercept slice: after a
//!   status-intent handshake, validates the status request, answers it
//!   immediately from configuration (version echo, MOTD, favicon) without
//!   contacting any backend, and echoes the ping.
//!
//! Everything is audited-library-backed (`socket2`, `tokio`,
//! `candyriya-ratelimit`); nothing here re-implements protocol work it
//! does not own.
//!
//! # Shutdown contract
//!
//! [`serve`] registers the accept loop as the named task
//! `proxy.accept-loop`. Shutdown is signalled by flipping the paired
//! `watch<bool>` to `true`; the accept loop then drops its listening
//! work and returns. Connections that are already accepted keep running
//! in their own `proxy.conn.<id>` tasks until the surrounding
//! `TaskService` is drained (see `TaskService::drain`) by the
//! composition root during process shutdown.
//!
//! # Safety
//!
//! This crate is `#![forbid(unsafe_code)]`. Where the platform cannot be
//! reached through a safe API (notably `TCP_FASTOPEN`), the option is
//! skipped with a warning instead of reaching for `unsafe`.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use std::io;

mod accept;
mod handshake;
mod limit;
mod login;
mod play;
mod registry;
mod roster;
mod socket;
mod status;
mod wire;

pub use crate::accept::{ProxyState, accept_loop, handle_connection, serve};
pub use crate::handshake::{
    DEFAULT_MAX_ADDRESS_LEN, HandshakeLimits, HandshakeRead, HandshakeResult, read_handshake,
};
pub use crate::limit::{
    ConnectionSlot, DEFAULT_PER_IP_MAX, SlotAcquireError, SlotPhase, try_acquire_slot,
};
pub use crate::login::{handle_login, registry_from_config};
pub use crate::roster::{PlayerRoster, RosterEntry};
pub use crate::socket::{ProxySocket, REUSE_PORT_SUPPORTED, SocketConfig};
pub use crate::status::{FaviconCache, handle_status};

/// Errors the proxy core can return.
#[derive(Debug, thiserror::Error)]
pub enum ProxyError {
    /// Creating the raw socket failed (out of file descriptors, unsupported
    /// address family, ...).
    #[error("failed to create the listener socket: {0}")]
    SocketCreate(#[source] io::Error),

    /// A socket option could not be applied before binding.
    #[error("failed to set socket option {option}: {source}")]
    SetOption {
        /// The option that failed, e.g. `SO_REUSEADDR`.
        option: &'static str,
        /// The underlying OS error.
        #[source]
        source: io::Error,
    },

    /// Binding the socket to `addr` failed (address in use, permission
    /// denied, ...).
    #[error("failed to bind to {addr}: {source}")]
    Bind {
        /// The address that was being bound.
        addr: std::net::SocketAddr,
        /// The underlying OS error.
        #[source]
        source: io::Error,
    },

    /// `listen(2)` failed on an otherwise bound socket.
    #[error("failed to listen on the socket: {0}")]
    Listen(#[source] io::Error),

    /// The socket could not be handed over to `tokio` as a listener.
    #[error("failed to install the listener as a non-blocking tokio listener: {0}")]
    ListenerConversion(#[source] io::Error),

    /// Querying the bound local address of the listener failed.
    #[error("failed to read the listener local address: {0}")]
    LocalAddr(#[source] io::Error),

    /// The peer sent a frame larger than the maximum frame size
    /// ([`candyriya_protocol::MAX_PACKET_SIZE`]). The connection is dropped
    /// without reading any further bytes.
    #[error("handshake frame exceeds the maximum frame size")]
    HandshakeTooLarge,

    /// The first frame was malformed: bad framing, a truncated payload, or
    /// a packet id other than the handshake id.
    #[error("malformed handshake: {0}")]
    BadPacket(String),

    /// The handshake intent was not one of `1` (status), `2` (login) or
    /// `3` (transfer) and `allow_legacy_intents` was not set.
    #[error("unsupported handshake intent: {0}")]
    BadIntent(i32),

    /// The `server_address` field failed validation: over the configured
    /// length cap, or containing characters outside the safe hostname set
    /// (an address can smuggle control bytes or MOTD markup into logs and
    /// forced-host matching).
    #[error("illegal handshake server address: {0}")]
    IllegalAddress(String),

    /// The peer did not finish the frame it was asked for (handshake or
    /// any status-state frame) within the configured timeout.
    #[error("timed out")]
    Timeout,

    /// The proxy is misconfigured in a way that makes serving impossible —
    /// e.g. the MOTD is empty or is not valid MiniMessage, so the status
    /// response cannot be produced. Details are safe to log, never sent
    /// to the client.
    #[error("proxy is misconfigured: {0}")]
    Misconfigured(String),

    /// The embedded protocol registry failed to load. This is a build
    /// bug, not a runtime condition: the compiled-in tables are invalid.
    #[error("protocol registry unavailable: {0}")]
    Registry(String),

    /// Socket I/O failed while reading the handshake.
    #[error("handshake I/O failed: {0}")]
    Io(#[from] io::Error),
}
