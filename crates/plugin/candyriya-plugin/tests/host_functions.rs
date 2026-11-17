//! Host functions: the `candyriya:*` surface end to end.
//!
//! Fixtures are real extism plugins compiled from WAT at test time; every
//! byte crosses the WASM boundary for real. Direct
//! [`PluginInstance`] construction is used where a test needs the raw
//! host-function answer; the loader + dispatcher path is used where a test
//! needs event wiring.
//!
//! No network, no mocks.

mod common;

use std::path::PathBuf;
use std::sync::Arc;

use candyriya_plugin::{
    DISPATCHER_SERVICE_KEY, HostState, Limits, Outcome, PLAYERS_PING_SINK_KEY, PLAYERS_ROSTER_KEY,
    PlayerPingSink, PluginInstance, PluginLoader, PluginSet, ProxyEventDispatcher,
    SERVERS_NAMES_KEY, ServiceRegistry,
};
use candyriya_plugin_api::{EventEnvelope, EventKind, PlayerRef, ProxyEvent};

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

fn connection_kicked_event() -> ProxyEvent {
    ProxyEvent::new(EventEnvelope::new(
        1,
        EventKind::ConnectionKicked {
            uuid: "069a79f4-44e9-4726-a5be-fca90e38aaf5".to_owned(),
            name: "Notch".to_owned(),
            reason: "you got kicked".to_owned(),
        },
        1_730_000_000_000,
    ))
}

fn write_wasm(temp: &tempfile::TempDir, name: &str, bytes: &[u8]) -> PathBuf {
    let path = temp.path().join(name);
    std::fs::write(&path, bytes).expect("wasm fixture written");
    path
}

fn direct_instance(
    temp: &tempfile::TempDir,
    id: &str,
    wasm: &[u8],
    services: Arc<ServiceRegistry>,
) -> PluginInstance {
    let wasm_path = write_wasm(temp, &format!("{id}.wasm"), wasm);
    // fresh namespace per instance: plugins own their derived directories
    let state = HostState::new(temp.path().join(format!("data-{id}")), id).with_services(services);
    PluginInstance::new(&wasm_path, state, id, &Limits::new()).expect("instance builds")
}

fn load_with_services(
    fx: &Fixture,
    plugins: &[(&str, Vec<u8>)],
    services: ServiceRegistry,
) -> PluginSet {
    for (id, wasm) in plugins {
        common::write_plugin(&fx.plugins_dir, id, wasm);
    }
    let report = PluginLoader::new(fx.plugins_dir.clone(), fx.libraries_dir.clone(), &fx.cache)
        .with_services(services)
        .load();
    assert!(
        report.issues.is_empty(),
        "unexpected issues: {:?}",
        report.issues
    );
    PluginSet::from_vec(report.plugins)
}

fn output_json(bytes: &[u8]) -> serde_json::Value {
    serde_json::from_slice(bytes).expect("host function answered with JSON")
}

#[test]
fn hello_world_roundtrips_through_the_host_function() {
    let temp = tempfile::tempdir().expect("tempdir");
    let services = Arc::new(ServiceRegistry::new());
    let wasm = common::echo_host_fn_wasm("candyriya", "helloWorld");
    let instance = direct_instance(&temp, "greeter", &wasm, services);

    let out = instance.call("on_event", b"Steve").expect("call works");

    assert_eq!(out, b"hello, Steve");
}

#[test]
fn get_locale_returns_en_by_default() {
    let temp = tempfile::tempdir().expect("tempdir");
    let services = Arc::new(ServiceRegistry::new());
    let wasm = common::echo_host_fn_wasm("candyriya", "getLocale");
    let instance = direct_instance(&temp, "localed", &wasm, services);

    let out = instance.call("on_event", b"").expect("call works");

    assert_eq!(out, b"en");
}

