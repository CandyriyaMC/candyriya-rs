//! Wire connection state, mirroring the protocol states of the Java Edition
//! protocol. Configuration and Play are present for completeness of the API;
//! the registry does not classify their packet ids yet.

/// The state of a Minecraft protocol connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ConnectionState {
    /// Initial state, right after the transport is established. Serverbound
    /// only: the client sends the handshake (set_protocol) and switches to
    /// Status or Login.
    Handshaking,
    /// Status state (server list ping).
    Status,
    /// Login state (encryption, compression, plugin messages, disconnect).
    Login,
    /// Configuration state. Introduced in 1.20.2 (protocol 764); both peers
    /// enter it right after a successful login. Not classified yet.
    Configuration,
    /// Play state. Not classified yet; ids shift between releases and the
    /// proxy does not need them.
    Play,
}

impl std::fmt::Display for ConnectionState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self {
            Self::Handshaking => "handshaking",
            Self::Status => "status",
            Self::Login => "login",
            Self::Configuration => "configuration",
            Self::Play => "play",
        };
        f.write_str(name)
    }
}

/// The direction a packet travels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Direction {
    /// Client to server.
    Serverbound,
    /// Server to client.
    Clientbound,
}

impl std::fmt::Display for Direction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self {
            Self::Serverbound => "serverbound",
            Self::Clientbound => "clientbound",
        };
        f.write_str(name)
    }
}

/// The slot a packet id occupies inside one protocol version's tables.
///
/// Every slot the proxy can reliably track login and status traffic with is
/// listed in [`PacketSlot::ALL`]. The `as_str` values are the keys used in
/// `protocol_ids.json`; the override file uses the same keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PacketSlot {
    /// Serverbound status request (client asks for the server list ping
    /// payload). Id 0 in every supported release.
    StatusServerboundRequest,
    /// Serverbound status ping.
    StatusServerboundPing,
    /// Clientbound status response (server list ping payload). Id 0 in every
    /// supported release.
    StatusClientboundResponse,
    /// Clientbound status pong.
    StatusClientboundPong,
    /// Serverbound login start (client profile).
    LoginServerboundStart,
    /// Serverbound encryption response.
    LoginServerboundEncryptionResponse,
    /// Serverbound login plugin response. Introduced in 1.13 (protocol 393).
    LoginServerboundPluginResponse,
    /// Clientbound login disconnect.
    LoginClientboundDisconnect,
    /// Clientbound encryption request.
    LoginClientboundEncryptionRequest,
    /// Clientbound login success. In 1.20.2+ (protocol 764) both peers move
    /// to the configuration state right after this packet.
    LoginClientboundLoginSuccess,
    /// Clientbound set compression. Introduced in 1.8 (protocol 47).
    LoginClientboundSetCompression,
    /// Clientbound login plugin request. Introduced in 1.13 (protocol 393).
    LoginClientboundPluginRequest,
}

impl PacketSlot {
    /// All slots, in the canonical order used in `protocol_ids.json`.
    pub const ALL: &'static [Self] = &[
        Self::StatusServerboundRequest,
        Self::StatusServerboundPing,
        Self::StatusClientboundResponse,
        Self::StatusClientboundPong,
        Self::LoginServerboundStart,
        Self::LoginServerboundEncryptionResponse,
        Self::LoginServerboundPluginResponse,
        Self::LoginClientboundDisconnect,
        Self::LoginClientboundEncryptionRequest,
        Self::LoginClientboundLoginSuccess,
        Self::LoginClientboundSetCompression,
        Self::LoginClientboundPluginRequest,
    ];

    /// Slots that may legitimately hold `null` in `protocol_ids.json` for
    /// versions predating the feature. These must be listed in the file's
    /// `known_gaps`; every other slot must carry a numeric id.
    pub const NULLABLE: &'static [Self] = &[
        Self::LoginServerboundPluginResponse,
        Self::LoginClientboundSetCompression,
        Self::LoginClientboundPluginRequest,
    ];

    /// The key used in `protocol_ids.json`.
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::StatusServerboundRequest => "status.serverbound.request",
            Self::StatusServerboundPing => "status.serverbound.ping",
            Self::StatusClientboundResponse => "status.clientbound.response",
            Self::StatusClientboundPong => "status.clientbound.pong",
            Self::LoginServerboundStart => "login.serverbound.start",
            Self::LoginServerboundEncryptionResponse => "login.serverbound.encryption_response",
            Self::LoginServerboundPluginResponse => "login.serverbound.plugin_response",
            Self::LoginClientboundDisconnect => "login.clientbound.disconnect",
            Self::LoginClientboundEncryptionRequest => "login.clientbound.encryption_request",
            Self::LoginClientboundLoginSuccess => "login.clientbound.login_success",
            Self::LoginClientboundSetCompression => "login.clientbound.set_compression",
            Self::LoginClientboundPluginRequest => "login.clientbound.plugin_request",
        }
    }

    /// Parse the JSON slot key. Returns `None` for an unknown key.
    pub fn from_key(key: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|slot| slot.as_str() == key)
    }

    pub(crate) const fn index(&self) -> usize {
        *self as usize
    }
}

impl std::fmt::Display for PacketSlot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}
