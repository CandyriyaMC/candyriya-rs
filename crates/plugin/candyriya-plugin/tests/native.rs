//! End-to-end smoke tests for the native plugin path.
//!
//! Unlike the unit tests, these dlopen the real `greeting` cdylib through
//! `PluginInstance::native`, so they are `#[ignore]`d by default:
//!
//! ```sh
//! cargo build --release -p greeting
//! cargo test -p candyriya-plugin --test native -- --ignored
//! ```
//!
//! They are the phase-2 proof that the native path is wired, not a fake:
//! the library really maps, `candyriya_plugin_init` really answers Ok, the
//! lifecycle really walks `on_load` → `on_enable`, and the supervisor-facing
//! registry really records the launch and the teardown. A failure here is a
//! bug in the boundary, never something to waive away — if this file has to
//! grow a `#[ignore]` beyond these cdylib requirements, that is a red flag,
//! not a convenience.

use std::path::PathBuf;
use std::sync::Arc;

use candyriya_plugin::{
    HostState, Limits, NativeLoadError, PluginDescriptor, PluginFormat, PluginInstance,
    PluginRegistry, PluginWireState,
};

/// Locates the reference cdylib relative to this crate's manifest dir.
///
/// Workspace layout: `crates/plugin/candyriya-plugin` → the workspace target
/// directory is three levels up. Release is preferred (that is what the CI
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

fn native_host_state(dir: &tempfile::TempDir, id: &str) -> HostState {
    HostState::with_instance_slots(
        dir.path().to_path_buf(),
        Arc::new(candyriya_plugin::InstanceSlots::new()),
        id,
    )
}

/// A cdylib-name for a bogus plugin file, per platform; the contents are
/// never a loadable library, which is the whole point of the test.
fn bogus_library_name() -> &'static str {
    if cfg!(windows) {
        "fake-plugin.dll"
    } else if cfg!(target_os = "macos") {
        "libfake_plugin.dylib"
    } else {
        "libfake_plugin.so"
    }
}

/// Candidate system libraries that the OS loader maps fine but which know
/// nothing about candyriya. The first one whose bring-up reaches the init
/// handshake is the one the test asserts against; candidates are tried in
/// order, and if none maps at all the test fails loudly naming the platform
/// assumption, never silently green.
fn system_non_plugin_library_candidates() -> &'static [&'static str] {
    if cfg!(windows) {
        &[r"C:\Windows\System32\kernel32.dll"]
    } else if cfg!(target_os = "macos") {
        &["libSystem.B.dylib"]
    } else {
        &[
            "libc.so.6",
            "libc.musl-x86_64.so.1",
            "libc.musl-aarch64.so.1",
            "libc.so",
        ]
    }
}

/// The full happy path, end to end: dlopen the reference cdylib, run the
/// init handshake, `on_load`, `on_enable`, serve commands on the dedicated
/// thread, unload cleanly — and watch the registry walk the whole ladder.
#[test]
#[ignore = "requires the greeting cdylib: cargo build --release -p greeting"]
fn greeting_brings_up_through_the_native_path() {
    let path = greeting_cdylib();
    let dir = tempfile::tempdir().expect("a temp data dir");
    let registry = Arc::new(PluginRegistry::new());
    let descriptor = PluginDescriptor::native("greeting", "Greeting", "0.1.0", path.clone());

    let instance = PluginInstance::native(
        descriptor,
        native_host_state(&dir, "greeting"),
        "greeting",
        &Limits::default(),
        Arc::clone(&registry),
    )
    .expect("the reference cdylib must bring up cleanly");

    // the unifying descriptor: format + the ABI echo from the handshake
    assert_eq!(
        instance.descriptor().wasm_or_native_format,
        PluginFormat::Native
    );
    assert_eq!(instance.descriptor().abi_version, Some(1));
    assert_eq!(instance.descriptor().id, "greeting");
    assert_eq!(instance.descriptor().path, path);

    // the table the plugin validated at every callback is a genuine V1
    // table built from this instance's own token
    let table = instance
        .native_table()
        .expect("a native instance carries its table");
    assert!(table.is_valid());
    assert_eq!(table.abi_version, 1);
    assert_eq!(
        table.size as usize,
        std::mem::size_of::<candyriya_native_abi::CandyriyaPluginApiV1>()
    );

    // the registry walked DISCOVERED -> ... -> RUNNING, nothing skipped
    let record = registry.get("greeting").expect("the record is registered");
    assert_eq!(record.state(), PluginWireState::Running);
    assert_eq!(record.restart_count, 0);
    assert_eq!(record.reason, None);

    // the dedicated thread really serves commands
    assert_eq!(
        instance.worker_thread_name().expect("worker answers"),
        "candyriya-plugin-greeting"
    );
    assert!(
        instance
            .function_exists("candyriya_plugin_on_enable")
            .unwrap()
    );
    assert!(
        !instance
            .function_exists("candyriya_plugin_definitely_missing")
            .unwrap()
    );

    // arbitrary export calls do not exist on the v1 surface; refused, not faked
    let refused = instance
        .call("on_event", b"{}")
        .expect_err("native instances must refuse arbitrary export calls");
    assert!(matches!(
        refused,
        candyriya_plugin::PluginError::CallFailed { .. }
    ));

    instance.unload().expect("a clean unload joins the worker");
    let record = registry.get("greeting").expect("the record is kept for ps");
    assert_eq!(record.state(), PluginWireState::Unloaded);
    assert_eq!(record.reason, None);
}

