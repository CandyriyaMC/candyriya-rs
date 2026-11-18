//! Type-model tests for the controller core: the registry walks a plugin
//! through its lifecycle, refuses illegal jumps, and dumps snapshots for
//! ps/tree. No I/O, no async, no fixtures.

use std::path::PathBuf;

use candyriya_plugin_controller::{
    ALL_STATES, AckStatus, Generation, PluginAck, PluginManifest, PluginMsg, PluginRecord,
    PluginRegistry, PluginWireState, RegistryError,
};

fn demo_record(id: &str) -> PluginRecord {
    PluginRecord::from_manifest(
        &PluginManifest::new(
            id,
            "1.0.0",
            format!("{id} demo"),
            Some("a demo plugin".into()),
        ),
        format!("plugins/{id}/{id}.dll"),
    )
}

/// The full happy path, one step at a time.
const HAPPY_PATH: &[PluginWireState] = &[
    PluginWireState::Discovered,
    PluginWireState::Loading,
    PluginWireState::Loaded,
    PluginWireState::Initializing,
    PluginWireState::Initialized,
    PluginWireState::Enabling,
    PluginWireState::Running,
    PluginWireState::Disabling,
    PluginWireState::Disabled,
    PluginWireState::Unloading,
    PluginWireState::Unloaded,
];

#[test]
fn happy_path_walks_the_whole_lifecycle() {
    let registry = PluginRegistry::new();
    registry.insert(demo_record("greeting")).unwrap();

    // DISCOVERED is the landing state; legal steps consume the remaining 10
    for window in HAPPY_PATH.windows(2) {
        let from = window[0];
        let to = window[1];
        assert_eq!(registry.get("greeting").unwrap().state(), from);
        registry.set_state("greeting", to).unwrap();
    }
    assert_eq!(
        registry.get("greeting").unwrap().state(),
        PluginWireState::Unloaded
    );
    assert!(registry.get("greeting").unwrap().state().is_terminal());
}

#[test]
fn enable_disable_re_enable_cycle_is_legal() {
    let registry = PluginRegistry::new();
    registry.insert(demo_record("greeting")).unwrap();
    registry
        .set_state("greeting", PluginWireState::Loading)
        .unwrap();
    registry
        .set_state("greeting", PluginWireState::Loaded)
        .unwrap();
    registry
        .set_state("greeting", PluginWireState::Initializing)
        .unwrap();
    registry
        .set_state("greeting", PluginWireState::Initialized)
        .unwrap();
    registry
        .set_state("greeting", PluginWireState::Enabling)
        .unwrap();
    registry
        .set_state("greeting", PluginWireState::Running)
        .unwrap();
    registry
        .set_state("greeting", PluginWireState::Disabling)
        .unwrap();
    registry
        .set_state("greeting", PluginWireState::Disabled)
        .unwrap();
    registry
        .set_state("greeting", PluginWireState::Enabling)
        .unwrap();
    registry
        .set_state("greeting", PluginWireState::Running)
        .unwrap();
}

#[test]
fn teardown_from_a_live_state_lands_unloaded_via_mark_failed() {
    // once the plugin is up, the teardown hatch is: -> UNLOADING, then
    // mark_failed -> UNLOADED, reason preserved
    let registry = PluginRegistry::new();
    registry.insert(demo_record("doomed")).unwrap();
    for state in [
        PluginWireState::Loading,
        PluginWireState::Loaded,
        PluginWireState::Initializing,
        PluginWireState::Initialized,
        PluginWireState::Enabling,
        PluginWireState::Running,
    ] {
        registry.set_state("doomed", state).unwrap();
    }
    registry.set_reason("doomed", "fuel exhausted").unwrap();
    registry
        .set_state("doomed", PluginWireState::Unloading)
        .unwrap();
    registry.mark_failed("doomed").unwrap();
    let record = registry.get("doomed").unwrap();
    assert_eq!(record.state(), PluginWireState::Unloaded);
    assert_eq!(record.reason.as_deref(), Some("fuel exhausted"));
}

