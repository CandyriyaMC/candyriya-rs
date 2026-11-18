//! Native-ABI smoke: build the reference `greeting` plugin cdylib on the
//! spot, dlopen it through `candyriya-native-instance` (the phase-2 dlopen
//! boundary) and drive the V1 lifecycle - the init handshake, `on_load`,
//! `on_enable` - the same way the plugin controller brings the plugin up.
//!
//! Unlike the loopback fixtures this touches nothing on the network; it
//! exercises the OS dynamic loader and maps plugin code into this process.
//! It is deliberately self-contained: the cdylib is built with
//! `cargo build --release` invoked with paths relative to this crate (never
//! the workspace root) and located at `crates/plugins/greeting/target/release/`.
//!
//! On the platform artifact name: `greeting.dll` on Windows (`crate-type`
//! cdylib + no `lib` prefix, matching what the CI native-abi job ships),
//! `libgreeting.dylib` on macOS, `libgreeting.so` everywhere else (every
//! non-Apple unix we care about). The cfg lives in one small function and
//! everything else is shared.
//!
//! # Print capture
//!
//! `on_enable` in the greeting plugin prints one line to stdout. Capturing
//! it from inside the same process is not possible on stable:
//! `std::io::set_output_capture` is nightly-only (`internal_output_capture`)
//! and libtest's per-thread capture buffer is not readable from a test. So
//! the test re-runs itself as a child process with `--no-capture` under
//! `NATIVE_SMOKE_CHILD=1` and reads the child's stdout pipe. The child runs
//! only the FFI flow; the parent asserts both the `CandyriyaStatus::Ok`
//! and the presence of the exact hello line. With `--nocapture` locally the
//! line also shows up straight in the terminal.
//!
//! # No unsafe here, none at all
//!
//! Everything past this comment goes through the safe doors
//! `candyriya-native-instance` ships for non-unsafe callers (`init_plugin`,
//! `on_load`, `on_enable`, `export_exists`, `PluginToken`/`TableSlot`) - the
//! typed casts live in that crate and this test only carries its types.
//! `init_plugin` *is* `candyriya_native_instance::initialize` through the
//! loader: it builds the same `PluginInitArgs` from the same table and runs
//! the same fail-closed handshake with the unsafe inside the crate. The
//! crate's raw `initialize` stays available for raw call sites; this one
//! does not need it.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use candyriya_native_abi::{CandyriyaPluginApiV1, CandyriyaStatus};
use candyriya_native_instance::{
    LibraryLoader, OpenFlags, PLUGIN_INIT_EXPORT, PLUGIN_ON_ENABLE_EXPORT, PLUGIN_ON_LOAD_EXPORT,
    PluginToken, TableSlot, export_exists, init_plugin, on_enable, on_load,
};

/// Set in the child process; see the module docs for why the capture dance
/// exists and why it is not possible in-process.
const CHILD_ENV: &str = "NATIVE_SMOKE_CHILD";

/// The exact line the greeting plugin prints from `on_enable`.
const HELLO: &str = "greeting: hello from a native candyriya plugin (ABI v1)";

/// This crate's manifest dir - every path below is anchored relative to the
/// test crate, not the workspace root.
fn crate_dir() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

/// `crates/plugins/greeting/Cargo.toml`, spelled relative to this crate.
fn greeting_manifest() -> PathBuf {
    crate_dir().join("../../plugins/greeting/Cargo.toml")
}

/// The greeting plugin gets its own `target/` under `crates/plugins/greeting/`;
/// `target/` is gitignored at every level, so this never dirties the repo.
fn greeting_target_dir() -> PathBuf {
    crate_dir().join("../../plugins/greeting/target")
}

/// The cdylib file name for this platform; see the module docs for the
/// cfg rationale.
fn greeting_artifact_name() -> &'static str {
    if cfg!(windows) {
        "greeting.dll"
    } else if cfg!(target_os = "macos") {
        "libgreeting.dylib"
    } else {
        "libgreeting.so"
    }
}

/// Where the built cdylib must end up: `../../plugins/greeting/target/release/<artifact>`.
fn greeting_artifact() -> PathBuf {
    greeting_target_dir()
        .join("release")
        .join(greeting_artifact_name())
}

/// Builds the greeting cdylib with `cargo build --release`, using the cargo
/// that is running this very test (or PATH cargo as a fallback) and paths
/// relative to this crate.
///
/// Returns `false` - and the test skips with a message - only when cargo
/// cannot be executed at all (no Rust toolchain in the environment). A
/// failing build is a hard error and panics with full output: if the
/// reference plugin cannot be built, that is a real failure, not something
/// to paper over.
fn build_greeting() -> bool {
    let cargo = option_env!("CARGO").unwrap_or("cargo");
    eprintln!("building the greeting cdylib with `{cargo}` (this can take a few seconds)...");
    let output = Command::new(cargo)
        .arg("build")
        .arg("--release")
        .arg("--manifest-path")
        .arg(greeting_manifest())
        .arg("--target-dir")
        .arg(greeting_target_dir())
        .current_dir(crate_dir())
        .output();
    match output {
        Ok(out) if out.status.success() => {
            eprintln!(
                "greeting cdylib built in {}",
                greeting_target_dir().display()
            );
            true
        }
        Ok(out) => panic!(
            "`{cargo} build --release --manifest-path {}` failed (status {}):\n\
             stdout:\n{}\nstderr:\n{}",
            greeting_manifest().display(),
            out.status,
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        ),
        Err(error) => {
            eprintln!(
                "skipping native_abi_smoke: `{cargo}` could not be executed ({error}); \
                 no usable Rust toolchain in this environment"
            );
            false
        }
    }
}

