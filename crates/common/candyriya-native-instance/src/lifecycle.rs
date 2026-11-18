//! Safe wrappers for the lifecycle exports and the per-instance token.
//!
//! `candyriya-native-instance` owns every typed cast into plugin code. The
//! init handshake already has one typed door ([`crate::initialize`]); the two
//! mandatory lifecycle callbacks — `candyriya_plugin_on_load` and
//! `candyriya_plugin_on_enable` — live behind equally pinned doors built on
//! the exact same machinery: pre-flight table validation, a probe by the
//! frozen export name, a single transmute at the one site that knows the
//! signature, and a by-value return both sides treat as the ack.
//!
//! Everything here takes the table **by shared reference**, so callers cannot
//! hand in a fabricated or dangling table pointer — the borrow rule keeps the
//! contract enforceable by the type system instead of by review comments.
//!
//! The unsafe opt-out stays scoped to this crate (and `candyriya-native-abi`);
//! the proxy's crates never see an `unsafe` block for any of this.

use core::ffi::{c_char, c_void};
use std::ffi::CString;

use candyriya_native_abi::{
    CandyriyaPluginApiV1, CandyriyaPluginHandle, CandyriyaStatus, PluginInitArgs,
};

use crate::export_names::{PLUGIN_ON_ENABLE_EXPORT, PLUGIN_ON_LOAD_EXPORT};
use crate::handshake::validate_handshake;
use crate::instance::LibraryLoader;
use crate::plugin_init::{InitError, InitializedPlugin};

/// The exact shape of a `fn(*const CandyriyaPluginApiV1) -> CandyriyaStatus`
/// export (`on_load`, `on_enable`).
type TableCallback = unsafe extern "C" fn(*const CandyriyaPluginApiV1) -> CandyriyaStatus;

/// The pre-flight check every lifecycle call shares, mirroring the one
/// [`crate::initialize`] performs before the plugin is asked anything.
fn checked_table(table: &CandyriyaPluginApiV1) -> Result<(), InitError> {
    // SAFETY: `table` is a live shared reference for the duration of the
    // call; the identical pre-flight `initialize` runs on the same table.
    unsafe {
        validate_handshake(
            table as *const CandyriyaPluginApiV1 as *const c_void,
            table.abi_version,
        )
    }?;
    Ok(())
}

/// Probes `name` on `library` and invokes the resolved export with `table`.
///
/// Shared by both lifecycle callbacks: the signature is identical, only the
/// frozen symbol name differs.
fn call_table_export(
    library: &LibraryLoader,
    name: &core::ffi::CStr,
    table: &CandyriyaPluginApiV1,
) -> Result<CandyriyaStatus, InitError> {
    // SAFETY: `name` is a static nul-terminated export name.
    let probe = unsafe { library.probe_export(name.as_ptr()) }?;
    // SAFETY: the symbol was resolved by its frozen name from a library the
    // caller vouched to be a V1 plugin; both lifecycle signatures are exactly
    // `fn(*const CandyriyaPluginApiV1) -> CandyriyaStatus`. `*probe` is the
    // symbol's address, which is what a `no_mangle` export denotes.
    let callback: TableCallback =
        unsafe { core::mem::transmute::<*const c_char, TableCallback>(*probe) };
    // SAFETY: `table` is a live shared reference valid for the call; the
    // plugin reads it and returns its answer by value, exactly like the
    // init contract. It never retains the pointer (V1 has nowhere to put
    // it).
    Ok(unsafe { callback(table as *const CandyriyaPluginApiV1) })
}

/// The per-instance opaque token the proxy hands to the plugin.
///
/// The ABI needs a token identifying one plugin instance. This is an opaque
/// box holding a single byte: unique, stable and never dereferenced anywhere.
/// (`Box<()>` would not allocate at all, and two tokens would dangle at the
/// same address — the byte is what makes the tokens distinct.) The plugin
/// receives a copy of the raw pointer (inside
/// [`CandyriyaPluginApiV1::instance`]) and only ever hands it back — the box
/// itself stays owned by this guard for the plugin's whole lifetime.
///
/// Dropping the guard reclaims the box; the table must not be used after
/// that (in V1 there is nothing left to call anyway).
#[derive(Debug)]
pub struct PluginToken {
    handle: CandyriyaPluginHandle,
    /// The allocation the handle denotes; kept purely for ownership (drop
    /// reclaims it), never read — that is the point of the guard.
    _owned: Box<u8>,
}

impl PluginToken {
    /// Mints a fresh token. Each plugin instance gets exactly one.
    #[must_use]
    pub fn mint() -> Self {
        let owned = Box::new(0_u8);
        // SAFETY: `owned` is a live heap allocation kept alive by `Self`;
        // the handle denotes exactly that allocation and is never
        // dereferenced by anyone.
        let handle =
            unsafe { CandyriyaPluginHandle::from_raw(&*owned as *const u8 as *mut c_void) };
        Self {
            handle,
            _owned: owned,
        }
    }

    /// The handle to build the V1 table around. The token lives next to the
    /// table in the owner's struct, so the allocation outlives the plugin's
    /// use of the table.
    #[must_use]
    pub fn handle(&self) -> CandyriyaPluginHandle {
        self.handle
    }
}

