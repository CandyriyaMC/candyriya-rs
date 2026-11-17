//! The error type for the `candyriya:main` protocol codec.
//!
//! Failures are split by where they happen: before the payload is even
//! inspected ([`EmptyPayload`](CandyriyaMessageError::EmptyPayload),
//! [`PacketTooLarge`](CandyriyaMessageError::PacketTooLarge),
//! [`DirectionMismatch`](CandyriyaMessageError::DirectionMismatch)),
//! while framing the header ([`ProtocolVersionMismatch`](CandyriyaMessageError::ProtocolVersionMismatch),
//! [`UnknownPacketId`](CandyriyaMessageError::UnknownPacketId)),
//! inside a field ([`UnexpectedEof`](CandyriyaMessageError::UnexpectedEof),
//! [`VarIntTooBig`](CandyriyaMessageError::VarIntTooBig),
//! [`StringTooLong`](CandyriyaMessageError::StringTooLong),
//! [`InvalidUtf8`](CandyriyaMessageError::InvalidUtf8),
//! [`InvalidBoolean`](CandyriyaMessageError::InvalidBoolean),
//! [`ByteArrayTooLarge`](CandyriyaMessageError::ByteArrayTooLarge)),
//! and after the last field
//! ([`TrailingBytes`](CandyriyaMessageError::TrailingBytes)).
//!
//! The codec is total on arbitrary bytes: every failure is one of these
//! variants, never a panic.

use thiserror::Error;

use crate::Direction;

/// Errors produced by encoding and decoding `candyriya:main` packets.
///
/// Every variant carries enough context to report the failure without
/// re-walking the buffer: the offending size, direction, packet id, field
/// name or trailing byte count, depending on the variant.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum CandyriyaMessageError {
    /// The payload held zero bytes: there is no protocol version and no
    /// packet id to dispatch on.
    #[error("packet payload is empty")]
    EmptyPayload,

    /// The payload exceeded [`MAX_PACKET_BYTES`](crate::MAX_PACKET_BYTES).
    /// Raised on encode before the bytes leave the crate and on decode
    /// before the first field is read, so an oversized payload never
    /// reaches the field parsers.
    #[error("packet payload of {actual} bytes exceeds the maximum of {max} bytes")]
    PacketTooLarge {
        /// Size of the offending payload.
        actual: usize,
        /// The enforced maximum ([`MAX_PACKET_BYTES`](crate::MAX_PACKET_BYTES)).
        max: usize,
    },

    /// The caller passed a direction that does not match the message
    /// family being encoded or decoded. Decoding a serverbound payload as
    /// clientbound (or the reverse) is a programming error, not a wire
    /// error, and is reported instead of silently reinterpreting bytes.
    #[error("direction mismatch: expected {expected}, got {found}")]
    DirectionMismatch {
        /// The direction the message family requires.
        expected: Direction,
        /// The direction the caller passed.
        found: Direction,
    },

    /// The `protocol_version` field did not equal
    /// [`CANDYRIYA_PROTOCOL_VERSION`](crate::CANDYRIYA_PROTOCOL_VERSION).
    /// Other versions are rejected outright; there is no silent fallback,
    /// because a mismatched version means the two sides disagree about
    /// the meaning of every subsequent byte.
    #[error("unsupported protocol version {found} (expected {expected})")]
    ProtocolVersionMismatch {
        /// The only version this crate implements.
        expected: i32,
        /// The version found on the wire.
        found: i32,
    },

    /// The `packet_id` field is not one of the ids defined for the
    /// direction the payload was decoded as. Ids outside `0x01..=0x0F`
    /// (serverbound) or `0x80..=0x8F` (clientbound), and ids inside the
    /// range that have no packet assigned, are rejected here.
    #[error("unknown packet id {id:#04x} for the {direction} direction")]
    UnknownPacketId {
        /// The direction the payload was decoded as.
        direction: Direction,
        /// The offending packet id.
        id: i32,
    },

    /// The payload ended before the current field was complete.
    #[error("unexpected end of payload while reading {field}")]
    UnexpectedEof {
        /// Name of the field being read when the payload ran out.
        field: &'static str,
    },

    /// A `VarInt` encoding used more than five bytes, or was truncated in
    /// a way the scanner cannot recover from.
    #[error("varint encoding is too big (more than five bytes)")]
    VarIntTooBig,

    /// A string's length prefix was negative, or the declared byte length
    /// or the decoded UTF-16 code unit count exceeded the Minecraft cap
    /// of 32767 units.
    #[error("string length prefix is negative or exceeds the 32767 UTF-16 code unit cap")]
    StringTooLong,

    /// A string field's payload was not valid UTF-8.
    #[error("string payload is not valid UTF-8")]
    InvalidUtf8,

    /// A boolean field held a byte other than `0x00` or `0x01`.
    #[error("invalid boolean byte {0:#04x} (expected 0x00 or 0x01)")]
    InvalidBoolean(u8),

    /// A byte array's length prefix was negative on read, or the byte
    /// array exceeded `i32::MAX` bytes on write. (A declared length that
    /// overruns the remaining payload is reported as
    /// [`UnexpectedEof`](Self::UnexpectedEof), like any other truncation.)
    #[error("byte array length prefix is negative or too large")]
    ByteArrayTooLarge,

    /// Extra bytes were present after the last field of the packet. The
    /// packet grammar is fixed-width per packet id, so a well-formed
    /// payload is exactly consumed; trailing bytes mean the payload was
    /// corrupted or forged, and silently ignoring them would hide that.
    #[error("trailing {bytes} byte(s) after end of packet")]
    TrailingBytes {
        /// Number of unconsumed bytes after the last field.
        bytes: usize,
    },
}
