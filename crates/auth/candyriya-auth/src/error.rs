//! The error type for authentication operations.

use thiserror::Error;

/// Errors produced by [`crate::SessionValidator`] implementations and by
/// [`crate::validate_username`].
///
/// Every variant carries a human-readable, English explanation; none of
/// them embeds secrets (the `serverId` is not secret material, but we
/// still keep it out of error strings to make accidental logging safe).
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum AuthError {
    /// The username does not satisfy the Java Edition rule: 3–16 ASCII
    /// alphanumeric characters or underscores. See [`crate::validate_username`].
    #[error("invalid username: {0}")]
    InvalidUsername(String),

    /// The session server completed the request but rejected the join check:
    /// it returned 204 (no matching session), a non-200 status, a body that
    /// is not the expected JSON document, or the request timed out.
    #[error("session rejected: {0}")]
    SessionRejected(String),

    /// The request to the session server could not be carried out at all:
    /// DNS, connect, TLS handshake or body streaming failed.
    #[error("session server transport error: {0}")]
    Transport(String),

    /// An I/O error occurred. Not produced by the built-in validators;
    /// provided for downstream implementors of [`crate::SessionValidator`].
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),

    /// The session server answered with a well-formed envelope but a field
    /// could not be decoded, e.g. the `id` is not a valid UUID.
    #[error("failed to parse session server response: {0}")]
    Parse(String),
}
