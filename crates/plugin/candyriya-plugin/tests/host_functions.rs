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

use candyriya_i18n::I18n;
use candyriya_plugin::{
    HostState, Limits, Outcome, PluginInstance, PluginLoader, PluginSet, ProxyEventDispatcher,
    ServiceRegistry, SharedLocales,
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
fn call_api_without_a_host_link_reports_the_unavailable_error() {
    let temp = tempfile::tempdir().expect("tempdir");
    let services = Arc::new(ServiceRegistry::new());
    // standalone host state: no link installed, so the host function cannot
    // reach the `host` unit — the honest unavailable error, not a panic
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
    assert_eq!(
        json["error"],
        "callApi: host dispatcher unavailable (host unit link is not installed)"
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
fn ping_player_reports_an_unavailable_roster_without_a_host_link() {
    let temp = tempfile::tempdir().expect("tempdir");
    let services = Arc::new(ServiceRegistry::new());
    let wasm = common::echo_host_fn_wasm("candyriya", "pingPlayer");
    let instance = direct_instance(&temp, "pinger", &wasm, services);

    let out = instance
        .call("on_event", br#"{"name": "Steve"}"#)
        .expect("call works");
    let json = output_json(&out);

    assert_eq!(json["ok"], false);
    assert_eq!(
        json["error"].as_str().expect("error string"),
        "player roster unavailable (host unit link is not installed)"
    );
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
fn get_player_roster_defaults_to_an_empty_list() {
    let temp = tempfile::tempdir().expect("tempdir");
    let services = Arc::new(ServiceRegistry::new());
    let wasm = common::echo_host_fn_wasm("candyriya", "getPlayerRoster");
    let instance = direct_instance(&temp, "roster-reader", &wasm, services);

    let out = instance.call("on_event", b"").expect("call works");
    let json: Vec<serde_json::Value> = serde_json::from_slice(&out).expect("array of players");

    assert!(json.is_empty());
}

// ---- i18n host functions ----

fn en_ex_girl_pair() -> (I18n, I18n, SharedLocales) {
    let en = I18n::new("en").expect("embedded en loads");
    let ex_girl = I18n::new("ex_girl").expect("embedded ex_girl loads");
    let shared = SharedLocales::new(en.clone(), ex_girl.clone());
    (en, ex_girl, shared)
}

fn direct_instance_with_locales(
    temp: &tempfile::TempDir,
    id: &str,
    wasm: &[u8],
    services: Arc<ServiceRegistry>,
    locales: SharedLocales,
) -> PluginInstance {
    let wasm_path = write_wasm(temp, &format!("{id}.wasm"), wasm);
    let state = HostState::new(temp.path().join(format!("data-{id}")), id)
        .with_services(services)
        .with_locales(locales);
    PluginInstance::new(&wasm_path, state, id, &Limits::new()).expect("instance builds")
}

const LOCALES_MANIFEST: &str = "id = \"localer\"\nversion = \"0.1.0\"\nname = \"Fixture localer\"\nplugin_main = \"on_event\"\nplugin_api = \"0.1\"\n\n[locales]\n";

fn write_plugin_locales(root: &std::path::Path, id: &str, lang: &str, body: &str) {
    let dir = root.join(id).join("locales").join(lang);
    std::fs::create_dir_all(&dir).expect("locales dir");
    std::fs::write(dir.join(format!("{id}.toml")), body).expect("plugin locale file");
}

#[test]
fn i18n_locale_returns_en_by_default() {
    let temp = tempfile::tempdir().expect("tempdir");
    let services = Arc::new(ServiceRegistry::new());
    let wasm = common::echo_host_fn_wasm("candyriya", "i18nLocale");
    let instance = direct_instance(&temp, "locale-default", &wasm, services);

    let out = instance.call("on_event", b"").expect("call works");

    assert_eq!(out, b"en");
}

#[test]
fn i18n_locale_returns_the_configured_locale() {
    let temp = tempfile::tempdir().expect("tempdir");
    let services = Arc::new(ServiceRegistry::new());
    let wasm = common::echo_host_fn_wasm("candyriya", "i18nLocale");
    let wasm_path = write_wasm(&temp, "locale-override.wasm", &wasm);
    let state = HostState::new(temp.path().join("data-locale-override"), "locale-override")
        .with_services(services)
        .with_locale("ex_girl");
    let instance = PluginInstance::new(&wasm_path, state, "locale-override", &Limits::new())
        .expect("instance builds");

    let out = instance.call("on_event", b"").expect("call works");

    assert_eq!(out, b"ex_girl");
}

#[test]
fn i18n_t_resolves_a_key_from_the_plugin_locale_file() {
    let fx = Fixture::new();
    let (_en, _ex_girl, shared) = en_ex_girl_pair();
    let wasm = common::echo_host_fn_wasm("candyriya", "i18nT");
    common::write_plugin_with_manifest(&fx.plugins_dir, "localer", LOCALES_MANIFEST, &wasm);
    write_plugin_locales(
        &fx.plugins_dir,
        "localer",
        "en",
        "\"plugin.localer.connected\" = \"Connected to {server}\"\n",
    );
    let report = PluginLoader::new(fx.plugins_dir.clone(), fx.libraries_dir.clone(), &fx.cache)
        .with_locales(shared)
        .load();
    assert!(report.issues.is_empty(), "issues: {:?}", report.issues);
    let instance = report
        .plugins
        .iter()
        .find(|plugin| plugin.id() == "localer")
        .and_then(|plugin| plugin.instance())
        .expect("localer is loaded with a live instance");

    let out = instance
        .call(
            "on_event",
            br#"{"key": "plugin.localer.connected", "args": {"server": "hub"}}"#,
        )
        .expect("call works");

    assert_eq!(out, b"Connected to hub");
}

#[test]
fn i18n_t_interpolates_args_host_side() {
    let temp = tempfile::tempdir().expect("tempdir");
    let services = Arc::new(ServiceRegistry::new());
    let (_en, _ex_girl, shared) = en_ex_girl_pair();
    write_plugin_locales(
        temp.path(),
        "inter",
        "en",
        "\"plugin.inter.welcome\" = \"Hi {name}, you have {count} mails and {ratio} score\"\n",
    );
    // register through the plugin's own locales dir: the host only needs the tables
    shared
        .register_plugin_locale("inter", &temp.path().join("inter").join("locales"))
        .expect("register");
    let wasm = common::echo_host_fn_wasm("candyriya", "i18nT");
    let instance = direct_instance_with_locales(&temp, "inter", &wasm, services, shared);

    let out = instance
        .call(
            "on_event",
            br#"{"key": "plugin.inter.welcome", "args": {"name": "Steve", "count": 3, "ratio": 1.5}}"#,
        )
        .expect("call works");

    assert_eq!(out, b"Hi Steve, you have 3 mails and 1.5 score");
}

#[test]
fn i18n_t_falls_back_to_the_en_plugin_file_when_active_is_ex_girl() {
    let temp = tempfile::tempdir().expect("tempdir");
    let services = Arc::new(ServiceRegistry::new());
    let (_en, _ex_girl, shared) = en_ex_girl_pair();
    write_plugin_locales(
        temp.path(),
        "fallbacker",
        "ex_girl",
        "\"plugin.fallbacker.only_active\" = \"Только тут\"\n",
    );
    write_plugin_locales(
        temp.path(),
        "fallbacker",
        "en",
        "\"plugin.fallbacker.only_en\" = \"only in en\"\n",
    );
    shared
        .register_plugin_locale(
            "fallbacker",
            &temp.path().join("fallbacker").join("locales"),
        )
        .expect("register");
    let wasm = common::echo_host_fn_wasm("candyriya", "i18nT");
    let wasm_path = write_wasm(&temp, "fallbacker.wasm", &wasm);
    let state = HostState::new(temp.path().join("data-fallbacker"), "fallbacker")
        .with_services(services)
        .with_locale("ex_girl")
        .with_locales(shared);
    let instance = PluginInstance::new(&wasm_path, state, "fallbacker", &Limits::new())
        .expect("instance builds");

    let active_hit = instance
        .call("on_event", br#"{"key": "plugin.fallbacker.only_active"}"#)
        .expect("call works");
    assert_eq!(active_hit, "Только тут".as_bytes());

    let en_hit = instance
        .call("on_event", br#"{"key": "plugin.fallbacker.only_en"}"#)
        .expect("call works");
    assert_eq!(en_hit, b"only in en");
}

#[test]
fn i18n_t_falls_back_to_common_for_shared_keys() {
    let temp = tempfile::tempdir().expect("tempdir");
    let services = Arc::new(ServiceRegistry::new());
    let (_en, _ex_girl, shared) = en_ex_girl_pair();
    let wasm = common::echo_host_fn_wasm("candyriya", "i18nT");
    let instance = direct_instance_with_locales(&temp, "commoner", &wasm, services, shared);

    let out = instance
        .call("on_event", br#"{"key": "common.yes"}"#)
        .expect("call works");

    assert_eq!(out, b"Yes");
}

#[test]
fn i18n_t_refuses_core_and_other_plugin_keys() {
    let fx = Fixture::new();
    let (_en, _ex_girl, shared) = en_ex_girl_pair();
    let wasm = common::echo_host_fn_wasm("candyriya", "i18nT");
    common::write_plugin_with_manifest(
        &fx.plugins_dir,
        "scopey",
        LOCALES_MANIFEST.replace("localer", "scopey").as_str(),
        &wasm,
    );
    write_plugin_locales(
        &fx.plugins_dir,
        "scopey",
        "en",
        "\"plugin.scopey.own\" = \"mine\"\n\"plugin.other.claimed\" = \"not yours\"\n",
    );
    let report = PluginLoader::new(fx.plugins_dir.clone(), fx.libraries_dir.clone(), &fx.cache)
        .with_locales(shared)
        .load();
    assert!(report.issues.is_empty(), "issues: {:?}", report.issues);
    let instance = report
        .plugins
        .iter()
        .find(|plugin| plugin.id() == "scopey")
        .and_then(|plugin| plugin.instance())
        .expect("scopey is loaded with a live instance");

    for key in [
        "candyriya.reload.config_ok", // core proxy string
        "proxy.backend.unreachable",  // other core prefix
        "plugin.other.claimed",       // a key the plugin file itself claims for another namespace
    ] {
        let input = format!(r#"{{"key": "{key}"}}"#);
        let out = instance
            .call("on_event", input.as_bytes())
            .expect("call works");
        assert_eq!(
            out,
            key.as_bytes(),
            "out-of-scope key must bounce back as itself"
        );
    }

    let out = instance
        .call("on_event", br#"{"key": "plugin.scopey.own"}"#)
        .expect("call works");
    assert_eq!(out, b"mine");
}

#[test]
fn i18n_t_returns_the_key_for_missing_keys() {
    let temp = tempfile::tempdir().expect("tempdir");
    let services = Arc::new(ServiceRegistry::new());
    let (_en, _ex_girl, shared) = en_ex_girl_pair();
    let wasm = common::echo_host_fn_wasm("candyriya", "i18nT");
    let instance = direct_instance_with_locales(&temp, "missingman", &wasm, services, shared);

    let out = instance
        .call("on_event", br#"{"key": "plugin.missingman.no_such_key"}"#)
        .expect("call works");

    assert_eq!(out, b"plugin.missingman.no_such_key");
}

#[test]
fn i18n_t_malformed_input_gets_an_error_document() {
    let temp = tempfile::tempdir().expect("tempdir");
    let services = Arc::new(ServiceRegistry::new());
    let (_en, _ex_girl, shared) = en_ex_girl_pair();
    let wasm = common::echo_host_fn_wasm("candyriya", "i18nT");
    let instance = direct_instance_with_locales(&temp, "malformed", &wasm, services, shared);

    // not JSON at all
    let out = instance.call("on_event", b"not json").expect("call works");
    let json = output_json(&out);
    assert_eq!(json["ok"], false);

    // an object without `key`
    let out = instance
        .call("on_event", br#"{"args": {}}"#)
        .expect("call works");
    let json = output_json(&out);
    assert_eq!(json["ok"], false);
}

#[test]
fn plugin_locale_edits_apply_after_a_locales_reload() {
    let fx = Fixture::new();
    let (en, _ex_girl, shared) = en_ex_girl_pair();
    let wasm = common::echo_host_fn_wasm("candyriya", "i18nT");
    common::write_plugin_with_manifest(
        &fx.plugins_dir,
        "reloader",
        LOCALES_MANIFEST.replace("localer", "reloader").as_str(),
        &wasm,
    );
    write_plugin_locales(
        &fx.plugins_dir,
        "reloader",
        "en",
        "\"plugin.reloader.line\" = \"before\"\n",
    );
    let report = PluginLoader::new(fx.plugins_dir.clone(), fx.libraries_dir.clone(), &fx.cache)
        .with_locales(shared)
        .load();
    assert!(report.issues.is_empty(), "issues: {:?}", report.issues);
    let instance = report
        .plugins
        .iter()
        .find(|plugin| plugin.id() == "reloader")
        .and_then(|plugin| plugin.instance())
        .expect("reloader is loaded with a live instance");

    let out = instance
        .call("on_event", br#"{"key": "plugin.reloader.line"}"#)
        .expect("call works");
    assert_eq!(out, b"before");

    // simulate `/candyriya reload`: re-read the registered files
    write_plugin_locales(
        &fx.plugins_dir,
        "reloader",
        "en",
        "\"plugin.reloader.line\" = \"after\"\n",
    );
    en.reload().expect("locales reload must succeed");

    let out = instance
        .call("on_event", br#"{"key": "plugin.reloader.line"}"#)
        .expect("call works");
    assert_eq!(out, b"after");
}
