//! Shared test plumbing: real WAT fixtures compiled at test time and a
//! throwaway on-disk layout (plugins/, libraries/, data/) per test.
//!
//! No network, no mocks — every byte of wasm here is real and runs
//! through extism.
#![allow(dead_code)]

use std::fs;
use std::path::{Path, PathBuf};

use candyriya_cache::CacheManager;
use tempfile::TempDir;

/// The manifest for a test plugin whose `plugin_main` is `on_event`.
pub fn event_manifest(id: &str) -> String {
    format!(
        "id = \"{id}\"\nversion = \"0.1.0\"\nname = \"Fixture {id}\"\nplugin_main = \"on_event\"\nplugin_api = \"0.1\"\n"
    )
}

/// A module whose `<export>` always answers with the given static bytes.
pub fn static_export_plugin_wat(export: &str, response: &str) -> String {
    let bytes = response.as_bytes();
    let stores: String = bytes
        .iter()
        .enumerate()
        .map(|(i, b)| {
            format!(
                "\n    (call $store_u8 (i64.add (local.get $p) (i64.const {i})) (i32.const {b}))"
            )
        })
        .collect();
    format!(
        r#"(module
  (import "extism:host/env" "alloc" (func $alloc (param i64) (result i64)))
  (import "extism:host/env" "output_set" (func $output_set (param i64 i64)))
  (import "extism:host/env" "store_u8" (func $store_u8 (param i64 i32)))
  (memory (export "memory") 1)
  (func (export "{export}") (result i32)
    (local $p i64)
    (local.set $p (call $alloc (i64.const {len}))){stores}
    (call $output_set (local.get $p) (i64.const {len}))
    (i32.const 0)))
"#,
        len = bytes.len(),
    )
}

/// Compile the static-export module.
pub fn static_export_plugin_wasm(export: &str, response: &str) -> Vec<u8> {
    wat::parse_str(static_export_plugin_wat(export, response))
        .expect("static-export fixture wat compiles")
}

/// A module whose `on_event` forwards its input to a namespaced host
/// function and returns the host function's raw output.
///
/// Proves the host-function round trip: the guest calls the host and the
/// host's bytes land in the caller's hands untouched.
pub fn echo_host_fn_wat(import_module: &str, import_name: &str) -> String {
    format!(
        r#"(module
  (import "extism:host/env" "alloc" (func $alloc (param i64) (result i64)))
  (import "extism:host/env" "input_length" (func $input_length (result i64)))
  (import "extism:host/env" "input_load_u8" (func $input_load_u8 (param i64) (result i32)))
  (import "extism:host/env" "store_u8" (func $store_u8 (param i64 i32)))
  (import "extism:host/env" "output_set" (func $output_set (param i64 i64)))
  (import "extism:host/env" "length" (func $length (param i64) (result i64)))
  (import "{module}" "{name}" (func $hostfn (param i64) (result i64)))
  (memory (export "memory") 1)
  (func (export "on_event") (result i32)
    (local $len i64)
    (local $in i64)
    (local $out i64)
    (local $i i64)
    (local.set $len (call $input_length))
    (local.set $in (call $alloc (local.get $len)))
    (block $done
      (loop $loop
        (br_if $done (i64.ge_u (local.get $i) (local.get $len)))
        (call $store_u8
          (i64.add (local.get $in) (local.get $i))
          (call $input_load_u8 (local.get $i)))
        (local.set $i (i64.add (local.get $i) (i64.const 1)))
        (br $loop)))
    (local.set $out (call $hostfn (local.get $in)))
    (call $output_set (local.get $out) (call $length (local.get $out)))
    (i32.const 0)))
"#,
        module = import_module,
        name = import_name,
    )
}

/// Compile the echo fixture.
pub fn echo_host_fn_wasm(import_module: &str, import_name: &str) -> Vec<u8> {
    wat::parse_str(echo_host_fn_wat(import_module, import_name)).expect("echo fixture wat compiles")
}

