//! [`PlayerStore`]: the store contract, and [`InMemoryPlayerStore`].
//!
//! The contract is deliberately **synchronous** and deliberately small.
//! The in-memory implementation below is what this crate ships; the
//! file-backed, crash-safe implementation (atomic write + JSON, like the
//! `candyriya-player-store` crate next in the chain) is expected to implement
//! this same trait. Persistent backends will need I/O errors, so every
//! fallible method returns [`PlayerStoreError`] rather than `Option` alone;
//! the in-memory store never fails and simply never produces `Err`.
//!
//! Rationale for sync: the proxy reads and writes profile sightings at
//! login/logout frequency, never on the packet hot path. Async plumbing and
//! fsync policy belong to the persistence crate, not to the domain model.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::offline::OfflinePlayer;
use crate::profile::GameProfile;

/// Errors a [`PlayerStore`] backend can report.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PlayerStoreError {
    /// The backing store failed. The in-memory implementation never
    /// reports this; file-backed backends use it for I/O and (de)serialization
    /// failures, with a human-readable detail string.
    #[error("player store backend failed: {0}")]
    Backend(String),
}

/// The read/write contract for the offline player store.
///
/// Implementations must be deterministic and must not panic on bad input;
/// unknown UUIDs are `Ok(None)`, not an error.
pub trait PlayerStore {
    /// Load the record for `uuid`, or `None` if the player was never seen.
    fn load(&self, uuid: Uuid) -> Result<Option<OfflinePlayer>, PlayerStoreError>;

    /// Insert or replace the record for the player it describes.
    ///
    /// Replacement is wholesale: the stored record becomes exactly `player`
    /// (the caller decides what is fresh — see the upsert helper on
    /// [`InMemoryPlayerStore`] for the merge semantics the proxy uses).
    fn save(&mut self, player: OfflinePlayer) -> Result<(), PlayerStoreError>;
}

/// A BTreeMap-based, synchronous, in-memory [`PlayerStore`].
///
/// Intended for tests, for the proxy's hot-path sightings cache and as the
/// reference implementation of the trait contract. Records are kept in UUID
/// order, so iteration is deterministic. Contents are lost on restart; the
/// durable store lives in the persistence crate.
#[derive(Debug, Default, Clone)]
pub struct InMemoryPlayerStore {
    players: BTreeMap<Uuid, OfflinePlayer>,
}

impl InMemoryPlayerStore {
    /// Create an empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of stored records.
    pub fn len(&self) -> usize {
        self.players.len()
    }

    /// Whether the store holds no records.
    pub fn is_empty(&self) -> bool {
        self.players.is_empty()
    }

    /// Upsert the record for an online session: the proxy watched this
    /// profile at `now`.
    ///
    /// Merge semantics for an existing record:
    ///
    /// - `uuid` keys the record — a Mojang UUID is account-permanent.
    /// - `first_seen` is kept from the *existing* record (the first sighting
    ///   wins, forever).
    /// - `last_seen` becomes `now`; if the clock went backwards it is the
    ///   maximum of the stored value and `now`, so timestamps never regress.
    /// - `name` and `properties` are overwritten with the latest sighting —
    ///   Mojang account names change, so the newest value is the correct one.
    ///
    /// Returns the record as stored after the upsert.
    pub fn upsert_from_session(
        &mut self,
        profile: &GameProfile,
        now: DateTime<Utc>,
    ) -> OfflinePlayer {
        match self.players.get_mut(&profile.id) {
            Some(existing) => {
                existing.name = profile.name.clone();
                existing.properties = profile.properties.clone();
                existing.last_seen = existing.last_seen.max(now);
                existing.clone()
            }
            None => {
                let fresh = OfflinePlayer::observed(profile, now);
                self.players.insert(profile.id, fresh.clone());
                fresh
            }
        }
    }
}

impl PlayerStore for InMemoryPlayerStore {
    fn load(&self, uuid: Uuid) -> Result<Option<OfflinePlayer>, PlayerStoreError> {
        Ok(self.players.get(&uuid).cloned())
    }

    fn save(&mut self, player: OfflinePlayer) -> Result<(), PlayerStoreError> {
        self.players.insert(player.uuid, player);
        Ok(())
    }
}
