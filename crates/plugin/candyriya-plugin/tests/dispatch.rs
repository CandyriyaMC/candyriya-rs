//! Event dispatcher behaviour: load-order fanout, cancellation wins,
//! per-plugin timeouts, payload crossing the ABI.
//!
//! Fixtures are real extism plugins compiled from WAT at test time.

mod common;

use std::time::Duration;

use candyriya_plugin::{
    LibraryPlugin, Outcome, PluginLoader, PluginSet, PluginState, ProxyEventDispatcher,
};
use candyriya_plugin_api::{EventEnvelope, EventKind, ProxyEvent};

use common::Fixture;

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

fn load_set(fx: &Fixture, plugins: &[(&str, Vec<u8>)]) -> PluginSet {
    for (id, wasm) in plugins {
        common::write_plugin(&fx.plugins_dir, id, wasm);
    }
    let report =
        PluginLoader::new(fx.plugins_dir.clone(), fx.libraries_dir.clone(), &fx.cache).load();
    assert!(
        report.issues.is_empty(),
        "unexpected issues: {:?}",
        report.issues
    );
    PluginSet::from_vec(report.plugins)
}

#[test]
fn every_loaded_plugin_is_asked_in_load_order() {
    let fx = Fixture::new();
    // alphabetical within plugins/ is the load order
    let set = load_set(
        &fx,
        &[
            (
                "alpha",
                common::response_plugin_wasm("{\"cancelled\":false}"),
            ),
            (
                "beta",
                common::response_plugin_wasm("{\"cancelled\":false}"),
            ),
            (
                "gamma",
                common::response_plugin_wasm(
                    "{\"cancelled\":true,\"cancel_reason\":\"gamma says no\"}",
                ),
            ),
        ],
    );
    let dispatcher = ProxyEventDispatcher::new(set);

    let result = dispatcher.dispatch_cancellable(&login_event());

    let ids: Vec<&str> = result
        .per_plugin_outcomes
        .iter()
        .map(|(id, _)| id.as_str())
        .collect();
    assert_eq!(ids, ["alpha", "beta", "gamma"]);
    assert!(result.cancelled);
    assert_eq!(result.cancel_reason.as_deref(), Some("gamma says no"));
    assert_eq!(
        result.outcome_for("alpha"),
        Some(&Outcome::Allowed),
        "plugins before the cancelling one ran and were not cancelled"
    );
    assert_eq!(
        result.outcome_for("gamma"),
        Some(&Outcome::Cancelled {
            cancel_reason: Some("gamma says no".to_owned()),
        })
    );
}

#[test]
fn later_outcomes_are_included_after_a_cancel() {
    let fx = Fixture::new();
    // the cancelling plugin comes first; everything after it must still
    // be asked and listed — load order is execution order
    let set = load_set(
        &fx,
        &[
            (
                "a-cancels",
                common::response_plugin_wasm("{\"cancelled\":true,\"cancel_reason\":\"nope\"}"),
            ),
            (
                "b-allows",
                common::response_plugin_wasm("{\"cancelled\":false}"),
            ),
            (
                "c-allows",
                common::response_plugin_wasm("{\"cancelled\":false}"),
            ),
        ],
    );
    let dispatcher = ProxyEventDispatcher::new(set);

    let result = dispatcher.dispatch_cancellable(&login_event());

    assert!(result.cancelled);
    assert_eq!(result.cancel_reason.as_deref(), Some("nope"));
    assert_eq!(result.per_plugin_outcomes.len(), 3);
    assert_eq!(result.allowed_count(), 2);
    assert_eq!(
        result.outcome_for("b-allows"),
        Some(&Outcome::Allowed),
        "a plugin listed after a cancelling one still gets asked"
    );
}

#[test]
fn first_cancelling_reason_wins() {
    let fx = Fixture::new();
    let set = load_set(
        &fx,
        &[
            (
                "a-first",
                common::response_plugin_wasm("{\"cancelled\":true,\"cancel_reason\":\"first\"}"),
            ),
            (
                "b-second",
                common::response_plugin_wasm("{\"cancelled\":true,\"cancel_reason\":\"second\"}"),
            ),
        ],
    );
    let dispatcher = ProxyEventDispatcher::new(set);

    let result = dispatcher.dispatch_cancellable(&login_event());

    assert!(result.cancelled);
    assert_eq!(
        result.cancel_reason.as_deref(),
        Some("first"),
        "the reason from the first cancelling plugin in load order is reported"
    );
}

#[test]
fn cancellation_from_any_position_cancels() {
    let fx = Fixture::new();
    for n in 0..3 {
        let allow = common::response_plugin_wasm("{\"cancelled\":false}");
        let cancel = common::response_plugin_wasm("{\"cancelled\":true}");
        let mut plugins = vec![
            ("p0", allow.clone()),
            ("p1", allow.clone()),
            ("p2", allow.clone()),
        ];
        plugins[n].1 = cancel;

        let set = load_set(&fx, &plugins);
        let dispatcher = ProxyEventDispatcher::new(set);
        let result = dispatcher.dispatch_cancellable(&login_event());

        assert!(result.cancelled, "a cancel at position {n} must register");
        assert_eq!(
            result.outcome_for(&format!("p{n}")),
            Some(&Outcome::Cancelled {
                cancel_reason: None,
            })
        );
    }
}

