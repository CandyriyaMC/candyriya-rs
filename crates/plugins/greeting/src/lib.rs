//! greeting — the reference native plugin for the candyriya native ABI.
//!
//! It does the smallest thing that exercises the whole contract: on
//! `candyriya_plugin_on_enable` it prints one line to stdout. No host
//! functions, no proxy calls — those land with the later ABI versions.
//!
//! Real plugins built this way are `cdylib`s the proxy opens with `dlopen`
//! / `LoadLibrary`; this one is also an `rlib` so its tests can drive the
//! exports in-process. It is deliberately platform-agnostic: no `cfg`s,
//! no platform crates, nothing target-specific.
//!
//! # What the proxy sees
//!
//! 1. opens the library, resolves [`candyriya_plugin_init`];
//! 2. `init` validates the [`PluginInitArgs`] handshake and answers with
//!    [`PluginInitOutput`] — an [`PluginMetadata::new`] identity on success;
//! 3. `on_load` re-checks the table and reports [`CandyriyaStatus::Ok`];
//! 4. `on_enable` does it again and prints the hello line.
//!
//! A plugin that sees anything it does not recognise — wrong ABI echo, a
//! mis-sized table, a null pointer — returns [`CandyriyaStatus::Failed`]
//! and is refused. Failing closed is the whole point of the handshake.

#![allow(unsafe_code)] // the exports are the FFI boundary; see the crate docs

use candyriya_native_abi::{
    CandyriyaPluginApiV1, CandyriyaStatus, PluginInitArgs, PluginInitOutput, PluginMetadata,
};

static ID: &[u8] = b"greeting";
static VERSION: &[u8] = b"0.1.0";
static NAME: &[u8] = b"Greeting";
static DESCRIPTION: &[u8] = b"Reference native plugin for the candyriya native ABI";

/// One-liner the plugin prints on enable. Kept in one place so the test
/// can assert on the same text the operator will see.
const HELLO: &str = "greeting: hello from a native candyriya plugin (ABI v1)";

fn metadata() -> PluginMetadata {
    PluginMetadata::new(ID, VERSION, NAME, DESCRIPTION)
}

/// Checks a table pointer the way the ABI demands: null-tolerant,
/// version-aware, size-aware. Returns the table only when it is a genuine
/// V1 table built by this crate's contract.
///
/// # Safety
///
/// `api` must be null or point at a live [`CandyriyaPluginApiV1`] — true by
/// construction for anything the proxy hands us, which is the only legal
/// caller.
unsafe fn checked_table<'a>(api: *const CandyriyaPluginApiV1) -> Option<&'a CandyriyaPluginApiV1> {
    // SAFETY: the caller upholds the pointer contract above.
    let table = unsafe { api.as_ref() }?;
    table.is_valid().then_some(table)
}

/// The init handshake. Resolved first by the proxy; on failure the plugin
/// is not called again.
///
/// # Safety
///
/// Called across the FFI boundary. `args` is null or a valid
/// [`PluginInitArgs`] for the duration of the call; buffers in the output
/// point at `&'static` memory owned by this crate and stay valid forever.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn candyriya_plugin_init(args: *const PluginInitArgs) -> PluginInitOutput {
    // SAFETY: `args` is null or a live `PluginInitArgs`; `is_valid`
    // re-checks the embedded table pointer before derefercing it.
    let valid = match unsafe { args.as_ref() } {
        Some(args) => unsafe { args.is_valid() },
        None => false,
    };
    if !valid {
        return PluginInitOutput::failed();
    }
    PluginInitOutput::ok(metadata())
}

/// Called once the proxy has the plugin registered. Phase 1 does nothing
/// here beyond proving the table again.
///
/// # Safety
///
/// Called across the FFI boundary with the proxy-owned table; see
/// [`candyriya_plugin_init`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn candyriya_plugin_on_load(
    api: *const CandyriyaPluginApiV1,
) -> CandyriyaStatus {
    // SAFETY: the proxy guarantees `api` is null or a live table.
    if unsafe { checked_table(api) }.is_none() {
        return CandyriyaStatus::Failed;
    }
    CandyriyaStatus::Ok
}

/// Called once the plugin is enabled. Prints hello and reports success.
///
/// # Safety
///
/// Called across the FFI boundary with the proxy-owned table; see
/// [`candyriya_plugin_init`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn candyriya_plugin_on_enable(
    api: *const CandyriyaPluginApiV1,
) -> CandyriyaStatus {
    // SAFETY: the proxy guarantees `api` is null or a live table.
    if unsafe { checked_table(api) }.is_none() {
        return CandyriyaStatus::Failed;
    }
    println!("{HELLO}");
    CandyriyaStatus::Ok
}
