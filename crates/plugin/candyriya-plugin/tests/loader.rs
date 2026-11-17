//! Loader discovery, lifecycle and extism instantiation tests.
//!
//! The wasm modules here are real: they are compiled from WAT text at test
//! time with the pure-Rust `wat` crate (no C toolchain) and loaded through
//! extism. No network, no mocks.

use std::fs;
use std::path::{Path, PathBuf};

use candyriya_cache::CacheManager;
use candyriya_plugin::{LoadReport, PluginLoader, PluginOrigin, PluginState};
use tempfile::TempDir;

/// A minimal extism-ABI plugin: the module keeps its own exported memory;
/// input and output bytes live in the host's kernel memory and are moved
/// through kernel functions (`alloc`/`output_set`/`store_u8`). Importing the
/// kernel's `memory` is rejected by the host.
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

/// Same shape, but exports `not_greet` — used for plugin_main enforcement.
const NOT_GREET_WAT: &str = r#"
(module
  (memory (export "memory") 1)
  (func (export "not_greet") (result i32)
    (i32.const 0)))
"#;

struct Fixture {
    _root: TempDir,
    plugins_dir: PathBuf,
    libraries_dir: PathBuf,
    cache_root: PathBuf,
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
            cache_root,
            cache,
        }
    }

    fn loader(&self) -> PluginLoader<'_> {
        PluginLoader::new(
            self.plugins_dir.clone(),
            self.libraries_dir.clone(),
            &self.cache,
        )
    }

    fn load(&self) -> LoadReport {
        self.loader().load()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.cache.shutdown();
    }
}

fn manifest(id: &str, plugin_main: &str) -> String {
    format!(
        "id = \"{id}\"\nversion = \"0.1.0\"\nname = \"Test plugin {id}\"\nplugin_main = \"{plugin_main}\"\nplugin_api = \"0.1\"\n"
    )
}

fn write_plugin(root: &Path, id: &str, manifest_text: &str, wasm: &[u8]) {
    let dir = root.join(id);
    fs::create_dir_all(&dir).expect("plugin dir");
    fs::write(dir.join("plugin.toml"), manifest_text).expect("manifest");
    fs::write(dir.join(format!("{id}.wasm")), wasm).expect("wasm");
}

fn greet_wasm() -> Vec<u8> {
    wat::parse_str(GREET_WAT).expect("greet wat compiles")
}

#[test]
fn lists_plugins_from_both_roots_in_order() {
    let fx = Fixture::new();
    write_plugin(
        &fx.libraries_dir,
        "lib-core",
        &manifest("lib-core", "greet"),
        &greet_wasm(),
    );
    write_plugin(
        &fx.libraries_dir,
        "lib-util",
        &manifest("lib-util", "greet"),
        &greet_wasm(),
    );
    write_plugin(
        &fx.plugins_dir,
        "beta",
        &manifest("beta", "greet"),
        &greet_wasm(),
    );
    write_plugin(
        &fx.plugins_dir,
        "alpha",
        &manifest("alpha", "greet"),
        &greet_wasm(),
    );

    let report = fx.load();

    let ids: Vec<&str> = report.plugins.iter().map(|p| p.id()).collect();
    // libraries first, then plugins, alphabetical within each root
    assert_eq!(ids, ["lib-core", "lib-util", "alpha", "beta"]);
    let origins: Vec<PluginOrigin> = report.plugins.iter().map(|p| p.origin()).collect();
    assert_eq!(
        origins,
        [
            PluginOrigin::Library,
            PluginOrigin::Library,
            PluginOrigin::Plugin,
            PluginOrigin::Plugin
        ]
    );
    assert!(
        report.plugins.iter().all(|p| p.state().is_loaded()),
        "all fixtures load: {:?}",
        report.issues
    );
    assert!(report.issues.is_empty());
}

#[test]
fn stray_wasm_without_manifest_is_reported_not_fatal() {
    let fx = Fixture::new();
    let stray = fx.plugins_dir.join("foo");
    fs::create_dir_all(&stray).expect("plugin dir");
    fs::write(stray.join("bar.wasm"), b"\0asm\x01\0\0\0").expect("stray wasm");
    write_plugin(
        &fx.plugins_dir,
        "good",
        &manifest("good", "greet"),
        &greet_wasm(),
    );

    let report = fx.load();

    assert_eq!(report.plugins.len(), 1);
    assert_eq!(report.plugins[0].id(), "good");
    assert_eq!(report.issues.len(), 1);
    let (where_, why) = &report.issues[0];
    assert_eq!(where_, &stray.join("bar.wasm"));
    assert!(why.contains("plugin.toml"), "unexpected reason: {why}");
}

#[test]
fn manifest_missing_and_no_wasm_is_silently_ignored() {
    let fx = Fixture::new();
    fs::create_dir_all(fx.plugins_dir.join("empty-dir")).expect("empty dir");

    let report = fx.load();

    assert!(report.plugins.is_empty());
    assert!(report.issues.is_empty());
}

