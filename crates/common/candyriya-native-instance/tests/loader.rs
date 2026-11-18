//! Integration tests that dlopen the real reference plugin cdylib.
//!
//! These need the `greeting` cdylib to exist on disk first:
//!
//! ```sh
//! cargo build --release -p greeting
//! cargo test -p candyriya-native-instance -- --ignored
//! ```
//!
//! They are `#[ignore]`d because the cdylib is a build artifact, not something
//! `cargo test` builds — running them without the artifact fails honestly
//! with a message naming the exact command to run first. CI's native-abi job
//! builds the artifact and can drive these directly.

use std::path::PathBuf;

use candyriya_native_instance::{LibraryLoader, LoaderError, OpenFlags, PLUGIN_INIT_EXPORT};

/// Locates the reference cdylib relative to this crate's manifest dir.
///
/// Workspace layout: `crates/common/candyriya-native-instance` → the workspace
/// target directory is three levels up, and `greeting` is
/// `crates/plugins/greeting`. Release is preferred (that is what the CI
/// native-abi job builds); debug is accepted as a fallback.
fn greeting_cdylib() -> PathBuf {
    let artifact = if cfg!(windows) {
        "greeting.dll"
    } else if cfg!(target_os = "macos") {
        "libgreeting.dylib"
    } else {
        "libgreeting.so"
    };
    let manifest = env!("CARGO_MANIFEST_DIR");
    let target_dir = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(manifest).join("../../..").join("target"));
    for profile in ["release", "debug"] {
        let candidate = target_dir.join(profile).join(artifact);
        if candidate.exists() {
            return candidate;
        }
    }
    panic!(
        "reference cdylib `{artifact}` not found under {}; build it first with `cargo build --release -p greeting`",
        target_dir.display()
    );
}

/// Opens the reference cdylib, checks the reported path, and unloads it.
#[test]
#[ignore = "requires the greeting cdylib: cargo build --release -p greeting"]
fn opens_the_greeting_cdylib_and_closes_it() {
    let path = greeting_cdylib();
    let library = LibraryLoader::open(&path, OpenFlags::Now)
        .expect("the reference cdylib should load with RTLD_NOW/LoadLibraryW");
    assert_eq!(library.path(), path.as_path());
    library.close().expect("the mapping should unload cleanly");
}

/// `PLUGIN_INIT_EXPORT` resolves to a usable, non-null symbol; a bogus name
/// fails cleanly with `SymbolNotFound` instead of UB or a panic.
#[test]
#[ignore = "requires the greeting cdylib: cargo build --release -p greeting"]
fn probes_plugin_init_export() {
    let path = greeting_cdylib();
    let library =
        LibraryLoader::open(&path, OpenFlags::Now).expect("the reference cdylib should load");

    // SAFETY: `PLUGIN_INIT_EXPORT` is a static nul-terminated name.
    let symbol = unsafe { library.probe_export(PLUGIN_INIT_EXPORT.as_ptr()) }
        .expect("the reference cdylib must export candyriya_plugin_init");
    // SAFETY: consumes the probe and yields the raw symbol address; reading
    // it as a pointer value is what the probe returns. Null is the only
    // forbidden value, and a successful dlsym/GetProcAddress never is.
    let address = unsafe { symbol.try_as_raw_ptr() }
        .expect("resolved symbols are non-null on both platforms");
    assert!(!address.is_null());

    // a mismatched name must fail cleanly: typed error, no UB, no panic
    let bogus = c"candyriya_plugin_this_symbol_does_not_exist";
    // SAFETY: `bogus` is a static nul-terminated name.
    let error = unsafe { library.probe_export(bogus.as_ptr()) }
        .expect_err("a bogus symbol name must not resolve");
    assert!(matches!(error, LoaderError::SymbolNotFound { .. }));
}

/// The end-to-end handshake against the real cdylib: open, call
/// `candyriya_plugin_init`, and verify status, ABI echo and the copied
/// identity. This is the exact flow the controller will drive later.
#[test]
#[ignore = "requires the greeting cdylib: cargo build --release -p greeting"]
fn initializes_the_greeting_plugin() {
    use candyriya_native_abi::{CandyriyaPluginApiV1, CandyriyaPluginHandle, PluginInitArgs};
    use candyriya_native_instance::initialize;

    let path = greeting_cdylib();
    let library =
        LibraryLoader::open(&path, OpenFlags::Now).expect("the reference cdylib should load");

    // SAFETY: the proxy mints one token per plugin instance; here the token
    // is an opaque allocation the plugin never dereferences.
    let handle = unsafe {
        CandyriyaPluginHandle::from_raw(Box::into_raw(Box::new(())) as *mut core::ffi::c_void)
    };
    let table = CandyriyaPluginApiV1::new(handle);
    let args = PluginInitArgs::new(&table);

    // SAFETY: the cdylib is the V1 reference plugin; calling its init export
    // with these args is exactly what the ABI contract defines.
    let plugin = unsafe { initialize(&library, &args) }.expect("the handshake should succeed");
    assert_eq!(plugin.abi_version, 1);
    assert_eq!(plugin.identity.id, b"greeting");
    assert_eq!(plugin.identity.name, b"Greeting");
    assert!(!plugin.identity.version.is_empty());
}
