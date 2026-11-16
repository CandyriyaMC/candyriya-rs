//! The BungeeCord compatibility set.
//!
//! Velocity's built-in plugin-message behavior is this exact compat layer:
//! the proxy answers the BungeeCord subchannel protocol on the two
//! well-known channels when the `bungee-plugin-message-channel` configuration
//! flag is enabled (it is enabled by default). There is nothing to learn for
//! a backend that already speaks BungeeCord — it talks to `bungeecord:main`
//! (or legacy `BungeeCord`, remapped internally) and gets the same answers.
//!
//! This module re-exports the constants and the subchannel inventory from
//! [`candyriya_messaging_bungeecord`] instead of restating them; the numbers
//! below are the contract between the two crates and are pinned by tests.
//!
//! # The channels
//!
//! - [`BungeeChannelSet::MAIN_CHANNEL_NAME`] (`bungeecord:main`) — the
//!   modern `namespace:key` name used since Minecraft 1.13. Paper remaps the
//!   legacy name onto it internally.
//! - [`BungeeChannelSet::LEGACY_CHANNEL_NAME`] (`BungeeCord`) — the
//!   pre-1.13 legacy single-part name.
//!
//! A proxy should register both names so old and new backends both work;
//! [`BungeeChannelSet::channels`] returns them as validated identifiers in
//! registration order (modern first).
//!
//! # The 18 compat subchannels
//!
//! [`BungeeChannelSet::COMPAT_SUBCHANNELS`] is exactly
//! [`SUBCHANNEL_NAMES`] —
//! the same 18 subchannels the BungeeCord protocol defines, enumerated in
//! the upstream implementations' dispatch switches (BungeeCord's
//! `DownstreamBridge`, Velocity's `BungeeCordMessageResponder`) and on the
//! SpigotMC wiki:
//!
//! | # | Subchannel | | # | Subchannel |
//! |---|------------|---|---|------------|
//! | 1 | `Connect` | | 10 | `MessageRaw` |
//! | 2 | `ConnectOther` | | 11 | `GetServer` |
//! | 3 | `IP` | | 12 | `Forward` |
//! | 4 | `IPOther` | | 13 | `ForwardToPlayer` |
//! | 5 | `PlayerCount` | | 14 | `UUID` |
//! | 6 | `PlayerList` | | 15 | `UUIDOther` |
//! | 7 | `GetPlayerServer` | | 16 | `ServerIP` |
//! | 8 | `GetServers` | | 17 | `KickPlayer` |
//! | 9 | `Message` | | 18 | `KickPlayerRaw` |
//!
//! The often-quoted "19 subchannels" is a miscount; the wire protocol has
//! exactly these 18, and this crate asserts that at compile time.

use candyriya_messaging::ChannelIdentifier;
pub use candyriya_messaging_bungeecord::{
    BUNGEECORD_CHANNEL, BUNGEECORD_CHANNEL_NAME, BUNGEECORD_LEGACY_CHANNEL,
    BUNGEECORD_LEGACY_CHANNEL_NAME, SUBCHANNEL_NAMES, is_bungeecord_channel,
};

/// The BungeeCord compatibility channel set and subchannel inventory.
///
/// All members are associated constants or free functions; the struct itself
/// is never instantiated.
pub struct BungeeChannelSet;

impl BungeeChannelSet {
    /// The modern channel name, `bungeecord:main`.
    pub const MAIN_CHANNEL_NAME: &str = BUNGEECORD_CHANNEL_NAME;

    /// The legacy pre-1.13 channel name, `BungeeCord`.
    pub const LEGACY_CHANNEL_NAME: &str = BUNGEECORD_LEGACY_CHANNEL_NAME;

    /// The 18 subchannels of the BungeeCord protocol, in a stable order.
    ///
    /// Identical to [`SUBCHANNEL_NAMES`]; restated here so the compat set is
    /// a single named thing. Pinned by a unit assertion in this module and by
    /// integration tests.
    pub const COMPAT_SUBCHANNELS: [&str; 18] = SUBCHANNEL_NAMES;

    /// How many compat subchannels exist: exactly 18.
    pub const COMPAT_SUBCHANNEL_COUNT: usize = Self::COMPAT_SUBCHANNELS.len();

    /// Both compat channels as validated identifiers, modern first.
    ///
    /// Register both with the backend and the client channel set so old and
    /// new peers both see a channel they can speak on.
    pub fn channels() -> [ChannelIdentifier; 2] {
        [
            BUNGEECORD_CHANNEL.clone(),
            BUNGEECORD_LEGACY_CHANNEL.clone(),
        ]
    }

    /// Whether `channel` is one of the compat channels
    /// (`bungeecord:main` or `BungeeCord`).
    pub fn is_compat_channel(channel: &ChannelIdentifier) -> bool {
        is_bungeecord_channel(channel)
    }
}

// The BungeeCord protocol defines exactly 18 subchannels. If the bungeecord
// codec crate ever grows or shrinks its inventory, this fails the build
// before any test does.
const _: () = assert!(
    BungeeChannelSet::COMPAT_SUBCHANNELS.len() == 18,
    "the BungeeCord compat set must enumerate exactly 18 subchannels",
);
