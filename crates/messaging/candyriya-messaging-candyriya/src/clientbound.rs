//! Clientbound packets (proxy → backend), packet ids `0x80..=0x8F`.
//!
//! [`ClientboundMessage::packet_id`] is the single source of truth for id
//! assignment; the table in [`crate::schema`] documents the same layout.
//! Every response packet id equals the id of the request that triggered it
//! with bit `0x80` set.

use bytes::Bytes;

use crate::codec;
use crate::direction::Direction;
use crate::error::CandyriyaMessageError;
use crate::frame;

/// A packet the proxy sends to a backend on `candyriya:main`.
///
/// The variants are exactly the packets of the `0x80..=0x8F` id range;
/// there is no catch-all variant because the wire format has no room for
/// one (unknown ids are rejected on decode, never represented).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientboundMessage {
    /// `0x81` — answer to `0x01` (`Transfer`): whether the current player
    /// was moved to `target_server`. `error` is empty on success and
    /// carries a human-readable reason on failure.
    TransferResult {
        /// Whether the transfer succeeded.
        ok: bool,
        /// Empty on success; a reason on failure.
        error: String,
    },
    /// `0x82` — answer to `0x02` (`TransferOther`): whether `target_player`
    /// was moved to the requested server. `error` is empty on success and
    /// carries a human-readable reason on failure.
    TransferOtherResult {
        /// Username of the player the request referred to.
        target_player: String,
        /// Whether the transfer succeeded.
        ok: bool,
        /// Empty on success; a reason on failure.
        error: String,
    },
    /// `0x83` — answer to `0x03` (`GetServer`): the name of the server the
    /// current player is on.
    Server {
        /// Name of the backend server the player is on.
        name: String,
    },
    /// `0x84` — answer to `0x04` (`GetServers`): the names of the servers
    /// the proxy knows about, as a CSV string
    /// (`"lobby, pvp, factions"`, comma + single space separator).
    Servers {
        /// Comma-separated server names.
        names: String,
    },
    /// `0x85` — answer to `0x05` (`ServerInfo`): the online status, player
    /// counts and measured latency of `target_server`. `players`,
    /// `max_players` and `latency_ms` are VarInts.
    ServerInfo {
        /// Name of the backend server that was pinged.
        target_server: String,
        /// Whether the backend answered the ping.
        online: bool,
        /// Players currently on the backend.
        players: i32,
        /// Maximum players the backend accepts.
        max_players: i32,
        /// Measured round-trip time of the ping, in milliseconds.
        latency_ms: i32,
    },
    /// `0x86` — answer to `0x06` (`KickPlayer`): whether the kick was
    /// accepted by the proxy.
    KickResult {
        /// Whether the kick was accepted.
        ok: bool,
    },
    /// `0x87` — answer to `0x07` (`Message`): whether the message was
    /// delivered to the player.
    MessageResult {
        /// Whether the message was delivered.
        ok: bool,
    },
    /// `0x88` — answer to `0x08` (`Broadcast`): whether the broadcast was
    /// sent.
    BroadcastResult {
        /// Whether the broadcast was sent.
        ok: bool,
    },
    /// `0x89` — answer to `0x09` (`UUID`): the UUID of `target_player` in
    /// the dashed hyphenated form, or the empty string when the proxy
    /// does not know the player.
    UUID {
        /// Username of the player the request referred to.
        target_player: String,
        /// Dashed UUID, or empty when unknown.
        uuid: String,
    },
    /// `0x8A` — answer to `0x0A` (`PlayerOnServer`): the server
    /// `target_player` is on, or the empty string when the proxy does not
    /// know.
    PlayerOnServer {
        /// Username of the player the request referred to.
        target_player: String,
        /// Name of the backend server the player is on, or empty when
        /// unknown.
        server: String,
    },
    /// `0x8B` — unsolicited notification sent to **all** backends when a
    /// player switched **to** this backend. A security/audit signal; there
    /// is no reply.
    TransferNotify {},
}

impl ClientboundMessage {
    const DIRECTION: Direction = Direction::Clientbound;

    /// The `packet_id` of `self` on the wire.
    pub fn packet_id(&self) -> i32 {
        match self {
            Self::TransferResult { .. } => 0x81,
            Self::TransferOtherResult { .. } => 0x82,
            Self::Server { .. } => 0x83,
            Self::Servers { .. } => 0x84,
            Self::ServerInfo { .. } => 0x85,
            Self::KickResult { .. } => 0x86,
            Self::MessageResult { .. } => 0x87,
            Self::BroadcastResult { .. } => 0x88,
            Self::UUID { .. } => 0x89,
            Self::PlayerOnServer { .. } => 0x8A,
            Self::TransferNotify {} => 0x8B,
        }
    }

