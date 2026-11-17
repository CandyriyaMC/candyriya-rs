//! Host-side state owned by each plugin instance.

use std::path::PathBuf;
use std::sync::Arc;

use crate::limits::InstanceSlots;

/// State the host keeps per plugin and passes to (future) host functions.
///
/// This is deliberately small for now: the proxy will wire richer state
/// (event bus, task service, i18n handle, permission provider) into it as the
/// host-function layer lands. What it carries today is already real — the
/// plugin's own data directory, a path the plugin owns outright and no other
/// plugin may write to, and the live-instance accounting shared across one
/// load scope.
///
/// `data_dir` points at the plugin's directory under the proxy's config
/// tree (`<data_dir>/config/<plugin-id>/` in the default layout). The loader
/// creates it before instantiation; a plugin whose directory cannot be
/// created is rejected before it ever reaches wasm.
///
/// `instance_slots` is the live-instance registry [`crate::PluginInstance`]
/// checks for the `max-instances` limit. [`HostState::new`] creates a fresh,
/// private registry (standalone host states and hot-reload staging each get
/// their own); [`HostState::with_instance_slots`] shares one registry across
/// every host state created for a single scope (e.g. one loader run). The
/// loader uses the shared form so its whole run enforces the cap
/// collectively.
#[derive(Debug)]
pub struct HostState {
    /// The directory this plugin owns. Created by the loader if missing.
    pub data_dir: PathBuf,
    instance_slots: Arc<InstanceSlots>,
}

impl HostState {
    /// Wrap `data_dir` in host state with a fresh, private instance registry.
    pub fn new(data_dir: PathBuf) -> Self {
        Self {
            data_dir,
            instance_slots: Arc::new(InstanceSlots::new()),
        }
    }

    /// Wrap `data_dir` in host state sharing an existing instance registry.
    pub fn with_instance_slots(data_dir: PathBuf, instance_slots: Arc<InstanceSlots>) -> Self {
        Self {
            data_dir,
            instance_slots,
        }
    }

    /// The live-instance registry this state participates in. Cloning the
    /// `Arc` shares the same counts.
    pub fn instance_slots(&self) -> Arc<InstanceSlots> {
        Arc::clone(&self.instance_slots)
    }
}
