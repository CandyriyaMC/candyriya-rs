//! The controller's view of one plugin.
//!
//! A [`PluginRecord`] is born at discovery (`DISCOVERED`) and dies at
//! teardown (`UNLOADED`). It carries identity fields (id, version, name,
//! description, entry point), the wire state under allow-list discipline,
//! and the failure bookkeeping the tree wants for ps and metrics.
//!
//! Two shapes are deliberately local to this crate:
//!
//! - [`ExitStatus`] mirrors the supervision tree's exit vocabulary
//!   (`Success` / `Failure(message)` / `Panicked` / `Cancelled` /
//!   `TimedOut`). The integration layer maps it to/from the tree's own
//!   type at the boundary; this crate must not depend on the kernel.
//! - [`LoadedThreadHandle`] is a test-only opaque tag standing in for the
//!   real handle the integration phase stashes here (the unit's identity,
//!   or a join token). It carries no join semantics — this crate never
//!   blocks on plugin threads.

use std::fmt;
use std::path::PathBuf;

use crate::state::PluginWireState;

/// The discovery-side slice of `plugin.toml` the controller tracks.
///
/// This is *not* a full manifest re-implementation: discovery and
/// validation live upstream (the loader parses and validates the whole
/// file, id charset included). The record only needs the identity fields
/// to show up in ps and to build a record from. If a field changes
/// upstream, this struct changes with it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PluginManifest {
    /// The plugin id (`[a-z0-9_.-]{1,64}`), validated upstream.
    pub id: String,
    /// The version string as written in the manifest (validated as semver
    /// upstream; kept raw here).
    pub version: String,
    /// The human-readable plugin name.
    pub name: String,
    /// The human-readable description, if any.
    pub description: Option<String>,
}

impl PluginManifest {
    /// Builds a manifest from raw parts. No checks here — discovery
    /// validates, this just carries.
    pub fn new(
        id: impl Into<String>,
        version: impl Into<String>,
        name: impl Into<String>,
        description: Option<String>,
    ) -> Self {
        Self {
            id: id.into(),
            version: version.into(),
            name: name.into(),
            description,
        }
    }
}

/// How a plugin unit finished, as observed by the controller.
///
/// Mirrors the supervision tree's exit vocabulary so the integration
/// layer can convert without a translation table. Messages inside
/// [`ExitStatus::Failure`] are produced bounded upstream (the tree caps
/// them at 1 KiB); this crate does not re-bound them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExitStatus {
    /// The unit body ran to completion and reported success.
    Success,
    /// The unit body finished but reported a failure.
    Failure(String),
    /// The unit body panicked.
    Panicked,
    /// The unit was cancelled (a requested stop, never a failure).
    Cancelled,
    /// The unit exceeded its per-unit timeout.
    TimedOut,
}

impl ExitStatus {
    /// Whether the exit is a success.
    pub fn is_success(&self) -> bool {
        matches!(self, Self::Success)
    }

    /// Build a failure status from raw text. No bounding here; bounding is
    /// the producer's contract.
    pub fn failure(message: impl Into<String>) -> Self {
        Self::Failure(message.into())
    }
}

impl fmt::Display for ExitStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Success => f.write_str("success"),
            Self::Failure(message) => write!(f, "failure({message})"),
            Self::Panicked => f.write_str("panicked"),
            Self::Cancelled => f.write_str("cancelled"),
            Self::TimedOut => f.write_str("timed-out"),
        }
    }
}

/// Opaque stand-in for the thread handle the integration phase will stash
/// on the record.
///
/// This crate deliberately does not hold a real join handle or any thread
/// machinery — the plugin thread is a later phase's business. The handle
/// exists so the record shape is stable across that change: tests use
/// [`LoadedThreadHandle::for_test`] to tag records, the integration swaps
/// the innards without touching call sites.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct LoadedThreadHandle(u64);

impl LoadedThreadHandle {
    /// The raw tag value. Stable for a unit's lifetime once set.
    pub const fn tag(self) -> u64 {
        self.0
    }

    /// Builds a test-only tag. Not for production use — the integration
    /// phase constructs real handles.
    #[doc(hidden)]
    pub const fn for_test(tag: u64) -> Self {
        Self(tag)
    }
}

impl fmt::Display for LoadedThreadHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "thread#{}", self.0)
    }
}

