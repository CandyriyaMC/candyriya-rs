//! # candyriya-protocol-registry
//!
//! Data-driven registry of Minecraft Java Edition protocol tables, covering
//! stable releases from 1.7.2 (protocol 4) through the newest release -
//! 26.3 (protocol 777) at the time the embedded snapshot was refreshed -
//! and the packet ids the proxy needs to recognize status and login traffic.
//! It does **not** parse packets (that is the job of the codec crate sitting
//! upstream of this one) and does **not** do any networking of its own;
//! the optional refresh path performs exactly one HTTP GET and nothing else.
//!
//! ## The three layers
//!
//! 1. **Embedded snapshot.** `protocol_versions.json` (a verbatim copy of
//!    PrismarineJS's `data/pc/common/protocolVersions.json`) and
//!    `protocol_ids.json` (login/status packet ids generated from the
//!    per-version `protocol.json` files) are compiled into the binary via
//!    `include_bytes!`. Every load parses and validates them; they can never
//!    be "half loaded". See `protocol_versions.provenance.txt` and
//!    `protocol_ids.provenance.txt` next to the data files for sources and
//!    fetch dates.
//! 2. **File override.** `<override_dir>/protocols.json` and
//!    `<override_dir>/protocol_ids.json`, if present and valid, replace the
//!    corresponding embedded file (complete replacement, not a merge - this
//!    keeps behavior predictable). A missing file is not an error; an
//!    invalid file is rejected with a
//!    [`RegistryWarning::OverrideFallback`] and the embedded snapshot is used
//!    for it. Overrides are the supported way to pin new tables without
//!    waiting for a release of this crate.
//! 3. **Auto-refresh.** [`Registry::load_or_refresh`] fetches a fresh
//!    `protocols.json`-shaped document with a short timeout, validates it,
//!    and atomically persists it to the override directory. Any failure -
//!    network, HTTP status, oversized body, invalid JSON, validation,
//!    missing packet-id coverage - leaves the current tables untouched and
//!    reports [`RegistryWarning::RefreshFailed`]; packet-id tables are never
//!    fetched (they have no upstream home; they ship with this crate or via
//!    the override directory). The refresh fetch itself is gated behind the
//!    `refresh` cargo feature (minreq with native TLS) because native TLS
//!    chains do not build on bare windows-gnu boxes; the refresh *logic* -
//!    validation, atomic persistence, fallback - is always compiled and
//!    tested. Without the feature every refresh attempt fails into the
//!    documented fallback with a [`RegistryWarning::RefreshFailed`]; enable
//!    it with `--features refresh`. Refresh never panics and never fails
//!    startup; it degrades to the embedded/override tables with a warning.
//!
//! Data directory resolution is the caller's job: this crate takes paths
//! explicitly (`override_dir: Option<&Path>`) and knows nothing about
//! configuration files or platform data dirs.
//!
//! ## Validation rules
//!
//! Applied identically to the embedded snapshot, the override files and any
//! fetched document:
//!
//! `protocols.json` (a `protocolVersions.json`-shaped array):
//!
//! - JSON parses; every element has a non-empty `minecraftVersion` (at most
//!   64 chars, unique across the whole file) and an integer `version` in
//!   `0..=2147483647`.
//! - Only stable releases from 1.7.2 onwards are kept: plain dotted-numeric
//!   names `M.m[.p]` with `M > 1 || (M == 1 && m >= 7)`.
//! - At least 83 releases must remain, including the anchor `1.7.2`
//!   (protocol 4), and at least one entry with protocol 777 or newer (the
//!   26.x line).
//! - The kept set is sorted by `(protocol, numeric version)`; protocol ids are
//!   non-decreasing throughout.
//!
//! `protocol_ids.json` (schema 1):
//!
//! - JSON parses; `schema` must be 1; `protocols` is non-empty and its
//!   `protocol` values are unique and non-negative.
//! - Every entry lists at least one stable release `names` value
//!   (dotted-numeric) and its `ids` object contains exactly the twelve
//!   documented slot keys, each an id in `0..=2147483647` or an explicit
//!   `null`.
//! - `null` is only allowed for the three slots that are genuinely absent
//!   on old versions (`login.clientbound.set_compression` before 1.8,
//!   `login.*.plugin_*` before 1.13), and every `null` must be covered by a
//!   `known_gaps` entry; a `null` with no gap - or a gap with no `null` -
//!   is a validation error.
//! - Packet ids inside one direction+state group are pairwise distinct.
//!
//! Pair integrity (versions table vs packet-id tables): a supported release
//! with no packet-id entry is reported as [`RegistryWarning::PacketIdsMissing`]
//! and classifying any of its packets returns [`PacketKind::Unknown`]; the
//! reverse direction is reported as [`RegistryWarning::PacketIdsExtraNames`].
//! For a refresh, missing coverage is a hard failure (the fetched document is
//! rejected); for the override files it is a warning, so overriding the
//! versions table to a release with no packet ids does not brick startup.
//!
//! ## Handling releases with one protocol id
//!
//! Several releases share one protocol (763 = 1.20/1.20.1, 754 = 1.16.4/1.16.5,
//! 775 = 26.1/26.1.1/26.1.2, ...). [`Registry::from_protocol`] returns the
//! newest release in the protocol group (by numeric version order, not string
//! order) - the last release shipped with that protocol. Packet ids are
//! shared by construction.
//!
//! ## Feature stability
//!
//! `VersionInfo::has_configuration_state` is true from protocol 764 (1.20.2),
//! when both peers began entering the configuration state after login.
//! `VersionInfo::has_transfer_intent` is true from protocol 766 (1.20.5),
//! when the transfer feature and its login plugin message arrived. Both were
//! verified against the packet tables at snapshot time, not from memory.
//!
//! ## Testing
//!
//! The test suite never touches the network: the embedded snapshot is used
//! for defaults, and the refresh/override paths are exercised with hand-built
//! JSON in temporary directories (and, for the refresh fetch itself, an
//! injected in-process fetcher).
//!
//! ```
//! let registry = candyriya_protocol_registry::Registry::embedded()
//!     .expect("the embedded snapshot is valid");
//! assert_eq!(registry.latest().name, "26.3");
//! assert_eq!(registry.protocol_id_of("1.12.2"), Some(340));
//! ```

#![forbid(unsafe_code)]

mod classify;
mod ids;
mod kind;
mod loader;
mod state;
mod version;
mod versions;

pub use crate::kind::PacketKind;
pub use crate::loader::{
    LoadOutcome, Registry, RegistryError, RegistryOrigin, RegistryWarning, TableOrigin,
};
pub use crate::state::{ConnectionState, Direction, PacketSlot};
pub use crate::version::VersionInfo;