#[test]
fn manifest_with_toml_syntax_error_is_reported() {
    let fx = Fixture::new();
    let dir = fx.plugins_dir.join("broken");
    fs::create_dir_all(&dir).expect("plugin dir");
    fs::write(dir.join("plugin.toml"), "this is not toml [").expect("manifest");

    let report = fx.load();

    assert!(report.plugins.is_empty());
    assert_eq!(report.issues.len(), 1);
    assert!(report.issues[0].0.ends_with("plugin.toml"));
}

#[test]
fn manifest_with_bad_metadata_is_reported() {
    let fx = Fixture::new();
    write_plugin(
        &fx.plugins_dir,
        "badver",
        "id = \"badver\"\nversion = \"not-semver\"\nname = \"Bad\"\nplugin_main = \"greet\"\nplugin_api = \"0.1\"\n",
        &greet_wasm(),
    );

    let report = fx.load();

    assert!(report.plugins.is_empty());
    assert_eq!(report.issues.len(), 1);
    assert!(report.issues[0].1.contains("not semver"));
}

#[test]
fn manifest_id_must_match_directory_name() {
    let fx = Fixture::new();
    let dir = fx.plugins_dir.join("dirname");
    fs::create_dir_all(&dir).expect("plugin dir");
    fs::write(dir.join("plugin.toml"), manifest("other-id", "greet")).expect("manifest");
    fs::write(dir.join("dirname.wasm"), greet_wasm()).expect("wasm");

    let report = fx.load();

    assert!(report.plugins.is_empty());
    assert_eq!(report.issues.len(), 1);
    let why = &report.issues[0].1;
    assert!(
        why.contains("other-id") && why.contains("dirname"),
        "unexpected: {why}"
    );
}

#[test]
fn duplicate_id_across_libraries_and_plugins_is_rejected() {
    let fx = Fixture::new();
    write_plugin(
        &fx.libraries_dir,
        "dup",
        &manifest("dup", "greet"),
        &greet_wasm(),
    );
    write_plugin(
        &fx.plugins_dir,
        "dup",
        &manifest("dup", "greet"),
        &greet_wasm(),
    );

    let report = fx.load();

    assert_eq!(report.plugins.len(), 1);
    assert_eq!(report.plugins[0].origin(), PluginOrigin::Library);
    assert_eq!(report.issues.len(), 1);
    assert_eq!(
        report.issues[0].0,
        fx.plugins_dir.join("dup"),
        "the plugins/ copy is the one dropped"
    );
    assert!(report.issues[0].1.contains("duplicate"));
}

#[test]
fn validate_unique_reports_all_collisions() {
    use candyriya_plugin::validate_unique;
    use std::path::Path;

    let err = validate_unique([
        ("a", Path::new("libraries/a")),
        ("b", Path::new("plugins/b")),
        ("a", Path::new("plugins/a")),
        ("b", Path::new("libraries/b")),
    ])
    .unwrap_err();

    assert_eq!(err.groups.len(), 2);
    assert_eq!(err.groups[0].id, "a");
    assert_eq!(
        err.groups[0].origins,
        vec![PathBuf::from("libraries/a"), PathBuf::from("plugins/a")]
    );
}

#[test]
fn loads_real_wat_plugin_and_calls_export() {
    let fx = Fixture::new();
    write_plugin(
        &fx.plugins_dir,
        "greeter",
        &manifest("greeter", "greet"),
        &greet_wasm(),
    );

    let report = fx.load();

    assert_eq!(report.plugins.len(), 1);
    let plugin = &report.plugins[0];
    assert_eq!(plugin.state(), &PluginState::Loaded);

    let instance = plugin.instance().expect("loaded plugin has an instance");
    let out = instance.call("greet", b"").expect("greet call");
    assert_eq!(out, b"hello");

    // the plugin executes on its own dedicated, named thread —
    // never on the calling (test) thread
    let worker = instance.worker_thread_name().expect("worker name");
    assert_eq!(worker, "candyriya-plugin-greeter");
    assert_ne!(worker, std::thread::current().name().unwrap_or(""));
}

#[test]
fn bad_wat_does_not_break_other_plugins() {
    let fx = Fixture::new();
    write_plugin(
        &fx.plugins_dir,
        "broken",
        &manifest("broken", "greet"),
        b"\0asm\xde\xad\xbe\xef not wasm",
    );
    write_plugin(
        &fx.plugins_dir,
        "good",
        &manifest("good", "greet"),
        &greet_wasm(),
    );

    let report = fx.load();

    assert_eq!(report.plugins.len(), 2);
    let broken = report
        .plugins
        .iter()
        .find(|p| p.id() == "broken")
        .expect("broken record");
    let good = report
        .plugins
        .iter()
        .find(|p| p.id() == "good")
        .expect("good record");
    assert!(
        good.state().is_loaded(),
        "good plugin loads despite broken sibling"
    );
    match broken.state() {
        PluginState::Failed(reason) => assert!(!reason.is_empty()),
        other => panic!("broken plugin must be Failed, got {other:?}"),
    }
    assert!(
        report
            .issues
            .iter()
            .any(|(path, _)| path.starts_with(fx.plugins_dir.join("broken"))),
        "the wasm failure is listed in issues"
    );
}