/// The controller's record for one discovered plugin.
///
/// Fields are public except `state`: state may only move through the
/// registry's allow-list-checked transition, never by assignment. The
/// identity fields are set at discovery and never mutated afterwards.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PluginRecord {
    /// The plugin id (`[a-z0-9_.-]{1,64}`).
    pub id: String,
    /// The version string from the manifest.
    pub version: String,
    /// The human-readable plugin name.
    pub name: String,
    /// The human-readable description, if the manifest carries one.
    pub description: Option<String>,
    /// Path of the entry point: the discovered library file
    /// (`<id>/<id>.dll` or the platform sibling), set at discovery.
    pub entry_point: PathBuf,
    state: PluginWireState,
    /// How many times this record's unit has been restarted by the tree.
    pub restart_count: u32,
    /// How the unit exited the last time, once it did.
    pub last_exit: Option<ExitStatus>,
    /// Why the plugin failed, if it did. Set on the failure path and
    /// preserved into terminal `UNLOADED`.
    pub reason: Option<String>,
    /// Whether this plugin can be restaged automatically (the tree's
    /// restart policy consumes this later; this crate only carries it).
    pub can_fail_fast: bool,
    /// The loaded thread handle, filled by the integration phase.
    pub loaded_thread_handle: Option<LoadedThreadHandle>,
}

impl PluginRecord {
    /// Builds a `DISCOVERED` record from a manifest slice and the entry
    /// point path discovery resolved.
    ///
    /// `can_fail_fast` defaults to `false` — opt-in per plugin, flipped by
    /// the discovery side once policy is known.
    pub fn from_manifest(manifest: &PluginManifest, entry_point: impl Into<PathBuf>) -> Self {
        Self {
            id: manifest.id.clone(),
            version: manifest.version.clone(),
            name: manifest.name.clone(),
            description: manifest.description.clone(),
            entry_point: entry_point.into(),
            state: PluginWireState::Discovered,
            restart_count: 0,
            last_exit: None,
            reason: None,
            can_fail_fast: false,
            loaded_thread_handle: None,
        }
    }

    /// The current wire state.
    pub const fn state(&self) -> PluginWireState {
        self.state
    }

    /// Writes a state that has already been checked by the allow-list.
    ///
    /// Crate-internal on purpose: the only writer in this crate is the
    /// registry, after it validated the move. The machine module uses it
    /// as well (its stub bypasses the table deliberately, see
    /// [`crate::machine::apply`]).
    pub(crate) fn set_state(&mut self, state: PluginWireState) {
        self.state = state;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn demo_manifest() -> PluginManifest {
        PluginManifest::new(
            "greeting",
            "1.2.3",
            "Greeting Demo",
            Some("a demo plugin".into()),
        )
    }

    #[test]
    fn from_manifest_lands_in_discovered() {
        let record = PluginRecord::from_manifest(&demo_manifest(), "plugins/greeting/greeting.dll");
        assert_eq!(record.id, "greeting");
        assert_eq!(record.version, "1.2.3");
        assert_eq!(record.name, "Greeting Demo");
        assert_eq!(record.description.as_deref(), Some("a demo plugin"));
        assert_eq!(
            record.entry_point,
            PathBuf::from("plugins/greeting/greeting.dll")
        );
        assert_eq!(record.state(), PluginWireState::Discovered);
        assert_eq!(record.restart_count, 0);
        assert_eq!(record.last_exit, None);
        assert_eq!(record.reason, None);
        assert!(!record.can_fail_fast);
        assert_eq!(record.loaded_thread_handle, None);
    }

    #[test]
    fn manifest_without_description_round_trips_none() {
        let manifest = PluginManifest::new("minimal", "0.1.0", "Minimal", None);
        let record = PluginRecord::from_manifest(&manifest, "plugins/minimal/minimal.so");
        assert_eq!(record.description, None);
    }

    #[test]
    fn test_handle_tag_is_opaque_and_round_trips() {
        let handle = LoadedThreadHandle::for_test(42);
        assert_eq!(handle.tag(), 42);
        assert_eq!(handle, LoadedThreadHandle::for_test(42));
        assert_ne!(handle, LoadedThreadHandle::for_test(43));
        assert_eq!(handle.to_string(), "thread#42");
    }

    #[test]
    fn exit_status_spelling() {
        assert_eq!(ExitStatus::Success.to_string(), "success");
        assert_eq!(ExitStatus::failure("boom").to_string(), "failure(boom)");
        assert_eq!(ExitStatus::Panicked.to_string(), "panicked");
        assert_eq!(ExitStatus::Cancelled.to_string(), "cancelled");
        assert_eq!(ExitStatus::TimedOut.to_string(), "timed-out");
        assert!(ExitStatus::Success.is_success());
        assert!(!ExitStatus::failure("x").is_success());
    }
}
