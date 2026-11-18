//! The `host` unit over a real tree, through the same wiring the
//! composition root uses, and the host functions over a real wasm
//! boundary.
//!
//! No fakes anywhere: the unit under test is exactly the one production
//! spawns, the tree is the real supervision tree, the wasm modules are
//! compiled from WAT at test time, and every byte crosses a mailbox or a
//! WASM boundary for real.
//!
//! The `callApi` target is a static-export module whose `hello_world`
//! export answers `howdy` (the same fixture shape the plugin crate's
//! host-function tests use); the `caller` is the echo module that
//! forwards its input to one `candyriya:*` host function and returns
//! the host's raw answer.

use std::sync::Arc;
use std::time::Duration;

use candyriya_config::{Config, ConfigStore};
use candyriya_init::{Init, Pid, RestartPolicy, UnitKind, UnitSpec};
use candyriya_plugin::{
    HostIpc, HostMsg, HostResp, HostState, Limits, PluginInstance, PluginLoader, PluginSet,
    ProxyEventDispatcher, ServiceRegistry,
};
use candyriya_plugin_api::PlayerRef;
use candyriya_route::{BackendRegistry, RouterStore, route_spec, router_store};
use candyriya_scheduler::TaskService;
use uuid::Uuid;

use candyriya_proxy::{ProxyState, config_spec, host_spec};

/// The fixture target module: `hello_world` always answers `howdy`.
const TARGET_WAT: &str = r#"(module
  (import "extism:host/env" "alloc" (func $alloc (param i64) (result i64)))
  (import "extism:host/env" "output_set" (func $output_set (param i64 i64)))
  (import "extism:host/env" "store_u8" (func $store_u8 (param i64 i32)))
  (memory (export "memory") 1)
  (func (export "hello_world") (result i32)
    (local $p i64)
    (local.set $p (call $alloc (i64.const 5)))
    (call $store_u8 (i64.add (local.get $p) (i64.const 0)) (i32.const 104))
    (call $store_u8 (i64.add (local.get $p) (i64.const 1)) (i32.const 111))
    (call $store_u8 (i64.add (local.get $p) (i64.const 2)) (i32.const 119))
    (call $store_u8 (i64.add (local.get $p) (i64.const 3)) (i32.const 100))
    (call $store_u8 (i64.add (local.get $p) (i64.const 4)) (i32.const 121))
    (call $output_set (local.get $p) (i64.const 5))
    (i32.const 0)))
"#;

/// The target manifest: exports `hello_world` to other plugins.
const TARGET_MANIFEST: &str = "id = \"targeter\"\nversion = \"0.1.0\"\nname = \"Fixture targeter\"\nplugin_main = \"hello_world\"\nplugin_api = \"0.1\"\n[api]\nexports = [ { name = \"hello_world\" } ]\n";

/// A module whose `on_event` forwards its input to the `candyriya` host
/// function `import_name` and returns the host's raw answer.
fn echo_host_fn_wat(import_name: &str) -> String {
    format!(
        r#"(module
  (import "extism:host/env" "alloc" (func $alloc (param i64) (result i64)))
  (import "extism:host/env" "input_length" (func $input_length (result i64)))
  (import "extism:host/env" "input_load_u8" (func $input_load_u8 (param i64) (result i32)))
  (import "extism:host/env" "store_u8" (func $store_u8 (param i64 i32)))
  (import "extism:host/env" "output_set" (func $output_set (param i64 i64)))
  (import "extism:host/env" "length" (func $length (param i64) (result i64)))
  (import "candyriya" "{import_name}" (func $hostfn (param i64) (result i64)))
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
    )
}

fn echo_host_fn_wasm(import_name: &str) -> Vec<u8> {
    wat::parse_str(echo_host_fn_wat(import_name)).expect("echo fixture wat compiles")
}

fn target_wasm() -> Vec<u8> {
    wat::parse_str(TARGET_WAT).expect("target fixture wat compiles")
}

/// The live tree slice under test: config + route + host units, one
/// shared router store, one shared state.
struct Fixture {
    init: Init,
    state: Arc<ProxyState>,
    host_pid: Pid,
    store: RouterStore,
    _plugins_dir: tempfile::TempDir,
}