/// A file that is not a dynamic library must be refused at the dlopen step:
/// log-worthy failure, record terminally `UNLOADED` with the reason, no
/// thread, nothing in the running set.
#[test]
fn dlopen_failure_never_enters_the_tree() {
    let dir = tempfile::tempdir().expect("a temp data dir");
    let bogus = dir.path().join(bogus_library_name());
    std::fs::write(&bogus, b"this is not a dynamic library").expect("write the bogus file");
    let registry = Arc::new(PluginRegistry::new());
    let descriptor = PluginDescriptor::native("bogus", "Bogus", "0.1.0", bogus);

    let error = match PluginInstance::native(
        descriptor,
        native_host_state(&dir, "bogus"),
        "bogus",
        &Limits::default(),
        Arc::clone(&registry),
    ) {
        Ok(_) => panic!("a text file cannot be mapped as a plugin"),
        Err(error) => error,
    };
    assert!(matches!(error, NativeLoadError::Open { .. }));

    let record = registry
        .get("bogus")
        .expect("the failed record stays visible");
    assert_eq!(record.state(), PluginWireState::Unloaded);
    let reason = record.reason.as_deref().unwrap_or_default();
    assert!(
        reason.contains("failed to load"),
        "the reason names the dlopen failure, got `{reason}`"
    );
}

/// A library that maps fine but knows nothing about candyriya must be
/// refused at the init handshake: terminal `UNLOADED`, reason preserved,
/// never `RUNNING`.
#[test]
fn non_plugin_library_is_refused_at_the_handshake() {
    let mut open_errors = Vec::new();
    for candidate in system_non_plugin_library_candidates() {
        let dir = tempfile::tempdir().expect("a temp data dir");
        let registry = Arc::new(PluginRegistry::new());
        let descriptor =
            PluginDescriptor::native("syslib", "Syslib", "0.1.0", PathBuf::from(*candidate));
        match PluginInstance::native(
            descriptor,
            native_host_state(&dir, "syslib"),
            "syslib",
            &Limits::default(),
            Arc::clone(&registry),
        ) {
            Err(NativeLoadError::Init { message, .. }) => {
                assert!(
                    message.contains("candyriya_plugin_init"),
                    "the reason names the missing export, got `{message}`"
                );
                let record = registry
                    .get("syslib")
                    .expect("the failed record stays visible");
                assert_eq!(record.state(), PluginWireState::Unloaded);
                assert!(
                    record.reason.is_some(),
                    "the handshake failure reason is preserved"
                );
                return;
            }
            Err(other) => open_errors.push(format!("{candidate}: {other}")),
            Ok(_) => panic!(
                "the system library `{candidate}` was brought up as a candyriya plugin; that is not possible through the real bring-up path"
            ),
        }
    }
    panic!(
        "no candidate system library mapped on this platform; open errors: {}",
        open_errors.join("; ")
    );
}

/// A duplicate id in the same registry is refused before the library is
/// even mapped — the live plugin's record stays `RUNNING`.
#[test]
#[ignore = "requires the greeting cdylib: cargo build --release -p greeting"]
fn duplicate_native_registration_is_rejected() {
    let path = greeting_cdylib();
    let dir = tempfile::tempdir().expect("a temp data dir");
    let registry = Arc::new(PluginRegistry::new());
    let descriptor = PluginDescriptor::native("greeting", "Greeting", "0.1.0", path.clone());

    let instance = PluginInstance::native(
        descriptor,
        native_host_state(&dir, "greeting"),
        "greeting",
        &Limits::default(),
        Arc::clone(&registry),
    )
    .expect("the first plugin brings up cleanly");

    let second = match PluginInstance::native(
        PluginDescriptor::native("greeting", "Greeting", "0.1.0", path),
        native_host_state(&dir, "greeting"),
        "greeting",
        &Limits::default(),
        Arc::clone(&registry),
    ) {
        Ok(_) => panic!("the duplicate must be refused by the registry"),
        Err(error) => error,
    };
    assert!(matches!(second, NativeLoadError::Registry { .. }));

    // the live plugin is untouched by the refused duplicate
    let record = registry.get("greeting").expect("the record is there");
    assert_eq!(record.state(), PluginWireState::Running);

    instance.unload().expect("a clean unload");
}

/// Dropping the instance (instead of `unload`) walks the record down to
/// `UNLOADED` too — the drop path can never strand the registry.
#[test]
#[ignore = "requires the greeting cdylib: cargo build --release -p greeting"]
fn dropping_the_instance_retires_the_record() {
    let path = greeting_cdylib();
    let dir = tempfile::tempdir().expect("a temp data dir");
    let registry = Arc::new(PluginRegistry::new());
    let descriptor = PluginDescriptor::native("greeting", "Greeting", "0.1.0", path);

    {
        let _instance = PluginInstance::native(
            descriptor,
            native_host_state(&dir, "greeting"),
            "greeting",
            &Limits::default(),
            Arc::clone(&registry),
        )
        .expect("the reference cdylib must bring up cleanly");
        assert_eq!(
            registry
                .get("greeting")
                .expect("the record is there")
                .state(),
            PluginWireState::Running
        );
        // dropped here without unload(): the worker detaches, the record walks down
    }

    let record = registry.get("greeting").expect("the record is there");
    assert_eq!(record.state(), PluginWireState::Unloaded);
    assert_eq!(record.reason, None);
}
