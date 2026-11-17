//! Hot reload: swap a running plugin for its on-disk state, revoke
//! plugin-owned services, keep the failed plugin's predecessor serving.
//!
//! Fixtures are real extism plugins compiled from WAT at test time.

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use candyriya_plugin::{
    HotReloadError, Outcome, PluginLoader, PluginReloader, PluginSet, PluginState,
    ProxyEventDispatcher, ServiceRegistry, Watch,
};
use candyriya_plugin_api::{EventEnvelope, EventKind, ProxyEvent};
use candyriya_scheduler::TaskService;

use common::Fixture;

#[derive(Debug)]
struct ShimService {
    n: AtomicU64,
}

fn login_event() -> ProxyEvent {
    ProxyEvent::new(EventEnvelope::new(
        7,
        EventKind::Login {
            uuid: "069a79f4-44e9-4726-a5be-fca90e38aaf5".to_owned(),
            name: "Notch".to_owned(),
            protocol_version: 769,
        },
        1_730_000_000_000,
    ))
}

struct Setup {
    fx: Fixture,
    plugins: PluginSet,
    services: ServiceRegistry,
}

impl Setup {
    /// One `reloadme` plugin answering with cancel reason `v1`.
    fn new() -> Self {
        let fx = Fixture::new();
        common::write_plugin(
            &fx.plugins_dir,
            "reloadme",
            &common::response_plugin_wasm("{\"cancelled\":true,\"cancel_reason\":\"v1\"}"),
        );
        let report =
            PluginLoader::new(fx.plugins_dir.clone(), fx.libraries_dir.clone(), &fx.cache).load();
        assert_eq!(report.plugins.len(), 1);
        assert!(
            report.issues.is_empty(),
            "unexpected issues: {:?}",
            report.issues
        );
        assert!(report.plugins[0].state().is_loaded());
        Self {
            fx,
            plugins: PluginSet::from_vec(report.plugins),
            services: ServiceRegistry::new(),
        }
    }

    fn reloader(&self) -> PluginReloader {
        PluginReloader::new(
            self.plugins.clone(),
            self.fx.cache_root.clone(),
            self.services.clone(),
        )
    }

    fn dispatch_reason(&self) -> String {
        let dispatcher = ProxyEventDispatcher::new(self.plugins.clone());
        let result = dispatcher.dispatch_cancellable(&login_event());
        match result.outcome_for("reloadme") {
            Some(Outcome::Cancelled { cancel_reason }) => {
                cancel_reason.clone().unwrap_or_else(|| "<none>".to_owned())
            }
            other => panic!("expected Cancelled, got {other:?}"),
        }
    }

    fn write_v2_wasm(&self) {
        std::fs::write(
            self.fx.plugins_dir.join("reloadme").join("reloadme.wasm"),
            common::response_plugin_wasm("{\"cancelled\":true,\"cancel_reason\":\"v2\"}"),
        )
        .expect("v2 wasm");
    }
}

#[test]
fn reload_swaps_in_the_new_wasm_and_revokes_plugin_services() {
    let setup = Setup::new();
    setup.services.register_service(
        "config.manager".to_owned(),
        Arc::new(ShimService {
            n: AtomicU64::new(42),
        }),
    );
    setup.services.register_owned_service(
        "fixture.greeting".to_owned(),
        Arc::new("hello-from-v1"),
        "reloadme",
    );

    assert_eq!(setup.dispatch_reason(), "v1");

    setup.write_v2_wasm();

    let report = setup
        .reloader()
        .reload("reloadme")
        .expect("reload succeeds");
    assert_eq!(report.plugin, "reloadme");
    assert_eq!(report.state, PluginState::Loaded);
    assert_eq!(report.revoked_services, 1);
    assert!(report.wasm_path.ends_with("reloadme.wasm"));

    // the new wasm is the one running now — old state is gone
    assert_eq!(setup.dispatch_reason(), "v2");

    // the service registry was updated: the plugin-owned entry is gone,
    // the core one is untouched
    assert!(
        setup
            .services
            .get_service::<&'static str>("fixture.greeting")
            .is_none()
    );
    let shim = setup
        .services
        .get_service::<ShimService>("config.manager")
        .expect("core service survives a plugin reload");
    assert_eq!(shim.n.load(Ordering::SeqCst), 42);

    // the slot is re-registered in place: same id, same order, still Loaded
    assert_eq!(setup.plugins.ids(), ["reloadme"]);
    assert_eq!(setup.plugins.state("reloadme"), Some(PluginState::Loaded));
}

