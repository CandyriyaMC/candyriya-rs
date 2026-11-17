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
//! - The per-connection task itself, [`handle_connection`], which today
//!   holds the socket open and drains inbound bytes until EOF — a valid
//!   checkpoint that later slices replace with the handshake/login
//!   pipeline.
//! - [`read_handshake`] — reading and validating the client's first frame:
//!   framing through [`candyriya_protocol::PacketCodec`], the frame-size
//!   cap, the packet id and intent checks, and the `server_address`
//!   validation. [`HandshakeRead`] is the incremental state machine behind
//!   it; the routing of a validated [`HandshakeResult`] to status or login
//!   is a later slice.
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
mod socket;

pub use crate::accept::{ProxyState, accept_loop, handle_connection, serve};
pub use crate::handshake::{
    DEFAULT_MAX_ADDRESS_LEN, HandshakeLimits, HandshakeRead, HandshakeResult, read_handshake,
};
pub use crate::limit::{ConnectionSlot, DEFAULT_PER_IP_MAX, SlotAcquireError, try_acquire_slot};
pub use crate::socket::{ProxySocket, REUSE_PORT_SUPPORTED, SocketConfig};

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

    /// The client did not finish the handshake within the configured
    /// timeout.
    #[error("handshake timed out")]
    Timeout,

    /// Socket I/O failed while reading the handshake.
    #[error("handshake I/O failed: {0}")]
    Io(#[from] io::Error),
}