/// A module with two exports:
///
/// - `subscribe` calls `candyriya:subscribeToEvents` with a static
///   `["login"]` argument (input sniffing is impossible in a static
///   fixture, so the allowlist is hardcoded);
/// - `on_event` answers `{"cancelled": true}` unconditionally.
///
/// Used to prove the dispatcher's subscriber gate — the guest that would
/// cancel the event is never asked, because it did not subscribe to the
/// event kind being dispatched.
pub fn subscribe_login_plugin_wat() -> String {
    let subscribe_input = br#"["login"]"#;
    let subscribe_stores: String = subscribe_input
        .iter()
        .enumerate()
        .map(|(i, b)| {
            format!(
                "\n    (call $store_u8 (i64.add (local.get $s) (i64.const {i})) (i32.const {b}))"
            )
        })
        .collect();
    let report = br#"{"cancelled": true}"#;
    let report_stores: String = report
        .iter()
        .enumerate()
        .map(|(i, b)| {
            format!(
                "\n    (call $store_u8 (i64.add (local.get $p) (i64.const {i})) (i32.const {b}))"
            )
        })
        .collect();
    format!(
        r#"(module
  (import "extism:host/env" "alloc" (func $alloc (param i64) (result i64)))
  (import "extism:host/env" "store_u8" (func $store_u8 (param i64 i32)))
  (import "extism:host/env" "output_set" (func $output_set (param i64 i64)))
  (import "candyriya" "subscribeToEvents" (func $subscribe (param i64) (result i64)))
  (memory (export "memory") 1)
  (func (export "subscribe") (result i32)
    (local $s i64)
    (local.set $s (call $alloc (i64.const {subscribe_len}))){subscribe_stores}
    (drop (call $subscribe (local.get $s)))
    (call $output_set (local.get $s) (i64.const {subscribe_len}))
    (i32.const 0))
  (func (export "on_event") (result i32)
    (local $p i64)
    (local.set $p (call $alloc (i64.const {report_len}))){report_stores}
    (call $output_set (local.get $p) (i64.const {report_len}))
    (i32.const 0)))
"#,
        subscribe_len = subscribe_input.len(),
        report_len = report.len(),
    )
}

/// Compile the subscribe/on_event fixture.
pub fn subscribe_login_plugin_wasm() -> Vec<u8> {
    wat::parse_str(subscribe_login_plugin_wat()).expect("subscribe fixture wat compiles")
}

/// A module whose `on_event` first records a cancellation through the
/// `candyriya:cancelEvent` host function (with a static reason) and only
/// then answers `{"cancelled": false}`.
///
/// Used to prove the cancellation recorded via `cancelEvent` wins over
/// whatever the export returns.
pub fn cancel_event_plugin_wat() -> String {
    let host_input = br#"{"cancel": true, "reason": "nope"}"#;
    let host_stores: String = host_input
        .iter()
        .enumerate()
        .map(|(i, b)| {
            format!(
                "\n    (call $store_u8 (i64.add (local.get $in) (i64.const {i})) (i32.const {b}))"
            )
        })
        .collect();
    let report = br#"{"cancelled": false}"#;
    let report_stores: String = report
        .iter()
        .enumerate()
        .map(|(i, b)| {
            format!(
                "\n    (call $store_u8 (i64.add (local.get $p) (i64.const {i})) (i32.const {b}))"
            )
        })
        .collect();
    format!(
        r#"(module
  (import "extism:host/env" "alloc" (func $alloc (param i64) (result i64)))
  (import "extism:host/env" "store_u8" (func $store_u8 (param i64 i32)))
  (import "extism:host/env" "output_set" (func $output_set (param i64 i64)))
  (import "candyriya" "cancelEvent" (func $cancel (param i64) (result i64)))
  (memory (export "memory") 1)
  (func (export "on_event") (result i32)
    (local $in i64)
    (local $p i64)
    (local.set $in (call $alloc (i64.const {host_len}))){host_stores}
    (drop (call $cancel (local.get $in)))
    (local.set $p (call $alloc (i64.const {report_len}))){report_stores}
    (call $output_set (local.get $p) (i64.const {report_len}))
    (i32.const 0)))
"#,
        host_len = host_input.len(),
        report_len = report.len(),
    )
}

