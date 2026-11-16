//! The BungeeCord subchannel message enum.
//!
//! One variant per subchannel, with the arguments carried by the wire as
//! plain fields. The variants model the *request* direction (a backend
//! server asking the proxy, or telling the proxy to do something). Replies
//! are produced by the free functions in [`crate::response`], not by this
//! enum.
//!
//! [`BungeeMessage::subchannel`] returns the wire name of the variant, and
//! [`crate::channels::SUBCHANNEL_NAMES`] enumerates every name.

use crate::channels::{
    SUBCHANNEL_CONNECT, SUBCHANNEL_CONNECT_OTHER, SUBCHANNEL_FORWARD, SUBCHANNEL_FORWARD_TO_PLAYER,
    SUBCHANNEL_GET_PLAYER_SERVER, SUBCHANNEL_GET_SERVER, SUBCHANNEL_GET_SERVERS, SUBCHANNEL_IP,
    SUBCHANNEL_IP_OTHER, SUBCHANNEL_KICK_PLAYER, SUBCHANNEL_KICK_PLAYER_RAW, SUBCHANNEL_MESSAGE,
    SUBCHANNEL_MESSAGE_RAW, SUBCHANNEL_PLAYER_COUNT, SUBCHANNEL_PLAYER_LIST, SUBCHANNEL_SERVER_IP,
    SUBCHANNEL_UUID, SUBCHANNEL_UUID_OTHER,
};

/// A decoded BungeeCord subchannel message.
///
/// The `cat` of every variant is `arg` in the crate-level reference table
/// (these are the arguments sent to the proxy). Wire order of the fields is
/// the declaration order; [`BungeeCodec::encode`](crate::BungeeCodec::encode)
/// writes them in that order and [`BungeeCodec::parse`](crate::BungeeCodec::parse)
/// reads them in it.
///
/// # Notes on specific fields
///
/// - `PlayerCount::server` accepts the sentinel `"ALL"` (global player
///   count) in addition to a server name.
/// - `PlayerList::server` likewise accepts `"ALL"`.
/// - `Forward::target` accepts a server name, `"ALL"` (every server except
///   the sender) or `"ONLINE"` (every server with at least one player,
///   except the sender).
/// - `Message::target` and `MessageRaw::target` accept `"ALL"` to broadcast.
/// - The `Forward::channel` and `ForwardToPlayer::channel` are the *nested*
///   plugin-message channel name to route the payload on, **not** the
///   BungeeCord subchannel; `message` is the nested payload, carried as raw
///   bytes between the BungeeCord length prefix and the end of the message.
///
/// The enum is exhaustive on purpose: the wire protocol defines exactly
/// these variants, so adding one is a breaking change by design.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BungeeMessage {
    /// `Connect` — ask the proxy to move the sending player to `server`.
    Connect {
        /// Name of the target server, as defined in the proxy's config.
        server: String,
    },
    /// `ConnectOther` — ask the proxy to move `target` to `server`.
    ConnectOther {
        /// Username of the player to move.
        target: String,
        /// Name of the target server, as defined in the proxy's config.
        server: String,
    },
    /// `IP` — ask the proxy to report the real address of the sender.
    IP,
    /// `IPOther` — ask the proxy to report the real address of `target`.
    IPOther {
        /// Username of the player whose address is requested.
        target: String,
    },
    /// `PlayerCount` — ask the proxy to report how many players are on
    /// `server` (or on all servers).
    PlayerCount {
        /// Server name, or `"ALL"` for the global count.
        server: String,
    },
    /// `PlayerList` — ask the proxy to report which players are on `server`
    /// (or on all servers).
    PlayerList {
        /// Server name, or `"ALL"` for the global list.
        server: String,
    },
    /// `GetPlayerServer` — ask the proxy to report which server `target` is
    /// on.
    GetPlayerServer {
        /// Username of the player to resolve.
        target: String,
    },
    /// `GetServers` — ask the proxy to report the names of all servers it
    /// proxies.
    GetServers,
    /// `Message` — ask the proxy to relay a plain chat message to `target`
    /// (a username, or `"ALL"` to broadcast).
    Message {
        /// Recipient username, or `"ALL"` to broadcast.
        target: String,
        /// The chat message text (legacy color codes, not JSON).
        message: String,
    },
    /// `MessageRaw` — like [`Message`](Self::Message), but `message` is a raw
    /// JSON chat component allowing click/hover events.
    MessageRaw {
        /// Recipient username, or `"ALL"` to broadcast.
        target: String,
        /// The chat component as JSON text.
        message: String,
    },
    /// `GetServer` — ask the proxy to report the name of the server this
    /// message was sent from.
    GetServer,
    /// `Forward` — ask the proxy to route `message` to `target` (a server
    /// name, `"ALL"` or `"ONLINE"`) on the nested plugin-message `channel`.
    Forward {
        /// Destination server name, or `"ALL"` / `"ONLINE"`.
        target: String,
        /// The nested plugin-message channel to route the payload on.
        channel: String,
        /// The nested payload bytes.
        message: Vec<u8>,
    },
    /// `ForwardToPlayer` — ask the proxy to route `message` to player
    /// `target` on the nested plugin-message `channel`.
    ForwardToPlayer {
        /// Username of the player to send the nested message to.
        target: String,
        /// The nested plugin-message channel to route the payload on.
        channel: String,
        /// The nested payload bytes.
        message: Vec<u8>,
    },
    /// `UUID` — ask the proxy to report the UUID of the sender.
    UUID,
    /// `UUIDOther` — ask the proxy to report the UUID of `target`.
    UUIDOther {
        /// Username of the player whose UUID is requested.
        target: String,
    },
    /// `ServerIP` — ask the proxy to report the address of `server`.
    ServerIP {
        /// Name of the server whose address is requested.
        server: String,
    },
    /// `KickPlayer` — ask the proxy to kick `target` with `reason`
    /// (plain text).
    KickPlayer {
        /// Username of the player to kick.
        target: String,
        /// The kick reason (plain text).
        reason: String,
    },
    /// `KickPlayerRaw` — like [`KickPlayer`](Self::KickPlayer), but `reason`
    /// is a raw JSON chat component.
    KickPlayerRaw {
        /// Username of the player to kick.
        target: String,
        /// The kick reason as JSON chat component text.
        reason: String,
    },
}

