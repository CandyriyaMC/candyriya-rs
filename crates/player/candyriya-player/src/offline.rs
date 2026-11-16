//! [`OfflinePlayer`]: the durable record of a profile seen before.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::profile::Property;

/// A historical record for a profile the proxy has seen at least once.
///
/// This is the unit of persistence: it survives restarts, whereas [`crate::Player`]
/// does not. `first_seen`/`last_seen` use chrono's default serde, which
/// writes RFC 3339 / ISO 8601 UTC timestamps (e.g. `2026-11-17T03:43:00Z`).
///
/// `properties` mirrors the Mojang profile attached to the account at the
/// time it was last seen; consumers must treat them as opaque and must not
/// cache Mojang session secrets — only these public profile properties are
/// ever stored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OfflinePlayer {
    /// The stable account UUID.
    pub uuid: Uuid,
    /// The name the account used when it was last seen.
    pub name: String,
    /// The account properties (textures, ...) as of the last sighting.
    #[serde(default)]
    pub properties: Vec<Property>,
    /// When the proxy first saw this player.
    pub first_seen: DateTime<Utc>,
    /// When the proxy most recently saw this player.
    pub last_seen: DateTime<Utc>,
}

impl OfflinePlayer {
    /// Observe a brand-new profile at `now`: both sightings are `now`.
    pub fn observed(profile: &crate::profile::GameProfile, now: DateTime<Utc>) -> Self {
        Self {
            uuid: profile.id,
            name: profile.name.clone(),
            properties: profile.properties.clone(),
            first_seen: now,
            last_seen: now,
        }
    }
}
