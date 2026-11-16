//! The legacy (BungeeCord) forwarding handshake address.
//!
//! In legacy mode the proxy does not send a plugin message at all; it
//! smuggles the player identity into the handshake's `server_address`
//! field, separated by NUL bytes. The backend server splits the address on
//! `\0` and reads the parts positionally:
//!
//! ```text
//! server_address \0 player_address \0 undashed_uuid \0 properties_json
//! ```
//!
//! * `server_address` — the backend's address as configured; passed through
//!   opaquely so the backend can route the connection onwards.
//! * `player_address` — the client's remote IP *without* a port. IPv6 is
//!   forwarded as-is with colons; strip any `%scope` suffix beforehand with
//!   [`crate::clean_ipv6_scope`] (the reference implementations do the same).
//! * `undashed_uuid` — the player's UUID as 32 lowercase hex characters, no
//!   dashes (Java's `UUID.toString().replace("-", "")`).
//! * `properties_json` — the player's profile properties as a JSON array of
//!   `{"name": "...", "value": "...", "signature": "..."?}` objects. The
//!   `signature` key is omitted for unsigned properties (the Java Gson
//!   encoder skips null fields), and property key order is irrelevant to
//!   the receiving side, which reads by key.
//!
//! # BungeeGuard
//!
//! Plain legacy forwarding authenticates nothing: anyone who can reach the
//! backend directly can claim any identity. BungeeGuard closes that with a
//! shared token: when set, it is appended as the *last* property of the
//! array, named `bungeeguard-token`, with the token bytes interpreted as a
//! UTF-8 string. The backend rejects handshakes without an allowed token.
//! The token property is unsigned by design.
//!
//! Verified against the reference implementations: BungeeCord's
//! `ServerConnector#connected` (address layout), `InitialHandler#getUUID`
//! (undashed UUID) and BungeeGuard's `SpoofedLoginResult` /
//! `BungeeCordHandshake` (token property injection and backend-side
//! check).

use std::fmt::Write as _;

use candyriya_player::{GameProfile, Property};
use uuid::Uuid;

use crate::error::ForwardingError;

/// The property name BungeeGuard uses to carry its token.
pub const BUNGEEGUARD_TOKEN_NAME: &str = "bungeeguard-token";

/// Build the legacy forwarding handshake address.
///
/// See the module-level docs for the format. `bungeeguard_token`, when
/// set, is appended as the final property, after any profile properties.
///
/// The token is interpreted as UTF-8 because that is what the receiving
/// side compares against. Tokens produced by a UTF-8 secret (for example a
/// base64 secret file, which is pure ASCII) round-trip exactly; bytes that
/// are not UTF-8 are replaced character-for-character with U+FFFD and will
/// not match any sensible backend configuration.
pub fn create_legacy_forwarding_address(
    server_address: &str,
    player_address: &str,
    profile: &GameProfile,
    bungeeguard_token: Option<&[u8]>,
) -> String {
    let mut properties = profile.properties.clone();
    if let Some(token) = bungeeguard_token {
        properties.push(Property::unsigned(
            BUNGEEGUARD_TOKEN_NAME,
            String::from_utf8_lossy(token),
        ));
    }

    // serde_json cannot fail here: serializing a Vec<Property> into a
    // String involves no IO and no non-string map keys.
    let json = serde_json::to_string(&properties).expect("property serialization is infallible");

    let mut out =
        String::with_capacity(server_address.len() + player_address.len() + 35 + json.len() + 3);
    write!(
        out,
        "{server_address}\0{player_address}\0{}\0{json}",
        profile.id.simple()
    )
    .expect("writing to a String cannot fail");
    out
}

/// Parse a legacy forwarding handshake address.
///
/// Inverse of [`create_legacy_forwarding_address`], kept so the proxy and
/// tests can decode what a peer (or a misbehaving backend) sent. Accepts
/// both the 3-part form (no properties JSON, e.g. what BungeeCord emits
/// when the profile has no properties) and the 4-part form produced by
/// [`create_legacy_forwarding_address`].
///
/// # Errors
///
/// Returns [`ForwardingError::MalformedLegacyAddress`] for anything that is
/// not exactly 3 or 4 `\0`-separated parts, [`ForwardingError::InvalidUuid`]
/// for an unreadable UUID, or [`ForwardingError::InvalidLegacyProperties`]
/// for properties JSON that is not a list of property objects.
pub fn parse_legacy_forwarding_address(
    address: &str,
) -> Result<(String, String, Uuid, Vec<Property>), ForwardingError> {
    let parts: Vec<&str> = address.split('\0').collect();
    if parts.len() != 3 && parts.len() != 4 {
        return Err(ForwardingError::MalformedLegacyAddress(
            "expected 3 or 4 NUL-separated parts (host, client ip, uuid[, properties])",
        ));
    }

    let server_address = parts[0].to_owned();
    let player_address = parts[1].to_owned();
    let uuid = Uuid::parse_str(parts[2])?;

    let properties = match parts.get(3) {
        Some(json) => serde_json::from_str(json)?,
        None => Vec::new(),
    };

    Ok((server_address, player_address, uuid, properties))
}