/// The table+token pair for one live native instance.
///
/// Bundled so the two travel together; the bundle is `Send + Sync` because:
///
/// - the table is `#[repr(C)]` plain data plus one raw pointer value, and is
///   never dereferenced through a shared reference;
/// - the token exclusively owns the allocation the table's raw handle
///   denotes; nothing casts it back, and it is dropped exactly once, with
///   the owning instance;
/// - shared references only ever read the POD bytes of the table — no
///   mutation exists to race with.
///
/// This is the one unsafe opt-out beyond the boundary crate's usual
/// invariants, and it is documented here so the audit covers it.
#[derive(Debug)]
pub struct TableSlot {
    table: CandyriyaPluginApiV1,
    _token: PluginToken,
}

// SAFETY: the bundle never dereferences its raw handle from any thread;
// the token's box is uniquely owned and dropped in place; the table is
// plain data. Sharing &TableSlot reads POD bytes only.
unsafe impl Send for TableSlot {}
// SAFETY: see above — no mutable state is reachable through &TableSlot.
unsafe impl Sync for TableSlot {}

impl TableSlot {
    /// Bundles the table and its token. The mint is per plugin instance;
    /// the slot is dropped with the owning instance, at unload.
    #[must_use]
    pub fn new(table: CandyriyaPluginApiV1, token: PluginToken) -> Self {
        Self {
            table,
            _token: token,
        }
    }

    /// The table the plugin saw at every lifecycle call. Valid for as long
    /// as the plugin instance is alive.
    #[must_use]
    pub fn table(&self) -> &CandyriyaPluginApiV1 {
        &self.table
    }
}

/// Whether `library` exports `name`.
///
/// A convenience for callers that must answer "does this plugin export X?"
/// without any unsafe code at the call site — the probe happens inside this
/// crate and a failed probe maps to `false`. A name containing an interior
/// nul reports `false` as well: such an export cannot exist through any C
/// lookup, so there is nothing to find.
#[must_use]
pub fn export_exists(library: &LibraryLoader, name: &str) -> bool {
    let Ok(cname) = CString::new(name) else {
        return false;
    };
    // SAFETY: `cname` is a live nul-terminated string for the duration of
    // the call.
    unsafe { library.probe_export(cname.as_ptr()) }.is_ok()
}

/// Runs the plugin's `candyriya_plugin_init` and validates the handshake.
///
/// The same flow as [`crate::initialize`] — pre-flight table validation,
/// probe of `candyriya_plugin_init`, call, echo/status/identity checks,
/// copy-out of the reported identity — with the table taken by shared
/// reference so the caller cannot fabricate the pointer. This is the entry
/// point non-unsafe crates call. A refusal in any step is reported as an
/// [`InitError`]; nothing about the plugin's identity is trusted on failure.
///
/// # Errors
///
/// - [`InitError::Loader`] when the init export cannot be probed;
/// - [`InitError::Handshake`] when the pre-flight or the plugin's answer is
///   refused.
pub fn init_plugin(
    library: &LibraryLoader,
    table: &CandyriyaPluginApiV1,
) -> Result<InitializedPlugin, InitError> {
    let args = PluginInitArgs::new(table);
    // SAFETY: `table` is a live shared reference; `args` borrows it for the
    // call and dies when this function returns. The plugin reads the table
    // by the exact contract it was built against.
    unsafe { crate::initialize(library, &args) }
}

/// Calls the plugin's `candyriya_plugin_on_load` with `table`.
///
/// `on_load` is a mandatory V1 export: a library without it is not a V1
/// plugin. The status is returned as answered — `CandyriyaStatus::Failed`
/// means the plugin refuses to run further and the caller must not enable it.
///
/// # Errors
///
/// Loader-class failures (export missing, probe failed) and handshake
/// refusals are reported as [`InitError`]; a plugin answering `Failed` is not
/// an `Err`, it is the plugin's answer and lives in the returned status.
pub fn on_load(
    library: &LibraryLoader,
    table: &CandyriyaPluginApiV1,
) -> Result<CandyriyaStatus, InitError> {
    checked_table(table)?;
    call_table_export(library, PLUGIN_ON_LOAD_EXPORT, table)
}

/// Calls the plugin's `candyriya_plugin_on_enable` with `table`.
///
/// Mandatory V1 export; same status semantics as [`on_load`].
///
/// # Errors
///
/// See [`on_load`].
pub fn on_enable(
    library: &LibraryLoader,
    table: &CandyriyaPluginApiV1,
) -> Result<CandyriyaStatus, InitError> {
    checked_table(table)?;
    call_table_export(library, PLUGIN_ON_ENABLE_EXPORT, table)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minting_two_tokens_gives_distinct_handles() {
        let token_a = PluginToken::mint();
        let token_b = PluginToken::mint();
        assert_ne!(token_a.handle().as_raw(), token_b.handle().as_raw());
    }

    #[test]
    fn init_args_point_at_the_shared_table() {
        let token = PluginToken::mint();
        let table = CandyriyaPluginApiV1::new(token.handle());
        let args = PluginInitArgs::new(&table);
        assert_eq!(args.abi_version, 1);
        assert_eq!(args.api as *const CandyriyaPluginApiV1, &table);
    }
}
