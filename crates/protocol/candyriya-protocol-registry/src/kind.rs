//! Packet classification result.

/// What a packet is, as far as the proxy cares.
///
/// Only the login and status states are covered; everything the registry does
/// not track - the play state, the configuration state, and login/handshake
/// packets it has no slot for - comes back as [`PacketKind::Unknown`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PacketKind {
    /// Handshake-state serverbound packet 0x00 (`set_protocol`). It is not in
    /// any table; the id has been constant across every Java Edition release.
    Handshake,
    /// Serverbound status request (`ping_start` in the PrismarineJS data).
    StatusRequest,
    /// Serverbound status ping.
    StatusPing,
    /// Clientbound status response (`server_info` in the PrismarineJS data).
    StatusResponse,
    /// Clientbound status pong.
    StatusPong,
    /// Serverbound login start; carries the client profile.
    LoginStart,
    /// Serverbound encryption response.
    EncryptionResponse,
    /// Serverbound login plugin response. Introduced in 1.13 (protocol 393).
    LoginPluginResponse,
    /// Clientbound set compression. Introduced in 1.8 (protocol 47).
    SetCompression,
    /// Clientbound encryption request.
    EncryptionRequest,
    /// Clientbound login success.
    LoginSuccess,
    /// Clientbound login disconnect.
    ///
    /// The login state has no serverbound disconnect packet in any release
    /// from 1.7.2 (protocol 4) through 26.3 (protocol 777); the data-driven
    /// tables would have surfaced one as a slot otherwise. The variant is
    /// returned for the clientbound 0x00 packet.
    LoginDisconnect,
    /// Clientbound login plugin request. Introduced in 1.13 (protocol 393).
    LoginPluginRequest,
    /// Any other packet id, or a packet in a class the registry does not
    /// classify (play and configuration states, non-zero handshake ids).
    Unknown(i32),
}

impl std::fmt::Display for PacketKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self {
            Self::Handshake => "handshake",
            Self::StatusRequest => "status_request",
            Self::StatusPing => "status_ping",
            Self::StatusResponse => "status_response",
            Self::StatusPong => "status_pong",
            Self::LoginStart => "login_start",
            Self::EncryptionResponse => "encryption_response",
            Self::LoginPluginResponse => "login_plugin_response",
            Self::SetCompression => "set_compression",
            Self::EncryptionRequest => "encryption_request",
            Self::LoginSuccess => "login_success",
            Self::LoginDisconnect => "login_disconnect",
            Self::LoginPluginRequest => "login_plugin_request",
            Self::Unknown(id) => return write!(f, "unknown({id})"),
        };
        f.write_str(name)
    }
}
