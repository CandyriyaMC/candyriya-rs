//! Extism WASM plugin host runtime: metadata, dependency resolution, lifecycle and hot reload.
//!
//! This crate is the proxy's plugin engine. Today it covers the loading half:
//!
//! - [`PluginManifest`] — the parsed, validated `plugin.toml`.
//! - [`validate_unique`] — set-level duplicate id detection across libraries
//!   and plugins.
//! - [`PluginLoader`] — directory discovery across `plugins/` and
//!   `libraries/`, per-plugin records, and [`LoadReport`] issues as
//!   `(where, why)` tuples.
//! - [`PluginInstance`] — a wasm module living on its dedicated,
//!   named OS thread (`candyriya-plugin-<id>`), reachable through a bounded
//!   mailbox; calls never run on async workers.
//! - [`LibraryPlugin`] / [`PluginState`] — per-plugin records and lifecycle
//!   (`Loaded` / `Failed` / `Disabled`).
//! - [`HostState`] — host-side state handed to each instance.
//!
//! Not implemented here yet (by design, they land next): the dependency
//! resolver, host functions and the task API, fuel/memory limits and WASI,
//! and the hot-reload machinery. No host functions are registered today.

#![forbid(unsafe_code)]

mod error;
mod host_state;
mod instance;
mod library_plugin;
mod manifest;
mod plugin_loader;

pub use crate::error::PluginError;
pub use crate::host_state::HostState;
pub use crate::instance::PluginInstance;
pub use crate::library_plugin::{LibraryPlugin, PluginOrigin, PluginState};
pub use crate::manifest::{DuplicateId, DuplicatePluginIds, PluginManifest, validate_unique};
pub use crate::plugin_loader::{LoadIssue, LoadReport, PluginLoader};
