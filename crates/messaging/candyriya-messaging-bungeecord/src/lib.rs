//! The BungeeCord plugin-messaging subchannel codec: a byte-exact Rust
//! implementation of the `BungeeCord` / `bungeecord:main` plugin-message
//! channel protocol.
//!
//! This crate implements the **protocol only**. There is no proxy wiring,
//! no routing, no networking and no decisions about *when* to send a
//! message — it turns subchannel messages into bytes and back, the same
//! bytes BungeeCord, Spigot/Paper backends and Velocity exchange.
//!
//! # The channel
//!
//! Two channel names carry the protocol, both shipped here as validated
//! [`candyriya_messaging::ChannelIdentifier`]s:
//!
//! - [`BUNGEECORD_CHANNEL`] — `bungeecord:main`, the modern `namespace:key`
//!   name used since Minecraft 1.13. Paper remaps the legacy name to this
//!   one internally, so Paper backends can keep registering `BungeeCord`.
//! - [`BUNGEECORD_LEGACY_CHANNEL`] — `BungeeCord`, the pre-1.13 legacy
//!   single-part name.
//!
//! [`is_bungeecord_channel`] recognizes either.
//!
//! # Wire format
//!
//! Payloads are `java.io.DataOutputStream` byte arrays: every payload starts
//! with the subchannel as a `writeUTF` string, followed by the subchannel's
//! arguments in order. `writeUTF` is a two-byte big-endian unsigned length
//! prefix plus modified UTF-8 (CESU-8); the codec and the Java primitives
//! are implemented in [`candyriya_messaging`] and are reused here, never
//! reimplemented. Integers are big-endian: `writeInt` is four bytes signed
//! (`i32`), `writeUnsignedShort` is two bytes unsigned (`u16`), `writeShort`
//! is two bytes (`u16` semantics for the nested `Forward` length prefix).
//!
//! # Subchannel reference
//!
//! `cat` is either `arg` (the argument form sent *to* the proxy; this is
//! what [`BungeeCodec::parse`] decodes and [`BungeeCodec::encode`] emits)
//! or `response` (the reply the proxy writes *for* the requester; produced
//! by the free functions in [`response`]). `", "` marks the comma-space CSV
//! separator; `unix://` marks the Unix-socket address form with port `0`.
//!
//! | Subchannel | cat | Payload |
//! |------------|-----|---------|
//! | `Connect` | arg | `writeUTF(server)` |
//! | `ConnectOther` | arg | `writeUTF(player)`, `writeUTF(server)` |
//! | `IP` | arg | *(no arguments)* |
//! | `IP` | response | `writeUTF(ip)`, `writeInt(port)` |
//! | `IPOther` | arg | `writeUTF(player)` |
//! | `IPOther` | response | `writeUTF(username)`, `writeUTF(address)` or `"unix://<path>"`, `writeInt(port)` |
//! | `PlayerCount` | arg | `writeUTF(server)` or `writeUTF("ALL")` for the global count |
//! | `PlayerCount` | response | `writeUTF(server)`, `writeInt(count)` |
//! | `PlayerList` | arg | `writeUTF(server)` or `writeUTF("ALL")` for the global list |
//! | `PlayerList` | response | `writeUTF(server)`, `writeUTF(", ".join(players))` |
//! | `GetPlayerServer` | arg | `writeUTF(player)` |
//! | `GetPlayerServer` | response | `writeUTF(username)`, `writeUTF(server)` — `""` when unknown |
//! | `GetServers` | arg | *(no arguments)* |
//! | `GetServers` | response | `writeUTF(", ".join(servers))` |
//! | `Message` | arg | `writeUTF(target)` or `writeUTF("ALL")`, `writeUTF(message)` |
//! | `MessageRaw` | arg | `writeUTF(target)` or `writeUTF("ALL")`, `writeUTF(message)` (JSON chat component) |
//! | `GetServer` | arg | *(no arguments)* |
//! | `GetServer` | response | `writeUTF(server)` |
//! | `Forward` | arg | `writeUTF(target)` (server, `"ALL"` or `"ONLINE"`), `writeUTF(channel)`, `writeShort(len)`, `len` bytes |
//! | `Forward` | response | `writeUTF(channel)`, `writeShort(len)`, `len` bytes |
//! | `ForwardToPlayer` | arg | `writeUTF(player)`, `writeUTF(channel)`, `writeShort(len)`, `len` bytes |
//! | `ForwardToPlayer` | response | `writeUTF(channel)`, `writeShort(len)`, `len` bytes |
//! | `UUID` | arg | *(no arguments)* |
//! | `UUID` | response | `writeUTF(uuid)` — dashed string form |
//! | `UUIDOther` | arg | `writeUTF(player)` |
//! | `UUIDOther` | response | `writeUTF(username)`, `writeUTF(uuid)` — dashed string form |
//! | `ServerIP` | arg | `writeUTF(server)` |
//! | `ServerIP` | response | `writeUTF(server)`, `writeUTF(ip)`, `writeUnsignedShort(port)` |
//! | `KickPlayer` | arg | `writeUTF(player)`, `writeUTF(reason)` |
//! | `KickPlayerRaw` | arg | `writeUTF(player)`, `writeUTF(reason)` (JSON chat component) |
//!
//! ## Conventions that cross-cut the table
//!
//! - The `cat=response` payload is written on the same channel back to the
//!   requester. The response always starts with its subchannel as `writeUTF`
//!   (matching what BungeeCord writes), **except** `Forward`/`ForwardToPlayer`:
//!   the receiving server sees the nested `writeUTF(channel)` as the first
//!   field, never the string `Forward`.
//! - `PlayerCount`/`PlayerList` accept the sentinel `"ALL"`; the response
//!   echoes it. `Forward` additionally accepts `"ONLINE"`.
//! - Subchannels that take no arguments (`IP`, `GetServers`, `GetServer`,
//!   `UUID`) produce a payload of exactly one `writeUTF`.
//! - UUID responses use the dashed string form as a plain `readUTF` string.
//! - `ServerIP` ports are `writeUnsignedShort` (`u16`); the IP subchannels'
//!   ports are `writeInt` (`i32`). Mixing them up flips the byte count.
//! - `MessageRaw`/`KickPlayerRaw` carry raw JSON chat components; this crate
//!   does not parse or validate that JSON.
//! - `Forward`'s nested `message` is opaque payload bytes; this crate does
//!   not inspect it. The two-byte unsigned length prefix caps it at 65535
//!   bytes.
//!
//! The well-known number 19 attached to this protocol is a miscount: the
//! wiki documents exactly the subchannels in the table, and the upstream
//! implementations switch on exactly the same set (BungeeCord's
//! `DownstreamBridge`, Velocity's `BungeeCordMessageResponder`). The enum
//! has one variant per row.
//!
//! # Example
//!
//! ```
//! use candyriya_messaging_bungeecord::{BungeeCodec, BungeeMessage};
//!
//! let codec = BungeeCodec;
//!
//! let request = BungeeMessage::Connect {
//!     server: "pvp".to_owned(),
//! };
//! let bytes = codec.encode(&request)?;
//! let parsed = codec.parse(&bytes)?;
//! assert_eq!(parsed, request);
//! # Ok::<(), Box<dyn core::error::Error>>(())
//! ```
//!
//! # Safety
//!
//! `#![forbid(unsafe_code)]`; all fallible operations report errors through
//! return values.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod channels;
mod codec;
mod error;
mod message;
pub mod response;

pub use channels::{
    BUNGEECORD_CHANNEL, BUNGEECORD_CHANNEL_NAME, BUNGEECORD_LEGACY_CHANNEL,
    BUNGEECORD_LEGACY_CHANNEL_NAME, SUBCHANNEL_NAMES, is_bungeecord_channel,
};
pub use codec::BungeeCodec;
pub use error::BungeeMessagingError;
pub use message::BungeeMessage;