#[test]
fn reload_of_unknown_plugin_fails_without_touching_anything() {
    let setup = Setup::new();
    setup.services.register_owned_service(
        "fixture.greeting".to_owned(),
        Arc::new("kept"),
        "reloadme",
    );

    let err = setup.reloader().reload("no-such-plugin").unwrap_err();

    assert!(
        matches!(err, candyriya_plugin::PluginError::InvalidState { .. }),
        "unexpected error: {err}"
    );
    assert_eq!(
        setup.dispatch_reason(),
        "v1",
        "the running plugin is untouched"
    );
    assert!(
        setup
            .services
            .get_service::<&'static str>("fixture.greeting")
            .is_some()
    );
}

#[test]
fn reload_with_uncompilable_wasm_keeps_the_running_plugin() {
    let setup = Setup::new();
    setup.services.register_owned_service(
        "fixture.greeting".to_owned(),
        Arc::new("kept"),
        "reloadme",
    );

    std::fs::write(
        setup.fx.plugins_dir.join("reloadme").join("reloadme.wasm"),
        b"\0asm\xde\xad\xbe\xef not a real module",
    )
    .expect("broken wasm");

    let err = setup.reloader().reload("reloadme").unwrap_err();
    assert!(
        matches!(err, candyriya_plugin::PluginError::WasmLoad { .. }),
        "unexpected error: {err}"
    );

    assert_eq!(setup.dispatch_reason(), "v1", "old plugin keeps serving");
    assert_eq!(setup.plugins.state("reloadme"), Some(PluginState::Loaded));
    assert!(
        setup
            .services
            .get_service::<&'static str>("fixture.greeting")
            .is_some(),
        "an unstageable reload revokes nothing"
    );
}

#[test]
fn reload_with_missing_plugin_main_keeps_the_running_plugin() {
    let setup = Setup::new();

    std::fs::write(
        setup.fx.plugins_dir.join("reloadme").join("reloadme.wasm"),
        common::no_main_wasm(),
    )
    .expect("no-main wasm");

    let err = setup.reloader().reload("reloadme").unwrap_err();
    assert!(
        matches!(err, candyriya_plugin::PluginError::MissingExport { .. }),
        "unexpected error: {err}"
    );

    assert_eq!(setup.dispatch_reason(), "v1");
    assert_eq!(setup.plugins.state("reloadme"), Some(PluginState::Loaded));
}

#[test]
fn reload_with_broken_manifest_keeps_the_running_plugin() {
    let setup = Setup::new();

    std::fs::write(
        setup.fx.plugins_dir.join("reloadme").join("plugin.toml"),
        "this is not [toml",
    )
    .expect("broken manifest");

    let err = setup.reloader().reload("reloadme").unwrap_err();
    assert!(
        matches!(err, candyriya_plugin::PluginError::Manifest(_)),
        "unexpected error: {err}"
    );

    assert_eq!(setup.dispatch_reason(), "v1");
}

#[test]
fn reload_with_a_manifest_moved_to_another_id_keeps_the_running_plugin() {
    let setup = Setup::new();

    std::fs::write(
        setup.fx.plugins_dir.join("reloadme").join("plugin.toml"),
        common::event_manifest("impostor"),
    )
    .expect("impostor manifest");

    let err = setup.reloader().reload("reloadme").unwrap_err();
    assert!(
        matches!(err, candyriya_plugin::PluginError::InvalidState { .. }),
        "unexpected error: {err}"
    );

    assert_eq!(setup.dispatch_reason(), "v1");
}

#[test]
fn a_disabled_plugin_stays_disabled_across_a_reload() {
    let fx = Fixture::new();
    common::write_plugin(
        &fx.plugins_dir,
        "reloadme",
        &common::response_plugin_wasm("{\"cancelled\":true,\"cancel_reason\":\"v1\"}"),
    );
    let report =
        PluginLoader::new(fx.plugins_dir.clone(), fx.libraries_dir.clone(), &fx.cache).load();
    let plugins: Vec<_> = report
        .plugins
        .into_iter()
        .map(|mut p| {
            p.disable().expect("disable");
            p
        })
        .collect();
    let set = PluginSet::from_vec(plugins);
    let services = ServiceRegistry::new();
    let reloader = PluginReloader::new(set.clone(), fx.cache_root.clone(), services.clone());

    std::fs::write(
        fx.plugins_dir.join("reloadme").join("reloadme.wasm"),
        common::response_plugin_wasm("{\"cancelled\":true,\"cancel_reason\":\"v2\"}"),
    )
    .expect("v2 wasm");

    let report = reloader.reload("reloadme").expect("reload succeeds");

    assert_eq!(report.state, PluginState::Disabled);
    assert_eq!(set.state("reloadme"), Some(PluginState::Disabled));
}

