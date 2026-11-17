//! One discovered plugin: its directory, manifest and runtime state.

use std::path::PathBuf;

use crate::error::PluginError;
use crate::instance::PluginInstance;
use crate::manifest::PluginManifest;

/// Where a plugin was discovered.
///
/// Libraries and plugins share one layout and one loader; they only differ
/// in which directory they came from, and in load order (libraries first).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PluginOrigin {
    /// `libraries/<id>/` — a plugin whose main job is to export API for
    /// other plugins. Loaded before everything in `plugins/`.
    Library,
    /// `plugins/<id>/` — a regular plugin.
    Plugin,
}

/// Lifecycle state of a discovered plugin.
///
/// `Failed` records keep the manifest so operators can see *what* failed and
/// why; a failed plugin never runs wasm. `Disabled` plugins instantiated
/// fine but are switched off; the toggle does not touch the wasm instance
/// (invoking the plugin's disable hook is a separate milestone).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginState {
    /// The wasm module compiled, `plugin_main` is exported, the plugin's
    /// thread is running.
    Loaded,
    /// The plugin was discovered and its manifest was valid, but it could
    /// not be prepared for execution. The string is a human-readable reason.
    Failed(String),
    /// The plugin loaded and was switched off by the operator/proxy.
    Disabled,
}

impl PluginState {
    /// Is this plugin currently able to serve calls?
    pub fn is_loaded(&self) -> bool {
        matches!(self, PluginState::Loaded)
    }
}

/// A discovered plugin and its runtime slot.
///
/// Records in `Failed` state carry no instance; [`LibraryPlugin::instance`]
/// returns `None` for them. Records are cheap to keep around regardless of
/// state, so discovery problems never take a plugin silently off the map.
pub struct LibraryPlugin {
    dir: PathBuf,
    manifest: PluginManifest,
    origin: PluginOrigin,
    instance: Option<PluginInstance>,
    state: PluginState,
}

impl LibraryPlugin {
    /// Build a record from parts (loader-internal plumbing).
    pub(crate) fn new(
        dir: PathBuf,
        manifest: PluginManifest,
        origin: PluginOrigin,
        instance: PluginInstance,
    ) -> Self {
        Self {
            dir,
            manifest,
            origin,
            instance: Some(instance),
            state: PluginState::Loaded,
        }
    }

    /// Build a record for a plugin that was discovered but cannot run.
    pub(crate) fn failed(
        dir: PathBuf,
        manifest: PluginManifest,
        origin: PluginOrigin,
        reason: String,
    ) -> Self {
        Self {
            dir,
            manifest,
            origin,
            instance: None,
            state: PluginState::Failed(reason),
        }
    }

    /// The directory this plugin lives in (`<dir>/<id>/`).
    pub fn dir(&self) -> &std::path::Path {
        &self.dir
    }

    /// The parsed and validated manifest.
    pub fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }

    /// The plugin id from the manifest.
    pub fn id(&self) -> &str {
        self.manifest.id()
    }

    /// Where the plugin was discovered.
    pub fn origin(&self) -> PluginOrigin {
        self.origin
    }

    /// Current lifecycle state.
    pub fn state(&self) -> &PluginState {
        &self.state
    }

    /// The underlying wasm instance, if this plugin is running.
    pub fn instance(&self) -> Option<&PluginInstance> {
        self.instance.as_ref()
    }

    /// Switch the plugin off (`Loaded` -> `Disabled`).
    ///
    /// Idempotent when already disabled. Fails for plugins in `Failed`
    /// state — there is no instance to switch off. Does not call into the
    /// wasm module; invoking a plugin-authored disable hook is out of scope
    /// for now.
    pub fn disable(&mut self) -> Result<(), PluginError> {
        match self.state {
            PluginState::Loaded | PluginState::Disabled => {
                self.state = PluginState::Disabled;
                Ok(())
            }
            PluginState::Failed(_) => Err(PluginError::InvalidState {
                plugin_id: self.id().to_owned(),
                action: "disable",
                state: format!("{:?}", self.state),
            }),
        }
    }

    /// Switch the plugin on (`Disabled` -> `Loaded`).
    ///
    /// Idempotent when already running. Fails for plugins in `Failed`
    /// state — there is no instance to switch on.
    pub fn enable(&mut self) -> Result<(), PluginError> {
        match self.state {
            PluginState::Disabled => {
                self.state = PluginState::Loaded;
                Ok(())
            }
            PluginState::Loaded => Ok(()),
            PluginState::Failed(_) => Err(PluginError::InvalidState {
                plugin_id: self.id().to_owned(),
                action: "enable",
                state: format!("{:?}", self.state),
            }),
        }
    }

    /// Shut the plugin's worker thread down and wait for it.
    ///
    /// Failed plugins have no instance and only their record is dropped.
    /// See [`PluginInstance::unload`] for the semantics of the wait.
    pub fn unload(self) -> Result<(), PluginError> {
        if let Some(instance) = self.instance {
            instance.unload()?;
        }
        Ok(())
    }
}