/// Compile the cancelEvent fixture.
pub fn cancel_event_plugin_wasm() -> Vec<u8> {
    wat::parse_str(cancel_event_plugin_wat()).expect("cancel fixture wat compiles")
}

/// A module whose `on_event` records a cancellation through the
/// `candyriya:cancelEvent` host function (with a static reason) and then
/// never returns.
///
/// Used to prove a cancellation recorded before the call died beats the
/// timeout — the merge prefers `cancelEvent` over everything.
pub fn cancel_event_then_hang_plugin_wat() -> String {
    let host_input = br#"{"cancel": true, "reason": "nope"}"#;
    let host_stores: String = host_input
        .iter()
        .enumerate()
        .map(|(i, b)| {
            format!(
                "\n    (call $store_u8 (i64.add (local.get $in) (i64.const {i})) (i32.const {b}))"
            )
        })
        .collect();
    format!(
        r#"(module
  (import "extism:host/env" "alloc" (func $alloc (param i64) (result i64)))
  (import "extism:host/env" "store_u8" (func $store_u8 (param i64 i32)))
  (import "extism:host/env" "output_set" (func $output_set (param i64 i64)))
  (import "candyriya" "cancelEvent" (func $cancel (param i64) (result i64)))
  (memory (export "memory") 1)
  (func (export "on_event") (result i32)
    (local $in i64)
    (local.set $in (call $alloc (i64.const {host_len}))){host_stores}
    (drop (call $cancel (local.get $in)))
    (loop $spin
      (br $spin))
    (i32.const 0)))
"#,
        host_len = host_input.len(),
    )
}

/// Compile the hang-after-cancel fixture.
pub fn cancel_event_then_hang_plugin_wasm() -> Vec<u8> {
    wat::parse_str(cancel_event_then_hang_plugin_wat()).expect("cancel fixture wat compiles")
}

/// A module whose `on_event` always answers with the given static JSON
/// `OutcomeReport` document.
pub fn response_plugin_wat(response: &str) -> String {
    let bytes = response.as_bytes();
    let stores: String = bytes
        .iter()
        .enumerate()
        .map(|(i, b)| {
            format!(
                "\n    (call $store_u8 (i64.add (local.get $p) (i64.const {i})) (i32.const {b}))"
            )
        })
        .collect();
    format!(
        r#"(module
  (import "extism:host/env" "alloc" (func $alloc (param i64) (result i64)))
  (import "extism:host/env" "output_set" (func $output_set (param i64 i64)))
  (import "extism:host/env" "store_u8" (func $store_u8 (param i64 i32)))
  (memory (export "memory") 1)
  (func (export "on_event") (result i32)
    (local $p i64)
    (local.set $p (call $alloc (i64.const {len}))){stores}
    (call $output_set (local.get $p) (i64.const {len}))
    (i32.const 0)))
"#,
        len = bytes.len(),
    )
}

/// Compile the static-response module.
pub fn response_plugin_wasm(response: &str) -> Vec<u8> {
    wat::parse_str(response_plugin_wat(response)).expect("response fixture wat compiles")
}

/// A module that cancels when the input it is handed starts with `{`
/// (every real event envelope does) and allows otherwise.
///
/// Proves the dispatched payload actually crosses the ABI into wasm —
/// a plugin that never sees the bytes cannot pass this test.
pub fn sniff_plugin_wat() -> String {
    let cancelled = "{\"cancelled\":true}";
    let allowed = "{\"cancelled\":false}";
    fn branch(response: &str) -> String {
        response
            .as_bytes()
            .iter()
            .enumerate()
            .map(|(i, b)| {
                format!(
                    "\n        (call $store_u8 (i64.add (local.get $p) (i64.const {i})) (i32.const {b}))"
                )
            })
            .collect()
    }
    format!(
        r#"(module
  (import "extism:host/env" "alloc" (func $alloc (param i64) (result i64)))
  (import "extism:host/env" "output_set" (func $output_set (param i64 i64)))
  (import "extism:host/env" "store_u8" (func $store_u8 (param i64 i32)))
  (import "extism:host/env" "input_load_u8" (func $input_load_u8 (param i64) (result i32)))
  (memory (export "memory") 1)
  (func (export "on_event") (result i32)
    (local $p i64)
    (local $len i64)
    (local.set $p (call $alloc (i64.const 32)))
    (if (i32.eq (call $input_load_u8 (i64.const 0)) (i32.const 123))
      (then
        (local.set $len (i64.const {cancelled_len})){cancelled_stores}
      )
      (else
        (local.set $len (i64.const {allowed_len})){allowed_stores}
      ))
    (call $output_set (local.get $p) (local.get $len))
    (i32.const 0)))
"#,
        cancelled_len = cancelled.len(),
        cancelled_stores = branch(cancelled),
        allowed_len = allowed.len(),
        allowed_stores = branch(allowed),
    )
}

