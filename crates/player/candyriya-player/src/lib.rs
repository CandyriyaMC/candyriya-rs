//! # candyriya-player
//!
//! The domain model of identities for a Minecraft Java Edition proxy:
//!
//! - [`GameProfile`] / [`Property`] — the identity document Mojang issues for
//!   an account: stable UUID, current name and an opaque list of signed (or
//!   unsigned) properties such as textures.
//! - [`offline_uuid`] — the deterministic UUID a server in offline mode can
//!   derive from a player name, per the Minecraft protocol (UUID version 3,
//!   MD5 of `OfflinePlayer:<name>`, version nibble 3 and RFC 4122 variant
//!   bits).
//! - [`Player`] — a live, authenticated session: the profile, the remote
//!   address, the negotiated protocol version and the state of the session.
//! - [`OfflinePlayer`] — a historical record of a profile the proxy has seen
//!   before, with first/last seen timestamps (RFC 3339 via chrono's default
//!   serde).
//! - [`PlayerStore`] — the synchronous read/write contract for persisting
//!   those records, plus [`InMemoryPlayerStore`], a BTreeMap-based
//!   implementation. File-based and asynchronous implementations belong to
//!   the `candyriya-player-store` crate; see the trait docs for the intended
//!   split.
//!
//! This crate performs no network I/O and contains no unsafe code.
#![forbid(unsafe_code)]

mod offline;
mod offline_uuid;
mod profile;
mod session;
mod store;

pub use crate::offline::OfflinePlayer;
pub use crate::offline_uuid::offline_uuid;
pub use crate::profile::{GameProfile, Property};
pub use crate::session::{ConnectionState, Player, UNKNOWN_PROTOCOL_NAME};
pub use crate::store::{InMemoryPlayerStore, PlayerStore, PlayerStoreError};
