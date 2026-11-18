//! The frozen export names of the native plugin ABI.
//!
//! These are the exact symbol strings the loader looks up. The first three
//! are the mandatory phase-1 exports, defined by the ABI contract crate — the
//! unit tests below assert they stay in lockstep with it. The last two are
//! **optional, probed** exports: a library without them is still accepted,
//! and callers must treat a failed probe as "this plugin has no callback"
//! rather than as an error. V1 exports no disable/unload entry points, so
//! these symbols become contractual only in a future ABI version.
//!
//! Probe with [`crate::LibraryLoader::probe_export`]; `c"…"` literals hand you
//! a `&CStr` whose `as_ptr` is what the probe wants.

use core::ffi::CStr;

/// `candyriya_plugin_init` — mandatory.
///
/// Resolved first:
/// `unsafe extern "C" fn candyriya_plugin_init(*const PluginInitArgs) -> PluginInitOutput`.
/// A library without this export is not loadable.
pub const PLUGIN_INIT_EXPORT: &CStr = c"candyriya_plugin_init";

/// `candyriya_plugin_on_load` — mandatory.
///
/// Called after a successful init:
/// `unsafe extern "C" fn candyriya_plugin_on_load(*const CandyriyaPluginApiV1) -> CandyriyaStatus`.
pub const PLUGIN_ON_LOAD_EXPORT: &CStr = c"candyriya_plugin_on_load";

/// `candyriya_plugin_on_enable` — mandatory.
///
/// Called once the plugin is fully wired:
/// `unsafe extern "C" fn candyriya_plugin_on_enable(*const CandyriyaPluginApiV1) -> CandyriyaStatus`.
pub const PLUGIN_ON_ENABLE_EXPORT: &CStr = c"candyriya_plugin_on_enable";

/// `candyriya_plugin_on_disable` — optional, probed.
///
/// Absent on V1 libraries: probing fails with
/// [`crate::LoaderError::SymbolNotFound`] and the caller must not call
/// anything for disable — the disable path is a cooperative drain and
/// unload with no plugin code running.
pub const PLUGIN_ON_DISABLE_EXPORT: &CStr = c"candyriya_plugin_on_disable";

/// `candyriya_plugin_on_unload` — optional, probed.
///
/// Absent on V1 libraries: probing fails with
/// [`crate::LoaderError::SymbolNotFound`] and the caller must not call
/// anything for unload — teardown is mailbox close, join and `dlclose` only.
pub const PLUGIN_ON_UNLOAD_EXPORT: &CStr = c"candyriya_plugin_on_unload";

#[cfg(test)]
mod tests {
    use super::*;

    /// The loader's names for the three mandatory exports must be exactly
    /// the names the ABI contract defines — a drift here means the proxy
    /// loads a library and then looks for symbols that are not the contract.
    #[test]
    fn mandatory_export_names_match_the_abi_contract() {
        assert_eq!(
            PLUGIN_INIT_EXPORT.to_bytes(),
            candyriya_native_abi::CANDYRIYA_PLUGIN_INIT_EXPORT.as_bytes()
        );
        assert_eq!(
            PLUGIN_ON_LOAD_EXPORT.to_bytes(),
            candyriya_native_abi::CANDYRIYA_PLUGIN_ON_LOAD_EXPORT.as_bytes()
        );
        assert_eq!(
            PLUGIN_ON_ENABLE_EXPORT.to_bytes(),
            candyriya_native_abi::CANDYRIYA_PLUGIN_ON_ENABLE_EXPORT.as_bytes()
        );
    }

    /// Every name is nul-terminated and free of interior nuls — the probe
    /// hands these pointers straight to `dlopen`/`LoadLibraryW` lookups.
    #[test]
    fn export_names_are_well_formed_cstrings() {
        for name in [
            PLUGIN_INIT_EXPORT,
            PLUGIN_ON_LOAD_EXPORT,
            PLUGIN_ON_ENABLE_EXPORT,
            PLUGIN_ON_DISABLE_EXPORT,
            PLUGIN_ON_UNLOAD_EXPORT,
        ] {
            assert_eq!(name.to_bytes_with_nul().last(), Some(&0));
            assert!(!name.to_bytes().contains(&0));
        }
    }
}
