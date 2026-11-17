//! Host-side state owned by each plugin instance.

use std::path::PathBuf;

/// State the host keeps per plugin and passes to (future) host functions.
///
/// This is deliberately small for now: the proxy will wire richer state
/// (event bus, task service, i18n handle, permission provider) into it as the
/// host-function layer lands. What it carries today is already real — the
/// plugin's own data directory, a path the plugin owns outright and no other
/// plugin may write to.
///
/// `data_dir` points at the plugin's directory under the proxy's config
/// tree (`<data_dir>/config/<plugin-id>/` in the default layout). The loader
/// creates it before instantiation; a plugin whose directory cannot be
/// created is rejected before it ever reaches wasm.
#[derive(Debug)]
pub struct HostState {
    /// The directory this plugin owns. Created by the loader if missing.
    pub data_dir: PathBuf,
}

impl HostState {
    /// Wrap `data_dir` in host state.
    pub fn new(data_dir: PathBuf) -> Self {
        Self { data_dir }
    }
}
