//! Serverbound packets (backend → proxy), packet ids `0x01..=0x0F`.
//!
//! [`ServerboundMessage::packet_id`] is the single source of truth for id
//! assignment; the table in [`crate::schema`] documents the same layout.
//! Requests that expect a reply pair with a clientbound packet whose id
//! is `request_id | 0x80`.

use bytes::Bytes;

use crate::codec;
use crate::direction::Direction;
use crate::error::CandyriyaMessageError;
use crate::frame;

/// A packet a backend sends to the proxy on `candyriya:main`.
///
/// The variants are exactly the packets of the `0x01..=0x0F` id range;
/// there is no catch-all variant because the wire format has no room for
/// one (unknown ids are rejected on decode, never represented).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerboundMessage {
    /// `0x01` — request to move the **current** player of this connection
    /// to `target_server`. Reply expected on `0x81` (`TransferResult`).
    Transfer {
        /// Name of the backend server to move the player to.
        target_server: String,
    },
    /// `0x02` — request to move `target_player` (not necessarily the current
    /// player) to `target_server`. Reply expected on `0x82`
    /// (`TransferOtherResult`).
    TransferOther {
        /// Username of the player to move.
        target_player: String,
        /// Name of the backend server to move the player to.
        target_server: String,
    },
    /// `0x03` — ask which server the current player is on. Reply expected
    /// on `0x83` (`Server`).
    GetServer {},
    /// `0x04` — ask for the list of servers the proxy knows about. Reply
    /// expected on `0x84` (`Servers`).
    GetServers {},
    /// `0x05` — ping `target_server`: ask for its online status, player
    /// count and measured latency. Reply expected on `0x85` (`ServerInfo`).
    ServerInfo {
        /// Name of the backend server to ping.
        target_server: String,
    },
    /// `0x06` — kick `target_player` with the given reason. There is no
    /// confirmation unless the kick is refused; the proxy answers on
    /// `0x86` (`KickResult`). `reason` is opaque: JSON chat component or
    /// plain text, rendered by the proxy.
    KickPlayer {
        /// Username of the player to kick.
        target_player: String,
        /// Reason shown to the kicked player.
        reason: String,
    },
    /// `0x07` — send `target_player` a chat message. Reply expected on
    /// `0x87` (`MessageResult`). `message` is a MiniMessage string; it is
    /// not parsed by this crate.
    Message {
        /// Username of the player to message.
        target_player: String,
        /// MiniMessage payload of the message.
        message: String,
    },
    /// `0x08` — broadcast `message` to every player on every backend.
    /// Reply expected on `0x88` (`BroadcastResult`). `message` is a
    /// MiniMessage string; it is not parsed by this crate.
    Broadcast {
        /// MiniMessage payload of the broadcast.
        message: String,
    },
    /// `0x09` — ask for the UUID of `target_player`. Reply expected on
    /// `0x89` (`UUID`).
    UUID {
        /// Username of the player to look up.
        target_player: String,
    },
    /// `0x0A` — ask which server `target_player` is on. Reply expected on
    /// `0x8A` (`PlayerOnServer`).
    PlayerOnServer {
        /// Username of the player to look up.
        target_player: String,
    },
    /// `0x0B` — backend confirms that a player transfer/switch to this
    /// backend completed. There is no reply; this is a proxy-internal
    /// book-keeping signal.
    Transferred {},
}

impl ServerboundMessage {
    const DIRECTION: Direction = Direction::Serverbound;

    /// The `packet_id` of `self` on the wire.
    pub fn packet_id(&self) -> i32 {
        match self {
            Self::Transfer { .. } => 0x01,
            Self::TransferOther { .. } => 0x02,
            Self::GetServer {} => 0x03,
            Self::GetServers {} => 0x04,
            Self::ServerInfo { .. } => 0x05,
            Self::KickPlayer { .. } => 0x06,
            Self::Message { .. } => 0x07,
            Self::Broadcast { .. } => 0x08,
            Self::UUID { .. } => 0x09,
            Self::PlayerOnServer { .. } => 0x0A,
            Self::Transferred {} => 0x0B,
        }
    }

