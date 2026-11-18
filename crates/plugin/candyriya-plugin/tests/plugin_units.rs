//! Supervision-unit tests for the plugin subsystem.
//!
//! The "broken plugin" signal used to be contained but invisible: a wasm
//! trap ended up as a `PluginError` at the call site and nowhere else, so
//! no watcher could ever act on it. These tests pin the bridge end to
//! end: the health cell the wasm worker flags, the `plugin-<id>` unit
//! parked on that flag, the `Failure` exit it reports into the tree, and
//! the `OnFailure` policy that re-stages the plugin through the reload
//! machinery — bounded by the start limit, terminal after it.
//!
//! Fixtures are real extism plugins compiled from WAT at test time; no
//! network, no mocks. Fuel is set low on the trap fixtures so the wasm
//! runs out of it in microseconds.

mod common;

use std::time::Duration;

use candyriya_init::{
    ExitStatus, Init, InitOptions, Pid, RestartLimits, RestartPolicy, ServiceKind, UnitKind,
    UnitSpec, UnitState,
};
use candyriya_plugin::{
    EventDispatcher, Limits, PluginHealthSet, PluginLoader, PluginReloader, PluginSet,
    PluginUnitContext, ProxyEventDispatcher, ServiceRegistry, sync_plugin_units,
};
use candyriya_plugin_api::{EventEnvelope, EventKind, ProxyEvent};
use candyriya_scheduler::TaskService;

/// A WAT module whose `on_event` never returns — it spins forever, which
/// is exactly what a low fuel budget is for.
const SPIN_WAT: &str = r#"
(module
  (memory (export "memory") 1)
  (func (export "on_event") (result i32)
    (loop $spin
      (br $spin))
    (i32.const 0)))
"#;

/// A manifest for a test plugin whose `plugin_main` is `on_event`.
fn manifest(id: &str) -> String {
    format!(
        "id = \"{id}\"\nversion = \"0.1.0\"\nname = \"Fixture {id}\"\nplugin_main = \"on_event\"\nplugin_api = \"0.1\"\n"
    )
}

fn spin_wasm() -> Vec<u8> {
    wat::parse_str(SPIN_WAT).expect("spin fixture wat compiles")
}

/// A plugin that answers `{"cancelled":false}` from `on_event`.
fn healthy_wasm() -> Vec<u8> {
    common::response_plugin_wasm("{\"cancelled\":false}")
}

fn limits_with_fuel(fuel: u64) -> Limits {
    Limits {
        max_fuel: fuel,
        ..Limits::new()
    }
}

fn login_event() -> ProxyEvent {
    ProxyEvent::new(EventEnvelope::new(
        1,
        EventKind::Login {
            uuid: "069a79f4-44e9-4726-a5be-fca90e38aaf5".to_owned(),
            name: "Notch".to_owned(),
            protocol_version: 769,
        },
        1_730_000_000_000,
    ))
}

/// A tree whose restart backoff collapses to milliseconds so the
/// supervisor's retries are observable in a test budget.
fn test_tree(max_attempts: u32) -> Init {
    let service = TaskService::builder()
        .handle(tokio::runtime::Handle::current())
        .blocking_threads(2)
        .build()
        .expect("service builds");
    let options = InitOptions {
        restart_limits: RestartLimits {
            base: Duration::from_millis(2),
            cap: Duration::from_millis(10),
            max_attempts,
            window: Duration::from_secs(600),
            jitter: 0.0,
        },
        ..InitOptions::default()
    };
    Init::with_options(service, options)
}

fn spawn_group(tree: &Init) -> Pid {
    tree.spawn_unit(
        Pid::INIT,
        UnitSpec::new(
            "plugins",
            UnitKind::LongLived,
            RestartPolicy::Never,
            std::future::pending::<ExitStatus>,
        )
        .with_service_kind(ServiceKind::Plugins),
    )
    .expect("plugins group spawns")
}

