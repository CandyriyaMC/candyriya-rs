//! Extism WASM plugin host runtime: metadata, dependency resolution, lifecycle and hot reload.
//!
//! This crate is the proxy's plugin engine:
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
//! - [`SharedLocales`] — the shared pair of i18n machines plugin host
//!   functions (`candyriya:i18nT` / `candyriya:i18nLocale`) resolve
//!   through; cloning shares tables and registrations.
//! - [`dep_resolution::resolve_plugin_order`] / [`dep_resolution::resolve_plugins`] —
//!   the dependency resolver: turns the discovered set into a dependency
//!   load order (libraries first, then plugins, alphabetical, then
//!   topological) with cycle and missing-hard-dependency diagnostics.
//! - [`PluginSet`] — the ordered, shared set of loaded plugins every other
//!   piece of host code operates on.
//! - [`ServiceRegistry`] — global, type-safe `Arc<dyn Any>` service
//!   registry (route handle, config store, permissions resolver, ... keys).
//! - [`ProxyEventDispatcher`] — fans a [`candyriya_plugin_api::ProxyEvent`]
//!   into every loaded plugin, in load order, with a per-plugin timeout.
//!   Host functions that route between plugins go through its
//!   [`ProxyEventDispatcher::call_api`].
//! - [`PluginReloader`] / [`Watch`] - hot reload primitives: swap a
//!   plugin for its on-disk state, watch `plugins/` and trigger reloads
//!   through the task service. [`PluginReloader::reload_all`] re-reads
//!   every running plugin and picks up newcomers under the roots.
//! - **Host functions** - the `candyriya:*` functions a plugin can call
//!   from WASM (`candyriya:helloWorld`, `candyriya:callApi`,
//!   `candyriya:cancelEvent`, ...), registered on every instance by
//!   [`PluginInstance::new`]. They read the plugin's identity from
//!   [`HostState`] (never from the wire), resolve cross-subsystem handles
//!   through the [`ServiceRegistry`], and are documented as the
//!   [`candyriya_plugin_api::CandyriyaHostApi`] contract for plugin authors.
//!
//! Sandbox limits for modules (memory/fuel/time/byte/handle accounting and
//! the per-window call rate limit) live in [`Limits`] and are applied by
//! [`PluginInstance::new`].

#![forbid(unsafe_code)]

mod dep_resolution;
mod dispatch;
mod error;
mod host_functions;
mod host_state;
mod hot_reload;
mod instance;
mod library_plugin;
mod limits;
mod locales;
mod manifest;
mod metrics;
mod plugin_loader;
mod plugin_set;
mod service_registry;

pub use crate::dep_resolution::{
    DependencyFailure, DependencyResolution, SoftDependency, resolve_plugin_order, resolve_plugins,
    validate_set,
};
pub use crate::dispatch::{
    CallApiError, DEFAULT_EVENT_TIMEOUT, DispatchResult, ON_EVENT_EXPORT, Outcome,
    ProxyEventDispatcher,
};
pub use crate::error::PluginError;
pub use crate::host_functions::{
    DISPATCHER_SERVICE_KEY, PLAYERS_PING_SINK_KEY, PLAYERS_ROSTER_KEY, PlayerPingSink,
    SERVERS_NAMES_KEY,
};
pub use crate::host_state::HostState;
pub use crate::hot_reload::{HotReloadError, PluginReloader, ReloadAllReport, ReloadReport, Watch};
pub use crate::instance::PluginInstance;
pub use crate::library_plugin::{LibraryPlugin, PluginOrigin, PluginState};
pub use crate::limits::{
    CALL_WINDOW, HandleTable, HostHandle, InstanceSlots, LimitBuilder, Limits, LogBudget,
};
pub use crate::locales::SharedLocales;
pub use crate::manifest::{DuplicateId, DuplicatePluginIds, PluginManifest, validate_unique};
pub use crate::plugin_loader::{LoadIssue, LoadReport, PluginLoader};
pub use crate::plugin_set::PluginSet;
pub use crate::service_registry::ServiceRegistry;
