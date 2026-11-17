//! The `candyriya:main` protocol contract, as data.
//!
//! This module is documentation-only: it exists so the wire specification
//! lives in one place, next to the code that implements it. The tables
//! below are the source of truth for [`crate::ServerboundMessage`],
//! [`crate::ClientboundMessage`] and their tests.
//!
//! # Channel
//!
//! `candyriya:main` — namespace `candyriya`, key `main`. This is Candyriya's
//! own channel, distinct from the BungeeCord (`bungeecord:main`) and
//! Velocity semantics implemented by the other crates in this workspace.
//! It is deliberately **not** compatible with them: the framing, string
//! encoding and packet layout are different on purpose.
//!
//! # Framing
//!
//! Packets produced and consumed by this crate are **raw payloads**, not
//! wrapped in an outer VarInt packet length. The proxy writes the payload
//! as a Minecraft *Plugin Message* packet whose channel is `candyriya:main`;
//! that outer framing is the proxy's job and out of scope here.
//!
//! The payload layout is:
//!
//! | Field | Type | Value |
//! |-------|------|-------|
//! | `protocol_version` | VarInt | `1` ([`crate::CANDYRIYA_PROTOCOL_VERSION`]); any other value is rejected with [`crate::CandyriyaMessageError::ProtocolVersionMismatch`] |
//! | `packet_id` | VarInt | one of the ids in the tables below; anything else is rejected with [`crate::CandyriyaMessageError::UnknownPacketId`] |
//! | `payload...` | — | the fields of the packet, in table order |
//!
//! A well-formed payload is exactly consumed: trailing bytes are rejected
//! with [`crate::CandyriyaMessageError::TrailingBytes`]. An empty payload
//! is rejected with [`crate::CandyriyaMessageError::EmptyPayload`]. The
//! total payload size is capped at [`crate::MAX_PACKET_BYTES`] bytes on
//! both encode and decode.
//!
//! The direction is not on the wire; it is implied by which side produced
//! the payload. Encoding or decoding with the wrong direction is rejected
//! with [`crate::CandyriyaMessageError::DirectionMismatch`].
//!
//! # Common types
//!
//! | Type | Encoding |
//! |------|----------|
//! | VarInt | 32-bit two's complement, 1–5 bytes, little-endian 7-bit groups (Minecraft VarInt) |
//! | String | VarInt byte-length prefix + UTF-8 bytes. **Not** CESU-8 / Java modified UTF-8 |
//! | ByteArray | VarInt length prefix + raw bytes |
//! | Boolean | one byte: `0x00` = false, `0x01` = true |
//! | Uuid | 16 bytes: most significant 64 bits then least significant 64 bits, each big-endian |
//!
//! Integers (`players`, `max_players`, `latency_ms`) are VarInts. Strings
//! are capped at 32767 UTF-16 code units, the largest value the format
//! allows; the whole payload is additionally capped by
//! [`crate::MAX_PACKET_BYTES`].
//!
//! # Direction and packet id pairing
//!
//! Serverbound packets (backend → proxy) use ids `0x01..=0x0F`.
//! Clientbound packets (proxy → backend) use ids `0x80..=0x8F`.
//! For every request/response pair, the clientbound id equals the
//! serverbound id with bit `0x80` set (`0x01 | 0x80 == 0x81`). A backend
//! that sends `0x03` may assume the answer, if any, arrives as `0x83`.
//!
//! # Serverbound packets (backend → proxy), ids `0x01..=0x0F`
//!
//! | ID | Packet | Fields | Reply |
//! |----|--------|--------|-------|
//! | `0x01` | `Transfer` | `target_server: String` | `0x81` `TransferResult` |
//! | `0x02` | `TransferOther` | `target_player: String`, `target_server: String` | `0x82` `TransferOtherResult` |
//! | `0x03` | `GetServer` | *(none)* | `0x83` `Server` |
//! | `0x04` | `GetServers` | *(none)* | `0x84` `Servers` |
//! | `0x05` | `ServerInfo` | `target_server: String` | `0x85` `ServerInfo` |
//! | `0x06` | `KickPlayer` | `target_player: String`, `reason: String` (JSON or plain text) | `0x86` `KickResult` |
//! | `0x07` | `Message` | `target_player: String`, `message: String` (MiniMessage) | `0x87` `MessageResult` |
//! | `0x08` | `Broadcast` | `message: String` (MiniMessage) | `0x88` `BroadcastResult` |
//! | `0x09` | `UUID` | `target_player: String` | `0x89` `UUID` |
//! | `0x0A` | `PlayerOnServer` | `target_player: String` | `0x8A` `PlayerOnServer` |
//! | `0x0B` | `Transferred` | *(none)* | none — backend confirms a player transfer/switch completed (proxy-internal book-keeping signal) |
//!
//! # Clientbound packets (proxy → backend), ids `0x80..=0x8F`
//!
//! | ID | Packet | Fields | Request |
//! |----|--------|--------|---------|
//! | `0x81` | `TransferResult` | `ok: bool`, `error: String` (empty on success) | ← `0x01` |
//! | `0x82` | `TransferOtherResult` | `target_player: String`, `ok: bool`, `error: String` (empty on success) | ← `0x02` |
//! | `0x83` | `Server` | `name: String` | ← `0x03` |
//! | `0x84` | `Servers` | `names: String` (CSV; see below) | ← `0x04` |
//! | `0x85` | `ServerInfo` | `target_server: String`, `online: bool`, `players: VarInt` (i32), `max_players: VarInt` (i32), `latency_ms: VarInt` (i32) | ← `0x05` |
//! | `0x86` | `KickResult` | `ok: bool` | ← `0x06` |
//! | `0x87` | `MessageResult` | `ok: bool` | ← `0x07` |
//! | `0x88` | `BroadcastResult` | `ok: bool` | ← `0x08` |
//! | `0x89` | `UUID` | `target_player: String`, `uuid: String` (dashed, or empty when unknown) | ← `0x09` |
//! | `0x8A` | `PlayerOnServer` | `target_player: String`, `server: String` (or empty when unknown) | ← `0x0A` |
//! | `0x8B` | `TransferNotify` | *(none)* | unsolicited — proxy informs **all** backends that a player switched **to** them (arrival notification; security/audit signal) |
//!
//! # Field conventions
//!
//! - `names` (`0x84`) is a CSV string: server names joined by `, `
//!   (comma + single space), matching the convention Minecraft ecosystem
//!   plugins already parse. This crate does not split or join it; the
//!   proxy does, at its own boundary.
//! - `uuid` (`0x89`) is the dashed hyphenated form (`8-4-4-4-12`), or the
//!   empty string when the proxy does not know the player.
//! - `server` (`0x8A`) is the empty string when the proxy does not know
//!   which backend the player is on.
//! - `error` (`0x81`, `0x82`) is the empty string when `ok` is true.
//! - `reason` (`0x06`) is opaque to this crate: JSON chat component or
//!   plain text; the proxy decides how to render it.
//! - `message` (`0x07`, `0x08`) is a MiniMessage string; this crate does
//!   not parse it.
//! - `online` and `players` (`0x85`) describe the target backend at the
//!   moment of the ping; `latency_ms` is the measured round-trip in
//!   milliseconds. What a proxy reports for an unreachable server (and
//!   how it signals `players == 0`) is the proxy's policy, not part of
//!   this codec.