fn loader_with(fx: &common::Fixture, limits: Limits, health: PluginHealthSet) -> PluginLoader<'_> {
    PluginLoader::with_limits(
        fx.plugins_dir.clone(),
        fx.libraries_dir.clone(),
        &fx.cache,
        limits,
    )
    .with_plugin_health(health)
}

fn reloader_with(
    set: &PluginSet,
    fx: &common::Fixture,
    limits: Limits,
    health: PluginHealthSet,
) -> PluginReloader {
    PluginReloader::with_limits(
        set.clone(),
        fx.cache_root.clone(),
        ServiceRegistry::new(),
        limits,
    )
    .with_plugin_health(health)
}

fn unit_ctx(tree: &Init, reloader: PluginReloader, health: &PluginHealthSet) -> PluginUnitContext {
    PluginUnitContext {
        service: tree.service().clone(),
        reloader,
        health: health.clone(),
    }
}

fn unit_pid(tree: &Init, name: &str) -> Option<Pid> {
    tree.units()
        .into_iter()
        .find(|u| u.name == name)
        .map(|u| u.pid)
}

async fn poll_until(
    mut predicate: impl FnMut() -> bool,
    deadline: Duration,
    step: Duration,
) -> bool {
    let start = std::time::Instant::now();
    let mut heartbeats = 0u64;
    loop {
        if predicate() {
            return true;
        }
        let elapsed_ms = start.elapsed().as_millis();
        heartbeats += 1;
        if elapsed_ms % 5000 < step.as_millis() * 2 {
            eprintln!(
                "[diag] poll_until heartbeat #{heartbeats}: elapsed={:?}",
                start.elapsed()
            );
        }
        if start.elapsed() > deadline {
            return false;
        }
        tokio::time::sleep(step).await;
    }
}

