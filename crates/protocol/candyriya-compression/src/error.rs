//! The single error type of this crate.

use thiserror::Error;

/// Everything that can go wrong while encoding or decoding a frame.
///
/// All variants are data errors or configuration errors: either the peer
/// sent bytes this crate cannot make sense of, or the caller asked for
/// something the format cannot represent. There is no I/O variant other
/// than zlib stream failures (see [`Zlib`](CompressionError::Zlib)); in
/// particular, running out of input is always [`Truncated`](CompressionError::Truncated).
#[derive(Error, Debug)]
#[non_exhaustive]
pub enum CompressionError {
    /// The input ran out of bytes before a complete frame or value could
    /// be read.
    ///
    /// When this is returned from
    /// [`Compression::decode_frame`](crate::Compression::decode_frame) the
    /// buffer was too short for the frame it claimed to hold; the caller
    /// may retry once more bytes arrive.
    #[error("unexpected end of input")]
    Truncated,

    /// A `VarInt` was encoded in more than 5 bytes, or its fifth byte still
    /// had the continuation bit set.
    #[error("VarInt is too big (more than 5 bytes)")]
    VarIntTooBig,

    /// The `Packet Length` prefix was encoded in more than 3 bytes, or its
    /// value exceeded 2097151 (2^21 - 1), the maximum a 3-byte `VarInt` can
    /// hold.
    ///
    /// This is also returned by
    /// [`Compression::encode_frame`](crate::Compression::encode_frame) when
    /// the produced frame would exceed the same limit (for example a
    /// 2 MiB incompressible packet), so the crate never emits a frame that
    /// a vanilla peer would reject.
    #[error("packet length exceeds 2097151 bytes or does not fit in 3 bytes")]
    PacketLengthTooLarge,

    /// The declared uncompressed data length of a frame (or the input
    /// packet passed to [`Compression::encode_frame`](crate::Compression::encode_frame))
    /// is negative or exceeds [`crate::MAX_UNPACKED`].
    ///
    /// This check happens *before* any decompression, so a hostile peer
    /// cannot make this crate allocate or expand data based on a bogus
    /// frame header.
    #[error("uncompressed data length out of range (maximum {max} bytes, got {declared})")]
    UnpackedTooLarge {
        /// The uncompressed size written in the `Data Length` field (or the
        /// input packet size, on encode).
        declared: i64,
        /// The configured hard cap, [`crate::MAX_UNPACKED`].
        max: usize,
    },

    /// The frame declared one uncompressed size, but the zlib stream
    /// produced a different number of bytes.
    ///
    /// When the stream produces *more* than declared, the decoder is
    /// stopped one byte past the declaration and `actual` is therefore
    /// `declared + 1` — the true total is intentionally not materialized.
    #[error(
        "decompressed data length does not match the frame header (declared {declared}, actual {actual})"
    )]
    DataLengthMismatch {
        /// The uncompressed size from the `Data Length` field.
        declared: i32,
        /// The number of bytes the zlib stream produced before the decoder
        /// stopped; when the stream would produce more than declared this
        /// is `declared + 1`.
        actual: usize,
    },

    /// Compression level outside of -1..=9.
    #[error("compression level must be between -1 and 9 (got {level})")]
    InvalidLevel {
        /// The rejected level.
        level: i32,
    },

    /// Threshold below -1. `-1` is the "compression disabled" sentinel;
    /// anything below it is meaningless.
    #[error("compression threshold must be -1 (disabled) or non-negative (got {threshold})")]
    InvalidThreshold {
        /// The rejected threshold.
        threshold: i32,
    },

    /// A zlib stream failed to decode or encode.
    ///
    /// A stream that ends prematurely is reported as [`Truncated`](CompressionError::Truncated)
    /// when the decoder surfaces an end-of-file; everything else (bad
    /// checksum, corrupt blocks, wrong format) shows up here.
    #[error("zlib error: {0}")]
    Zlib(std::io::Error),
}