#[test]
fn terminal_unloaded_refuses_outgoing_transitions() {
    let registry = PluginRegistry::new();
    registry.insert(demo_record("greeting")).unwrap();
    registry
        .set_state("greeting", PluginWireState::Unloading)
        .unwrap();
    registry
        .set_state("greeting", PluginWireState::Unloaded)
        .unwrap();
    for target in ALL_STATES {
        assert!(
            registry.set_state("greeting", target).is_err(),
            "UNLOADED must not move to {target}"
        );
    }
}

#[test]
fn failure_path_never_touches_disabling() {
    // a plugin that dies at enable time goes RUNNING-less: ENABLING -> UNLOADING
    let registry = PluginRegistry::new();
    registry.insert(demo_record("doomed")).unwrap();
    registry
        .set_state("doomed", PluginWireState::Loading)
        .unwrap();
    registry
        .set_state("doomed", PluginWireState::Loaded)
        .unwrap();
    registry
        .set_state("doomed", PluginWireState::Initializing)
        .unwrap();
    registry
        .set_state("doomed", PluginWireState::Initialized)
        .unwrap();
    registry
        .set_state("doomed", PluginWireState::Enabling)
        .unwrap();
    registry
        .set_state("doomed", PluginWireState::Unloading)
        .unwrap();
    let err = registry
        .set_state("doomed", PluginWireState::Disabled)
        .unwrap_err();
    assert!(matches!(err, RegistryError::Transition { .. }));
}

#[test]
fn snapshot_dumps_ps_tree_copy_in_discovery_order() {
    let registry = PluginRegistry::new();
    for id in ["alpha", "beta", "gamma"] {
        registry.insert(demo_record(id)).unwrap();
    }
    let snap = registry.snapshot();
    let ids: Vec<&str> = snap.records().iter().map(|r| r.id.as_str()).collect();
    assert_eq!(ids, ["alpha", "beta", "gamma"]);

    // the snapshot is detached: mutating the registry afterwards does not
    // change what the dump captured
    registry
        .set_state("alpha", PluginWireState::Loading)
        .unwrap();
    let snap_again = registry.snapshot();
    assert_eq!(snap_again.records()[0].state(), PluginWireState::Loading);
    assert_eq!(snap.records()[0].state(), PluginWireState::Discovered);
}

#[test]
fn wire_types_shape() {
    let generation = Generation::ZERO.next();
    assert_eq!(generation.value(), 1);

    let msg = PluginMsg::Enable { generation };
    assert_eq!(msg.generation(), generation);

    let ack = PluginAck::Enable {
        generation,
        status: AckStatus::Success,
        duration_ms: 42,
    };
    assert!(ack.is_success());
    assert_eq!(ack.duration_ms(), 42);
    assert_eq!(ack.generation(), generation);

    let failed = PluginAck::Unload {
        generation,
        status: AckStatus::failure("join timed out"),
        duration_ms: 5_000,
    };
    assert!(!failed.is_success());
    assert_eq!(failed.status().to_string(), "failure(join timed out)");
}

#[test]
fn record_tracks_restart_and_exit_bookkeeping() {
    let registry = PluginRegistry::new();
    let mut record = demo_record("greeting");
    record.restart_count = 3;
    record.last_exit = Some(candyriya_plugin_controller::ExitStatus::Panicked);
    registry.insert(record).unwrap();

    let fetched = registry.get("greeting").unwrap();
    assert_eq!(fetched.restart_count, 3);
    assert_eq!(
        fetched.last_exit,
        Some(candyriya_plugin_controller::ExitStatus::Panicked)
    );
    assert_eq!(
        fetched.entry_point,
        PathBuf::from("plugins/greeting/greeting.dll")
    );
}
