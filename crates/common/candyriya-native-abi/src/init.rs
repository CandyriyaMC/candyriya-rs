//! [`PluginInitArgs`] / [`PluginInitOutput`]: the init handshake, and the
//! exact shape of the exports a native plugin must provide.

use crate::metadata::PluginMetadata;
use crate::status::CandyriyaStatus;
use crate::table::CandyriyaPluginApiV1;

/// The symbol the proxy resolves first:
/// `unsafe extern "C" fn candyriya_plugin_init(args: *const PluginInitArgs) -> PluginInitOutput`.
/// Missing from the library → the plugin is not loadable, period.
pub const CANDYRIYA_PLUGIN_INIT_EXPORT: &str = "candyriya_plugin_init";

/// The symbol the proxy calls after a successful init, while handing the
/// plugin its (read-only) environment:
/// `unsafe extern "C" fn candyriya_plugin_on_load(api: *const CandyriyaPluginApiV1) -> CandyriyaStatus`.
pub const CANDYRIYA_PLUGIN_ON_LOAD_EXPORT: &str = "candyriya_plugin_on_load";

/// The symbol the proxy calls once the plugin is fully wired:
/// `unsafe extern "C" fn candyriya_plugin_on_enable(api: *const CandyriyaPluginApiV1) -> CandyriyaStatus`.
pub const CANDYRIYA_PLUGIN_ON_ENABLE_EXPORT: &str = "candyriya_plugin_on_enable";

/// The signature of the [`CANDYRIYA_PLUGIN_INIT_EXPORT`] export.
pub type CandyriyaPluginInitFn = unsafe extern "C" fn(*const PluginInitArgs) -> PluginInitOutput;

/// The signature of the [`CANDYRIYA_PLUGIN_ON_LOAD_EXPORT`] export.
pub type CandyriyaPluginOnLoadFn =
    unsafe extern "C" fn(*const CandyriyaPluginApiV1) -> CandyriyaStatus;

/// The signature of the [`CANDYRIYA_PLUGIN_ON_ENABLE_EXPORT`] export.
pub type CandyriyaPluginOnEnableFn =
    unsafe extern "C" fn(*const CandyriyaPluginApiV1) -> CandyriyaStatus;

/// What the proxy passes to the plugin's `candyriya_plugin_init`.
///
/// `abi_version` is the proxy's ABI level (always
/// [`crate::CANDYRIYA_NATIVE_ABI_VERSION`] today) so the plugin can refuse a
/// host built against a different contract before reading anything else.
/// `api` is a pointer to the proxy-owned [`CandyriyaPluginApiV1`]; valid for
/// the duration of the call — plugins must clone what they keep (in V1:
/// nothing, there is nothing to keep yet) and must never free the pointer.
///
/// The struct is `#[repr(C)]` and will only ever grow *minor* helper types
/// in future versions — see the crate README for the versioning rules.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct PluginInitArgs {
    /// The ABI level the proxy speaks. `1` for V1.
    pub abi_version: u32,
    /// The proxy's API table; null is a contract violation.
    pub api: *const CandyriyaPluginApiV1,
}

impl PluginInitArgs {
    /// Builds args for the given table. Proxy-side helper.
    #[must_use]
    pub fn new(api: &CandyriyaPluginApiV1) -> Self {
        Self {
            abi_version: crate::CANDYRIYA_NATIVE_ABI_VERSION,
            api: api as *const CandyriyaPluginApiV1,
        }
    }

    /// Checks the args carry a genuine V1 table this crate can reason about:
    /// an [`crate::CANDYRIYA_NATIVE_ABI_VERSION`] echo and a non-null, valid V1 table.
    ///
    /// Plugins call this first and bail out with
    /// [`PluginInitOutput::failed`] on `false`.
    ///
    /// # Safety
    ///
    /// `api` must be null or point at a [`CandyriyaPluginApiV1`] that is
    /// valid for reads for the duration of this call. This is the one
    /// dereference a plugin makes before it trusts anything else.
    #[must_use]
    pub unsafe fn is_valid(&self) -> bool {
        if self.abi_version != crate::CANDYRIYA_NATIVE_ABI_VERSION {
            return false;
        }
        // SAFETY: the caller upholds the `api` pointer contract above.
        match unsafe { self.api.as_ref() } {
            Some(table) => table.is_valid(),
            None => false,
        }
    }
}

/// What the plugin returns from `candyriya_plugin_init`.
///
/// `status` is authoritative: [`CandyriyaStatus::Failed`] means the proxy
/// must not call any other export. `abi_version` echoes the plugin's ABI
/// level so the proxy can double-check the handshake. `metadata` is filled
/// either way — plugins that fail early return [`PluginMetadata::EMPTY`],
/// never uninitialised memory.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PluginInitOutput {
    /// Whether init succeeded. Unknown values are treated as failure.
    pub status: CandyriyaStatus,
    /// The ABI level the plugin was built against. `1` for V1.
    pub abi_version: u32,
    /// Who the plugin is; [`PluginMetadata::EMPTY`] on failure.
    pub metadata: PluginMetadata,
}

impl PluginInitOutput {
    /// A successful output carrying `metadata`.
    #[must_use]
    pub fn ok(metadata: PluginMetadata) -> Self {
        Self {
            status: CandyriyaStatus::Ok,
            abi_version: crate::CANDYRIYA_NATIVE_ABI_VERSION,
            metadata,
        }
    }

    /// A failed output: empty metadata, no plugin to load.
    #[must_use]
    pub fn failed() -> Self {
        Self {
            status: CandyriyaStatus::Failed,
            abi_version: crate::CANDYRIYA_NATIVE_ABI_VERSION,
            metadata: PluginMetadata::EMPTY,
        }
    }
}