/// Decodes a frozen export name to `&str` for the safe existence probes.
/// The names are pure ASCII by contract, so this cannot fail on real input;
/// it is split out so the fallible decode stays visible at the call sites.
fn export_name(export: &core::ffi::CStr) -> &str {
    export
        .to_str()
        .expect("the frozen export names are valid UTF-8")
}

/// Mints the table+token pair for one plugin instance, the shape
/// `candyriya-plugin` keeps in its own records.
fn mint_instance() -> TableSlot {
    let token = PluginToken::mint();
    TableSlot::new(CandyriyaPluginApiV1::new(token.handle()), token)
}

/// The whole open -> init -> load -> enable flow against `path`, with the
/// assertions shared between the parent and the re-exec'd child process.
/// Panics on the first violated expectation; returns on success.
fn drive_lifecycle(path: &Path) {
    let slot = mint_instance();
    let table = slot.table();

    let library = LibraryLoader::open(path, OpenFlags::Now).unwrap_or_else(|error| {
        panic!(
            "the greeting cdylib at `{}` must open: {error}",
            path.display()
        )
    });
    assert_eq!(
        library.path(),
        path,
        "the loader must report the path it opened"
    );

    // All three mandatory exports resolve. `export_exists` is the safe probe
    // `candyriya-native-instance` offers for this question; the typed calls
    // below would fail loudly on a missing export anyway, but a message
    // naming the export beats a generic loader error.
    for (export, name) in [
        (PLUGIN_INIT_EXPORT, "candyriya_plugin_init"),
        (PLUGIN_ON_LOAD_EXPORT, "candyriya_plugin_on_load"),
        (PLUGIN_ON_ENABLE_EXPORT, "candyriya_plugin_on_enable"),
    ] {
        assert!(
            export_exists(&library, export_name(export)),
            "the greeting cdylib must export `{name}`"
        );
    }

    // The init handshake through the loader boundary: `init_plugin` is
    // `initialize` with the unsafe inside the crate (same `PluginInitArgs`
    // from the same table, same fail-closed validation, same copy-out).
    let plugin = init_plugin(&library, table).unwrap_or_else(|error| {
        panic!("the init handshake must succeed for the greeting cdylib, got: {error}")
    });
    assert_eq!(
        plugin.abi_version, 1,
        "the plugin must echo ABI version 1, got {}",
        plugin.abi_version
    );
    assert_eq!(
        plugin.identity.id,
        b"greeting",
        "the plugin identity id must be `greeting`, got {:?}",
        String::from_utf8_lossy(&plugin.identity.id)
    );
    assert_eq!(
        plugin.identity.version,
        b"0.1.0",
        "the plugin identity version must be `0.1.0`, got {:?}",
        String::from_utf8_lossy(&plugin.identity.version)
    );
    assert_eq!(
        plugin.identity.name,
        b"Greeting",
        "the plugin identity name must be `Greeting`, got {:?}",
        String::from_utf8_lossy(&plugin.identity.name)
    );

    // on_load through the same call path the controller uses later.
    let status = on_load(&library, table)
        .unwrap_or_else(|error| panic!("the on_load call must reach the plugin: {error}"));
    assert_eq!(
        status,
        CandyriyaStatus::Ok,
        "on_load must return CandyriyaStatus::Ok, got {status:?}"
    );

    let status = on_enable(&library, table)
        .unwrap_or_else(|error| panic!("the on_enable call must reach the plugin: {error}"));
    assert_eq!(
        status,
        CandyriyaStatus::Ok,
        "on_enable must return CandyriyaStatus::Ok, got {status:?}"
    );

    library
        .close()
        .unwrap_or_else(|error| panic!("the mapping must unload cleanly, got: {error}"));
}

/// Re-execs this test binary in child mode (`NATIVE_SMOKE_CHILD=1`) so the
/// plugin's stdout reaches a pipe the parent can read. The child runs the
/// same FFI flow; the parent carries the capture assertion.
fn child_output() -> Output {
    let exe = std::env::current_exe().expect("the path of this test binary is always known");
    Command::new(exe)
        .args([
            "--exact",
            "native_abi_smoke",
            "--test-threads=1",
            "--no-capture",
        ])
        .env(CHILD_ENV, "1")
        .output()
        .expect("this test binary can re-exec itself to capture the plugin's stdout")
}

#[test]
fn native_abi_smoke() {
    let artifact = greeting_artifact();

    // Child mode: skip the build (the parent already built the cdylib) and
    // the re-exec; just drive the FFI flow so the plugin prints its hello on
    // this process's stdout, which is the pipe the parent is holding.
    if std::env::var_os(CHILD_ENV).is_some() {
        assert!(
            artifact.is_file(),
            "the greeting cdylib must exist at `{}` - the parent built it before re-exec",
            artifact.display()
        );
        drive_lifecycle(&artifact);
        return;
    }

    // 1. build the reference cdylib, paths relative to this crate
    if !build_greeting() {
        eprintln!("native_abi_smoke skipped: no usable cargo in this environment");
        return;
    }
    assert!(
        artifact.is_file(),
        "the greeting cdylib must exist at `{}` after a successful build \
         (expected `{}` in that directory)",
        artifact.display(),
        greeting_artifact_name()
    );

    // 2-5. open, resolve, handshake, drive the lifecycle exports in-process
    drive_lifecycle(&artifact);

    // 6. the hello line: capture it through a child of this very test binary
    let output = child_output();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "the re-exec of this test failed (status {}):\nstdout:\n{stdout}\nstderr:\n{stderr}",
        output.status
    );
    assert!(
        stdout.contains(HELLO),
        "the greeting plugin prints `{HELLO}` from on_enable; captured child stdout:\n{stdout}"
    );
}
