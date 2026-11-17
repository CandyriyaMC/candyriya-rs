//! The native Candyriya plugin-messaging protocol: packets on the
//! `candyriya:main` channel.
//!
//! This crate implements **our own** backend ↔ proxy channel protocol —
//! binary, versioned, with native Minecraft VarInt framing and UTF-8
//! strings. It is not a rename of the BungeeCord protocol and not a
//! wrapper around it: the wire format is specified in [`schema`] and
//! implemented here from scratch. CESU-8 / Java modified UTF-8 is not
//! used; strings are plain UTF-8.
//!
//! # The channel
//!
//! `candyriya:main` — namespace `candyriya`, key `main`, exported as the
//! validated [`CANDYRIYA_CHANNEL`] identifier (and [`CANDYRIYA_CHANNEL_NAME`]
//! as a raw string). A proxy registers it on both ends like any other
//! plugin-message channel; [`is_candyriya_channel`] recognizes it.
//!
//! # Framing
//!
//! Packets produced and consumed here are **raw payloads**: the proxy
//! writes them as a Minecraft Plugin Message packet whose channel is
//! `candyriya:main`. This crate does not add an outer VarInt packet length
//! — that framing is the proxy's job. The payload layout is
//! `VarInt protocol_version || VarInt packet_id || body`; see [`schema`]
//! for the full contract, the packet tables and the common types.
//!
//! Every packet family takes the [`Direction`] as an explicit argument on
//! encode and decode; passing the wrong one is an error, never a silent
//! reinterpretation.
//!
//! # Boundaries
//!
//! - No network, no async, no I/O — bytes in, bytes out.
//! - [`MAX_PACKET_BYTES`] is enforced on encode and decode.
//! - Parsing rejects unknown packet ids, truncated payloads and trailing
//!   junk; every failure is a [`CandyriyaMessageError`], never a panic.
//!
//! # Example
//!
//! ```
//! use candyriya_messaging_candyriya::{Direction, ServerboundMessage};
//!
//! let packet = ServerboundMessage::Transfer {
//!     target_server: "pvp".to_owned(),
//! };
//! let bytes = packet.encode(Direction::Serverbound)?;
//! let parsed = ServerboundMessage::decode(Direction::Serverbound, bytes)?;
//! assert_eq!(parsed, packet);
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! # Safety
//!
//! `#![forbid(unsafe_code)]`; all fallible operations report errors
//! through return values, and the decoders are total on arbitrary bytes.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use std::sync::LazyLock;

use candyriya_messaging::ChannelIdentifier;

mod clientbound;
pub mod codec;
mod direction;
mod error;
mod frame;
pub mod schema;
mod serverbound;

pub use clientbound::ClientboundMessage;
pub use direction::Direction;
pub use error::CandyriyaMessageError;
pub use serverbound::ServerboundMessage;

/// The only protocol version implemented by this crate.
///
/// `protocol_version` on the wire must equal this value; any other value
/// is rejected with [`CandyriyaMessageError::ProtocolVersionMismatch`]
/// before the `packet_id` is even read.
pub const CANDYRIYA_PROTOCOL_VERSION: i32 = 1;

/// Maximum size of an encoded packet payload, in bytes (32767, `0x7FFF`).
///
/// This cap is enforced on **both** encode and decode:
///
/// - on encode, a payload longer than this is rejected with
///   [`CandyriyaMessageError::PacketTooLarge`] and never reaches the caller;
/// - on decode, a payload longer than this is rejected before the first
///   field parser runs, so an oversized payload never reaches the field
///   parsers.
///
/// A payload of exactly 32767 bytes is accepted; 32768 bytes is rejected.
/// The value is chosen to match Minecraft's 32767-unit string cap
/// (`0x7FFF`, the largest positive signed 16-bit integer): nothing in this
/// protocol needs more, and a hard ceiling bounds allocation for every
/// buffer involved in moving a packet.
pub const MAX_PACKET_BYTES: usize = 32767;

/// The channel name in raw form. Use [`CANDYRIYA_CHANNEL`] for the
/// validated identifier.
pub const CANDYRIYA_CHANNEL_NAME: &str = "candyriya:main";

/// [`CANDYRIYA_CHANNEL_NAME`] as a validated [`ChannelIdentifier`].
///
/// The validation cannot fail (the name is a hardcoded constant and the
/// channel rules are exercised in the test suite), so the first access
/// always succeeds.
pub static CANDYRIYA_CHANNEL: LazyLock<ChannelIdentifier> = LazyLock::new(|| {
    ChannelIdentifier::new(CANDYRIYA_CHANNEL_NAME)
        .expect("hardcoded CANDYRIYA_CHANNEL_NAME must be a valid channel identifier")
});

/// Whether `channel` is the `candyriya:main` channel.
pub fn is_candyriya_channel(channel: &ChannelIdentifier) -> bool {
    channel.as_str() == CANDYRIYA_CHANNEL_NAME
}
