//! Zlib compression manager for the Minecraft Java Edition protocol.
//!
//! Pure data-in/data-out: no sockets, no network, no async, no clock.
//! All types are `#![forbid(unsafe_code)]`.
//!
//! ## Wire format
//!
//! Once a *Set Compression* packet (login `0x03`, non-negative `Threshold`)
//! has been sent or received, every following packet on the connection is
//! encoded as follows (see the *With compression* section of
//! <https://minecraft.wiki/w/Java_Edition_protocol/Packets>):
//!
//! | Field | Type | Notes |
//! | --- | --- | --- |
//! | Packet Length | [`VarInt`](https://minecraft.wiki/w/Java_Edition_protocol/Packets#Type:VarInt) | Length of everything after this field: `(Data Length VarInt) + (compressed or raw payload)` |
//! | Data Length | `VarInt` | `0` = payload is uncompressed; `> 0` = size of the uncompressed payload |
//! | Packet ID + Data | byte array | The packet, zlib compressed (RFC 1950) when `Data Length > 0`, raw otherwise |
//!
//! When the uncompressed size of `Packet ID + Data` is *smaller* than the
//! threshold from *Set Compression*, the packet is sent uncompressed with
//! `Data Length = 0`. When it is larger than or equal to the threshold, the
//! regular compressed layout is used. A *Set Compression* packet with a
//! negative threshold means compression is disabled and the plain packet
//! format applies; [`Compression::threshold`] `-1` mirrors that state for
//! this crate (frames are still produced, always with `Data Length = 0`).
//!
//! *Set Compression* itself, and every packet sent before it, use the plain
//! format and are out of this crate's scope.
//!
//! ## Limits enforced by this crate
//!
//! * The `Packet Length` prefix must fit in 3 bytes and not exceed
//!   2097151 (2^21 - 1). Longer encodings or larger values are rejected on
//!   decode, and refused on encode.
//! * The uncompressed payload is hard-capped at [`MAX_UNPACKED`] (2 MiB).
//!   On decode the declared `Data Length` is checked against the cap
//!   *before* a single byte is fed into the zlib decoder, so a hostile
//!   frame cannot make this crate allocate or expand unboundedly (zip
//!   bomb). On encode the input packet is checked against the same cap,
//!   which additionally guarantees that anything produced here can be
//!   consumed here. Note that vanilla imposes no limit on the uncompressed
//!   size of incoming compressed packets; the 2 MiB cap is this crate's
//!   own hardening, documented as such.
//! * The number of bytes inflated is also capped at `declared + 1`; a
//!   zlib stream that produces more (or fewer) bytes than the `Data Length`
//!   header promises is rejected with [`CompressionError::DataLengthMismatch`].
//!
//! ## Trailing bytes
//!
//! [`Compression::decode_frame`] parses exactly one frame from the *front*
//! of `src` and reports how many bytes it consumed. Bytes after the frame
//! are not an error; they are left unconsumed, and the caller (for example
//! a stream decoder) is responsible for them. To check that a buffer holds
//! exactly one frame and nothing else, compare the returned consumed count
//! with `src.len()`.
//!
//! ## VarInt coding
//!
//! Minecraft's `VarInt` is encoded with 7-bit little-endian groups and a
//! continuation bit, at most 5 bytes, accepting unnecessarily long legal
//! encodings — identical to the coding used elsewhere in this workspace.
//! This crate carries its own small implementation instead of depending on
//! `candyriya-protocol`, because that crate's codec is built on
//! `bytes`/`tokio-util` and this crate intentionally has no async or
//! runtime dependencies.
//!
//! ## Example
//!
//! ```
//! use candyriya_compression::{Compression, CompressionError};
//!
//! # fn main() -> Result<(), CompressionError> {
//! let compression = Compression::new(256, -1)?;
//! let frame = compression.encode_frame(b"\x01\x02\x03")?;
//! let (consumed, packet) = compression.decode_frame(&frame)?;
//! assert_eq!(consumed, frame.len());
//! assert_eq!(packet, b"\x01\x02\x03");
//! # Ok(())
//! # }
//! ```

#![forbid(unsafe_code)]

mod error;

pub use error::CompressionError;

use std::io::{Read, Write};

use flate2::Compression as Level;
use flate2::read::ZlibDecoder;
use flate2::write::ZlibEncoder;

/// Hard cap on the uncompressed size of a packet handled by this crate:
/// 2 MiB.
///
/// On decode this is enforced on the declared `Data Length` *before* any
/// decompression happens. On encode the input packet is refused above the
/// same cap. See the crate-level documentation for the rationale.
pub const MAX_UNPACKED: usize = 2 * 1024 * 1024;

/// Returns the hard cap on the uncompressed size of a packet, in bytes.
///
/// This is the same value as [`MAX_UNPACKED`], exposed as a function for
/// callers that prefer to query it at runtime.
pub const fn max_unpacked() -> usize {
    MAX_UNPACKED
}

