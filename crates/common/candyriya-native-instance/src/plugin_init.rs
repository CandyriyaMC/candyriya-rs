//! The wrapper around the plugin's `candyriya_plugin_init` export.
//!
//! [`initialize`] is the one typed call this crate makes into plugin code.
//! Everything it needs is pinned by the ABI contract: the init signature, the
//! handshake validation, and the output layout. The metadata buffers the
//! plugin returns point at plugin-owned memory that is not guaranteed to
//! outlive the call — they are copied into an owned [`PluginIdentity`]
//! before this function returns, and no raw pointer from the call escapes.

use core::ffi::{c_char, c_void};

use candyriya_native_abi::{CandyriyaBuf, CandyriyaPluginInitFn, PluginInitArgs, PluginInitOutput};
use thiserror::Error;

use crate::export_names::PLUGIN_INIT_EXPORT;
use crate::handshake::{HandshakeError, validate_handshake, validate_init_output};
use crate::instance::{LibraryLoader, LoaderError};

/// The identity a plugin reports at init, with the borrowed buffers copied
/// out before the caller keeps them.
///
/// The ABI carries raw bytes and does not require them to be UTF-8; real
/// plugins report UTF-8 here. The byte contents are checked against the
/// plugin manifest rules by the layer that owns the plugin registry — this
/// type only guarantees the copy happened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginIdentity {
    /// Plugin id bytes, e.g. `b"greeting"`.
    pub id: Vec<u8>,
    /// The plugin's own version string bytes, e.g. `b"0.1.0"`.
    pub version: Vec<u8>,
    /// Human-readable display name bytes, e.g. `b"Greeting"`.
    pub name: Vec<u8>,
    /// One-line description bytes; `None` when the plugin reported an empty
    /// description buffer.
    pub description: Option<Vec<u8>>,
}

/// The outcome of a successful init handshake.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InitializedPlugin {
    /// The ABI version the plugin echoed (`1` for V1).
    pub abi_version: u32,
    /// The plugin's identity, copied out of the call.
    pub identity: PluginIdentity,
}

/// Calls the plugin's `candyriya_plugin_init` and validates the handshake.
///
/// The steps, in order:
///
/// 1. [`validate_handshake`] — the caller speaks V1 and the table is a
///    genuine V1 table (fail-closed pre-flight);
/// 2. [`LibraryLoader::probe_export`] — resolve `candyriya_plugin_init`;
/// 3. the opaque probe is cast to [`CandyriyaPluginInitFn`], the exact
///    signature the contract pins, and called with `args`;
/// 4. [`validate_init_output`] — status `Ok`, ABI echo, complete identity;
/// 5. the metadata buffers are copied into an owned [`PluginIdentity`].
///
/// # Safety
///
/// - the library must be a plugin built against the V1 ABI, and its
///   `candyriya_plugin_init` export must be a sound function to call with
///   `args`;
/// - `args.api` must be null or point at a live `CandyriyaPluginApiV1` for
///   the duration of the call and for as long as the plugin dereferences it
///   during that call (true for args built by `PluginInitArgs::new`);
/// - the metadata buffers the plugin returns are plugin-owned and are
///   expected to outlive the return by the ABI contract (in practice they
///   are `&'static` bytes in the plugin); this crate copies them before any
///   other code runs, right here, and never retains the raw pointers.
pub unsafe fn initialize(
    library: &LibraryLoader,
    args: &PluginInitArgs,
) -> Result<InitializedPlugin, InitError> {
    // pre-flight: refuse to bother the plugin if our side of the handshake is
    // not exactly what the contract describes
    // SAFETY: `args` is a shared reference; the live-table contract is
    // upheld by the caller as documented.
    unsafe { validate_handshake(args.api as *const c_void, args.abi_version) }?;

    // SAFETY: `PLUGIN_INIT_EXPORT` is a static nul-terminated name.
    let probe = unsafe { library.probe_export(PLUGIN_INIT_EXPORT.as_ptr()) }?;
    // the cast happens here, at the one place that knows the exact V1
    // signature — never inside the loader
    // SAFETY: the symbol was resolved by name from a library the caller
    // vouches to be a V1 plugin; if it reports as `candyriya_plugin_init` it
    // is the init entry point, whose soundness with `args` the caller vouches
    // for. `*probe` is the symbol's address, which is what a function pointer
    // denotes for a `no_mangle` export.
    let init: CandyriyaPluginInitFn =
        unsafe { core::mem::transmute::<*const c_char, CandyriyaPluginInitFn>(*probe) };

    // SAFETY: `args` is a live shared reference valid for the call; the
    // plugin may read it, never retain it, and returns its answer by value.
    let output = unsafe { init(args as *const PluginInitArgs) };

    validate_init_output(&output, args.abi_version)?;

    Ok(InitializedPlugin {
        abi_version: output.abi_version,
        identity: copy_identity(&output),
    })
}

/// Copies the plugin-reported identity out of the call.
///
/// The buffers are read right after the call returned — still inside the
/// window where the plugin's returned buffers are valid by contract — and
/// nothing from the call escapes past this function.
fn copy_identity(output: &PluginInitOutput) -> PluginIdentity {
    let copy = |buf: &CandyriyaBuf| {
        // SAFETY: the buffers came out of the init call; the plugin contract
        // keeps plugin-owned memory alive across the return, and we read it
        // before any other plugin code or drop can run. Nothing is retained.
        unsafe { buf.as_slice() }.to_vec()
    };
    let description = if output.metadata.description.is_empty() {
        None
    } else {
        Some(copy(&output.metadata.description))
    };
    PluginIdentity {
        id: copy(&output.metadata.id),
        version: copy(&output.metadata.version),
        name: copy(&output.metadata.name),
        description,
    }
}

/// Errors from [`initialize`]: either the loader or the handshake refused.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum InitError {
    /// Probing the init export failed, or the library cannot be opened.
    #[error(transparent)]
    Loader(#[from] LoaderError),
    /// The handshake was refused: our pre-flight, or the plugin's answer.
    #[error(transparent)]
    Handshake(#[from] HandshakeError),
}
