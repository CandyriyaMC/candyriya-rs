//! In-process checks for the exports — no dynamic loading, just the FFI
//! surface driven the way the proxy would drive it.

use candyriya_native_abi::{
    CANDYRIYA_PLUGIN_INIT_EXPORT, CANDYRIYA_PLUGIN_ON_ENABLE_EXPORT,
    CANDYRIYA_PLUGIN_ON_LOAD_EXPORT, CandyriyaPluginApiV1, CandyriyaPluginHandle, CandyriyaStatus,
    PluginInitArgs,
};
use greeting::{candyriya_plugin_init, candyriya_plugin_on_enable, candyriya_plugin_on_load};

/// Wraps a stack address as the instance token. Never dereferenced —
/// existence is all the ABI needs from it.
fn table(anchor: &usize) -> CandyriyaPluginApiV1 {
    let raw = anchor as *const usize as *mut core::ffi::c_void;
    // SAFETY: `raw` borrows the caller's live frame; used as a token only.
    let handle = unsafe { CandyriyaPluginHandle::from_raw(raw) };
    CandyriyaPluginApiV1::new(handle)
}

#[test]
fn the_three_exports_exist_and_are_named_as_the_abi_expects() {
    // symbol names are the contract; keep them honest even in-process
    assert_eq!(CANDYRIYA_PLUGIN_INIT_EXPORT, "candyriya_plugin_init");
    assert_eq!(CANDYRIYA_PLUGIN_ON_LOAD_EXPORT, "candyriya_plugin_on_load");
    assert_eq!(
        CANDYRIYA_PLUGIN_ON_ENABLE_EXPORT,
        "candyriya_plugin_on_enable"
    );
}

#[test]
fn happy_path_init_load_enable() {
    let anchor = 0usize;
    let api = table(&anchor);
    let args = PluginInitArgs::new(&api);

    // SAFETY: `api` and `args` live on this stack frame.
    let output = unsafe { candyriya_plugin_init(&args) };
    assert_eq!(output.status, CandyriyaStatus::Ok);
    assert_eq!(output.abi_version, 1);
    assert!(output.metadata.is_complete());
    // SAFETY: bufs point at static memory owned by the plugin.
    assert_eq!(unsafe { output.metadata.id.as_slice() }, b"greeting");
    assert_eq!(unsafe { output.metadata.version.as_slice() }, b"0.1.0");

    // SAFETY: same table, still alive.
    assert_eq!(
        unsafe { candyriya_plugin_on_load(&api) },
        CandyriyaStatus::Ok
    );
    assert_eq!(
        unsafe { candyriya_plugin_on_enable(&api) },
        CandyriyaStatus::Ok
    );
}

#[test]
fn init_fails_closed_on_garbage_input() {
    // null args
    // SAFETY: null is a legal input; the plugin must not trust it.
    let out = unsafe { candyriya_plugin_init(core::ptr::null()) };
    assert_eq!(out.status, CandyriyaStatus::Failed);
    assert!(!out.metadata.is_complete());

    // args from another ABI level
    let anchor = 0usize;
    let api = table(&anchor);
    let foreign_args = PluginInitArgs {
        abi_version: 2,
        ..PluginInitArgs::new(&api)
    };
    // SAFETY: `api` lives on this stack frame.
    let out = unsafe { candyriya_plugin_init(&foreign_args) };
    assert_eq!(out.status, CandyriyaStatus::Failed);

    // null table inside otherwise fine args
    let null_table_args = PluginInitArgs {
        abi_version: 1,
        api: core::ptr::null(),
    };
    // SAFETY: null is a legal input.
    let out = unsafe { candyriya_plugin_init(&null_table_args) };
    assert_eq!(out.status, CandyriyaStatus::Failed);
}

#[test]
fn entry_points_refuse_foreign_tables() {
    // SAFETY: null tables must be handled, not dereferenced.
    assert_eq!(
        unsafe { candyriya_plugin_on_load(core::ptr::null()) },
        CandyriyaStatus::Failed
    );
    assert_eq!(
        unsafe { candyriya_plugin_on_enable(core::ptr::null()) },
        CandyriyaStatus::Failed
    );

    // a table stamped for another ABI level
    let anchor = 0usize;
    let api = table(&anchor);
    let foreign = CandyriyaPluginApiV1 {
        abi_version: 2,
        ..api
    };
    // SAFETY: `foreign` lives on this frame; the plugin only reads it.
    assert_eq!(
        unsafe { candyriya_plugin_on_load(&foreign) },
        CandyriyaStatus::Failed
    );
    assert_eq!(
        unsafe { candyriya_plugin_on_enable(&foreign) },
        CandyriyaStatus::Failed
    );

    // a mis-sized table (as if built against a different V1 layout)
    let mis_sized = CandyriyaPluginApiV1 {
        size: api.size + 8,
        ..api
    };
    // SAFETY: `mis_sized` lives on this frame; the plugin only reads it.
    assert_eq!(
        unsafe { candyriya_plugin_on_load(&mis_sized) },
        CandyriyaStatus::Failed
    );
    assert_eq!(
        unsafe { candyriya_plugin_on_enable(&mis_sized) },
        CandyriyaStatus::Failed
    );
}
