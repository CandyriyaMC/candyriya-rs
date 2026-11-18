//! Wiring for the plugin lifecycle events: real WAT plugins through the
//! real dispatcher, real loopback backends, the real accept loop.
//!
//! The fixtures below are real modules compiled from WAT at test time —
//! the same pattern the plugin crate uses — so every test proves the
//! bytes crossed the WASM boundary, not just that a function was called.
//! Each module cancels only the events it is built to cancel: the
//! dispatched payload starts with `{"id":N,"kind":"<kind>"` and single-digit
//! envelope ids put the tag at byte offset 16, which is where the fixtures
//! compare against their configured kind.

use std::collections::HashMap;
use std::fs;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::watch;

use candyriya_cache::CacheManager;
use candyriya_config::{Config, ForwardingMode};
use candyriya_it::{BackendFixture, BackendForwarding, FakeBackendConfig, Intent, TestClient};
use candyriya_plugin::{PluginLoader, PluginSet, ProxyEventDispatcher};
use candyriya_protocol::read_string;
use candyriya_protocol_registry::PacketSlot;
use candyriya_scheduler::TaskService;

use candyriya_proxy::{ProxySocket, SocketConfig, registry_from_config, serve};

mod common;

const PROTOCOL: i32 = 767; // 1.21.x, present in the embedded registry tables
const FORWARDING_SECRET_BYTES: [u8; 32] = [b's'; 32];

/// The offset of the tag inside a single-digit-id dispatch envelope
/// (`{"id":N,"kind":"<kind>"`). Envelope ids are monotonic from 1 and
/// every dispatch in these tests stays under 10 per connection, so the
/// fixtures below can rely on the fixed offset.
const KIND_OFFSET: usize = 16;

fn loopback_any() -> SocketAddr {
    "127.0.0.1:0".parse().expect("valid literal")
}

fn test_service() -> TaskService {
    TaskService::builder()
        .blocking_threads(2)
        .build()
        .expect("task service must build")
}

fn offline_config() -> Config {
    Config {
        online_mode: false,
        forwarding_secret: candyriya_config::ForwardingSecret::new(
            FORWARDING_SECRET_BYTES.to_vec(),
        ),
        ..Config::default()
    }
}

fn backend_config(forwarding: BackendForwarding) -> FakeBackendConfig {
    FakeBackendConfig {
        protocol: PROTOCOL,
        motd: "fixture".to_owned(),
        max_players: 8,
        forwarding,
    }
}

fn server_addr(name: &str, addr: SocketAddr) -> (String, String) {
    (name.to_owned(), format!("{}:{}", addr.ip(), addr.port()))
}

/// One client-login exchange, the way the other proxy tests do it.
async fn client_login(proxy: SocketAddr, name: &str) -> TestClient {
    let mut client = TestClient::connect(proxy).await.expect("connect");
    client
        .send_handshake(PROTOCOL, "play.example.com", 25565, Intent::Login)
        .await
        .expect("handshake");
    client
        .login_start(PROTOCOL, name)
        .await
        .expect("login start");
    client
}

/// A plugin that answers every `on_event` with `{"cancelled": true}` (or
/// false) and nothing else.
fn static_response_wat(response: &str) -> Vec<u8> {
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
    wat::parse_str(format!(
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
    ))
    .expect("static-response fixture wat compiles")
}

