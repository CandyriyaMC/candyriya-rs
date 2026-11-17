//! Error types for the plugin host runtime.
//!
//! Discovery problems (a stray `.wasm` with no manifest, a manifest that does
//! not parse, an id mismatch) do not fail the load; they land in the
//! [`crate::LoadReport::issues`] list as `(where, why)` tuples. This enum is
//! for hard failures: a single plugin that cannot be used, a broken host
//! invariant, or a discovered set that cannot be ordered at all.

use std::path::PathBuf;

use crate::dep_resolution::DependencyFailure;
use crate::library_plugin::LibraryPlugin;
use crate::manifest::DuplicatePluginIds;
use crate::plugin_loader::LoadIssue;

/// Errors produced while preparing, loading or calling one plugin.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum PluginError {
    /// A file the loader needed could not be read.
    #[error("failed to read {}: {source}", .path.display())]
    Io {
        /// The file in question.
        path: PathBuf,
        /// What went wrong.
        source: std::io::Error,
    },

    /// The manifest failed TOML parsing or metadata validation.
    #[error("invalid plugin manifest: {0}")]
    Manifest(#[from] candyriya_plugin_api::MetadataError),

    /// The directory was supposed to contain a plugin layout but does not.
    #[error("no wasm module found for plugin `{plugin_id}` in {} (expected `<id>.wasm`, or `lib.wasm`)", .dir.display())]
    MissingWasm {
        /// The plugin id from the manifest (already validated).
        plugin_id: String,
        /// The plugin's directory.
        dir: PathBuf,
    },

    /// The declared `plugin_main` export is missing from the wasm module.
    #[error(
        "plugin `{plugin_id}` declares plugin_main `{name}` but the wasm module does not export it"
    )]
    MissingExport {
        /// The plugin id from the manifest.
        plugin_id: String,
        /// The export name that was expected.
        name: String,
    },

    /// The plugin could not apply its declared cache quotas.
    #[error("failed to prepare cache namespace for plugin `{plugin_id}`: {reason}")]
    CacheSetup {
        /// The plugin id from the manifest.
        plugin_id: String,
        /// What the cache layer reported.
        reason: String,
    },

    /// The plugin's config directory could not be created.
    #[error("failed to create plugin data directory {}: {source}", .path.display())]
    DataDir {
        /// The directory that could not be created.
        path: PathBuf,
        /// What went wrong.
        source: std::io::Error,
    },

    /// The wasm module failed to compile or instantiate.
    #[error("failed to instantiate wasm module {}: {0}", .path.display())]
    WasmLoad {
        /// The wasm file that failed.
        path: PathBuf,
        /// extism's report.
        message: String,
    },

    /// The dedicated plugin thread could not be spawned or did not join cleanly.
    #[error("plugin thread for `{plugin_id}`: {reason}")]
    PluginThread {
        /// The plugin id from the manifest.
        plugin_id: String,
        /// What happened.
        reason: String,
    },

    /// The requested lifecycle transition is not possible from the current state.
    #[error("cannot {action} plugin `{plugin_id}` while it is in state `{state}`")]
    InvalidState {
        /// The plugin id from the manifest.
        plugin_id: String,
        /// What was attempted, e.g. `"disable"`.
        action: &'static str,
        /// The state the plugin is in (`Loaded` / `Failed(..)` / `Disabled`).
        state: String,
    },

    /// The plugin's mailbox is full; the plugin is busy with earlier work.
    #[error("plugin `{plugin_id}` is busy (mailbox full)")]
    Busy {
        /// The plugin id from the manifest.
        plugin_id: String,
    },

    /// The plugin did not answer within the call timeout.
    #[error("plugin `{plugin_id}` did not respond within {timeout_ms} ms")]
    Timeout {
        /// The plugin id from the manifest.
        plugin_id: String,
        /// The timeout that was hit.
        timeout_ms: u64,
    },

    /// The plugin's thread is gone (or the plugin panicked across a call).
    #[error("plugin `{plugin_id}` is not running")]
    PluginGone {
        /// The plugin id from the manifest.
        plugin_id: String,
    },

    /// A call into the plugin failed.
    #[error("call to `{export}` on plugin `{plugin_id}` failed: {message}")]
    CallFailed {
        /// The plugin id from the manifest.
        plugin_id: String,
        /// The export that was called.
        export: String,
        /// extism's report.
        message: String,
    },

    /// The discovered set contained a duplicate id. The loader already
    /// rejects cross-root collisions, so this means the input to the
    /// resolver was assembled by hand.
    #[error("{0}")]
    DuplicatePluginIds(DuplicatePluginIds),

    /// A dependency cycle was found in the plugin set. The `diagnosis`
    /// lines name the cycle members, one line per cycle, e.g.
    /// `` `a` depends on `b`; `b` depends on `a` ``. Downstream plugins
    /// blocked by a cycle are named on a separate line.
    #[error("dependency cycle(s) in the plugin dependency graph:\n{}", .diagnosis.join("\n"))]
    DependencyCycle {
        /// Self-consistent diagnosis, one line per cycle found.
        diagnosis: Vec<String>,
    },

    /// At least one plugin cannot load because a hard dependency is missing.
    /// Every such failure is listed in `failures` — resolution never stops
    /// at the first one. The plugins that do load are carried along in
    /// `loadable`, in load order; `issues` is the flat `(where, why)` list
    /// covering the drops plus any missing *soft* dependencies.
    #[error("cannot load {n} plugin(s) due to missing hard dependencies: {failures}", n = .failures.len(), failures = .failures.iter().map(ToString::to_string).collect::<Vec<_>>().join("; "))]
    UnresolvedDependencies {
        /// Every dropped plugin and the missing dependency that caused it,
        /// in drop order.
        failures: Vec<DependencyFailure>,
        /// Everything that is wrong with the set, as `(where, why)` tuples.
        issues: Vec<LoadIssue>,
        /// The plugins that load, in dependency order.
        loadable: Vec<LibraryPlugin>,
    },
}
