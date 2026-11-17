//! Dependency resolver tests on top of the real loader pipeline.
//!
//! The wasm modules are compiled from WAT text at test time (same approach
//! as the loader tests); the resolver itself only cares about manifests,
//! origins and discovery order.

use std::fs;
use std::path::{Path, PathBuf};

use candyriya_cache::CacheManager;
use candyriya_plugin::{PluginLoader, PluginOrigin, resolve_plugin_order, resolve_plugins};
use tempfile::TempDir;

const GREET_WAT: &str = r#"
(module
  (import "extism:host/env" "alloc" (func $alloc (param i64) (result i64)))
  (import "extism:host/env" "output_set" (func $output_set (param i64 i64)))
  (import "extism:host/env" "store_u8" (func $store_u8 (param i64 i32)))
  (memory (export "memory") 1)
  (func (export "greet") (result i32)
    (local $p i64)
    (local.set $p (call $alloc (i64.const 5)))
    (call $store_u8 (local.get $p) (i32.const 104))
    (call $store_u8 (i64.add (local.get $p) (i64.const 1)) (i32.const 101))
    (call $store_u8 (i64.add (local.get $p) (i64.const 2)) (i32.const 108))
    (call $store_u8 (i64.add (local.get $p) (i64.const 3)) (i32.const 108))
    (call $store_u8 (i64.add (local.get $p) (i64.const 4)) (i32.const 111))
    (call $output_set (local.get $p) (i64.const 5))
    (i32.const 0)))
"#;

struct Fixture {
    _root: TempDir,
    plugins_dir: PathBuf,
    libraries_dir: PathBuf,
    cache: CacheManager,
}

impl Fixture {
    fn new() -> Self {
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
            cache,
        }
    }

    fn load(&self) -> candyriya_plugin::LoadReport {
        PluginLoader::new(
            self.plugins_dir.clone(),
            self.libraries_dir.clone(),
            &self.cache,
        )
        .load()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.cache.shutdown();
    }
}

fn manifest(id: &str, depend: &[&str], softdepend: &[&str]) -> String {
    let depend = depend
        .iter()
        .map(|d| format!("\"{d}\""))
        .collect::<Vec<_>>()
        .join(", ");
    let softdepend = softdepend
        .iter()
        .map(|d| format!("\"{d}\""))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "id = \"{id}\"\n\
         version = \"0.1.0\"\n\
         name = \"Test plugin {id}\"\n\
         plugin_main = \"greet\"\n\
         plugin_api = \"0.1\"\n\
         depend = [{depend}]\n\
         softdepend = [{softdepend}]\n"
    )
}

fn write_plugin(root: &Path, id: &str, manifest_text: &str) {
    let dir = root.join(id);
    fs::create_dir_all(&dir).expect("plugin dir");
    fs::write(dir.join("plugin.toml"), manifest_text).expect("manifest");
    fs::write(
        dir.join(format!("{id}.wasm")),
        wat::parse_str(GREET_WAT).expect("greet wat compiles"),
    )
    .expect("wasm");
}

#[test]
fn chain_resolves_after_loader_discovery() {
    let fx = Fixture::new();
    write_plugin(&fx.plugins_dir, "alpha", &manifest("alpha", &["beta"], &[]));
    write_plugin(&fx.plugins_dir, "beta", &manifest("beta", &["gamma"], &[]));
    write_plugin(&fx.plugins_dir, "gamma", &manifest("gamma", &[], &[]));

    let report = fx.load();
    assert!(report.issues.is_empty());

    let ordered = resolve_plugin_order(report.plugins).expect("chain resolves");
    let ids: Vec<&str> = ordered.iter().map(|p| p.id()).collect();
    assert_eq!(ids, ["gamma", "beta", "alpha"]);
}

#[test]
fn missing_hard_depend_is_dropped_but_others_load() {
    let fx = Fixture::new();
    write_plugin(&fx.plugins_dir, "good", &manifest("good", &[], &[]));
    write_plugin(
        &fx.plugins_dir,
        "broken",
        &manifest("broken", &["absent"], &[]),
    );

    let report = fx.load();
    assert!(report.issues.is_empty());

    let resolution = resolve_plugins(report.plugins).expect("resolution completes");
    let ids: Vec<&str> = resolution.report.plugins.iter().map(|p| p.id()).collect();
    assert_eq!(ids, ["good"]);
    assert!(
        resolution
            .report
            .issues
            .iter()
            .any(|(_, why)| why.contains("broken") && why.contains("absent"))
    );
}

#[test]
fn library_loads_before_plugin_that_depends_on_it() {
    let fx = Fixture::new();
    // library sorts *after* the plugin alphabetically, dependency must win
    write_plugin(&fx.libraries_dir, "zzz-lib", &manifest("zzz-lib", &[], &[]));
    write_plugin(&fx.plugins_dir, "aaa", &manifest("aaa", &["zzz-lib"], &[]));

    let report = fx.load();
    assert!(report.issues.is_empty());

    let ordered = resolve_plugin_order(report.plugins).expect("resolves");
    let origins: Vec<PluginOrigin> = ordered.iter().map(|p| p.origin()).collect();
    assert_eq!(origins, [PluginOrigin::Library, PluginOrigin::Plugin]);
    let ids: Vec<&str> = ordered.iter().map(|p| p.id()).collect();
    assert_eq!(ids, ["zzz-lib", "aaa"]);
}

#[test]
fn soft_companion_loads_before_consumer_from_disk() {
    let fx = Fixture::new();
    write_plugin(
        &fx.plugins_dir,
        "consumer",
        &manifest("consumer", &[], &["companion"]),
    );
    write_plugin(
        &fx.plugins_dir,
        "companion",
        &manifest("companion", &[], &[]),
    );

    let report = fx.load();
    assert!(report.issues.is_empty());

    let resolution = resolve_plugins(report.plugins).expect("resolves");
    let ids: Vec<&str> = resolution.report.plugins.iter().map(|p| p.id()).collect();
    assert_eq!(ids, ["companion", "consumer"]);
    assert!(resolution.soft_deps["consumer"][0].present);
}
