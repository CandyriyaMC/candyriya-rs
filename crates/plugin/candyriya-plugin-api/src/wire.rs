//! Shared building blocks for the visual packet field codecs.
//!
//! This module holds the pieces every visual packet type uses on the wire:
//! the error enum, the JSON text-component codec and the UUID helpers. It is
//! deliberately `pub(crate)` — the wire layout is an implementation detail
//! of the individual packet modules, only the types and their `to_vec` /
//! `decode` entry points are public API.
//!
//! ## Wire conventions used throughout
//!
//! - All integers big-endian unless noted; the protocol's `VarInt` helper
//!   (from `candyriya-protocol`) is used instead of hand-rolled varints.
//! - A text component on the wire is its JSON document encoded as a
//!   length-prefixed UTF-8 string, capped at [`DEFAULT_MAX_STRING_LENGTH`]
//!   UTF-16 code units — the same cap the vanilla server applies to any
//!   `Text Component` field. Note this covers the pre-1.20.3 JSON encoding;
//!   from Minecraft 1.20.3 onwards many component positions use NBT instead
//!   of JSON strings. The proxy is responsible for that translation when
//!   talking to such clients; this crate always speaks JSON.
//! - Strict booleans: `0x00` / `0x01` only, anything else is rejected.
//!
//! ## What this module does NOT do
//!
//! No packet ids, no framing, no transport. The proxy resolves the packet id
//! for the negotiated protocol version from its own per-version registry and
//! writes it itself; everything here starts after the id.

use bytes::{Buf, BufMut};
use candyriya_protocol::{
    DEFAULT_MAX_STRING_LENGTH, ProtocolError, decode_varint, read_string, read_uuid, write_string,
    write_uuid,
};
use lobsterchat::component::Component;
use uuid::Uuid;

