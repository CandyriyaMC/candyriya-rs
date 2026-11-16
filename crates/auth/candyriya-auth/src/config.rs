//! Authentication mode and configuration.

use std::time::Duration;

/// The default Mojang session server endpoint for the
/// [`crate::MojangSessionValidator`]: `GET {url}?username=..&serverId=..`.
///
/// See <https://minecraft.wiki/w/Java_Edition_protocol/Packets#Encryption_Request>
/// (the `hasJoined` call is the read-verify counterpart to `join`).
pub const DEFAULT_SESSION_SERVER_URL: &str = "https://sessionserver.mojang.com/session/hasJoined";

/// The default timeout applied to the session server request.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);

/// Which authentication mode the proxy runs in.
///
/// * [`AuthMode::Online`] — clients are expected to have completed their own
///   Microsoft/Mojang login before joining; the proxy verifies the session
///   against Mojang's session server via `hasJoined`. No user credentials
///   ever touch the proxy.
/// * [`AuthMode::Offline`] — no session verification; the UUID is derived
///   deterministically from the player name (see
///   [`candyriya_player::offline_uuid`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum AuthMode {
    /// No Mojang verification; offline UUID derivation.
    Offline,
    /// Server-side `hasJoined` verification against the session server.
    #[default]
    Online,
}

/// Configuration for the authentication flow.
///
/// This type is intentionally independent of where the values come from —
/// the `candyriya-config` crate is expected to populate it from
/// `candyriya.toml` when the proxy is wired up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthConfig {
    /// Online or offline authentication.
    pub mode: AuthMode,
    /// Endpoint used for the `hasJoined` verify call. Only consulted in
    /// [`AuthMode::Online`].
    pub session_server_url: String,
    /// How long to wait for the session server before rejecting the join.
    pub timeout: Duration,
}

impl Default for AuthConfig {
    /// Defaults to online mode against the real Mojang session server with a
    /// 5 second timeout.
    fn default() -> Self {
        Self {
            mode: AuthMode::Online,
            session_server_url: DEFAULT_SESSION_SERVER_URL.to_owned(),
            timeout: DEFAULT_TIMEOUT,
        }
    }
}
