//! # candyriya-player-store
//!
//! File-backed persistence for [`OfflinePlayer`](candyriya_player::OfflinePlayer)
//! records: the proxy's first durable store.
//!
//! [`JsonPlayerStore`] keeps every record in a single JSON file — by
//! convention `<data_dir>/players.json` — and rewrites the file atomically
//! on every change (temporary file + `sync_all` + rename), so a crash can
//! lose the last save but never tears `players.json` itself. Records are
//! the exact `OfflinePlayer` values `candyriya-player` serializes, written
//! as a pretty-printed (2-space indent) JSON array in UUID order: humans
//! can read and edit the file, and the format round-trips properties with
//! or without Mojang signatures.
//!
//! The file is trusted input-adjacent: opening an existing store
//! validates size (hard cap, see [`MAX_FILE_SIZE_BYTES`]), that every
//! record deserializes, and that UUIDs are unique (duplicates are a hard
//! error reporting the offending indices). A missing file yields an empty
//! store. A store instance is meant to be owned by one proxy process;
//! concurrent access from multiple processes is out of scope.
//!
//! No network I/O, no unsafe code, no panics in API paths.
#![forbid(unsafe_code)]

mod error;
mod store;

pub use crate::error::StoreError;
pub use crate::store::{JsonPlayerStore, MAX_FILE_SIZE_BYTES};