#[test]
fn a_plugin_that_times_out_is_skipped_and_does_not_block_cancellation() {
    let fx = Fixture::new();
    let set = load_set(
        &fx,
        &[
            ("aa-slow", common::slow_plugin_wasm()),
            (
                "zz-cancels",
                common::response_plugin_wasm(
                    "{\"cancelled\":true,\"cancel_reason\":\"backend said no\"}",
                ),
            ),
        ],
    );
    // 1.5s: the infinite wasm loop will never make it within 1.5s either,
    // and the CI runner (which is slower under logging pressure) gets
    // enough headroom to finish the cancels plugin before the deadline.
    let dispatcher = ProxyEventDispatcher::with_timeout(set, Duration::from_millis(1500));

    let result = dispatcher.dispatch_cancellable(&login_event());

    assert_eq!(result.outcome_for("aa-slow"), Some(&Outcome::Timeout));
    assert_eq!(
        result.outcome_for("zz-cancels"),
        Some(&Outcome::Cancelled {
            cancel_reason: Some("backend said no".to_owned()),
        })
    );
    assert!(
        result.cancelled,
        "a timed-out plugin must not mask a real cancellation"
    );
    assert_eq!(result.cancel_reason.as_deref(), Some("backend said no"));
}

#[test]
fn a_timed_out_plugin_does_not_cancel_the_event() {
    let fx = Fixture::new();
    let set = load_set(&fx, &[("only-slow", common::slow_plugin_wasm())]);
    let dispatcher = ProxyEventDispatcher::with_timeout(set, Duration::from_millis(1));

    let result = dispatcher.dispatch_cancellable(&login_event());

    assert_eq!(result.outcome_for("only-slow"), Some(&Outcome::Timeout));
    assert!(
        !result.cancelled,
        "timeout means not-cancelled, not cancelled"
    );
    assert_eq!(result.cancel_reason, None);
}

#[test]
fn an_undecodable_outcome_is_reported_and_does_not_cancel() {
    let fx = Fixture::new();
    let set = load_set(
        &fx,
        &[(
            "broken-answer",
            common::response_plugin_wasm("this is definitely not json"),
        )],
    );
    let dispatcher = ProxyEventDispatcher::new(set);

    let result = dispatcher.dispatch_cancellable(&login_event());

    match result.outcome_for("broken-answer") {
        Some(Outcome::Failed { error }) => {
            assert!(error.contains("undecodable"), "unexpected error: {error}")
        }
        other => panic!("expected Failed, got {other:?}"),
    }
    assert!(!result.cancelled);
}

#[test]
fn the_event_payload_reaches_the_plugin() {
    let fx = Fixture::new();
    let set = load_set(&fx, &[("sniffer", common::sniff_plugin_wasm())]);
    let dispatcher = ProxyEventDispatcher::new(set);

    // the sniff fixture cancels only when the input it receives starts
    // with '{' — which a serialized EventEnvelope always does
    let result = dispatcher.dispatch_cancellable(&login_event());

    assert_eq!(
        result.outcome_for("sniffer"),
        Some(&Outcome::Cancelled {
            cancel_reason: None,
        }),
        "the serialized envelope must reach the wasm boundary"
    );
}

#[test]
fn disabled_plugins_are_not_dispatched_to_and_not_listed() {
    let fx = Fixture::new();
    for (id, wasm) in [
        (
            "keep",
            common::response_plugin_wasm("{\"cancelled\":false}"),
        ),
        ("off", common::response_plugin_wasm("{\"cancelled\":true}")),
    ] {
        common::write_plugin(&fx.plugins_dir, id, &wasm);
    }
    let report =
        PluginLoader::new(fx.plugins_dir.clone(), fx.libraries_dir.clone(), &fx.cache).load();
    let plugins: Vec<LibraryPlugin> = report
        .plugins
        .into_iter()
        .map(|mut p| {
            if p.id() == "off" {
                p.disable().expect("disable");
            }
            p
        })
        .collect();
    let set = PluginSet::from_vec(plugins);
    let dispatcher = ProxyEventDispatcher::new(set);

    let result = dispatcher.dispatch_cancellable(&login_event());

    let ids: Vec<&str> = result
        .per_plugin_outcomes
        .iter()
        .map(|(id, _)| id.as_str())
        .collect();
    assert_eq!(ids, ["keep"], "a disabled plugin is skipped entirely");
    assert!(!result.cancelled, "the disabled plugin cannot cancel");
}

#[test]
fn failed_plugins_are_not_dispatched_to_and_not_listed() {
    let fx = Fixture::new();
    // compiles nothing: the module is garbage, the loader marks it Failed
    common::write_plugin(&fx.plugins_dir, "broken", b"\0asm\xde\xad\xbe\xef not wasm");
    common::write_plugin(
        &fx.plugins_dir,
        "good",
        &common::response_plugin_wasm("{\"cancelled\":false}"),
    );
    let report =
        PluginLoader::new(fx.plugins_dir.clone(), fx.libraries_dir.clone(), &fx.cache).load();
    let states: Vec<(&str, &PluginState)> =
        report.plugins.iter().map(|p| (p.id(), p.state())).collect();
    assert!(
        states
            .iter()
            .any(|(id, state)| *id == "broken" && !state.is_loaded())
    );

    let dispatcher = ProxyEventDispatcher::new(PluginSet::from_vec(report.plugins));
    let result = dispatcher.dispatch_cancellable(&login_event());

    let ids: Vec<&str> = result
        .per_plugin_outcomes
        .iter()
        .map(|(id, _)| id.as_str())
        .collect();
    assert_eq!(ids, ["good"]);
}