/// Backends seeded into the shared store; `try_order` picks the order.
fn seeded_store(pairs: &[(&str, &str)], try_order: &[&str]) -> RouterStore {
    let registry = BackendRegistry::new(
        pairs
            .iter()
            .map(|(name, addr)| (name.to_string(), addr.parse().expect("literal address"))),
        try_order.iter().map(|name| name.to_string()),
    )
    .expect("registry builds");
    router_store(Some(Arc::new(registry)))
}

fn spawn_fixture(seed: Option<RouterStore>, plugins: &[(&str, &str, Vec<u8>)]) -> Fixture {
    let service = TaskService::builder()
        .handle(tokio::runtime::Handle::current())
        .blocking_threads(2)
        .build()
        .expect("task service builds");
    let init = Init::new(service.clone());

    let config_pid = init
        .spawn_unit(
            Pid::INIT,
            config_spec(Arc::new(ConfigStore::new(Config::default()))),
        )
        .expect("config unit spawns");
    let store = seed.unwrap_or_else(|| router_store(None));
    let route_pid = init
        .spawn_unit(Pid::INIT, route_spec(Arc::clone(&store)))
        .expect("route unit spawns");
    let state = Arc::new(ProxyState::new(
        init.clone(),
        config_pid,
        route_pid,
        Arc::clone(&store),
    ));

    // the plugins the host unit's dispatcher fans out into; an empty
    // set is an honest state for everything except callApi
    let plugins_dir = tempfile::tempdir().expect("plugins tempdir");
    std::fs::create_dir_all(plugins_dir.path().join("libraries"))
        .expect("libraries dir for the fixture loads");
    for (id, manifest, wasm) in plugins {
        let dir = plugins_dir.path().join(id);
        std::fs::create_dir_all(&dir).expect("plugin dir");
        std::fs::write(dir.join("plugin.toml"), manifest).expect("manifest written");
        std::fs::write(dir.join(format!("{id}.wasm")), wasm).expect("wasm written");
    }
    let dispatcher = if plugins.is_empty() {
        ProxyEventDispatcher::new(PluginSet::from_vec(Vec::new()))
    } else {
        let cache = candyriya_cache::CacheManager::new(plugins_dir.path()).expect("cache manager");
        let report = PluginLoader::new(
            plugins_dir.path().to_path_buf(),
            plugins_dir.path().join("libraries"),
            &cache,
        )
        .load();
        assert!(
            report.issues.is_empty(),
            "unexpected plugin issues: {:?}",
            report.issues
        );
        ProxyEventDispatcher::new(PluginSet::from_vec(report.plugins))
    };
    let host_pid = init
        .spawn_unit(
            Pid::INIT,
            host_spec(
                Arc::clone(&state),
                Arc::new(dispatcher),
                route_pid,
                init.clone(),
                service,
            ),
        )
        .expect("host unit spawns");
    Fixture {
        init,
        state,
        host_pid,
        store,
        _plugins_dir: plugins_dir,
    }
}

fn install_link(fx: &Fixture) -> HostIpc {
    let link = HostIpc::new();
    assert!(
        link.install(
            fx.init.clone(),
            fx.host_pid,
            tokio::runtime::Handle::current()
        ),
        "the link installs exactly once"
    );
    link
}

fn caller_instance(
    id: &str,
    import_name: &str,
    link: Option<HostIpc>,
    services: ServiceRegistry,
) -> PluginInstance {
    let dir = tempfile::tempdir().expect("caller tempdir");
    let wasm = echo_host_fn_wasm(import_name);
    let path = dir.path().join(format!("{id}.wasm"));
    std::fs::write(&path, &wasm).expect("caller wasm written");
    let data_root = tempfile::tempdir().expect("caller data tempdir");
    let mut state =
        HostState::new(data_root.path().to_path_buf(), id).with_services(Arc::new(services));
    if let Some(link) = link {
        state = state.with_host_ipc(link);
    }
    PluginInstance::new(&path, state, id, &Limits::new()).expect("caller instance builds")
}