/// Compile the input-sniffing module.
pub fn sniff_plugin_wasm() -> Vec<u8> {
    wat::parse_str(sniff_plugin_wat()).expect("sniff fixture wat compiles")
}

/// A module that never returns from `on_event`.
///
/// Used to prove the dispatcher's per-plugin timeout fires: the call is
/// awaited for at most the configured deadline and the rest of the
/// dispatch proceeds. The plugin thread is detached when the instance is
/// dropped; the test process does not wait for it.
pub const SLOW_PLUGIN_WAT: &str = r#"
(module
  (memory (export "memory") 1)
  (func (export "on_event") (result i32)
    (loop $spin
      (br $spin))
    (i32.const 0)))
"#;

/// Compile the never-returning module.
pub fn slow_plugin_wasm() -> Vec<u8> {
    wat::parse_str(SLOW_PLUGIN_WAT).expect("slow fixture wat compiles")
}

/// A module that exports nothing — the plugin_main check must reject it.
pub const NO_MAIN_WAT: &str = r#"
(module
  (memory (export "memory") 1)
  (func (export "something_else") (result i32)
    (i32.const 0)))
"#;

/// Compile the no-main module.
pub fn no_main_wasm() -> Vec<u8> {
    wat::parse_str(NO_MAIN_WAT).expect("no-main fixture wat compiles")
}

/// Per-test on-disk layout: `<root>/plugins`, `<root>/libraries`, a
/// cache-rooted `CacheManager` at `<root>/data`.
pub struct Fixture {
    pub _root: TempDir,
    pub plugins_dir: PathBuf,
    pub libraries_dir: PathBuf,
    pub cache_root: PathBuf,
    pub cache: CacheManager,
}

impl Fixture {
    pub fn new() -> Self {
        let root = TempDir::new().expect("tempdir");
        let plugins_dir = root.path().join("plugins");
        let libraries_dir = root.path().join("libraries");
        let cache_root = root.path().join("data");
        fs::create_dir_all(&plugins_dir).expect("plugins dir");
        fs::create_dir_all(&libraries_dir).expect("libraries dir");
        fs::create_dir_all(&cache_root).expect("cache root");
        let cache = CacheManager::new(&cache_root).expect("cache manager");
        Self {
            _root: root,
            plugins_dir,
            libraries_dir,
            cache_root,
            cache,
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.cache.shutdown();
    }
}

/// Write a plugin directory (`<id>/plugin.toml` + `<id>/<id>.wasm`).
pub fn write_plugin(root: &Path, id: &str, wasm: &[u8]) {
    let dir = root.join(id);
    fs::create_dir_all(&dir).expect("plugin dir");
    fs::write(dir.join("plugin.toml"), event_manifest(id)).expect("manifest");
    fs::write(dir.join(format!("{id}.wasm")), wasm).expect("wasm");
}

/// Write a plugin directory with a custom manifest (for reload tests).
pub fn write_plugin_with_manifest(root: &Path, id: &str, manifest: &str, wasm: &[u8]) {
    let dir = root.join(id);
    fs::create_dir_all(&dir).expect("plugin dir");
    fs::write(dir.join("plugin.toml"), manifest).expect("manifest");
    fs::write(dir.join(format!("{id}.wasm")), wasm).expect("wasm");
}