    /// Encodes `self` as a `candyriya:main` payload (`VarInt
    /// protocol_version || VarInt packet_id || body`).
    ///
    /// `direction` is the second parameter because Rust requires the receiver
    /// first; this is the method-syntax spelling of the brief's
    /// `encode(direction, &self)` sketch.
    ///
    /// The direction must be [`Direction::Clientbound`]; encoding this
    /// family as serverbound is rejected with
    /// [`CandyriyaMessageError::DirectionMismatch`]. The returned bytes are
    /// raw payload — the proxy wraps them in a Plugin Message packet.
    ///
    /// # Errors
    ///
    /// - [`CandyriyaMessageError::DirectionMismatch`] if `direction` is not
    ///   [`Direction::Clientbound`],
    /// - [`CandyriyaMessageError::StringTooLong`] if a string field exceeds
    ///   32767 UTF-16 code units,
    /// - [`CandyriyaMessageError::PacketTooLarge`] if the encoded payload
    ///   exceeds [`crate::MAX_PACKET_BYTES`] (32767) bytes.
    pub fn encode(&self, direction: Direction) -> Result<Bytes, CandyriyaMessageError> {
        let mut buf = frame::begin_packet(direction, Self::DIRECTION, self.packet_id())?;
        match self {
            Self::TransferResult { ok, error } => {
                codec::write_boolean(*ok, &mut buf);
                codec::write_string(error, &mut buf)?;
            }
            Self::TransferOtherResult {
                target_player,
                ok,
                error,
            } => {
                codec::write_string(target_player, &mut buf)?;
                codec::write_boolean(*ok, &mut buf);
                codec::write_string(error, &mut buf)?;
            }
            Self::Server { name } => {
                codec::write_string(name, &mut buf)?;
            }
            Self::Servers { names } => {
                codec::write_string(names, &mut buf)?;
            }
            Self::ServerInfo {
                target_server,
                online,
                players,
                max_players,
                latency_ms,
            } => {
                codec::write_string(target_server, &mut buf)?;
                codec::write_boolean(*online, &mut buf);
                codec::write_varint(*players, &mut buf);
                codec::write_varint(*max_players, &mut buf);
                codec::write_varint(*latency_ms, &mut buf);
            }
            Self::KickResult { ok } | Self::MessageResult { ok } | Self::BroadcastResult { ok } => {
                codec::write_boolean(*ok, &mut buf);
            }
            Self::UUID {
                target_player,
                uuid,
            } => {
                codec::write_string(target_player, &mut buf)?;
                codec::write_string(uuid, &mut buf)?;
            }
            Self::PlayerOnServer {
                target_player,
                server,
            } => {
                codec::write_string(target_player, &mut buf)?;
                codec::write_string(server, &mut buf)?;
            }
            Self::TransferNotify {} => {}
        }
        frame::finish_packet(buf)
    }

    /// Decodes a `candyriya:main` payload into a [`ClientboundMessage`].
    ///
    /// The direction must be [`Direction::Clientbound`]; passing
    /// [`Direction::Serverbound`] is rejected with
    /// [`CandyriyaMessageError::DirectionMismatch`] before any byte is
    /// interpreted. Unknown packet ids, a wrong protocol version, empty or
    /// oversized payloads, malformed fields and trailing bytes are all
    /// errors.
    ///
    /// # Errors
    ///
    /// - [`CandyriyaMessageError::DirectionMismatch`] if `direction` is not
    ///   [`Direction::Clientbound`],
    /// - [`CandyriyaMessageError::EmptyPayload`] if the payload is empty,
    /// - [`CandyriyaMessageError::PacketTooLarge`] if the payload exceeds
    ///   [`crate::MAX_PACKET_BYTES`] (32767) bytes,
    /// - [`CandyriyaMessageError::ProtocolVersionMismatch`] if the
    ///   `protocol_version` field is not 1,
    /// - [`CandyriyaMessageError::UnknownPacketId`] if `packet_id` is not a
    ///   clientbound packet id,
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
            0x81 => Self::TransferResult {
                ok: codec::read_boolean_at(&mut cursor, "ok")?,
                error: codec::read_string_at(&mut cursor, "error")?,
            },
            0x82 => Self::TransferOtherResult {
                target_player: codec::read_string_at(&mut cursor, "target_player")?,
                ok: codec::read_boolean_at(&mut cursor, "ok")?,
                error: codec::read_string_at(&mut cursor, "error")?,
            },
            0x83 => Self::Server {
                name: codec::read_string_at(&mut cursor, "name")?,
            },
            0x84 => Self::Servers {
                names: codec::read_string_at(&mut cursor, "names")?,
            },
            0x85 => Self::ServerInfo {
                target_server: codec::read_string_at(&mut cursor, "target_server")?,
                online: codec::read_boolean_at(&mut cursor, "online")?,
                players: codec::read_varint_at(&mut cursor, "players")?,
                max_players: codec::read_varint_at(&mut cursor, "max_players")?,
                latency_ms: codec::read_varint_at(&mut cursor, "latency_ms")?,
            },
            0x86 => Self::KickResult {
                ok: codec::read_boolean_at(&mut cursor, "ok")?,
            },
            0x87 => Self::MessageResult {
                ok: codec::read_boolean_at(&mut cursor, "ok")?,
            },
            0x88 => Self::BroadcastResult {
                ok: codec::read_boolean_at(&mut cursor, "ok")?,
            },
            0x89 => Self::UUID {
                target_player: codec::read_string_at(&mut cursor, "target_player")?,
                uuid: codec::read_string_at(&mut cursor, "uuid")?,
            },
            0x8A => Self::PlayerOnServer {
                target_player: codec::read_string_at(&mut cursor, "target_player")?,
                server: codec::read_string_at(&mut cursor, "server")?,
            },
            0x8B => Self::TransferNotify {},
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
