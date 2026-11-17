//! Error type shared by the backend and the test client.

use candyriya_protocol::ProtocolError;
use candyriya_protocol_registry::RegistryError;
use thiserror::Error;

/// Every failure the harness can report.
///
/// The harness never panics on hostile or broken peer input; all such
/// conditions surface as one of these variants.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum HarnessError {
    /// Socket-level failure (connection refused, reset, ...).
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),

    /// The wire codec rejected the bytes on the stream.
    #[error("protocol error: {0}")]
    Protocol(#[from] ProtocolError),

    /// The peer closed the connection before the expected packet arrived.
    #[error("connection closed before {0}")]
    UnexpectedEof(&'static str),

    /// A packet arrived with an id other than the one the protocol assigns
    /// to this step (resolved through the registry).
    #[error("expected packet id {expected} for {context}, got {actual}")]
    UnexpectedPacketId {
        /// Which protocol step was waiting for the packet.
        context: &'static str,
        /// The id the registry assigns in this protocol version.
        expected: i32,
        /// The id that actually arrived.
        actual: i32,
    },

    /// Structurally valid framing, but the content was not what the step expected.
    #[error("unexpected data: {0}")]
    UnexpectedData(String),

    /// The embedded protocol registry failed to load. This is a build-time
    /// invariant violation, never a runtime condition.
    #[error("protocol registry error: {0}")]
    Registry(#[from] RegistryError),
}