#[test]
fn plugin_main_export_is_enforced() {
    let fx = Fixture::new();
    write_plugin(
        &fx.plugins_dir,
        "noexport",
        &manifest("noexport", "greet"),
        &wat::parse_str(NOT_GREET_WAT).expect("wat"),
    );

    let report = fx.load();

    assert_eq!(report.plugins.len(), 1);
    match report.plugins[0].state() {
        PluginState::Failed(reason) => {
            assert!(
                reason.contains("plugin_main") && reason.contains("greet"),
                "unexpected: {reason}"
            );
        }
        other => panic!("plugin without plugin_main must be Failed, got {other:?}"),
    }
}

#[test]
fn missing_wasm_module_is_rejected_but_lib_wasm_fallback_works() {
    let fx = Fixture::new();
    let dir = fx.plugins_dir.join("nowasm");
    fs::create_dir_all(&dir).expect("plugin dir");
    fs::write(dir.join("plugin.toml"), manifest("nowasm", "greet")).expect("manifest");

    let report = fx.load();
    assert!(report.plugins.is_empty());
    assert_eq!(report.issues.len(), 1);
    assert!(report.issues[0].1.contains("no wasm module"));

    // lib.wasm is accepted as the module name
    fs::write(dir.join("lib.wasm"), greet_wasm()).expect("lib.wasm");
    let report = fx.load();
    assert_eq!(report.plugins.len(), 1);
    assert!(report.plugins[0].state().is_loaded());
    assert!(report.issues.is_empty());
}

#[test]
fn nonexistent_roots_are_reported_but_not_fatal() {
    let fx = Fixture::new();
    write_plugin(
        &fx.plugins_dir,
        "alive",
        &manifest("alive", "greet"),
        &greet_wasm(),
    );
    let missing_libraries = fx.cache_root.join("no-such-libraries");

    let report =
        PluginLoader::new(fx.plugins_dir.clone(), missing_libraries.clone(), &fx.cache).load();

    assert_eq!(report.plugins.len(), 1);
    assert!(report.issues.iter().any(|(p, _)| p == &missing_libraries));
}

#[test]
fn cache_quota_from_manifest_is_applied() {
    let fx = Fixture::new();
    let quota_manifest = format!(
        "{}cache_quota_bytes = 1024\ncache_quota_resources = 7\n",
        manifest("quoted", "greet")
    );
    write_plugin(&fx.plugins_dir, "quoted", &quota_manifest, &greet_wasm());

    let report = fx.load();

    assert_eq!(report.plugins.len(), 1);
    assert!(
        report.plugins[0].state().is_loaded(),
        "issues: {:?}",
        report.issues
    );
    let namespace = fx.cache.plugin_cache("quoted").expect("plugin cache ns");
    let quota = namespace.quota();
    assert_eq!(quota.max_bytes, Some(1024));
    assert_eq!(quota.max_resources, Some(7));
}

#[test]
fn disable_enable_switch_and_failed_state() {
    let fx = Fixture::new();
    write_plugin(
        &fx.plugins_dir,
        "switchable",
        &manifest("switchable", "greet"),
        &greet_wasm(),
    );

    let mut report = fx.load();
    let plugin = &mut report.plugins[0];
    assert_eq!(plugin.state(), &PluginState::Loaded);

    plugin.disable().expect("disable");
    assert_eq!(plugin.state(), &PluginState::Disabled);
    plugin.disable().expect("double disable is a no-op");
    plugin.enable().expect("enable");
    assert_eq!(plugin.state(), &PluginState::Loaded);

    // a failed plugin cannot be toggled
    write_plugin(
        &fx.plugins_dir,
        "broken",
        &manifest("broken", "greet"),
        b"garbage",
    );
    let mut report = fx.load();
    let broken = report
        .plugins
        .iter_mut()
        .find(|p| p.id() == "broken")
        .expect("broken record");
    assert!(broken.disable().is_err());
    assert!(broken.enable().is_err());
}

#[test]
fn unload_shuts_worker_down() {
    let fx = Fixture::new();
    write_plugin(
        &fx.plugins_dir,
        "stopme",
        &manifest("stopme", "greet"),
        &greet_wasm(),
    );

    let report = fx.load();
    let plugin = report.plugins.into_iter().next().expect("one plugin");
    plugin.unload().expect("unload succeeds");
}