/// Every failure that can occur while building or parsing a visual packet.
///
/// All variants are plain data errors: either the caller asked for
/// something illegal (an out-of-range boss bar health, a packet body
/// without its mandatory fields) or the peer sent bytes that do not
/// decode. No variant carries an OS error; the crate never does I/O.
///
/// This type is not `PartialEq`: [`ProtocolError`] and
/// [`serde_json::Error`] are not comparable. Tests match on the variants
/// they care about instead.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum VisualPacketError {
    /// An underlying protocol-level codec failure (varint, string, UUID).
    #[error("protocol codec error: {0}")]
    Protocol(#[from] ProtocolError),

    /// A text component failed (de)serialization of its JSON document.
    #[error("component JSON error: {0}")]
    ComponentJson(#[from] serde_json::Error),

    /// A mandatory component field was missing for the packet kind.
    ///
    /// Carries the [`crate::TitleKind`] this text belongs to, e.g. a
    /// `TitlePacket` of kind `Title` without a component fails here.
    #[error("title kind {kind:?} requires a text component")]
    MissingText {
        /// The title kind the offending packet carries.
        kind: crate::TitleKind,
    },

    /// A text component was set on a packet kind that carries none.
    #[error("title kind {kind:?} does not carry a text component")]
    UnexpectedText {
        /// The title kind the offending packet carries.
        kind: crate::TitleKind,
    },

    /// Timing information was set on a packet kind that carries none.
    #[error("title kind {kind:?} does not carry timing information")]
    UnexpectedTimes {
        /// The title kind the offending packet carries.
        kind: crate::TitleKind,
    },

    /// Timing information was required but absent.
    #[error("title kind {kind:?} requires timing information")]
    MissingTimes {
        /// The title kind the offending packet carries.
        kind: crate::TitleKind,
    },

    /// The `reset` boolean byte did not match the requested [`crate::TitleKind`].
    ///
    /// `Clear` encodes `false` and `Reset` encodes `true`; a mismatching
    /// byte means the body was paired with the wrong kind.
    #[error("reset flag {found} does not match the requested kind {kind:?}")]
    ResetFlagMismatch {
        /// The title kind the caller asked to decode.
        kind: crate::TitleKind,
        /// The byte found on the wire (`0x00` or `0x01`).
        found: u8,
    },

    /// A byte expected to be a strict boolean was neither `0x00` nor `0x01`.
    #[error("invalid boolean byte 0x{0:02X}")]
    InvalidBoolean(u8),

    /// Boss bar health was not within the inclusive `0.0..=1.0` range or
    /// not finite.
    #[error("boss bar health {0} is out of the 0.0..=1.0 range or not finite")]
    HealthOutOfRange(f32),

    /// The decoded boss bar health was NaN or infinite.
    ///
    /// Out-of-range but finite values pass through on decode: the client
    /// renders those without crashing (see the module docs of `bossbar`),
    /// but NaN/inf would poison the render math downstream.
    #[error("boss bar health is not finite on the wire")]
    HealthNotFinite,

    /// An enum discriminant arrived that this crate does not know.
    ///
    /// Carries the raw discriminant value; it is `i32` because all visual
    /// packet enums go over the wire as `VarInt`.
    #[error("unknown {what} discriminant {found}")]
    UnknownDiscriminant {
        /// Which enum the discriminant belongs to.
        what: &'static str,
        /// The received discriminant value.
        found: i32,
    },

    /// A collection that must not be empty was empty.
    #[error("{what} must not be empty")]
    EmptyCollection {
        /// Which collection was empty.
        what: &'static str,
    },

    /// A collection exceeded its cap.
    #[error("{what} exceeds the cap of {cap}")]
    CollectionTooLarge {
        /// Which collection exceeded its cap.
        what: &'static str,
        /// The cap that was exceeded.
        cap: usize,
    },

    /// A game mode value outside the vanilla `0..=3` range was supplied
    /// on the outgoing side. The decoder accepts any `VarInt` for forward
    /// compatibility with future game modes.
    #[error("game mode {0} is outside the vanilla 0..=3 range")]
    InvalidGameMode(i32),

    /// The `actions` mask implied by one entry disagrees with the mask
    /// implied by the other entries. The wire mask is per packet, so all
    /// entries must carry exactly the same set of payloads.
    #[error("player info entries disagree on the action mask: {0:#04x} vs {1:#04x}")]
    MaskMismatch(u8, u8),

    /// The entries imply no actions at all; encoding an empty action mask
    /// would be rejected by vanilla clients.
    #[error("player info entries imply an empty action mask")]
    EmptyActions,

    /// The mask contains the `Initialize Chat` bit, which this crate does
    /// not (de)serialize.
    ///
    /// Chat session data (session id, expiry, public key, signature) is
    /// deliberately not modelled: the host never forges client keys, and
    /// inventing a "chat session" payload would produce packets the client
    /// treats as authentic-looking. It can be added when a real use case
    /// shows up.
    #[error("the Initialize Chat action (0x02) is not supported by this crate")]
    UnsupportedInitializeChat,

    /// Fewer or more bytes than expected were present after a complete
    /// value, or a declared count exceeded the remaining bytes.
    #[error("trailing bytes after a complete value: {0} left")]
    TrailingBytes(usize),

    /// A `VarInt` prefixed-array count was negative, which cannot be
    /// represented as an array length.
    #[error("invalid {what} count {count}: must be non-negative")]
    InvalidArrayCount {
        /// Which array had the negative count.
        what: &'static str,
        /// The received count.
        count: i32,
    },
}

impl VisualPacketError {
    /// Whether `err` is a strict-boolean violation of some byte.
    ///
    /// Convenience predicate for call sites that want to report malformed
    /// packets distinctly from semantic errors.
    pub fn is_invalid_boolean(&self) -> bool {
        matches!(self, Self::InvalidBoolean(_))
    }
}

/// Maximum number of players encoded in one `Player Info Update` /
/// `Player Info Remove` packet. Vanilla does not cap this, but a hostile
/// backend or plugin could otherwise make the decoder allocate unboundedly;
/// 4096 is far above any realistic player-list size.
pub(crate) const MAX_ENTRIES: usize = 4096;

/// Maximum number of game profile properties in one `Add Player` payload.
/// Mirrors the `Prefixed Array (16)` cap from the protocol spec.
pub(crate) const MAX_PROPERTIES: usize = 16;

/// Writes `component` as its JSON document, length-prefixed as a string.
pub(crate) fn write_component(
    dst: &mut impl BufMut,
    component: &Component,
) -> Result<(), VisualPacketError> {
    let json = serde_json::to_string(component)?;
    write_string(dst, &json, DEFAULT_MAX_STRING_LENGTH)?;
    Ok(())
}

/// Reads a component previously written as a length-prefixed JSON string.
pub(crate) fn read_component(src: &mut impl Buf) -> Result<Component, VisualPacketError> {
    let json = read_string(src, DEFAULT_MAX_STRING_LENGTH)?;
    Ok(serde_json::from_str(&json)?)
}

/// Writes a UUID in the protocol's two-`i64` big-endian layout.
pub(crate) fn write_uuid_wire(dst: &mut impl BufMut, uuid: Uuid) {
    let (msb, lsb) = uuid.as_u64_pair();
    write_uuid(msb as i64, lsb as i64, dst);
}

/// Reads a UUID in the protocol's two-`i64` big-endian layout.
pub(crate) fn read_uuid_wire(src: &mut impl Buf) -> Result<Uuid, VisualPacketError> {
    let (msb, lsb) = read_uuid(src)?;
    Ok(Uuid::from_u64_pair(msb as u64, lsb as u64))
}

/// Reads a strict boolean: `0x00` is false, `0x01` is true, anything else
/// is rejected.
pub(crate) fn read_bool(src: &mut impl Buf) -> Result<bool, VisualPacketError> {
    let byte = read_u8(src)?;
    match byte {
        0x00 => Ok(false),
        0x01 => Ok(true),
        other => Err(VisualPacketError::InvalidBoolean(other)),
    }
}

/// Writes a strict boolean (`0x00` / `0x01`).
pub(crate) fn write_bool(dst: &mut impl BufMut, value: bool) {
    dst.put_u8(u8::from(value));
}

/// Reads one byte, erroring with [`ProtocolError::Truncated`] at end of input.
pub(crate) fn read_u8(src: &mut impl Buf) -> Result<u8, VisualPacketError> {
    if src.remaining() < 1 {
        return Err(ProtocolError::Truncated.into());
    }
    Ok(src.get_u8())
}

/// Reads a `VarInt`, surfacing protocol errors as [`VisualPacketError`].
pub(crate) fn read_varint(src: &mut impl Buf) -> Result<i32, VisualPacketError> {
    Ok(decode_varint(src)?)
}

/// Writes an `i32` big-endian (the protocol's `Int` type, e.g. title times).
pub(crate) fn write_int(dst: &mut impl BufMut, value: i32) {
    dst.put_i32(value);
}

/// Reads a big-endian `i32` (the protocol's `Int` type).
pub(crate) fn read_int(src: &mut impl Buf) -> Result<i32, VisualPacketError> {
    if src.remaining() < 4 {
        return Err(ProtocolError::Truncated.into());
    }
    Ok(src.get_i32())
}

/// Fails with [`VisualPacketError::TrailingBytes`] when any bytes remain.
pub(crate) fn ensure_exhausted(src: &impl Buf) -> Result<(), VisualPacketError> {
    let remaining = src.remaining();
    if remaining > 0 {
        return Err(VisualPacketError::TrailingBytes(remaining));
    }
    Ok(())
}