fn output_json(bytes: &[u8]) -> serde_json::Value {
    serde_json::from_slice(bytes).expect("host function answer is JSON")
}

async fn call_host(fx: &Fixture, msg: HostMsg) -> HostResp {
    fx.init
        .call::<HostMsg, HostResp>(Pid::INIT, fx.host_pid, msg)
        .await
        .expect("the host unit answers")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_roster_answers_live_joins_and_parts() {
    let fx = spawn_fixture(None, &[]);

    let HostResp::Roster(players) = call_host(&fx, HostMsg::Roster).await else {
        panic!("expected a roster reply");
    };
    assert!(players.is_empty(), "nobody is online yet");

    let steve = Uuid::from_u128(0x5E1E);
    let alex = Uuid::from_u128(0xA1EC);
    fx.state
        .roster
        .insert(1, steve, "Steve", Some("alpha".into()));
    fx.state.roster.insert(2, alex, "Alex", Some("beta".into()));

    let HostResp::Roster(players) = call_host(&fx, HostMsg::Roster).await else {
        panic!("expected a roster reply");
    };
    let names: Vec<(u64, &str)> = players
        .iter()
        .enumerate()
        .map(|(i, player)| (i as u64, player.name.as_str()))
        .collect();
    assert_eq!(names.len(), 2);
    // snapshot() sorts by (name, conn_id): Alex first
    assert_eq!(players[0].name, "Alex");
    assert_eq!(players[0].uuid, alex.to_string());
    assert_eq!(players[1].name, "Steve");

    // a part is visible on the very next call: no 1s refresh window
    fx.state.roster.remove(2);
    let HostResp::Roster(players) = call_host(&fx, HostMsg::Roster).await else {
        panic!("expected a roster reply");
    };
    assert_eq!(players.len(), 1);
    assert_eq!(players[0].name, "Steve");

    fx.state.roster.remove(1);
    let HostResp::Roster(players) = call_host(&fx, HostMsg::Roster).await else {
        panic!("expected a roster reply");
    };
    assert!(players.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_names_serve_the_shared_store_across_a_reload() {
    let fx = spawn_fixture(
        Some(seeded_store(
            &[("alpha", "127.0.0.1:9001"), ("beta", "127.0.0.1:9002")],
            &["alpha"],
        )),
        &[],
    );

    let HostResp::Names(names) = call_host(&fx, HostMsg::Names).await else {
        panic!("expected a names reply");
    };
    assert_eq!(names, ["alpha", "beta"]);

    // /candyriya reload swaps the very store the unit serves: the next
    // call sees the new table, and a cleared routing disables the names
    fx.store.store(std::sync::Arc::new(None));
    let HostResp::Names(names) = call_host(&fx, HostMsg::Names).await else {
        panic!("expected a names reply");
    };
    assert!(names.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_sibling_unit_without_a_grant_is_refused() {
    let fx = spawn_fixture(None, &[]);
    // a one-shot probe, spawned as a sibling of the host unit with no
    // grant to it; the subtree rule does not cover sideways calls
    let probe = {
        let init = fx.init.clone();
        let host_pid = fx.host_pid;
        UnitSpec::new(
            "host.probe",
            UnitKind::Oneshot,
            RestartPolicy::Never,
            move || {
                let init = init.clone();
                async move {
                    let caller =
                        candyriya_init::current_unit().expect("the probe runs as its unit");
                    match init
                        .call::<HostMsg, HostResp>(caller, host_pid, HostMsg::Roster)
                        .await
                    {
                        Ok(_) => candyriya_init::ExitStatus::Success,
                        Err(error) => candyriya_init::ExitStatus::failure(error.to_string()),
                    }
                }
            },
        )
    };
    let probe_pid = fx.init.spawn_unit(Pid::INIT, probe).expect("probe spawns");

    // wait for the probe to exit and read its status from the tree
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        for outcome in fx.init.reap_children(Pid::INIT) {
            let record = match outcome {
                candyriya_init::ReapOutcome::Reaped(record)
                | candyriya_init::ReapOutcome::Restarting { record, .. }
                | candyriya_init::ReapOutcome::FailedTerminal { record } => record,
            };
            if record.pid == probe_pid {
                let detail = record.status.to_string();
                assert!(
                    detail.contains("capability"),
                    "a sibling without a grant must be refused with a capability error, got: {detail}"
                );
                return;
            }
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the probe did not exit"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn callapi_fans_out_to_a_real_target() {
    let fx = spawn_fixture(None, &[("targeter", TARGET_MANIFEST, target_wasm())]);

    let HostResp::CallApi(Ok(output)) = call_host(
        &fx,
        HostMsg::CallApi {
            target_plugin: "targeter".to_owned(),
            function: "hello_world".to_owned(),
            args: br#"["Steve"]"#.to_vec(),
            caller: Some("an-other-plugin".to_owned()),
        },
    )
    .await
    else {
        panic!("expected a callApi reply");
    };
    assert_eq!(output, b"howdy");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn callapi_rejects_an_export_the_target_does_not_list() {
    // the module contains hello_world, but the manifest lists no exports
    let fx = spawn_fixture(
        None,
        &[(
            "targeter",
            "id = \"targeter\"\nversion = \"0.1.0\"\nname = \"Fixture targeter\"\nplugin_main = \"hello_world\"\nplugin_api = \"0.1\"\n",
            target_wasm(),
        )],
    );

    let HostResp::CallApi(Err(message)) = call_host(
        &fx,
        HostMsg::CallApi {
            target_plugin: "targeter".to_owned(),
            function: "hello_world".to_owned(),
            args: b"[]".to_vec(),
            caller: None,
        },
    )
    .await
    else {
        panic!("expected a callApi reply");
    };
    assert!(
        message.contains("does not export"),
        "unexpected decline message: {message}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn callapi_reports_a_missing_target() {
    let fx = spawn_fixture(None, &[]);

    let HostResp::CallApi(Err(message)) = call_host(
        &fx,
        HostMsg::CallApi {
            target_plugin: "ghost".to_owned(),
            function: "hello_world".to_owned(),
            args: b"[]".to_vec(),
            caller: None,
        },
    )
    .await
    else {
        panic!("expected a callApi reply");
    };
    assert_eq!(message, "plugin not loaded");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn host_fn_callapi_roundtrips_through_the_tree() {
    let fx = spawn_fixture(None, &[("targeter", TARGET_MANIFEST, target_wasm())]);
    let link = install_link(&fx);
    let instance = caller_instance("caller", "callApi", Some(link), ServiceRegistry::new());

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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn host_fn_callapi_reports_an_uninstalled_link() {
    // no link installed: the honest host-unavailable error, not a panic
    let instance = caller_instance("caller", "callApi", None, ServiceRegistry::new());

    let out = instance
        .call(
            "on_event",
            br#"{"target_plugin": "targeter", "fn": "hello_world", "args": []}"#,
        )
        .expect("call works");
    let json = output_json(&out);
    assert_eq!(json["ok"], false);
    assert_eq!(
        json["error"],
        "callApi: host dispatcher unavailable (host unit link is not installed)"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn host_fn_list_servers_answers_the_seeded_backends() {
    let fx = spawn_fixture(
        Some(seeded_store(
            &[("alpha", "127.0.0.1:9001"), ("beta", "127.0.0.1:9002")],
            &["alpha"],
        )),
        &[],
    );
    let link = install_link(&fx);
    let instance = caller_instance("caller", "listServers", Some(link), ServiceRegistry::new());

    let out = instance.call("on_event", b"").expect("call works");
    let json: Vec<String> = serde_json::from_slice(&out).expect("array of server names");
    assert_eq!(json, ["alpha", "beta"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn host_fn_list_servers_falls_back_to_local_without_backends() {
    // no [servers] configured: the documented stand-in, not an error
    let fx = spawn_fixture(None, &[]);
    let link = install_link(&fx);
    let instance = caller_instance("caller", "listServers", Some(link), ServiceRegistry::new());

    let out = instance.call("on_event", b"").expect("call works");
    let json: Vec<String> = serde_json::from_slice(&out).expect("array of server names");
    assert_eq!(json, ["local"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn host_fn_get_player_roster_tracks_live_joins_and_parts() {
    let fx = spawn_fixture(None, &[]);
    let link = install_link(&fx);
    let instance = caller_instance(
        "caller",
        "getPlayerRoster",
        Some(link),
        ServiceRegistry::new(),
    );

    let out = instance.call("on_event", b"").expect("call works");
    let json: Vec<PlayerRef> = serde_json::from_slice(&out).expect("array of players");
    assert!(json.is_empty());

    let steve = Uuid::from_u128(0x5E1E);
    fx.state
        .roster
        .insert(1, steve, "Steve", Some("alpha".into()));
    let out = instance.call("on_event", b"").expect("call works");
    let json: Vec<PlayerRef> = serde_json::from_slice(&out).expect("array of players");
    assert_eq!(json.len(), 1);
    assert_eq!(json[0].name, "Steve");
    assert_eq!(json[0].uuid, steve.to_string());

    fx.state.roster.remove(1);
    let out = instance.call("on_event", b"").expect("call works");
    let json: Vec<PlayerRef> = serde_json::from_slice(&out).expect("array of players");
    assert!(json.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn host_fn_ping_player_reports_the_missing_delivery_path() {
    let fx = spawn_fixture(None, &[]);
    // the player is online, but no ping sink is registered anywhere:
    // the honest error, not a pretended delivery
    fx.state
        .roster
        .insert(1, Uuid::from_u128(1), "Steve", Some("alpha".into()));
    let link = install_link(&fx);
    let instance = caller_instance("caller", "pingPlayer", Some(link), ServiceRegistry::new());

    let out = instance
        .call("on_event", br#"{"name": "Steve"}"#)
        .expect("call works");
    let json = output_json(&out);
    assert_eq!(json["ok"], false);
    assert_eq!(json["error"], "player messaging is not implemented yet");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn host_fn_ping_player_delivers_through_a_registered_sink() {
    let fx = spawn_fixture(None, &[]);
    fx.state
        .roster
        .insert(1, Uuid::from_u128(1), "Steve", Some("alpha".into()));

    let delivered = Arc::new(std::sync::Mutex::new(Vec::new()));
    let delivered_for_sink = Arc::clone(&delivered);
    let services = ServiceRegistry::new();
    services.register_service(
        candyriya_plugin::PLAYERS_PING_SINK_KEY.to_owned(),
        Arc::new(candyriya_plugin::PlayerPingSink::new(move |name| {
            delivered_for_sink
                .lock()
                .expect("sink mutex")
                .push(name.to_owned());
        })),
    );
    let link = install_link(&fx);
    let instance = caller_instance("caller", "pingPlayer", Some(link), services);

    let out = instance
        .call("on_event", br#"{"name": "Steve"}"#)
        .expect("call works");
    assert_eq!(output_json(&out)["ok"], true);
    assert_eq!(delivered.lock().expect("delivered").as_slice(), ["Steve"]);

    let out = instance
        .call("on_event", br#"{"name": "NotOnline"}"#)
        .expect("call works");
    let json = output_json(&out);
    assert_eq!(json["ok"], false);
    assert_eq!(json["error"], "player `NotOnline` is not online");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn host_fn_ping_player_reports_an_uninstalled_link() {
    // no link installed: the roster cannot be read, so the ping must not
    // be attempted — the honest error, not the sink's stub answer
    let instance = caller_instance("caller", "pingPlayer", None, ServiceRegistry::new());

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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn host_fn_get_player_roster_answers_empty_without_a_link() {
    let instance = caller_instance("caller", "getPlayerRoster", None, ServiceRegistry::new());

    let out = instance.call("on_event", b"").expect("call works");
    let json: Vec<PlayerRef> = serde_json::from_slice(&out).expect("array of players");
    assert!(json.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn host_fn_list_servers_answers_local_without_a_link() {
    let instance = caller_instance("caller", "listServers", None, ServiceRegistry::new());

    let out = instance.call("on_event", b"").expect("call works");
    let json: Vec<String> = serde_json::from_slice(&out).expect("array of server names");
    assert_eq!(json, ["local"]);
}
