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
//!   [`EventEnvelope`] / [`ProxyEvent`] / [`EventCancellation`] /
//!   [`OutcomeReport`] / [`MinecraftEvent`] / [`EventIdRegistry`] /
//!   [`PlayerRef`] — the typed event model the host fires into
//!   plugins: structured kinds, monotonic envelope ids, and the two
//!   response shapes a plugin can hand back.
//! - [`LoginEvent`] / [`KickEvent`] / [`DisconnectEvent`] /
//!   [`ConnectAllowedEvent`] / [`SwitchServerEvent`] — convenience
//!   aliases for [`ProxyEvent`], one per game event.
//! - [`ComponentBuilder`] — fluent wrapper around lobsterchat's component
//!   construction for the typical chat/kick/title line; components are
//!   built, serialized and handed over whole.
//! - [`MessageMode`] — how the host should deliver a message
//!   (plain chat, system message or the actionbar overlay).
//! - [`KickReason`] / [`DisconnectReason`] — the pre-built kick and
//!   disconnect reasons, renderable to MiniMessage via candyriya-i18n.
//! - [`text`] / [`translatable`] — the two component constructors plugin
//!   code generators target.
//! - [`parse_minimessage`] — the strict MiniMessage acceptance gate;
//!   rejects anything lobsterchat would silently truncate.
//! - [`escape_minimessage`] — escapes untrusted text for interpolation
//!   into MiniMessage strings.
//!
//! Errors are reported as [`MetadataError`]: manifest validation problems
//! and MiniMessage acceptance failures.
#![forbid(unsafe_code)]

mod events;
mod kick;
mod message;
mod metadata;
mod minimessage;
mod version;

pub use crate::events::{
    ConnectAllowedEvent, DisconnectEvent, EventCancellation, EventEnvelope, EventIdRegistry,
    EventKind, EventPayload, KickEvent, LoginEvent, MinecraftEvent, OutcomeReport, PlayerRef,
    PluginEvent, ProxyEvent, SwitchServerEvent,
};
pub use crate::kick::{DisconnectReason, KickReason};
pub use crate::message::{ComponentBuilder, MessageMode, text, translatable};
pub use crate::metadata::{ApiExport, ApiInfo, MetadataError, PluginMetadata};
pub use crate::minimessage::{escape_minimessage, parse_minimessage};
pub use crate::version::{API_VERSION, IMPLEMENTATION, ProxyVersion};

// lobsterchat is the project-wide component library; the ABI carries its
// types directly, so re-export the ones the host functions and the
// plugin SDK both name.
pub use lobsterchat::component::{ClickEvent, Component, HoverEvent, NamedColor, TextColor};