impl BungeeMessage {
    /// The wire name of the BungeeCord subchannel this variant decodes from
    /// (and encodes to).
    pub fn subchannel(&self) -> &'static str {
        match self {
            Self::Connect { .. } => SUBCHANNEL_CONNECT,
            Self::ConnectOther { .. } => SUBCHANNEL_CONNECT_OTHER,
            Self::IP => SUBCHANNEL_IP,
            Self::IPOther { .. } => SUBCHANNEL_IP_OTHER,
            Self::PlayerCount { .. } => SUBCHANNEL_PLAYER_COUNT,
            Self::PlayerList { .. } => SUBCHANNEL_PLAYER_LIST,
            Self::GetPlayerServer { .. } => SUBCHANNEL_GET_PLAYER_SERVER,
            Self::GetServers => SUBCHANNEL_GET_SERVERS,
            Self::Message { .. } => SUBCHANNEL_MESSAGE,
            Self::MessageRaw { .. } => SUBCHANNEL_MESSAGE_RAW,
            Self::GetServer => SUBCHANNEL_GET_SERVER,
            Self::Forward { .. } => SUBCHANNEL_FORWARD,
            Self::ForwardToPlayer { .. } => SUBCHANNEL_FORWARD_TO_PLAYER,
            Self::UUID => SUBCHANNEL_UUID,
            Self::UUIDOther { .. } => SUBCHANNEL_UUID_OTHER,
            Self::ServerIP { .. } => SUBCHANNEL_SERVER_IP,
            Self::KickPlayer { .. } => SUBCHANNEL_KICK_PLAYER,
            Self::KickPlayerRaw { .. } => SUBCHANNEL_KICK_PLAYER_RAW,
        }
    }
}
