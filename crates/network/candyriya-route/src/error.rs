//! Error type used throughout the crate.

use std::io;

use candyriya_protocol::ProtocolError;
use thiserror::Error;

/// All failures reported by this crate.
///
/// Backend health checks never fail hard through `?` in the registry API:
/// a failed ping is reported as an outcome with the error attached and the
/// backend is marked [`crate::BackendState::Offline`]. The variants below
/// surface in the public API only where a caller must act on them (invalid
/// configuration, unknown backends) or in the health-check outcome for
/// diagnostics and metrics.
#[derive(Error, Debug)]
#[non_exhaustive]
pub enum RouteError {
    /// A backend name is not present in the registry.
    #[error("unknown backend: {0}")]
    UnknownBackend(String),

    /// Two backends with the same name were registered.
    #[error("duplicate backend: {0}")]
    DuplicateBackend(String),

    /// A backend name contained illegal characters (empty, too long, or
    /// whitespace/control characters).
    #[error("invalid backend name: {0:?}")]
    InvalidBackendName(String),

    /// The login fallback chain is empty; without at least one backend no
    /// connection can ever be routed.
    #[error("the try list must reference at least one backend")]
    EmptyTryList,

    /// The try list mentions a backend that is not in the registry.
    #[error("the try list references unknown backend: {0}")]
    UnknownTryBackend(String),

    /// The try list mentions the same backend twice; fallback would retry a
    /// server the proxy already gave a shot at.
    #[error("the try list contains duplicate backend: {0}")]
    DuplicateTryBackend(String),

    /// A forced-hosts pattern could not be parsed.
    #[error("invalid forced-host pattern {pattern:?}: {reason}")]
    InvalidPattern {
        /// The offending pattern, verbatim from the configuration.
        pattern: String,
        /// Why it was rejected.
        reason: &'static str,
    },

    /// A forced-hosts entry maps a pattern to an empty backend list; it can
    /// never route anyone anywhere.
    #[error("forced host {0:?} lists an empty backend set")]
    EmptyForcedBackends(String),

    /// Two entries normalized to the same pattern.
    #[error("duplicate forced-host pattern: {0:?}")]
    DuplicatePattern(String),

    /// A forced-hosts entry references a backend that does not exist in the
    /// registry.
    #[error("forced host {host:?} references unknown backend: {backend}")]
    UnknownForcedBackend {
        /// The pattern whose backend set is broken.
        host: String,
        /// The missing backend name.
        backend: String,
    },

    /// The TCP connect or the subsequent I/O during a health check failed.
    #[error("i/o error during backend health check: {0}")]
    Io(#[from] io::Error),

    /// The status ping could not be framed or decoded.
    #[error("protocol error during backend status ping: {0}")]
    Protocol(#[from] ProtocolError),

    /// The backend did not answer within the configured timeout.
    #[error("backend status ping timed out")]
    PingTimeout,

    /// The backend answered with a packet id other than `0x00` (the
    /// clientbound status response).
    #[error("unexpected packet id {id} in status response")]
    UnexpectedPacketId {
        /// The packet id that arrived instead of `0x00`.
        id: i32,
    },

    /// The status response payload was not a valid Minecraft string.
    #[error("status response payload is not a valid string")]
    InvalidStatusResponse,
}
