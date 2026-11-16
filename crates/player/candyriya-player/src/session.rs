//! [`Player`]: a live, authenticated session.

use std::fmt;
use std::net::SocketAddr;

use candyriya_protocol_registry::Registry;

use crate::profile::GameProfile;

/// The protocol states a session can be in after the handshake and the
/// login decision have happened.
///
/// This is intentionally a subset of the wire connection states: `Player`
/// models an *authenticated* session, so the pre-login states
/// (handshaking, status) are out of scope. A session enters in
/// [`ConnectionState::Login`], moves to [`ConnectionState::Configuration`]
/// before 1.21-era play, and finally settles in [`ConnectionState::Play`].
///
/// The `candyriya-protocol-registry` crate also ships a
/// `ConnectionState` covering all wire states; it is not reused here
/// because this enum must stay a faithful "where is this session" signal,
/// not a protocol parse state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ConnectionState {
    /// Login: from login start to login success (or disconnect).
    Login,
    /// Configuration: entered after login success on 1.20.2+
    /// (protocol 764). The oldest supported protocols skip it entirely.
    Configuration,
    /// Play: the terminal state; the session is in the game.
    Play,
}

impl fmt::Display for ConnectionState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Self::Login => "login",
            Self::Configuration => "configuration",
            Self::Play => "play",
        };
        f.write_str(name)
    }
}

/// Placeholder for `protocol_name` when the negotiated protocol version is
/// not present in the embedded registry snapshot (e.g. a very new client
/// the snapshot predates).
pub const UNKNOWN_PROTOCOL_NAME: &str = "unknown";

/// A live, authenticated session: which account it is, where it came from,
/// which protocol it negotiated and where it is in the session lifecycle.
///
/// `Player` is deliberately not serializable — it holds a socket address
/// and is a snapshot of a live connection, not a transferable document.
/// Persisting the durable part (a UUID, name and last-seen timestamp) is
/// what [`crate::OfflinePlayer`] and [`crate::PlayerStore`] are for.
#[derive(Debug, Clone)]
pub struct Player {
    /// The authenticated profile.
    pub profile: GameProfile,
    /// The address the client connected from (ip and port).
    pub client_address: SocketAddr,
    /// The protocol version number the client negotiated.
    pub protocol_version: i32,
    /// The release name for `protocol_version` as known to
    /// `candyriya-protocol-registry` (e.g. `"1.12.2"`).
    ///
    /// Resolved best-effort: unknown versions yield [`UNKNOWN_PROTOCOL_NAME`]
    /// rather than failing session construction. Use
    /// [`Player::with_protocol_name`] when the caller already holds a
    /// registry and wants a guaranteed mapping.
    pub protocol_name: &'static str,
    /// Where the session is in the login/configuration/play lifecycle.
    pub connection_state: ConnectionState,
    /// The client's locale (`en_us`, ...), if it sent one and the proxy
    /// tracked it. Absent until known, never defaulted by this crate.
    pub locale: Option<String>,
}

impl Player {
    /// Create a session in [`ConnectionState::Login`], resolving
    /// `protocol_name` from the embedded protocol registry snapshot.
    ///
    /// Loads the embedded registry once per call; it is pure and cheap,
    /// and sessions are created once per login, not per packet. If the
    /// snapshot cannot be loaded (only possible with a corrupt binary
    /// build) or the version is unknown, `protocol_name` is
    /// [`UNKNOWN_PROTOCOL_NAME`] and no error is returned — an unknown
    /// client is still a perfectly valid session.
    pub fn new(profile: GameProfile, client_address: SocketAddr, protocol_version: i32) -> Self {
        let protocol_name = match Registry::embedded() {
            Ok(registry) => registry
                .from_protocol(protocol_version)
                .map(|info| info.name)
                .unwrap_or(UNKNOWN_PROTOCOL_NAME),
            Err(_) => UNKNOWN_PROTOCOL_NAME,
        };
        Self::with_protocol_name(profile, client_address, protocol_version, protocol_name)
    }

    /// Create a session with an explicitly supplied release name.
    ///
    /// Prefer this when the caller already holds a registry (the proxy
    /// does); it avoids reparsing the embedded snapshot and lets the caller
    /// decide what "unknown" means for their tables.
    pub fn with_protocol_name(
        profile: GameProfile,
        client_address: SocketAddr,
        protocol_version: i32,
        protocol_name: &'static str,
    ) -> Self {
        Self {
            profile,
            client_address,
            protocol_version,
            protocol_name,
            connection_state: ConnectionState::Login,
            locale: None,
        }
    }
}
