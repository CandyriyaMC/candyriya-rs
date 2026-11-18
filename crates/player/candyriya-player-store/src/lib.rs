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
//! as a compact single-line JSON array in UUID order: the output is
//! deterministic for the same set of records and meaningfully smaller
//! than pretty-printing (~1.1–1.4x depending on record shape, see the
//! cost model below). Pretty-printed files written by older versions
//! still load — the parser accepts any JSON `serde_json` can read.
//!
//! # Cost model
//!
//! Every save is O(roster): the full record vector is serialized and the
//! entire file is rewritten — no dirty tracking and no delta log. Each
//! save pays a full write + `sync_all` + rename cycle, and persistence is
//! synchronous: a mutating call returns only after the fsync completes.
//! There is no debounce and no batching. Compact JSON shrinks the payload
//! roughly 1.1–1.4x versus pretty-printing depending on record shape
//! (records dominated by long base64 texture blobs see the smallest win;
//! small records see the largest) — it does not otherwise reduce the
//! per-save cost. Rule of thumb for operators sizing for this store:
//!
//! - bytes per save ≈ roster size × per-record size (a bare record is
//!   ~150 bytes compact; one signed `textures` property pushes it to
//!   ~670 bytes)
//! - bytes written per second ≈ that × save rate (one save per sighting)
//!
//! so a 5-figure roster seen once per second rewrites several megabytes
//! and pays a full fsync that many times per second. Operators with very
//! large rosters should size disk throughput and fsync latency
//! accordingly, or accept the cost.
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
