//! Contract tests for the native plugin ABI, written from the consumer's side:
//! only the public API, only behavior the layout promises.

use candyriya_native_abi::{
    CANDYRIYA_NATIVE_ABI_VERSION, CANDYRIYA_NATIVE_ABI_VERSION_V1, CandyriyaBuf,
    CandyriyaPluginApiV1, CandyriyaPluginHandle, CandyriyaStatus, PluginInitArgs, PluginInitOutput,
    PluginMetadata,
};

#[test]
fn abi_version_is_one() {
    assert_eq!(CANDYRIYA_NATIVE_ABI_VERSION, 1);
    assert_eq!(CANDYRIYA_NATIVE_ABI_VERSION_V1, 1);
    // the public alias and the named constant must never drift apart
    assert_eq!(
        CANDYRIYA_NATIVE_ABI_VERSION,
        CANDYRIYA_NATIVE_ABI_VERSION_V1
    );
}

#[test]
fn status_discriminants_are_stable() {
    // the FFI contract is the number; if this ever trips, every plugin out
    // there is broken in exactly the way this test exists to prevent
    assert_eq!(CandyriyaStatus::Ok as u32, 0);
    assert_eq!(CandyriyaStatus::Failed as u32, 1);
    assert!(CandyriyaStatus::Ok.is_ok());
    assert!(!CandyriyaStatus::Failed.is_ok());
}

#[test]
fn buf_roundtrip() {
    let buf = CandyriyaBuf::new(b"native");
    assert_eq!(buf.len, 6);
    assert!(!buf.is_empty());
    // SAFETY: `buf` was built from a live slice that outlives this read.
    let bytes = unsafe { buf.as_slice() };
    assert_eq!(bytes, b"native");

    let empty = CandyriyaBuf::EMPTY;
    assert!(empty.is_empty());
    // SAFETY: the empty buffer is null with len 0 by definition.
    assert_eq!(unsafe { empty.as_slice() }, &[]);
}

#[test]
fn buf_display_is_lossy_but_safe() {
    let buf = CandyriyaBuf::new(&[0xff, 0xfe, b'a']);
    // never panics, never validates; purely diagnostics
    let _ = format!("{buf}");
}

#[test]
fn layout_sizes_do_not_regress() {
    // pointer-width-aware, so these pass on 32- and 64-bit targets
    assert_eq!(
        size_of::<CandyriyaBuf>(),
        2 * size_of::<*const u8>(),
        "CandyriyaBuf must stay (ptr, u32)"
    );
    assert_eq!(
        size_of::<CandyriyaPluginApiV1>(),
        2 * size_of::<u32>() + size_of::<*mut core::ffi::c_void>(),
        "V1 table must stay two u32s and a pointer"
    );
    assert_eq!(
        size_of::<PluginMetadata>(),
        4 * size_of::<CandyriyaBuf>(),
        "metadata must stay four buffers"
    );
    assert_eq!(
        size_of::<PluginInitOutput>(),
        2 * size_of::<u32>() + size_of::<PluginMetadata>(),
        "init output must stay status + abi_version + metadata"
    );
    assert!(
        core::mem::align_of::<CandyriyaBuf>() >= core::mem::align_of::<u32>(),
        "buffers are read as u32-aligned data at worst"
    );
}

#[test]
fn metadata_completeness_rules() {
    let empty = PluginMetadata::EMPTY;
    assert!(!empty.is_complete());

    let full = PluginMetadata::new(b"greeting", b"0.1.0", b"Greeting", b"says hi");
    assert!(full.is_complete());
    // SAFETY: bufs were built from live slices that outlive this read.
    assert_eq!(unsafe { full.id.as_slice() }, b"greeting");
    assert_eq!(unsafe { full.version.as_slice() }, b"0.1.0");
    assert_eq!(unsafe { full.name.as_slice() }, b"Greeting");
    assert_eq!(unsafe { full.description.as_slice() }, b"says hi");

    // description is allowed to be absent
    let no_desc = PluginMetadata::new(b"greeting", b"0.1.0", b"Greeting", b"");
    assert!(no_desc.is_complete());
    assert!(no_desc.description.is_empty());
}