/// A plugin that cancels `kind` (unrolled byte compare at [`KIND_OFFSET`])
/// with `reason`, and allows everything else.
fn cancel_on_kind_wat(kind: &str, reason: &str) -> Vec<u8> {
    let denied = format!(
        r#"{{"cancelled":true,"cancel_reason":{}}}"#,
        serde_json::to_string(reason).expect("reason is serializable")
    );
    let allowed = r#"{"cancelled":false}"#;

    let stores = |bytes: &str| -> String {
        bytes
            .as_bytes()
            .iter()
            .enumerate()
            .map(|(i, b)| {
                format!(
                    "\n        (call $store_u8 (i64.add (local.get $p) (i64.const {i})) (i32.const {b}))"
                )
            })
            .collect()
    };

    // fold the per-byte comparisons into one boolean; bytes are compared
    // against constants, no memory needed
    let mut condition = format!(
        "(i32.eq (call $input_load_u8 (i64.const {KIND_OFFSET})) (i32.const {first}))",
        first = kind.as_bytes()[0],
    );
    for (i, b) in kind.as_bytes().iter().enumerate().skip(1) {
        condition = format!(
            "(i32.and {condition} (i32.eq (call $input_load_u8 (i64.const {off})) (i32.const {b})))",
            off = KIND_OFFSET + i,
        );
    }

    wat::parse_str(format!(
        r#"(module
  (import "extism:host/env" "alloc" (func $alloc (param i64) (result i64)))
  (import "extism:host/env" "input_load_u8" (func $input_load_u8 (param i64) (result i32)))
  (import "extism:host/env" "store_u8" (func $store_u8 (param i64 i32)))
  (import "extism:host/env" "output_set" (func $output_set (param i64 i64)))
  (memory (export "memory") 1)
  (func (export "on_event") (result i32)
    (local $p i64)
    (local $len i64)
    (if {condition}
      (then
        (local.set $p (call $alloc (i64.const {denied_len})))
        (local.set $len (i64.const {denied_len})){denied_stores}
      )
      (else
        (local.set $p (call $alloc (i64.const {allowed_len})))
        (local.set $len (i64.const {allowed_len})){allowed_stores}
      ))
    (call $output_set (local.get $p) (local.get $len))
    (i32.const 0)))
"#,
        denied_len = denied.len(),
        denied_stores = stores(&denied),
        allowed_len = allowed.len(),
        allowed_stores = stores(allowed),
    ))
    .expect("cancel-on-kind fixture wat compiles")
}

/// Per-test on-disk layout: `<root>/plugins`, `<root>/libraries`, a
/// cache-rooted [`CacheManager`] at `<root>/data`.
struct PluginFixture {
    _root: tempfile::TempDir,
    plugins_dir: PathBuf,
    libraries_dir: PathBuf,
    cache: CacheManager,
}

