//! # candyriya-plugin-api
//!
//! The shared candyriya plugin ABI contract, seen from the host side.
//!
//! Candyriya proxies run plugins as [Extism](https://extism.org) WASM
//! modules. Everything that crosses the host/plugin boundary travels as
//! bytes; the convention for structured data is JSON (via extism-convert
//! on the plugin side, mirroring these types). Because of that, this
//! crate is deliberately boring:
//!
//! - **Data only.** No runtime, no I/O, no network, no threads. Every
//!   argument or return value of a call across the ABI is a plain
//!   struct/enum defined here, and every public type derives
//!   [`serde::Serialize`] + [`serde::Deserialize`].
//! - **No unsafe.** The crate is `#![forbid(unsafe_code)]`.
//! - **Both worlds.** The host (this proxy) and the plugin SDK serialize
//!   through these same shapes, so the two sides cannot silently drift.
//!
//! The actual binding functions (host functions the plugins call, exports
//! the host calls) live in `candyriya-plugin`; they are intentionally not
//! here. This crate is the *contract* those bindings are built on.
//!
//! Contents:
//!
//! - [`ProxyVersion`] — the host identity handshake, kept in sync with
//!   [`API_VERSION`] / [`IMPLEMENTATION`]; use
//!   [`ProxyVersion::current`] and [`ProxyVersion::is_api_compatible`]
//!   for compat checks.
//! - [`PluginMetadata`] — the parsed `plugin.toml` manifest. The loader
//!   deserializes it, then calls [`PluginMetadata::validate`]; anything
//!   stricter (dependencies on disk, quota bookkeeping, signature
//!   checks) is the loader's job, not this crate's.
//! - [`ApiInfo`] / [`ApiExport`] — the `[api]` section listing exports
//!   other plugins may call through the host.
//! - [`EventKind`] / [`EventPayload`] (alias [`PluginEvent`]) /
//!   [`PlayerRef`] — events the host dispatches into plugins.
//!
//! Errors from manifest validation are reported as [`MetadataError`].
#![forbid(unsafe_code)]

mod event;
mod metadata;
mod version;

pub use crate::event::{EventKind, EventPayload, PlayerRef, PluginEvent};
pub use crate::metadata::{ApiExport, ApiInfo, MetadataError, PluginMetadata};
pub use crate::version::{API_VERSION, IMPLEMENTATION, ProxyVersion};
