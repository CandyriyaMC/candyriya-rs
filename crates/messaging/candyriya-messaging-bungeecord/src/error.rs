//! The error type for the BungeeCord subchannel codec.
//!
//! Every variant carries the subchannel being processed (when one is known),
//! so a failure deep in a proxy handler can be reported with the full
//! context instead of a bare byte offset.

use thiserror::Error;

use candyriya_messaging::JavaCodecError;

/// Errors produced by [`BungeeCodec`](crate::BungeeCodec) and the response
/// encoders.
///
/// The variants split failures by where they happen: before the subchannel
/// is known ([`SubchannelPrefix`](Self::SubchannelPrefix)), while decoding a
/// known subchannel ([`Decode`](Self::Decode)), when the buffer is not
/// fully consumed ([`TrailingBytes`](Self::TrailingBytes)), when encoding
/// ([`Encode`](Self::Encode), [`ForwardPayloadTooLong`](Self::ForwardPayloadTooLong)),
/// and when the subchannel string is not one of the known ones
/// ([`UnknownSubchannel`](Self::UnknownSubchannel)).
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum BungeeMessagingError {
    /// The payload started with a valid `readUTF` subchannel, but the
    /// subchannel is not one of the known BungeeCord subchannels.
    #[error("unknown bungeecord subchannel {subchannel:?}")]
    UnknownSubchannel {
        /// The offending subchannel string.
        subchannel: String,
    },

    /// The subchannel is known, but a field failed to decode (truncated
    /// input, a length prefix that overflows a slice, invalid CESU-8, ...).
    #[error("failed to decode bungeecord subchannel {subchannel:?}: {source}")]
    Decode {
        /// The subchannel being decoded when the failure happened.
        subchannel: String,
        /// The underlying Java stream codec error.
        #[source]
        source: JavaCodecError,
    },

    /// The payload was too short to carry a subchannel, or the subchannel
    /// prefix itself is broken (e.g. an empty payload or a two-byte length
    /// prefix that cannot fit the buffer).
    #[error("failed to decode bungeecord subchannel prefix: {source}")]
    SubchannelPrefix {
        /// The underlying Java stream codec error.
        #[source]
        source: JavaCodecError,
    },

    /// Extra bytes were present after the last argument of the subchannel.
    ///
    /// The BungeeCord subchannel grammar is fixed-width per subchannel, so a
    /// well-formed payload is exactly consumed. Trailing bytes mean the
    /// payload was corrupted or forged, and silently ignoring them would
    /// hide exactly that.
    #[error(
        "unexpected {trailing} trailing byte(s) after the arguments of subchannel {subchannel:?}"
    )]
    TrailingBytes {
        /// The subchannel being decoded when the failure happened.
        subchannel: String,
        /// Number of unconsumed bytes after the arguments.
        trailing: usize,
    },

    /// A field failed to encode (e.g. a string exceeding the Java `writeUTF`
    /// limit of 65535 payload bytes).
    #[error("failed to encode bungeecord subchannel {subchannel:?}: {source}")]
    Encode {
        /// The subchannel being encoded when the failure happened.
        subchannel: String,
        /// The underlying Java stream codec error.
        #[source]
        source: JavaCodecError,
    },

    /// A `Forward`/`ForwardToPlayer` nested payload exceeds the unsigned
    /// two-byte length prefix of the `writeShort` that precedes it.
    #[error(
        "forwarded payload on subchannel {subchannel:?} is {len} bytes, but the length prefix is an unsigned 16-bit count (max {max})"
    )]
    ForwardPayloadTooLong {
        /// The subchannel being encoded (`Forward` or `ForwardToPlayer`).
        subchannel: String,
        /// The actual nested payload length in bytes.
        len: usize,
        /// The maximum representable nested payload (65535).
        max: usize,
    },
}