fn write_spinner(fx: &common::Fixture, id: &str) {
    common::write_plugin(&fx.plugins_dir, id, &spin_wasm());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn loading_a_plugin_spawns_a_unit_under_the_plugins_group() {
    let fx = common::Fixture::new();
    common::write_plugin(&fx.plugins_dir, "greet", &healthy_wasm());
    let health = PluginHealthSet::new();
    let limits = limits_with_fuel(0);

    let report = loader_with(&fx, limits.clone(), health.clone()).load();
    let set = PluginSet::from_vec(report.plugins);
    let tree = test_tree(5);
    let plugins_pid = spawn_group(&tree);
    let ctx = unit_ctx(
        &tree,
        reloader_with(&set, &fx, limits, health.clone()),
        &health,
    );

    let units = sync_plugin_units(&tree, plugins_pid, &set, &ctx);
    assert_eq!(units.spawned, vec!["greet".to_owned()]);
    assert!(units.cancelled.is_empty());
    assert!(units.failed.is_empty());

    let unit = tree
        .units()
        .into_iter()
        .find(|u| u.name == "plugin-greet")
        .expect("the plugin unit exists");
    assert_eq!(unit.ppid, plugins_pid);
    assert_eq!(unit.state, UnitState::Running);
    assert_eq!(unit.kind, UnitKind::LongLived);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sync_is_idempotent_and_marks_units_gone_when_the_set_empties() {
    let fx = common::Fixture::new();
    common::write_plugin(&fx.plugins_dir, "greet", &healthy_wasm());
    common::write_plugin(&fx.plugins_dir, "helper", &healthy_wasm());
    let health = PluginHealthSet::new();
    let limits = limits_with_fuel(0);

    let report = loader_with(&fx, limits.clone(), health.clone()).load();
    let set = PluginSet::from_vec(report.plugins);
    let tree = test_tree(5);
    let plugins_pid = spawn_group(&tree);
    let ctx = unit_ctx(
        &tree,
        reloader_with(&set, &fx, limits, health.clone()),
        &health,
    );

    let first = sync_plugin_units(&tree, plugins_pid, &set, &ctx);
    let mut ids = first.spawned;
    ids.sort();
    assert_eq!(ids, vec!["greet".to_owned(), "helper".to_owned()]);

    // a second sync changes nothing and spawns nothing
    let second = sync_plugin_units(&tree, plugins_pid, &set, &ctx);
    assert!(second.spawned.is_empty());
    assert!(second.cancelled.is_empty());

    // the set the proxy now holds lost both plugins: the units are
    // cancelled and harvested by the group reaper
    let empty = PluginSet::new();
    let gone = sync_plugin_units(&tree, plugins_pid, &empty, &ctx);
    let mut ids = gone.cancelled;
    ids.sort();
    assert_eq!(ids, vec!["greet".to_owned(), "helper".to_owned()]);

    // the parked bodies exit at their cancellation boundary; the group
    // reaper harvests them as they do
    let harvested = poll_until(
        || {
            tree.reap_children(plugins_pid);
            tree.units().iter().all(|u| !u.name.starts_with("plugin-"))
        },
        Duration::from_secs(2),
        Duration::from_millis(2),
    )
    .await;
    assert!(harvested, "cancelled plugin units left the registry");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_fuel_trap_fails_the_unit_and_restart_re_stages_from_disk() {
    let fx = common::Fixture::new();
    write_spinner(&fx, "spinner");
    let health = PluginHealthSet::new();
    let limits = limits_with_fuel(10_000);

    let report = loader_with(&fx, limits.clone(), health.clone()).load();
    let set = PluginSet::from_vec(report.plugins);
    let tree = test_tree(5);
    let plugins_pid = spawn_group(&tree);
    let ctx = unit_ctx(
        &tree,
        reloader_with(&set, &fx, limits, health.clone()),
        &health,
    );
    let units = sync_plugin_units(&tree, plugins_pid, &set, &ctx);
    assert_eq!(units.spawned, vec!["spinner".to_owned()]);

    let dispatcher = ProxyEventDispatcher::new(set.clone());
    let _ = dispatcher.dispatch_event(&login_event());
    assert!(
        health.cell("spinner").is_broken(),
        "the worker flags the plugin when fuel runs out"
    );

    let pid = unit_pid(&tree, "plugin-spinner").expect("unit exists");
    let saw_zombie = poll_until(
        || tree.unit(pid).is_some_and(|u| u.state == UnitState::Zombie),
        Duration::from_secs(2),
        Duration::from_millis(2),
    )
    .await;
    assert!(saw_zombie, "the parked body reported its failure");

    tree.reap_children(plugins_pid);

    // generation on disk is now a fixed plugin: the restart must restage
    // it from the files, not resurrect the old broken instance
    common::write_plugin_with_manifest(
        &fx.plugins_dir,
        "spinner",
        &manifest("spinner"),
        &healthy_wasm(),
    );

    let saw_restart = poll_until(
        || {
            tree.unit(pid)
                .is_some_and(|u| u.state == UnitState::Running && u.restart_count >= 1)
        },
        Duration::from_secs(8),
        Duration::from_millis(5),
    )
    .await;
    assert!(
        saw_restart,
        "the OnFailure policy restaged the plugin through the reload path: {:?}",
        tree.unit(pid)
    );
    let restaged = poll_until(
        || !health.cell("spinner").is_broken(),
        Duration::from_secs(8),
        Duration::from_millis(5),
    )
    .await;
    assert!(restaged, "a fresh instance clears the conviction");

    // the restage runs on the blocking pool; is_broken clears when the
    // new instance is built but the record swap may not be visible yet —
    // poll the dispatch itself until the new generation is talking
    let reloaded_and_swapped = poll_until(
        || {
            dispatcher
                .dispatch_event(&login_event())
                .per_plugin_outcomes
                .iter()
                .any(|(id, outcome)| {
                    id == "spinner" && matches!(outcome, candyriya_plugin_api::Outcome::Allowed)
                })
        },
        Duration::from_secs(8),
        Duration::from_millis(20),
    )
    .await;
    assert!(
        reloaded_and_swapped,
        "the re-staged plugin runs the new code from disk"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn repeated_traps_reach_the_start_limit_and_stay_visible() {
    let fx = common::Fixture::new();
    write_spinner(&fx, "spinner");
    let health = PluginHealthSet::new();
    let limits = limits_with_fuel(10_000);

    let report = loader_with(&fx, limits.clone(), health.clone()).load();
    let set = PluginSet::from_vec(report.plugins);
    let tree = test_tree(2);
    let plugins_pid = spawn_group(&tree);
    let ctx = unit_ctx(
        &tree,
        reloader_with(&set, &fx, limits, health.clone()),
        &health,
    );
    let units = sync_plugin_units(&tree, plugins_pid, &set, &ctx);
    assert_eq!(units.spawned, vec!["spinner".to_owned()]);

    let dispatcher = ProxyEventDispatcher::new(set.clone());
    let pid = unit_pid(&tree, "plugin-spinner").expect("unit exists");

    // two restarts are allowed, the third failure is terminal
    for expected_attempt in 1..=3u32 {
        eprintln!(
            "[diag] plugin state before dispatch: {:?}",
            set.state("spinner")
        );
        let outcome = dispatcher.dispatch_event(&login_event());
        eprintln!("[diag] attempt {expected_attempt} dispatch outcome: {outcome:?}");
        // CI ubuntu runners are shared/throttled and each restart restages a
        // WAT-compiled wasm instance; attempts 1+2 alone can consume most of a
        // minute before the third trap lands. The assertions are unchanged;
        // only the wait ceiling is generous.
        let saw_zombie = poll_until(
            || tree.unit(pid).is_some_and(|u| u.state == UnitState::Zombie),
            Duration::from_secs(180),
            Duration::from_millis(2),
        )
        .await;
        assert!(
            saw_zombie,
            "attempt {expected_attempt}: the body reported its failure"
        );
        tree.reap_children(plugins_pid);
        if expected_attempt < 3 {
            let saw_restart = poll_until(
                || {
                    tree.unit(pid).is_some_and(|u| {
                        u.state == UnitState::Running && u.restart_count >= expected_attempt
                    })
                },
                Duration::from_secs(8),
                Duration::from_millis(5),
            )
            .await;
            assert!(
                saw_restart,
                "attempt {expected_attempt}: the restart happened: {:?}",
                tree.unit(pid)
            );
        }
    }

    let unit = tree.unit(pid).expect("the terminal unit is still visible");
    assert_eq!(unit.state, UnitState::FailedTerminal);
    assert!(unit.restart_count >= 2);
    assert!(matches!(unit.last_exit, Some(ExitStatus::Failure(_))));
    // terminal units are never reaped away: ps shows them forever
    let all: Vec<_> = tree.units().iter().map(|u| u.name.clone()).collect();
    assert!(all.iter().any(|n| n == "plugin-spinner"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_plugin_gets_a_bounded_restage_path_to_terminal() {
    // the plugin is broken on disk: plugin_main is declared but absent
    let fx = common::Fixture::new();
    common::write_plugin(&fx.plugins_dir, "broken", &common::no_main_wasm());
    let health = PluginHealthSet::new();
    let limits = limits_with_fuel(0);

    let report = loader_with(&fx, limits.clone(), health.clone()).load();
    assert!(matches!(
        report.plugins[0].state(),
        candyriya_plugin::PluginState::Failed(_)
    ));
    let set = PluginSet::from_vec(report.plugins);
    let tree = test_tree(1);
    let plugins_pid = spawn_group(&tree);
    let ctx = unit_ctx(
        &tree,
        reloader_with(&set, &fx, limits, health.clone()),
        &health,
    );
    let units = sync_plugin_units(&tree, plugins_pid, &set, &ctx);
    assert_eq!(units.spawned, vec!["broken".to_owned()]);

    let pid = unit_pid(&tree, "plugin-broken").expect("unit exists");
    let went_terminal = poll_until(
        || {
            tree.reap_children(plugins_pid);
            unit_pid(&tree, "plugin-broken")
                .and_then(|pid| tree.unit(pid))
                .is_some_and(|u| u.state == UnitState::FailedTerminal)
        },
        Duration::from_secs(8),
        Duration::from_millis(2),
    )
    .await;
    assert!(
        went_terminal,
        "the bounded restage path runs to its limit: {:?}",
        tree.unit(pid)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reload_picks_up_a_newcomer_and_sync_spawns_its_unit() {
    let fx = common::Fixture::new();
    common::write_plugin(&fx.plugins_dir, "greeter", &healthy_wasm());
    let health = PluginHealthSet::new();
    let limits = limits_with_fuel(0);

    let report = loader_with(&fx, limits.clone(), health.clone()).load();
    let set = PluginSet::from_vec(report.plugins);
    let tree = test_tree(5);
    let plugins_pid = spawn_group(&tree);
    let ctx = unit_ctx(
        &tree,
        reloader_with(&set, &fx, limits.clone(), health.clone()),
        &health,
    );
    let units = sync_plugin_units(&tree, plugins_pid, &set, &ctx);
    assert_eq!(units.spawned, vec!["greeter".to_owned()]);

    // a new plugin lands on disk: `/candyriya reload plugins` registers it
    common::write_plugin(&fx.plugins_dir, "helper", &healthy_wasm());
    let reloader = reloader_with(&set, &fx, limits, health.clone());
    let loader = loader_with(&fx, limits_with_fuel(0), health);
    let reload_report = reloader.reload_all(&loader);
    assert!(
        reload_report.loaded.contains(&"helper".to_owned()),
        "the newcomer was registered: {reload_report:?}"
    );

    let units = sync_plugin_units(&tree, plugins_pid, &set, &ctx);
    assert_eq!(units.spawned, vec!["helper".to_owned()]);
    let helper = tree
        .units()
        .into_iter()
        .find(|u| u.name == "plugin-helper")
        .expect("the newcomer has its unit");
    assert_eq!(helper.ppid, plugins_pid);
    assert_eq!(helper.state, UnitState::Running);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unhealthy_plugin_can_be_killed_with_the_rest_of_the_tree() {
    // the whole tree sweep stops the supervision unit; plugin threads are
    // owned by the set and outlive nothing that drops it — there is no
    // path by which a cancelled unit leaves a running task behind
    let fx = common::Fixture::new();
    write_spinner(&fx, "spinner");
    let health = PluginHealthSet::new();
    let limits = limits_with_fuel(10_000);

    let report = loader_with(&fx, limits.clone(), health.clone()).load();
    let set = PluginSet::from_vec(report.plugins);
    let tree = test_tree(5);
    let plugins_pid = spawn_group(&tree);
    let ctx = unit_ctx(
        &tree,
        reloader_with(&set, &fx, limits, health.clone()),
        &health,
    );
    let units = sync_plugin_units(&tree, plugins_pid, &set, &ctx);
    assert_eq!(units.spawned, vec!["spinner".to_owned()]);

    let swept = tree.cancel_all();
    assert!(swept.contains(&plugins_pid));
    assert!(
        unit_pid(&tree, "plugin-spinner").is_some(),
        "a cancelled unit still exists until the sweep drops its body"
    );
    tree.reap_children(plugins_pid);
    // after the drop the exit is Cancelled (not Failure — a requested
    // stop is never second-guessed by the restart policy)
    let unit = tree
        .units()
        .into_iter()
        .find(|u| u.name == "plugin-spinner");
    // the registry may already have removed it; what it must never show
    // is a restart attempt triggered by the requested stop
    if let Some(unit) = unit {
        assert_eq!(unit.restart_count, 0);
    }
}

#[test]
fn unloading_a_plugin_joins_its_worker_thread() {
    let fx = common::Fixture::new();
    common::write_plugin(&fx.plugins_dir, "greeter", &healthy_wasm());
    let limits = limits_with_fuel(0);
    let report = loader_with(&fx, limits, PluginHealthSet::new()).load();
    let mut plugins = report.plugins;
    let plugin = plugins.remove(0);
    // unload joins the worker thread: no detached thread survives
    plugin.unload().expect("the worker thread joined");
}
