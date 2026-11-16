//! The single error type used throughout the crate.

use thiserror::Error;

/// All failures that can occur while reading or writing the wire format.
///
/// Everything here is a *data* error: either the peer sent bytes we cannot
/// make sense of, or the caller asked us to write something illegal (for
/// example a string longer than the configured maximum). I/O failures needed
/// to plug the frame codec into a `Framed` stream are carried by [`Io`](ProtocolError::Io).
#[derive(Error, Debug)]
#[non_exhaustive]
pub enum ProtocolError {
    /// The input ran out of bytes before a complete value could be read.
    ///
    /// For incremental decoding this also covers a frame that is shorter
    /// than the packet length prefix claims (for example a `VarInt` opcode
    /// that needs more bytes than the frame holds).
    ///
    /// When this is returned by a `decode_*` function taking `&mut impl Buf`,
    /// the buffer is left untouched.
    #[error("unexpected end of input")]
    Truncated,

    /// A `VarInt` was encoded in more than 5 bytes, or its fifth byte still
    /// had the continuation bit set.
    ///
    /// Minecraft never puts more than 5 bytes into a 32-bit `VarInt`,
    /// anything beyond that is rejected.
    #[error("VarInt is too big (more than 5 bytes)")]
    VarIntTooBig,

    /// A `VarLong` was encoded in more than 10 bytes, or its tenth byte
    /// still had the continuation bit set.
    #[error("VarLong is too big (more than 10 bytes)")]
    VarLongTooBig,

    /// A string broke its configured limits.
    ///
    /// The wire limit is expressed in UTF-16 code units (a scalar value
    /// above U+FFFF counts as two units) and an encoded size limit of
    /// `max_units * 3` bytes; a declared byte length that is negative or
    /// larger than that limit is reported through this variant as well.
    #[error("string exceeds the configured maximum length")]
    StringTooLong,

    /// A string's payload bytes were not valid UTF-8.
    #[error("invalid UTF-8 in string payload")]
    InvalidUtf8,

    /// A packet frame exceeded [`crate::MAX_PACKET_SIZE`].
    ///
    /// This also catches a negative declared length, which would
    /// otherwise wrap around to a gigantic size.
    #[error("packet frame exceeds the maximum allowed size")]
    PacketTooLarge,

    /// Extra bytes were present after a complete value where the format
    /// expects none.
    #[error("trailing bytes after a complete value")]
    TrailingData,

    /// An underlying I/O error, surfaced so the frame codec can be wired
    /// directly into a `Framed` stream without an additional error wrapper.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}