#[test]
fn plugin_handle_debug_hides_the_pointer() {
    // an address that is never dereferenced, only wrapped
    let handle =
        unsafe { CandyriyaPluginHandle::from_raw(0xdead_beefusize as *mut core::ffi::c_void) };
    let text = format!("{handle:?}");
    assert!(
        !text.contains("0x"),
        "address must not leak into logs: {text}"
    );
}

#[test]
fn v1_table_is_valid_only_when_version_and_size_match() {
    let anchor = 0usize;
    let raw = &anchor as *const usize as *mut core::ffi::c_void;
    let handle = unsafe { CandyriyaPluginHandle::from_raw(raw) };
    let table = CandyriyaPluginApiV1::new(handle);
    assert!(table.is_valid());
    assert_eq!(table.abi_version, 1);
    assert_eq!(table.size as usize, size_of::<CandyriyaPluginApiV1>());

    let wrong_version = CandyriyaPluginApiV1 {
        abi_version: 2,
        ..table
    };
    assert!(!wrong_version.is_valid());

    let wrong_size = CandyriyaPluginApiV1 {
        size: table.size + 4,
        ..table
    };
    assert!(!wrong_size.is_valid());
}

#[test]
fn init_args_validate_the_handshake() {
    let anchor = 0usize;
    let raw = &anchor as *const usize as *mut core::ffi::c_void;
    let handle = unsafe { CandyriyaPluginHandle::from_raw(raw) };
    let table = CandyriyaPluginApiV1::new(handle);
    let args = PluginInitArgs::new(&table);

    // SAFETY: `table` lives on this stack frame; the pointer is valid here.
    assert!(unsafe { args.is_valid() });

    // wrong ABI echo on the args side
    let wrong_abi = PluginInitArgs {
        abi_version: 2,
        ..args
    };
    // SAFETY: `table` outlives this call.
    assert!(!unsafe { wrong_abi.is_valid() });

    // null table
    let no_table = PluginInitArgs {
        abi_version: 1,
        api: core::ptr::null(),
    };
    // SAFETY: no dereference of null happens; `is_valid` checks first.
    assert!(!unsafe { no_table.is_valid() });

    // a table built for another ABI level
    let bad_table = CandyriyaPluginApiV1 {
        abi_version: 9,
        ..table
    };
    let weird = PluginInitArgs {
        abi_version: 1,
        api: &bad_table as *const CandyriyaPluginApiV1,
    };
    // SAFETY: `bad_table` lives on this stack frame; the pointer is valid.
    assert!(!unsafe { weird.is_valid() });
}

#[test]
fn full_v1_handshake_simulation() {
    // what a conforming plugin does at init, driven from the public API —
    // no dynamic loading, no host, just the contract end to end
    let anchor = 0usize;
    let raw = &anchor as *const usize as *mut core::ffi::c_void;
    let handle = unsafe { CandyriyaPluginHandle::from_raw(raw) };
    let table = CandyriyaPluginApiV1::new(handle);
    let args = PluginInitArgs::new(&table);

    // SAFETY: everything involved lives on this stack frame.
    let output = if unsafe { args.is_valid() } {
        PluginInitOutput::ok(PluginMetadata::new(
            b"greeting",
            b"0.1.0",
            b"Greeting",
            b"reference plugin",
        ))
    } else {
        PluginInitOutput::failed()
    };

    assert_eq!(output.status, CandyriyaStatus::Ok);
    assert_eq!(output.abi_version, CANDYRIYA_NATIVE_ABI_VERSION);
    assert!(output.metadata.is_complete());
    // SAFETY: bufs were built from live `&[u8]`s that outlive this read.
    assert_eq!(unsafe { output.metadata.id.as_slice() }, b"greeting");

    let failed = PluginInitOutput::failed();
    assert_eq!(failed.status, CandyriyaStatus::Failed);
    assert!(!failed.metadata.is_complete());
}
