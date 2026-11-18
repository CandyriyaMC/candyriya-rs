//! `candyriya-native-instance` — the dynamic-library boundary for native
//! candyriya plugins. The README is the module documentation; it is included
//! verbatim so the example flow it shows stays compiled by `cargo test --doc`.

#![doc = include_str!("../README.md")]
#![warn(missing_docs)]
// not `forbid`: this crate *is* the unsafe boundary; the opt-out lives in
// Cargo.toml so the rest of the workspace can keep the forbid
#![allow(unsafe_code)]

mod export_names;
mod handshake;
mod instance;
mod lifecycle;
mod plugin_init;

pub use export_names::{
    PLUGIN_INIT_EXPORT, PLUGIN_ON_DISABLE_EXPORT, PLUGIN_ON_ENABLE_EXPORT, PLUGIN_ON_LOAD_EXPORT,
    PLUGIN_ON_UNLOAD_EXPORT,
};
pub use handshake::{HandshakeError, validate_handshake, validate_init_output};
pub use instance::{LibraryLoader, LoaderError, OpenFlags};
pub use lifecycle::{PluginToken, TableSlot, export_exists, init_plugin, on_enable, on_load};
pub use plugin_init::{InitError, InitializedPlugin, PluginIdentity, initialize};