/// The wire-format limit of the `Packet Length` prefix: 2^21 - 1, the
/// maximum value a 3-byte `VarInt` can hold.
const MAX_PACKET_LENGTH: i64 = 2_097_151;

/// Maximum number of bytes the `Packet Length` `VarInt` may take on the
/// wire. The wiki requires the length field to fit in 3 bytes even when
/// the encoded value would fit in fewer.
const MAX_LENGTH_PREFIX_BYTES: usize = 3;

/// Threshold and level pair selected via the *Set Compression* packet.
///
/// * `threshold` — packets whose uncompressed `Packet ID + Data` payload is
///   *smaller* than this are sent uncompressed with `Data Length = 0`;
///   packets at or above it are zlib compressed. `-1` disables compression
///   entirely (every packet is sent with `Data Length = 0`); `0` compresses
///   everything, including empty packets. Values below `-1` are rejected.
/// * `level` — zlib compression level, `-1..=9`. `-1` is the zlib default
///   (level 6), `0` stores the data uncompressed, `9` is the smallest
///   output. Out-of-range values are rejected.
///
/// The fields are public for ergonomic inspection, but they are validated
/// every time [`Compression::encode_frame`] runs, so a directly constructed
/// `Compression` with an out-of-range level cannot smuggle an invalid value
/// into the zlib encoder. Decoding does not consult either field: it always
/// parses the two-`VarInt` frame and applies only the [`MAX_UNPACKED`] cap.
///
/// ```
/// use candyriya_compression::Compression;
///
/// # fn main() -> Result<(), candyriya_compression::CompressionError> {
/// let compression = Compression::new(256, -1)?;
/// assert_eq!(compression.threshold, 256);
/// assert_eq!(compression.level, -1);
/// # Ok(())
/// # }
/// ```
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Compression {
    /// Packets smaller than this are sent uncompressed.
    ///
    /// `-1` disables compression entirely; `0` compresses everything.
    pub threshold: i32,

    /// zlib compression level, `-1..=9`; `-1` is the zlib default.
    pub level: i32,
}

impl Compression {
    /// Creates a validated compression manager.
    ///
    /// Returns [`CompressionError::InvalidThreshold`] if `threshold` is
    /// below `-1`, and [`CompressionError::InvalidLevel`] if `level` is
    /// outside `-1..=9`.
    pub fn new(threshold: i32, level: i32) -> Result<Self, CompressionError> {
        let compression = Self { threshold, level };
        compression.validate()?;
        Ok(compression)
    }

    /// Encodes `packet` as one wire frame: `VarInt Packet Length`,
    /// `VarInt Data Length`, then the (possibly zlib compressed) payload.
    ///
    /// Compression is used when `threshold != -1` and the packet is at
    /// least `threshold` bytes; everything else is emitted uncompressed
    /// with `Data Length = 0`. A zero-length packet is always emitted
    /// uncompressed, because `Data Length = 0` is the uncompressed marker
    /// on the wire and a compressed empty packet could not be told apart
    /// from one.
    ///
    /// Returns [`CompressionError::UnpackedTooLarge`] if `packet` exceeds
    /// [`MAX_UNPACKED`], and [`CompressionError::PacketLengthTooLarge`] if
    /// the resulting frame would exceed the 3-byte `VarInt` limit of
    /// 2097151 bytes.
    pub fn encode_frame(&self, packet: &[u8]) -> Result<Vec<u8>, CompressionError> {
        self.validate()?;

        let packet_len = packet.len();
        if packet_len > MAX_UNPACKED {
            return Err(CompressionError::UnpackedTooLarge {
                declared: packet_len as i64,
                max: MAX_UNPACKED,
            });
        }

        let compress =
            self.threshold != -1 && !packet.is_empty() && packet_len >= self.threshold as usize;
        let (data_length, payload) = if compress {
            let mut encoder = ZlibEncoder::new(Vec::new(), self.flate_level());
            encoder.write_all(packet).map_err(CompressionError::Zlib)?;
            let compressed = encoder.finish().map_err(CompressionError::Zlib)?;
            (packet_len as i32, compressed)
        } else {
            (0, packet.to_vec())
        };

        // Frame body: Data Length prefix followed by the payload; the
        // Packet Length prefix counts everything after itself.
        let mut body = Vec::with_capacity(5 + payload.len());
        write_varint(data_length, &mut body);
        body.extend_from_slice(&payload);

        if body.len() as i64 > MAX_PACKET_LENGTH {
            return Err(CompressionError::PacketLengthTooLarge);
        }

        let mut frame = Vec::with_capacity(body.len() + 5);
        write_varint(body.len() as i32, &mut frame);
        frame.extend_from_slice(&body);
        Ok(frame)
    }

