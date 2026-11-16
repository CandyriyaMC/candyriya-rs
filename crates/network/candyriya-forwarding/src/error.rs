//! Error type shared by the modern and legacy forwarding codecs.

use thiserror::Error;

/// Everything that can go wrong while creating or consuming forwarding data.
///
/// The variants are deliberately granular so callers (and tests) can tell
/// "the bytes never were a valid payload" apart from "the bytes were a
/// payload, but not one signed by the expected secret": the two demand a
/// different response at the proxy layer.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ForwardingError {
    /// The buffer ended before a complete value could be read, or — for the
    /// sealed payload — had fewer than the 32 signature bytes.
    #[error("forwarding data is truncated")]
    Truncated,

    /// The HMAC-SHA256 signature over the payload did not match the expected
    /// value. Either the secret is wrong or the payload was tampered with.
    #[error("forwarding payload signature mismatch")]
    BadSignature,

    /// The forwarding version byte sequence decoded to a value other than
    /// [`crate::FORWARDING_VERSION`].
    #[error("unsupported forwarding version {0}, expected 1")]
    UnsupportedVersion(i32),

    /// The decoded property count was negative, which cannot happen for a
    /// well-formed payload.
    #[error("invalid property count {0}")]
    InvalidPropertyCount(i32),

    /// The boolean byte of a property was not `0x00` or `0x01`.
    #[error("invalid boolean byte {0:#04x} in forwarding payload")]
    InvalidBoolean(u8),

    /// The payload decoded cleanly but carried bytes after the last
    /// well-formed field. A well-behaved peer never produces these; a
    /// hostile one might, so rejecting them is the safe default.
    #[error("forwarding payload has {0} trailing byte(s)")]
    TrailingBytes(usize),

    /// The legacy handshake address was not of the shape
    /// `host\0ip\0uuid[\0properties]`.
    #[error("malformed legacy forwarding address: {0}")]
    MalformedLegacyAddress(&'static str),

    /// The UUID in a legacy forwarding address was not valid.
    #[error("invalid UUID in legacy forwarding address: {0}")]
    InvalidUuid(#[from] uuid::Error),

    /// The properties JSON in a legacy forwarding address was not a list of
    /// `{"name", "value", "signature"?}` objects.
    #[error("invalid properties JSON in legacy forwarding address: {0}")]
    InvalidLegacyProperties(#[from] serde_json::Error),

    /// A lower-level wire-format primitive failed.
    #[error("protocol error while decoding forwarding data: {0}")]
    Protocol(#[from] candyriya_protocol::ProtocolError),
}