    /// Encodes `self` as a `candyriya:main` payload (`VarInt
    /// protocol_version || VarInt packet_id || body`).
    ///
    /// `direction` is the second parameter because Rust requires the receiver
    /// first; this is the method-syntax spelling of the brief's
    /// `encode(direction, &self)` sketch.
    ///
    /// The direction must be [`Direction::Serverbound`]; encoding this
    /// family as clientbound is rejected with
    /// [`CandyriyaMessageError::DirectionMismatch`]. The returned bytes are
    /// raw payload — the proxy wraps them in a Plugin Message packet.
    ///
    /// # Errors
    ///
    /// - [`CandyriyaMessageError::DirectionMismatch`] if `direction` is not
    ///   [`Direction::Serverbound`],
    /// - [`CandyriyaMessageError::StringTooLong`] if a string field exceeds
    ///   32767 UTF-16 code units,
    /// - [`CandyriyaMessageError::PacketTooLarge`] if the encoded payload
    ///   exceeds [`crate::MAX_PACKET_BYTES`] (32767) bytes.
    pub fn encode(&self, direction: Direction) -> Result<Bytes, CandyriyaMessageError> {
        let mut buf = frame::begin_packet(direction, Self::DIRECTION, self.packet_id())?;
        match self {
            Self::Transfer { target_server } => {
                codec::write_string(target_server, &mut buf)?;
            }
            Self::TransferOther {
                target_player,
                target_server,
            } => {
                codec::write_string(target_player, &mut buf)?;
                codec::write_string(target_server, &mut buf)?;
            }
            Self::GetServer {} | Self::GetServers {} | Self::Transferred {} => {}
            Self::ServerInfo { target_server } => {
                codec::write_string(target_server, &mut buf)?;
            }
            Self::KickPlayer {
                target_player,
                reason,
            } => {
                codec::write_string(target_player, &mut buf)?;
                codec::write_string(reason, &mut buf)?;
            }
            Self::Message {
                target_player,
                message,
            } => {
                codec::write_string(target_player, &mut buf)?;
                codec::write_string(message, &mut buf)?;
            }
            Self::Broadcast { message } => {
                codec::write_string(message, &mut buf)?;
            }
            Self::UUID { target_player } | Self::PlayerOnServer { target_player } => {
                codec::write_string(target_player, &mut buf)?;
            }
        }
        frame::finish_packet(buf)
    }

    /// Decodes a `candyriya:main` payload into a [`ServerboundMessage`].
    ///
    /// The direction must be [`Direction::Serverbound`]; passing
    /// [`Direction::Clientbound`] is rejected with
    /// [`CandyriyaMessageError::DirectionMismatch`] before any byte is
    /// interpreted. Unknown packet ids, a wrong protocol version, empty or
    /// oversized payloads, malformed fields and trailing bytes are all
    /// errors.
    ///
    /// # Errors
    ///
    /// - [`CandyriyaMessageError::DirectionMismatch`] if `direction` is not
    ///   [`Direction::Serverbound`],
    /// - [`CandyriyaMessageError::EmptyPayload`] if the payload is empty,
    /// - [`CandyriyaMessageError::PacketTooLarge`] if the payload exceeds
    ///   [`crate::MAX_PACKET_BYTES`] (32767) bytes,
    /// - [`CandyriyaMessageError::ProtocolVersionMismatch`] if the
    ///   `protocol_version` field is not 1,
    /// - [`CandyriyaMessageError::UnknownPacketId`] if `packet_id` is not a
    ///   serverbound packet id,
    /// - [`CandyriyaMessageError::UnexpectedEof`] if the payload ends
    ///   before a field is complete,
    /// - [`CandyriyaMessageError::VarIntTooBig`],
    ///   [`CandyriyaMessageError::StringTooLong`],
    ///   [`CandyriyaMessageError::InvalidUtf8`] or
    ///   [`CandyriyaMessageError::InvalidBoolean`] for malformed fields,
    /// - [`CandyriyaMessageError::TrailingBytes`] if extra bytes follow
    ///   the last field.
    pub fn decode(direction: Direction, bytes: Bytes) -> Result<Self, CandyriyaMessageError> {
        let (packet_id, mut cursor) =
            frame::decode_head(direction, Self::DIRECTION, bytes.as_ref())?;
        let message = match packet_id {
            0x01 => Self::Transfer {
                target_server: codec::read_string_at(&mut cursor, "target_server")?,
            },
            0x02 => Self::TransferOther {
                target_player: codec::read_string_at(&mut cursor, "target_player")?,
                target_server: codec::read_string_at(&mut cursor, "target_server")?,
            },
            0x03 => Self::GetServer {},
            0x04 => Self::GetServers {},
            0x05 => Self::ServerInfo {
                target_server: codec::read_string_at(&mut cursor, "target_server")?,
            },
            0x06 => Self::KickPlayer {
                target_player: codec::read_string_at(&mut cursor, "target_player")?,
                reason: codec::read_string_at(&mut cursor, "reason")?,
            },
            0x07 => Self::Message {
                target_player: codec::read_string_at(&mut cursor, "target_player")?,
                message: codec::read_string_at(&mut cursor, "message")?,
            },
            0x08 => Self::Broadcast {
                message: codec::read_string_at(&mut cursor, "message")?,
            },
            0x09 => Self::UUID {
                target_player: codec::read_string_at(&mut cursor, "target_player")?,
            },
            0x0A => Self::PlayerOnServer {
                target_player: codec::read_string_at(&mut cursor, "target_player")?,
            },
            0x0B => Self::Transferred {},
            _ => {
                return Err(CandyriyaMessageError::UnknownPacketId {
                    direction,
                    id: packet_id,
                });
            }
        };
        frame::ensure_consumed(cursor)?;
        Ok(message)
    }
}