impl PluginFixture {
    fn new() -> Self {
        let root = tempfile::TempDir::new().expect("tempdir");
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

    /// Write `<id>/plugin.toml` + `<id>/<id>.wasm`.
    fn write_plugin(&self, id: &str, wasm: &[u8]) {
        let dir = self.plugins_dir.join(id);
        fs::create_dir_all(&dir).expect("plugin dir");
        fs::write(
            dir.join("plugin.toml"),
            format!(
                "id = \"{id}\"\nversion = \"0.1.0\"\nname = \"Fixture {id}\"\nplugin_main = \"on_event\"\nplugin_api = \"0.1\"\n"
            ),
        )
        .expect("manifest");
        fs::write(dir.join(format!("{id}.wasm")), wasm).expect("wasm");
    }
}

impl Drop for PluginFixture {
    fn drop(&mut self) {
        let _ = self.cache.shutdown();
    }
}

/// Start a proxy whose state carries the real dispatcher built from the
/// given plugins. Returns the address, the shutdown handle, the task
/// service and the fixture guard (must outlive the test).
fn start_proxy_with_plugins(
    config: Config,
    plugins: &[(&str, Vec<u8>)],
) -> (SocketAddr, watch::Sender<bool>, TaskService, PluginFixture) {
    let fixture = PluginFixture::new();
    for (id, wasm) in plugins {
        fixture.write_plugin(id, wasm);
    }
    let report = PluginLoader::new(
        fixture.plugins_dir.clone(),
        fixture.libraries_dir.clone(),
        &fixture.cache,
    )
    .load();
    assert!(
        report.issues.is_empty(),
        "unexpected load issues: {:?}",
        report.issues
    );
    let plugin_set = PluginSet::from_vec(report.plugins);
    let dispatcher = Arc::new(ProxyEventDispatcher::new(plugin_set));

    let router = registry_from_config(&config).expect("router from config must build");
    let mut state = common::state(config).with_router(Arc::new(router));
    state.event_dispatcher = Some(dispatcher);
    let state = Arc::new(state);

    let service = test_service();
    let socket = ProxySocket::bind(loopback_any(), &SocketConfig::default()).unwrap();
    let addr = socket.local_addr();
    let listener = socket.into_listener();
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    serve(Arc::clone(&state), listener, shutdown_rx, &service);
    (addr, shutdown_tx, service, fixture)
}

/// The next frame from the client, expecting a login-disconnect packet;
/// returns the reason string the client was shown.
async fn expect_login_disconnect_reason(client: &mut TestClient) -> String {
    let disconnect_id = candyriya_protocol_registry::Registry::embedded()
        .expect("embedded snapshot is valid")
        .packet_id(PROTOCOL, PacketSlot::LoginClientboundDisconnect)
        .expect("slot has a packet id at 767");
    let packet = client.next_packet().await.expect("read").expect("a packet");
    assert_eq!(
        packet.id, disconnect_id,
        "expected a login disconnect packet"
    );
    let mut body: &[u8] = &packet.payload;
    read_string(&mut body, 32767).expect("reason is utf-8")
}

#[tokio::test]
async fn plugin_veto_on_connection_allowed_denies_the_join() {
    let backend = BackendFixture::start(backend_config(BackendForwarding::Off))
        .await
        .expect("fixture starts");

    let mut config = offline_config();
    config.player_info_forwarding_mode = ForwardingMode::Disabled;
    config.servers.servers = HashMap::from([server_addr("lobby", backend.addr())]);
    config.servers.try_servers = vec!["lobby".to_owned()];

    let plugins: Vec<(&str, Vec<u8>)> = vec![(
        "gatekeeper",
        cancel_on_kind_wat("connection_allowed", "nope: stay out"),
    )];
    let (proxy, _shutdown, _service, _fx) = start_proxy_with_plugins(config, &plugins);

    let mut client = client_login(proxy, "Steve").await;
    let reason = expect_login_disconnect_reason(&mut client).await;
    assert!(
        reason.contains("nope: stay out"),
        "the plugin's cancel reason must be passed through, got: {reason}"
    );

    let stats = backend.close().await;
    assert_eq!(
        stats.connections_accepted, 0,
        "the vetoed player must never reach the backend"
    );
}

#[tokio::test]
async fn plugin_silent_cancel_uses_the_localized_kick_text() {
    let backend = BackendFixture::start(backend_config(BackendForwarding::Off))
        .await
        .expect("fixture starts");

    let mut config = offline_config();
    config.player_info_forwarding_mode = ForwardingMode::Disabled;
    config.servers.servers = HashMap::from([server_addr("lobby", backend.addr())]);
    config.servers.try_servers = vec!["lobby".to_owned()];

    // a plugin that cancels everything it is asked, without a reason
    let plugins: Vec<(&str, Vec<u8>)> =
        vec![("mute", static_response_wat(r#"{"cancelled":true}"#))];
    let (proxy, _shutdown, _service, _fx) = start_proxy_with_plugins(config, &plugins);

    let mut client = client_login(proxy, "Steve").await;
    let reason = expect_login_disconnect_reason(&mut client).await;
    assert!(
        reason.contains("Kicked by a plugin."),
        "the localized fallback must be shown, got: {reason}"
    );
    assert!(
        !reason.contains("<red>"),
        "the fallback must be parsed as MiniMessage, not shown raw: {reason}"
    );
}

#[tokio::test]
async fn plugin_veto_on_login_denies_before_the_client_is_told() {
    let backend = BackendFixture::start(backend_config(BackendForwarding::Off))
        .await
        .expect("fixture starts");

    let mut config = offline_config();
    config.player_info_forwarding_mode = ForwardingMode::Disabled;
    config.servers.servers = HashMap::from([server_addr("lobby", backend.addr())]);
    config.servers.try_servers = vec!["lobby".to_owned()];

    let plugins: Vec<(&str, Vec<u8>)> =
        vec![("veto", cancel_on_kind_wat("login", "no entry for you"))];
    let (proxy, _shutdown, _service, _fx) = start_proxy_with_plugins(config, &plugins);

    let mut client = client_login(proxy, "Steve").await;
    let reason = expect_login_disconnect_reason(&mut client).await;
    assert!(
        reason.contains("no entry for you"),
        "the plugin's cancel reason must be passed through, got: {reason}"
    );

    let stats = backend.close().await;
    assert_eq!(
        stats.logins, 1,
        "the backend completed its side of the login; the client just never learned of it"
    );
}

#[tokio::test]
async fn plugin_veto_on_switch_server_denies_the_fallback_hop() {
    let bad = BackendFixture::start(backend_config(BackendForwarding::Disconnect {
        reason: "maintenance".to_owned(),
    }))
    .await
    .expect("fixture starts");
    let good = BackendFixture::start(backend_config(BackendForwarding::Off))
        .await
        .expect("fixture starts");

    let mut config = offline_config();
    config.player_info_forwarding_mode = ForwardingMode::Disabled;
    config.servers.servers = HashMap::from([
        server_addr("bad", bad.addr()),
        server_addr("good", good.addr()),
    ]);
    config.servers.try_servers = vec!["bad".to_owned(), "good".to_owned()];

    // the plugin vetoes the hop to `good`; the veto names the hop, so it
    // does not strand the player on `bad` — it denies the fallback itself
    let plugins: Vec<(&str, Vec<u8>)> = vec![(
        "veto",
        cancel_on_kind_wat("switch_server", "not to that one"),
    )];
    let (proxy, _shutdown, _service, _fx) = start_proxy_with_plugins(config, &plugins);

    let mut client = client_login(proxy, "Steve").await;
    let reason = expect_login_disconnect_reason(&mut client).await;
    assert!(
        reason.contains("not to that one"),
        "the final disconnect must carry the plugin's reason, got: {reason}"
    );

    let good_stats = good.close().await;
    assert_eq!(
        good_stats.connections_accepted, 0,
        "the vetoed fallback hop must not dial the fallback backend"
    );
}

#[tokio::test]
async fn plugin_allows_the_fallback_hop_and_the_chain_proceeds() {
    let bad = BackendFixture::start(backend_config(BackendForwarding::Disconnect {
        reason: "maintenance".to_owned(),
    }))
    .await
    .expect("fixture starts");
    let good = BackendFixture::start(backend_config(BackendForwarding::Off))
        .await
        .expect("fixture starts");

    let mut config = offline_config();
    config.player_info_forwarding_mode = ForwardingMode::Disabled;
    config.servers.servers = HashMap::from([
        server_addr("bad", bad.addr()),
        server_addr("good", good.addr()),
    ]);
    config.servers.try_servers = vec!["bad".to_owned(), "good".to_owned()];

    // a plugin that cancels nothing on the fallback path: the dispatcher
    // is wired, events fire at every hop, the chain proceeds as before
    let plugins: Vec<(&str, Vec<u8>)> = vec![(
        "observer",
        cancel_on_kind_wat("disconnect", "should never be used"),
    )];
    let (proxy, _shutdown, _service, _fx) = start_proxy_with_plugins(config, &plugins);

    let mut client = client_login(proxy, "Steve").await;
    let success = client
        .expect_login_success(PROTOCOL)
        .await
        .expect("login succeeds through the fallback chain");
    assert_eq!(success.name(), "Steve");

    let good_stats = good.close().await;
    let bad_stats = bad.close().await;
    assert_eq!(
        good_stats.logins, 1,
        "the fallback backend served the player"
    );
    assert_eq!(
        bad_stats.forced_disconnects, 1,
        "the kicking backend was tried first"
    );
}
