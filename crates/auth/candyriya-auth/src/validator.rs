//! The session validators used by the proxy's login flow.
//!
//! [`SessionValidator`] is the single contract the proxy relies on during
//! login. Two implementations ship with this crate:
//!
//! * [`OfflineSessionValidator`] — no network at all; the profile UUID is
//!   derived from the name exactly like a vanilla offline-mode server does.
//! * [`MojangSessionValidator`] — verifies the join against Mojang's session
//!   server with the read-only `hasJoined` call. This is the server-side
//!   check; the client has already done its own Microsoft login and the
//!   proxy never sees any user credentials.
//!
//! Both validators enforce [`crate::validate_username`] before doing any
//! work, so a malformed name never reaches the network.

use std::time::Duration;

use candyriya_player::{GameProfile, Property, offline_uuid};
use reqwest::StatusCode;
use serde::Deserialize;
use uuid::Uuid;

use crate::{AuthError, username::validate_username};

/// Verify that `username` is allowed to join this proxy right now.
///
/// `server_id` is the non-standard hexdigest produced by the encryption
/// handshake (see the `candyriya-encryption` crate, `server_id_hash`).
/// Implementations in online mode send it to the session server; offline
/// mode ignores it.
///
/// The trait uses a native `async fn` (stable since Rust 1.75), which keeps
/// this crate free of async-trait-style allocation of boxed futures. As a
/// consequence the trait is not object-safe; consumers should be generic
/// over `V: SessionValidator` rather than store `dyn SessionValidator`.
pub trait SessionValidator {
    /// Check `username`/`server_id` and return the authenticated profile on
    /// success. The returned [`GameProfile`] always carries the account's
    /// current Mojang name and whatever properties the account has.
    #[allow(async_fn_in_trait)] // by design: stable native async, no boxed futures, no new deps
    async fn verify_client(
        &self,
        username: &str,
        server_id: &str,
    ) -> Result<GameProfile, AuthError>;
}

/// Offline-mode validator: no network, no Mojang involvement.
///
/// Derives the profile UUID with [`offline_uuid`] (UUID v3, MD5 of
/// `OfflinePlayer:<name>`), keeps the name verbatim (case included) and
/// attaches no properties. The `server_id` argument is ignored.
#[derive(Debug, Clone, Copy, Default)]
pub struct OfflineSessionValidator;

impl SessionValidator for OfflineSessionValidator {
    async fn verify_client(
        &self,
        username: &str,
        _server_id: &str,
    ) -> Result<GameProfile, AuthError> {
        validate_username(username)?;
        Ok(GameProfile::new(offline_uuid(username), username))
    }
}

/// Online-mode validator against Mojang's session server.
///
/// Performs `GET {session_server_url}?username={username}&serverId={server_id}`
/// and interprets the result as follows:
///
/// * `200 OK` with a JSON body `{"id", "name", "properties": [...]}` — the
///   join is verified; `id` must be a parseable UUID and the properties are
///   passed through verbatim (textures and their signatures included,
///   unsigned properties kept as-is).
/// * `204 No Content`, any other non-200 status, a body that is not the
///   expected JSON, or a request timeout — [`AuthError::SessionRejected`].
/// * Failure to carry the request out at all (DNS, connect, TLS) —
///   [`AuthError::Transport`].
///
/// Cheap to clone; the underlying [`reqwest::Client`] pools connections.
#[derive(Debug, Clone)]
pub struct MojangSessionValidator {
    /// The HTTP client used for the `hasJoined` call.
    pub client: reqwest::Client,
    /// The `hasJoined` endpoint, e.g. [`crate::DEFAULT_SESSION_SERVER_URL`].
    pub session_server_url: String,
    /// How long the `hasJoined` request may take before it is rejected.
    pub timeout: Duration,
}

impl MojangSessionValidator {
    /// Build a validator with a fresh client configured with `timeout`.
    ///
    /// `session_server_url` is used verbatim, without any query string;
    /// `username` and `serverId` are appended as URL-encoded query
    /// parameters.
    pub fn new(
        session_server_url: impl Into<String>,
        timeout: Duration,
    ) -> Result<Self, AuthError> {
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .map_err(|e| AuthError::Transport(format!("failed to build HTTP client: {e}")))?;
        Ok(Self {
            client,
            session_server_url: session_server_url.into(),
            timeout,
        })
    }

    /// Build a validator around an existing client.
    ///
    /// `timeout` is recorded for introspection via [`Self::timeout`]; the
    /// caller's client is responsible for its own timeouts.
    #[must_use]
    pub fn with_client(
        client: reqwest::Client,
        session_server_url: impl Into<String>,
        timeout: Duration,
    ) -> Self {
        Self {
            client,
            session_server_url: session_server_url.into(),
            timeout,
        }
    }
}

impl Default for MojangSessionValidator {
    /// Targets the real Mojang session server with a 5 second timeout.
    fn default() -> Self {
        Self::new(
            crate::DEFAULT_SESSION_SERVER_URL,
            crate::config::DEFAULT_TIMEOUT,
        )
        .expect("reqwest client with default configuration must build")
    }
}

impl SessionValidator for MojangSessionValidator {
    async fn verify_client(
        &self,
        username: &str,
        server_id: &str,
    ) -> Result<GameProfile, AuthError> {
        validate_username(username)?;

        let response = self
            .client
            .get(&self.session_server_url)
            .query(&[("username", username), ("serverId", server_id)])
            .send()
            .await
            .map_err(map_request_error)?;

        let status = response.status();
        if status != StatusCode::OK {
            // 204 is the documented "no session" answer; everything else
            // non-200 means the same thing for login purposes: reject.
            return Err(AuthError::SessionRejected(match status.as_u16() {
                204 => "the session server holds no join session for this username (HTTP 204)"
                    .to_owned(),
                other => format!("the session server returned HTTP {other}"),
            }));
        }

        let body = response.text().await.map_err(map_request_error)?;
        let wire: SessionProfileResponse = serde_json::from_str(&body).map_err(|e| {
            AuthError::SessionRejected(format!(
                "the session server returned an undecodable body: {e}"
            ))
        })?;

        let id = Uuid::parse_str(wire.id.trim()).map_err(|e| {
            AuthError::Parse(format!("profile id {:?} is not a valid UUID: {e}", wire.id))
        })?;

        Ok(GameProfile {
            id,
            name: wire.name,
            properties: wire.properties,
        })
    }
}

/// Map a reqwest error onto [`AuthError`]. Timeouts are reported as
/// [`AuthError::SessionRejected`] (a slow session server is indistinguishable
/// from an absent session for the caller); everything else is transport.
fn map_request_error(error: reqwest::Error) -> AuthError {
    if error.is_timeout() {
        AuthError::SessionRejected("the session server request timed out".to_owned())
    } else {
        AuthError::Transport(error.to_string())
    }
}

/// The wire shape of Mojang's `hasJoined` 200 response.
///
/// `id` is a lowercase hex UUID, usually *without* dashes; `properties`
/// entries may omit `signature`, which is preserved by
/// [`Property::signature`]'s `Option`.
#[derive(Debug, Deserialize)]
struct SessionProfileResponse {
    id: String,
    name: String,
    #[serde(default)]
    properties: Vec<Property>,
}
