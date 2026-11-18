//! The `extern "C"` contract between candyriya and its native plugins.
//!
//! This crate is the *whole* contract of the native plugin ABI, phase 1:
//! versioned `#[repr(C)]` structs, an opaque instance handle, byte buffers
//! and the versioned [`CandyriyaPluginApiV1`] function table. It carries no
//! runtime, no loader and no host functions — the loader (phase 2) and the
//! proxy-side APIs (phase 3+) build on top of it. WASM plugins are untouched
//! by this crate.
//!
//! # The call flow
//!
//! 1. The proxy opens the plugin's dynamic library and resolves the
//!    [`CANDYRIYA_PLUGIN_INIT_EXPORT`] symbol.
//! 2. The proxy calls it with [`PluginInitArgs`]: the ABI level it speaks and
//!    a pointer to the [`CandyriyaPluginApiV1`] table it hands out.
//! 3. The plugin fills [`PluginInitOutput`]: its [`PluginMetadata`] and a
//!    [`CandyriyaStatus`]. A failed init means the proxy must not call any
//!    other export.
//! 4. The proxy calls [`CANDYRIYA_PLUGIN_ON_LOAD_EXPORT`], then
//!    [`CANDYRIYA_PLUGIN_ON_ENABLE_EXPORT`], handing each the same
//!    [`CandyriyaPluginApiV1`] pointer.
//!
//! # Buffer rules
//!
//! [`CandyriyaBuf`] is `(ptr, len)` and carries **no ownership**. Buffers
//! point at memory owned by the side that produced them, valid for the
//! duration of the call they are passed in; the consumer must copy anything
//! it wants to keep and must never retain a pointer past the call. An empty
//! buffer is `ptr` free to be null with `len == 0`; a null pointer with a
//! non-zero `len` is a contract violation.
//!
//! # Versioning
//!
//! The ABI version lives in [`CANDYRIYA_NATIVE_ABI_VERSION`] and is echoed
//! inside [`PluginInitArgs::abi_version`], [`CandyriyaPluginApiV1::abi_version`]
//! and [`PluginInitOutput::abi_version`]. Version 1 is frozen: no field is
//! ever added to or removed from a V1 type, and no existing meaning changes.
//! Any change lands as a brand-new V2 table (see the crate README for the
//! full rules and the planned shape of V2).

#![warn(missing_docs)]
// not `forbid`: this crate *is* the unsafe boundary; the opt-out lives in
// Cargo.toml so the rest of the workspace can keep the forbid
#![allow(unsafe_code)]

mod buf;
mod init;
mod metadata;
mod status;
mod table;

pub use buf::CandyriyaBuf;
pub use init::{
    CANDYRIYA_PLUGIN_INIT_EXPORT, CANDYRIYA_PLUGIN_ON_ENABLE_EXPORT,
    CANDYRIYA_PLUGIN_ON_LOAD_EXPORT, CandyriyaPluginInitFn, CandyriyaPluginOnEnableFn,
    CandyriyaPluginOnLoadFn, PluginInitArgs, PluginInitOutput,
};
pub use metadata::PluginMetadata;
pub use status::CandyriyaStatus;
pub use table::{CandyriyaPluginApiV1, CandyriyaPluginHandle};

/// The ABI version this crate speaks: always [`CANDYRIYA_NATIVE_ABI_VERSION_V1`].
///
/// The proxy compares this against the plugin's echo in
/// [`PluginInitOutput::abi_version`] before trusting anything else in the
/// output.
pub const CANDYRIYA_NATIVE_ABI_VERSION: u32 = CANDYRIYA_NATIVE_ABI_VERSION_V1;

/// ABI version 1: the only version defined today.
pub const CANDYRIYA_NATIVE_ABI_VERSION_V1: u32 = 1;