#[test]
fn get_locale_returns_the_configured_locale_when_overridden() {
    let temp = tempfile::tempdir().expect("tempdir");
    let services = Arc::new(ServiceRegistry::new());
    let wasm = common::echo_host_fn_wasm("candyriya", "getLocale");
    let wasm_path = write_wasm(&temp, "override-locale.wasm", &wasm);
    let state = HostState::new(temp.path().join("data-override"), "override-locales")
        .with_services(services)
        .with_locale("ex_girl");
    let instance = PluginInstance::new(&wasm_path, state, "override-locales", &Limits::new())
        .expect("instance builds");

    let out = instance.call("on_event", b"").expect("call works");

    assert_eq!(out, b"ex_girl");
}

#[test]
fn switch_player_returns_the_not_implemented_yet_sentinel() {
    let temp = tempfile::tempdir().expect("tempdir");
    let services = Arc::new(ServiceRegistry::new());
    let wasm = common::echo_host_fn_wasm("candyriya", "switchPlayer");
    let instance = direct_instance(&temp, "switcher-sent", &wasm, services);

    let out = instance
        .call("on_event", br#"{"name": "Steve", "server": "local"}"#)
        .expect("call works");
    let json = output_json(&out);

    assert_eq!(json["ok"], false);
    assert_eq!(json["error"], "not implemented yet");
}

#[test]
fn call_api_of_a_non_loaded_plugin_reports_plugin_not_loaded() {
    let temp = tempfile::tempdir().expect("tempdir");
    let services = ServiceRegistry::new();
    // a dispatcher over an empty set answers "plugin not loaded" for anything
    let dispatcher = Arc::new(ProxyEventDispatcher::new(PluginSet::from_vec(vec![])));
    services.register_service(DISPATCHER_SERVICE_KEY.to_owned(), dispatcher);
    let services = Arc::new(services);

    let wasm = common::echo_host_fn_wasm("candyriya", "callApi");
    let instance = direct_instance(&temp, "api-caller", &wasm, services);

    let out = instance
        .call(
            "on_event",
            br#"{"target_plugin": "ghost-plugin", "fn": "hello_world", "args": []}"#,
        )
        .expect("call works");
    let json = output_json(&out);

    assert_eq!(json["ok"], false);
    assert_eq!(json["error"], "plugin not loaded");
}

#[test]
fn call_api_routes_into_a_target_that_exports_the_function() {
    let fx = Fixture::new();
    let services = ServiceRegistry::new();
    // the target exports hello_world only
    let target_manifest = "id = \"targeter\"\nversion = \"0.1.0\"\nname = \"Fixture targeter\"\nplugin_main = \"hello_world\"\nplugin_api = \"0.1\"\n[api]\nexports = [ { name = \"hello_world\" } ]\n";
    common::write_plugin_with_manifest(
        &fx.plugins_dir,
        "targeter",
        target_manifest,
        &common::static_export_plugin_wasm("hello_world", "howdy"),
    );
    let report = PluginLoader::new(fx.plugins_dir.clone(), fx.libraries_dir.clone(), &fx.cache)
        .with_services(services.clone())
        .load();
    assert!(report.issues.is_empty(), "issues: {:?}", report.issues);
    let dispatcher = Arc::new(ProxyEventDispatcher::new(PluginSet::from_vec(
        report.plugins,
    )));
    services.register_service(DISPATCHER_SERVICE_KEY.to_owned(), dispatcher);

    let temp = tempfile::tempdir().expect("tempdir");
    let wasm = common::echo_host_fn_wasm("candyriya", "callApi");
    let instance = direct_instance(&temp, "api-caller-2", &wasm, Arc::new(services));

    let out = instance
        .call(
            "on_event",
            br#"{"target_plugin": "targeter", "fn": "hello_world", "args": ["Steve"]}"#,
        )
        .expect("call works");
    let json = output_json(&out);

    assert_eq!(json["ok"], true, "unexpected answer: {json}");
    assert_eq!(json["data"], "howdy");
}

#[test]
fn call_api_rejects_an_export_the_target_does_not_list() {
    let fx = Fixture::new();
    let services = ServiceRegistry::new();
    // hello_world exists but the manifest lists no [api].exports: nothing
    // may be called on the target
    let target_manifest = "id = \"targeter\"\nversion = \"0.1.0\"\nname = \"Fixture targeter\"\nplugin_main = \"hello_world\"\nplugin_api = \"0.1\"\n";
    common::write_plugin_with_manifest(
        &fx.plugins_dir,
        "targeter",
        target_manifest,
        &common::static_export_plugin_wasm("hello_world", "howdy"),
    );
    let report = PluginLoader::new(fx.plugins_dir.clone(), fx.libraries_dir.clone(), &fx.cache)
        .with_services(services.clone())
        .load();
    assert!(report.issues.is_empty(), "issues: {:?}", report.issues);
    let dispatcher = Arc::new(ProxyEventDispatcher::new(PluginSet::from_vec(
        report.plugins,
    )));
    services.register_service(DISPATCHER_SERVICE_KEY.to_owned(), dispatcher);

    let temp = tempfile::tempdir().expect("tempdir");
    let wasm = common::echo_host_fn_wasm("candyriya", "callApi");
    let instance = direct_instance(&temp, "api-caller-3", &wasm, Arc::new(services));

    let out = instance
        .call(
            "on_event",
            br#"{"target_plugin": "targeter", "fn": "hello_world", "args": []}"#,
        )
        .expect("call works");
    let json = output_json(&out);

    assert_eq!(json["ok"], false);
    let error = json["error"].as_str().expect("error string");
    assert!(
        error.contains("does not export"),
        "unexpected error: {error}"
    );
}

#[test]
fn a_plugin_cannot_claim_to_be_another_plugin() {
    let temp = tempfile::tempdir().expect("tempdir");
    // the caller is "aaa-plugin" and tries to pass "bbb-plugin" in the
    // optional plugin_id field; the host must reject it
    let services = Arc::new(ServiceRegistry::new());
    let log_echo = common::echo_host_fn_wasm("candyriya", "logMessage");
    let instance = direct_instance(&temp, "aaa-plugin", &log_echo, services.clone());

    let out = instance
        .call(
            "on_event",
            br#"{"level": "info", "msg": "hi", "plugin_id": "bbb-plugin"}"#,
        )
        .expect("call works");
    let json = output_json(&out);
    assert_eq!(json["ok"], false);
    assert!(
        json["error"]
            .as_str()
            .expect("error string")
            .contains("plugin_id mismatch"),
        "unexpected error: {json}"
    );

    // the honest self-identification is accepted
    let out = instance
        .call(
            "on_event",
            br#"{"level": "info", "msg": "hi", "plugin_id": "aaa-plugin"}"#,
        )
        .expect("call works");
    let json = output_json(&out);
    assert_eq!(json["ok"], true);

    // the same self-check applies to callApi
    let api_echo = common::echo_host_fn_wasm("candyriya", "callApi");
    let instance = direct_instance(&temp, "aaa-caller", &api_echo, services);
    let out = instance
        .call(
            "on_event",
            br#"{"target_plugin": "whatever", "fn": "x", "args": [], "plugin_id": "bbb-plugin"}"#,
        )
        .expect("call works");
    let json = output_json(&out);
    assert_eq!(json["ok"], false);
    assert!(
        json["error"]
            .as_str()
            .expect("error string")
            .contains("plugin_id mismatch"),
        "unexpected error: {json}"
    );
}

#[test]
fn subscribers_gate_the_event_fanout() {
    let fx = Fixture::new();
    common::write_plugin(
        &fx.plugins_dir,
        "subby",
        &common::subscribe_login_plugin_wasm(),
    );
    let report =
        PluginLoader::new(fx.plugins_dir.clone(), fx.libraries_dir.clone(), &fx.cache).load();
    assert!(report.issues.is_empty(), "issues: {:?}", report.issues);
    // the instance is reachable on the record before the set is built;
    // run the plugin's `subscribe` export, which calls
    // candyriya:subscribeToEvents(["login"]) for real
    let instance = report
        .plugins
        .iter()
        .find(|plugin| plugin.id() == "subby")
        .and_then(|plugin| plugin.instance())
        .expect("subby is loaded with a live instance");
    let out = instance
        .call("subscribe", b"")
        .expect("subscribe export runs");
    assert!(!out.is_empty());

    let set = PluginSet::from_vec(report.plugins);
    let dispatcher = ProxyEventDispatcher::new(set);

    // connection_kicked is not in the plugin's allowlist: it must not be asked
    let result = dispatcher.dispatch_cancellable(&connection_kicked_event());
    assert_eq!(result.outcome_for("subby"), None);
    assert!(!result.cancelled);

    // login is in the allowlist: the plugin runs and cancels
    let result = dispatcher.dispatch_cancellable(&login_event());
    assert_eq!(
        result.outcome_for("subby"),
        Some(&Outcome::Cancelled {
            cancel_reason: None,
        })
    );
    assert!(result.cancelled);
}

#[test]
fn cancel_event_host_function_cancels_the_dispatch() {
    let fx = Fixture::new();
    let set = load_with_services(
        &fx,
        &[("canceler", common::cancel_event_plugin_wasm())],
        ServiceRegistry::new(),
    );
    let dispatcher = ProxyEventDispatcher::new(set);

    let result = dispatcher.dispatch_cancellable(&login_event());

    assert!(
        result.cancelled,
        "cancelEvent(true) must cancel the dispatch"
    );
    assert_eq!(result.cancel_reason.as_deref(), Some("nope"));
    assert_eq!(
        result.outcome_for("canceler"),
        Some(&Outcome::Cancelled {
            cancel_reason: Some("nope".to_owned()),
        })
    );
}

#[test]
fn a_cancellation_recorded_before_a_timeout_still_cancels() {
    let fx = Fixture::new();
    let set = load_with_services(
        &fx,
        &[(
            "hang-after-cancel",
            common::cancel_event_then_hang_plugin_wasm(),
        )],
        ServiceRegistry::new(),
    );
    // the plugin never returns; the recorded cancellation must win
    let dispatcher = ProxyEventDispatcher::with_timeout(set, std::time::Duration::from_millis(50));

    let result = dispatcher.dispatch_cancellable(&login_event());

    assert!(result.cancelled);
    assert_eq!(result.cancel_reason.as_deref(), Some("nope"));
    assert_eq!(
        result.outcome_for("hang-after-cancel"),
        Some(&Outcome::Cancelled {
            cancel_reason: Some("nope".to_owned()),
        })
    );
}

#[test]
fn ping_player_reports_the_stub_when_no_delivery_channel_exists() {
    let temp = tempfile::tempdir().expect("tempdir");
    let services = Arc::new(ServiceRegistry::new());
    let wasm = common::echo_host_fn_wasm("candyriya", "pingPlayer");
    let instance = direct_instance(&temp, "pinger", &wasm, services);

    let out = instance
        .call("on_event", br#"{"name": "Steve"}"#)
        .expect("call works");
    let json = output_json(&out);

    assert_eq!(json["ok"], false);
    assert_eq!(json["error"], "player messaging is not implemented yet");
}

#[test]
fn ping_player_delivers_through_a_registered_sink() {
    let temp = tempfile::tempdir().expect("tempdir");
    let delivered = Arc::new(std::sync::Mutex::new(Vec::new()));
    let delivered_for_sink = Arc::clone(&delivered);
    let services = ServiceRegistry::new();
    services.register_service(
        PLAYERS_ROSTER_KEY.to_owned(),
        Arc::new(vec![PlayerRef {
            uuid: "5627dd98-e6be-3c21-b8a8-e92344183641".to_owned(),
            name: "Steve".to_owned(),
        }]),
    );
    services.register_service(
        PLAYERS_PING_SINK_KEY.to_owned(),
        Arc::new(PlayerPingSink::new(move |name| {
            delivered_for_sink
                .lock()
                .expect("sink mutex")
                .push(name.to_owned());
        })),
    );

    let services = Arc::new(services);
    let wasm = common::echo_host_fn_wasm("candyriya", "pingPlayer");
    let instance = direct_instance(&temp, "pinger-2", &wasm, services);

    let out = instance
        .call("on_event", br#"{"name": "Steve"}"#)
        .expect("call works");
    assert_eq!(output_json(&out)["ok"], true);
    assert_eq!(
        delivered.lock().expect("delivered").as_slice(),
        ["Steve"],
        "the sink receives the player's name"
    );

    let out = instance
        .call("on_event", br#"{"name": "NotOnline"}"#)
        .expect("call works");
    let json = output_json(&out);
    assert_eq!(json["ok"], false);
    assert_eq!(json["error"], "player `NotOnline` is not online");
}

#[test]
fn list_servers_defaults_to_local() {
    let temp = tempfile::tempdir().expect("tempdir");
    let services = Arc::new(ServiceRegistry::new());
    let wasm = common::echo_host_fn_wasm("candyriya", "listServers");
    let instance = direct_instance(&temp, "lister", &wasm, services);

    let out = instance.call("on_event", b"").expect("call works");
    let json: Vec<String> = serde_json::from_slice(&out).expect("array of server names");

    assert_eq!(json, ["local"]);
}

#[test]
fn list_servers_prefers_the_registered_backend_list() {
    let temp = tempfile::tempdir().expect("tempdir");
    let services = ServiceRegistry::new();
    services.register_service(
        SERVERS_NAMES_KEY.to_owned(),
        Arc::new(vec!["alpha".to_owned(), "beta".to_owned()]),
    );
    let services = Arc::new(services);
    let wasm = common::echo_host_fn_wasm("candyriya", "listServers");
    let instance = direct_instance(&temp, "lister-2", &wasm, services);

    let out = instance.call("on_event", b"").expect("call works");
    let json: Vec<String> = serde_json::from_slice(&out).expect("array of server names");

    assert_eq!(json, ["alpha", "beta"]);
}

#[test]
fn get_player_roster_defaults_to_an_empty_list() {
    let temp = tempfile::tempdir().expect("tempdir");
    let services = Arc::new(ServiceRegistry::new());
    let wasm = common::echo_host_fn_wasm("candyriya", "getPlayerRoster");
    let instance = direct_instance(&temp, "roster-reader", &wasm, services);

    let out = instance.call("on_event", b"").expect("call works");
    let json: Vec<serde_json::Value> = serde_json::from_slice(&out).expect("array of players");

    assert!(json.is_empty());
}

#[test]
fn get_player_roster_returns_the_registered_roster() {
    let temp = tempfile::tempdir().expect("tempdir");
    let services = ServiceRegistry::new();
    services.register_service(
        PLAYERS_ROSTER_KEY.to_owned(),
        Arc::new(vec![PlayerRef {
            uuid: "5627dd98-e6be-3c21-b8a8-e92344183641".to_owned(),
            name: "Steve".to_owned(),
        }]),
    );
    let services = Arc::new(services);
    let wasm = common::echo_host_fn_wasm("candyriya", "getPlayerRoster");
    let instance = direct_instance(&temp, "roster-reader-2", &wasm, services);

    let out = instance.call("on_event", b"").expect("call works");
    let json: Vec<PlayerRef> = serde_json::from_slice(&out).expect("array of players");

    assert_eq!(json.len(), 1);
    assert_eq!(json[0].name, "Steve");
}
