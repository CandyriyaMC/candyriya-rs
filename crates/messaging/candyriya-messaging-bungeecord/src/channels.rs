//! The plugin-message channels BungeeCord (and BungeeCord-compatible
//! proxies) listen on, and the subchannel names that cross the wire.
//!
//! ## The 1.13 rename
//!
//! Before Minecraft 1.13 the BungeeCord channel was the legacy single-part
//! name `BungeeCord`. Since 1.13 it is the modern `namespace:key` identifier
//! `bungeecord:main`. Paper remaps `BungeeCord` to `bungeecord:main`
//! internally, so backend plugins may keep using the legacy name; a proxy
//! should register and accept either.
//!
//! Both names are exported as validated [`ChannelIdentifier`]s
//! ([`BUNGEECORD_CHANNEL`] and [`BUNGEECORD_LEGACY_CHANNEL`]) so callers
//! never validate them ad hoc. [`is_bungeecord_channel`] accepts either.

use std::sync::LazyLock;

use candyriya_messaging::ChannelIdentifier;

/// The channel name used before Minecraft 1.13.
pub const BUNGEECORD_LEGACY_CHANNEL_NAME: &str = "BungeeCord";

/// The channel name used since Minecraft 1.13.
pub const BUNGEECORD_CHANNEL_NAME: &str = "bungeecord:main";

/// [`BUNGEECORD_CHANNEL_NAME`] as a validated [`ChannelIdentifier`].
///
/// The validation cannot fail (the name is a hardcoded constant and the
/// channel rules are checked in a unit test), so the first access either
/// always succeeds or the test suite is red.
pub static BUNGEECORD_CHANNEL: LazyLock<ChannelIdentifier> = LazyLock::new(|| {
    ChannelIdentifier::new(BUNGEECORD_CHANNEL_NAME)
        .expect("hardcoded BUNGEECORD_CHANNEL_NAME must be a valid channel identifier")
});

/// [`BUNGEECORD_LEGACY_CHANNEL_NAME`] as a validated [`ChannelIdentifier`].
///
/// The validation cannot fail (the name is a hardcoded constant and the
/// channel rules are checked in a unit test), so the first access either
/// always succeeds or the test suite is red.
pub static BUNGEECORD_LEGACY_CHANNEL: LazyLock<ChannelIdentifier> = LazyLock::new(|| {
    ChannelIdentifier::new(BUNGEECORD_LEGACY_CHANNEL_NAME)
        .expect("hardcoded BUNGEECORD_LEGACY_CHANNEL_NAME must be a valid channel identifier")
});

/// Whether `channel` is one of the default BungeeCord channels
/// (`bungeecord:main` or `BungeeCord`).
///
/// Backends and proxies that want to speak the BungeeCord protocol accept
/// either name; this predicate is the one place that knows both.
pub fn is_bungeecord_channel(channel: &ChannelIdentifier) -> bool {
    channel.as_str() == BUNGEECORD_CHANNEL_NAME
        || channel.as_str() == BUNGEECORD_LEGACY_CHANNEL_NAME
}

/// The subchannel read off the wire for the `Connect` message.
pub const SUBCHANNEL_CONNECT: &str = "Connect";
/// The subchannel read off the wire for the `ConnectOther` message.
pub const SUBCHANNEL_CONNECT_OTHER: &str = "ConnectOther";
/// The subchannel read off the wire for the `IP` message.
pub const SUBCHANNEL_IP: &str = "IP";
/// The subchannel read off the wire for the `IPOther` message.
pub const SUBCHANNEL_IP_OTHER: &str = "IPOther";
/// The subchannel read off the wire for the `PlayerCount` message.
pub const SUBCHANNEL_PLAYER_COUNT: &str = "PlayerCount";
/// The subchannel read off the wire for the `PlayerList` message.
pub const SUBCHANNEL_PLAYER_LIST: &str = "PlayerList";
/// The subchannel read off the wire for the `GetPlayerServer` message.
pub const SUBCHANNEL_GET_PLAYER_SERVER: &str = "GetPlayerServer";
/// The subchannel read off the wire for the `GetServers` message.
pub const SUBCHANNEL_GET_SERVERS: &str = "GetServers";
/// The subchannel read off the wire for the `Message` message.
pub const SUBCHANNEL_MESSAGE: &str = "Message";
/// The subchannel read off the wire for the `MessageRaw` message.
pub const SUBCHANNEL_MESSAGE_RAW: &str = "MessageRaw";
/// The subchannel read off the wire for the `GetServer` message.
pub const SUBCHANNEL_GET_SERVER: &str = "GetServer";
/// The subchannel read off the wire for the `Forward` message.
pub const SUBCHANNEL_FORWARD: &str = "Forward";
/// The subchannel read off the wire for the `ForwardToPlayer` message.
pub const SUBCHANNEL_FORWARD_TO_PLAYER: &str = "ForwardToPlayer";
/// The subchannel read off the wire for the `UUID` message.
pub const SUBCHANNEL_UUID: &str = "UUID";
/// The subchannel read off the wire for the `UUIDOther` message.
pub const SUBCHANNEL_UUID_OTHER: &str = "UUIDOther";
/// The subchannel read off the wire for the `ServerIP` message.
pub const SUBCHANNEL_SERVER_IP: &str = "ServerIP";
/// The subchannel read off the wire for the `KickPlayer` message.
pub const SUBCHANNEL_KICK_PLAYER: &str = "KickPlayer";
/// The subchannel read off the wire for the `KickPlayerRaw` message.
pub const SUBCHANNEL_KICK_PLAYER_RAW: &str = "KickPlayerRaw";

/// Every subchannel the BungeeCord protocol defines, in a stable order.
///
/// This list is the exhaustive inventory of what a conforming proxy must
/// understand: the SpigotMC wiki documents exactly these subchannels, and
/// the upstream implementations switch on exactly these (BungeeCord's
/// `DownstreamBridge` and Velocity's `BungeeCordMessageResponder` both list
/// the same set). There is no 19th subchannel on the wire; the well-known
/// number 19 attached to this protocol is a miscount that propagated into
/// old documentation.
///
/// [`BungeeMessage`](crate::BungeeMessage) has exactly one variant per entry.
pub const SUBCHANNEL_NAMES: [&str; 18] = [
    SUBCHANNEL_CONNECT,
    SUBCHANNEL_CONNECT_OTHER,
    SUBCHANNEL_IP,
    SUBCHANNEL_IP_OTHER,
    SUBCHANNEL_PLAYER_COUNT,
    SUBCHANNEL_PLAYER_LIST,
    SUBCHANNEL_GET_PLAYER_SERVER,
    SUBCHANNEL_GET_SERVERS,
    SUBCHANNEL_MESSAGE,
    SUBCHANNEL_MESSAGE_RAW,
    SUBCHANNEL_GET_SERVER,
    SUBCHANNEL_FORWARD,
    SUBCHANNEL_FORWARD_TO_PLAYER,
    SUBCHANNEL_UUID,
    SUBCHANNEL_UUID_OTHER,
    SUBCHANNEL_SERVER_IP,
    SUBCHANNEL_KICK_PLAYER,
    SUBCHANNEL_KICK_PLAYER_RAW,
];