    /// Decodes exactly one frame from the *front* of `src`.
    ///
    /// Returns `(consumed, packet)`: the number of bytes the frame occupies
    /// in `src`, and the uncompressed packet bytes. Trailing bytes after the
    /// frame are left unconsumed and reported implicitly through `consumed`;
    /// they are not an error (compare `consumed` with `src.len()` if a
    /// buffer must hold exactly one frame).
    ///
    /// The declared `Data Length` is validated against [`MAX_UNPACKED`]
    /// *before* any decompression, the `Packet Length` prefix must fit in
    /// 3 bytes and stay below 2097151, and the zlib stream must produce
    /// exactly as many bytes as `Data Length` declares.
    pub fn decode_frame(&self, src: &[u8]) -> Result<(usize, Vec<u8>), CompressionError> {
        let (packet_len, packet_len_bytes) = scan_varint(src)?;
        if packet_len_bytes > MAX_LENGTH_PREFIX_BYTES {
            return Err(CompressionError::PacketLengthTooLarge);
        }
        // A negative VarInt always takes 5 bytes on the wire, so the
        // 3-byte cap above rules negatives out; the comparison is kept as
        // defence in depth.
        if packet_len < 0 || i64::from(packet_len) > MAX_PACKET_LENGTH {
            return Err(CompressionError::PacketLengthTooLarge);
        }

        let packet_len = packet_len as usize;
        let rest = &src[packet_len_bytes..];
        if rest.len() < packet_len {
            return Err(CompressionError::Truncated);
        }
        let frame = &rest[..packet_len];
        let consumed = packet_len_bytes + packet_len;

        let (data_length, data_length_bytes) = scan_varint(frame)?;
        let payload = &frame[data_length_bytes..];

        let declared = i64::from(data_length);
        if declared < 0 || declared > MAX_UNPACKED as i64 {
            // Rejected before a single byte reaches the zlib decoder: a
            // hostile frame must never control how much we allocate.
            return Err(CompressionError::UnpackedTooLarge {
                declared,
                max: MAX_UNPACKED,
            });
        }
        let declared = data_length as usize;

        if declared == 0 {
            // Uncompressed fast path: payload bytes are the packet verbatim.
            return Ok((consumed, payload.to_vec()));
        }

        // Inflate at most declared + 1 bytes; reading one byte past the
        // declared size lets us catch streams that produce more than the
        // header promised without ever expanding unboundedly.
        let mut decompressed = Vec::with_capacity(declared);
        let limit = declared as u64 + 1;
        let mut decoder = ZlibDecoder::new(payload).take(limit);
        decoder
            .read_to_end(&mut decompressed)
            .map_err(|error| match error.kind() {
                std::io::ErrorKind::UnexpectedEof => CompressionError::Truncated,
                _ => CompressionError::Zlib(error),
            })?;
        if decompressed.len() != declared {
            return Err(CompressionError::DataLengthMismatch {
                declared: data_length,
                actual: decompressed.len(),
            });
        }
        Ok((consumed, decompressed))
    }

    fn validate(&self) -> Result<(), CompressionError> {
        if self.threshold < -1 {
            return Err(CompressionError::InvalidThreshold {
                threshold: self.threshold,
            });
        }
        if !(-1..=9).contains(&self.level) {
            return Err(CompressionError::InvalidLevel { level: self.level });
        }
        Ok(())
    }

    fn flate_level(&self) -> Level {
        // flate2's Default is the zlib default (level 6); map our -1
        // sentinel onto it. Everything else passes through clamped-free:
        // validate() has already guaranteed -1..=9.
        if self.level < 0 {
            Level::default()
        } else {
            Level::new(self.level as u32)
        }
    }
}

/// Scans `src` for a complete `VarInt`.
///
/// Returns the decoded value and the number of bytes it occupied.
/// Two's complement bits beyond 32 are silently discarded, matching how
/// the reference implementation decodes `VarInt`s; unnecessarily long
/// legal encodings (e.g. `81 00` for `1`) are accepted.
fn scan_varint(src: &[u8]) -> Result<(i32, usize), CompressionError> {
    let mut value: u32 = 0;
    for position in (0..32).step_by(7) {
        let index = position / 7;
        let Some(&byte) = src.get(index) else {
            return Err(CompressionError::Truncated);
        };
        // The shift at position 28 only keeps the low 4 bits in the u32,
        // exactly like the reference implementation dropping the rest.
        value |= ((byte & 0x7F) as u32) << position;
        if byte & 0x80 == 0 {
            return Ok((value as i32, index + 1));
        }
    }
    Err(CompressionError::VarIntTooBig)
}

/// Writes `value` to `dst` as a `VarInt`.
///
/// Negative values are encoded as their full 32-bit two's complement bit
/// pattern, so they always take 5 bytes.
fn write_varint(value: i32, dst: &mut Vec<u8>) {
    let mut remaining = value as u32;
    loop {
        let mut byte = (remaining & 0x7F) as u8;
        remaining >>= 7;
        if remaining != 0 {
            byte |= 0x80;
            dst.push(byte);
        } else {
            dst.push(byte);
            return;
        }
    }
}

#[cfg(test)]
mod tests;