#[test]
fn a_failed_plugin_is_resurrected_by_a_reload() {
    let fx = Fixture::new();
    // the plugin arrives broken; fix the files, reload, it comes up
    common::write_plugin_with_manifest(
        &fx.plugins_dir,
        "reloadme",
        &common::event_manifest("reloadme"),
        b"\0asm garbage",
    );
    let report =
        PluginLoader::new(fx.plugins_dir.clone(), fx.libraries_dir.clone(), &fx.cache).load();
    assert!(!report.plugins[0].state().is_loaded());
    let set = PluginSet::from_vec(report.plugins);
    let services = ServiceRegistry::new();
    let reloader = PluginReloader::new(set.clone(), fx.cache_root.clone(), services);

    std::fs::write(
        fx.plugins_dir.join("reloadme").join("reloadme.wasm"),
        common::response_plugin_wasm("{\"cancelled\":false}"),
    )
    .expect("fixed wasm");

    let report = reloader.reload("reloadme").expect("reload succeeds");

    assert_eq!(report.state, PluginState::Loaded);
    assert_eq!(set.state("reloadme"), Some(PluginState::Loaded));
}

#[test]
fn reload_preserves_the_position_in_load_order() {
    let fx = Fixture::new();
    for id in ["aa-first", "bb-reloadme", "cc-last"] {
        common::write_plugin(
            &fx.plugins_dir,
            id,
            &common::response_plugin_wasm("{\"cancelled\":false}"),
        );
    }
    let report =
        PluginLoader::new(fx.plugins_dir.clone(), fx.libraries_dir.clone(), &fx.cache).load();
    let set = PluginSet::from_vec(report.plugins);
    let reloader = PluginReloader::new(set.clone(), fx.cache_root.clone(), ServiceRegistry::new());

    std::fs::write(
        fx.plugins_dir.join("bb-reloadme").join("bb-reloadme.wasm"),
        common::response_plugin_wasm("{\"cancelled\":true,\"cancel_reason\":\"fresh\"}"),
    )
    .expect("v2 wasm");

    reloader.reload("bb-reloadme").expect("reload succeeds");

    assert_eq!(set.ids(), ["aa-first", "bb-reloadme", "cc-last"]);
}

#[test]
fn watch_reloads_a_changed_plugin_through_the_task_service() {
    let setup = Setup::new();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .build()
        .expect("tokio runtime");
    let task_service = Arc::new(
        TaskService::builder()
            .handle(runtime.handle().clone())
            .blocking_threads(2)
            .build()
            .expect("task service"),
    );

    let watcher = Watch::new(
        setup.fx.plugins_dir.clone(),
        Arc::clone(&task_service),
        setup.reloader(),
    )
    .expect("watcher arms");

    // mutate the wasm on disk; the watcher observes it and schedules
    // plugin-reload-reloadme on the task service
    setup.write_v2_wasm();

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if setup.dispatch_reason() == "v2" {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the watcher did not reload the plugin within 10s"
        );
        std::thread::sleep(Duration::from_millis(50));
    }

    drop(watcher);
    // let the detached rayon task finish before the harness starts
    // tearing the fixture down
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(setup.plugins.state("reloadme"), Some(PluginState::Loaded));
}

#[test]
fn watch_rejects_a_missing_plugins_directory() {
    let setup = Setup::new();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .build()
        .expect("tokio runtime");
    let task_service = Arc::new(
        TaskService::builder()
            .handle(runtime.handle().clone())
            .blocking_threads(1)
            .build()
            .expect("task service"),
    );

    let err = Watch::new(
        setup.fx.plugins_dir.join("no-such-subdir"),
        task_service,
        setup.reloader(),
    )
    .unwrap_err();

    assert!(matches!(err, HotReloadError::Resolve { .. }));
}
