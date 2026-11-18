//! `candyriya-native-instance` — the dynamic-library boundary for native
//! candyriya plugins.
//!
//! This crate opens a plugin's `.dll` / `.so` / `.dylib`, resolves the frozen
//! ABI exports and runs the fail-closed init handshake. It deliberately does
//! very little:
//!
//! - it never calls a typed plugin function on its own — the init entry point
//!   is wrapped in [`initialize`], whose signature is pinned by the ABI
//!   contract crate;
//! - it hands out opaque [`libloading::Symbol`] handles; casting them to the
//!   signatures callers expect is the caller's visible, reviewed step;
//! - it does not own threads, mailboxes, deadlines or teardown — that
//!   supervision layer builds on top of this crate.
//!
//! The ABI contract itself (struct layouts, versioning rules, the table and
//! the exact export signatures) lives in `candyriya-native-abi`; this crate
//! depends on it and never redefines any of those types.
//!
//! # Unsafe surface
//!
//! This crate carries the workspace's scoped `unsafe` opt-out for the plugin
//! boundary. Every `unsafe fn` and every `unsafe` block documents the
//! invariant that makes it sound; auditing those invariants is the review
//! that comes with this opt-out.

#![warn(missing_docs)]
// not `forbid`: this crate *is* the unsafe boundary; the opt-out lives in
// Cargo.toml so the rest of the workspace can keep the forbid
#![allow(unsafe_code)]

mod export_names;
mod handshake;
mod instance;
mod plugin_init;

pub use export_names::{
    PLUGIN_INIT_EXPORT, PLUGIN_ON_DISABLE_EXPORT, PLUGIN_ON_ENABLE_EXPORT, PLUGIN_ON_LOAD_EXPORT,
    PLUGIN_ON_UNLOAD_EXPORT,
};
pub use handshake::{HandshakeError, validate_handshake, validate_init_output};
pub use instance::{LibraryLoader, LoaderError, OpenFlags};
pub use plugin_init::{InitializedPlugin, InitError, PluginIdentity, initialize};
