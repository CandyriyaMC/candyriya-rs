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
