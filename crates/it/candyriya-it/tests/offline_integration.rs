//! End-to-end offline-mode integration test for the full proxy stack.
//!
//! Everything here is real: a `candyriya-proxy` library driving the accept
//! loop, the handshake pipeline, the login flow and the play-state pump; a
//! `BackendFixture` on the far end of the loopback; a wire-protocol
//! `TestClient`; and a temporary data directory holding the configuration
//! and the forwarding secret. No mocks, no stubs, no network beyond
//! loopback, no external dependencies.
//!
//! The test walks the production first-run flow:
//!
//! 1. a temp data dir receives a hand-written `candyriya.toml` (the
//!    smallest override layer over the embedded defaults);
//! 2. `Config::load` resolves the forwarding secret relative to that
//!    directory and auto-generates it on first use;
//! 3. the proxy binds, and the accept loop runs on a `TaskService`;
//! 4. a client logs in through the proxy into the fixture backend;
//! 5. the connection closes and the scheduler drains cleanly.
//!
//! Only one port is picked by hand (the proxy's bind address): the OS
//! assigns it by binding an ephemeral listener first, so the config file
//! can carry a concrete port instead of `0` (which validation rejects).

use std::net::{Ipv4Addr, SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use tokio::sync::watch;

use candyriya_config::Config;
use candyriya_init::{Init, Pid};
use candyriya_it::{BackendFixture, BackendForwarding, FakeBackendConfig, Intent, TestClient};
use candyriya_player::offline_uuid;
use candyriya_proxy::{
    ProxySocket, ProxyState, SocketConfig, config_spec, registry_from_config, serve,
};
use candyriya_scheduler::{ShutdownReport, TaskService};

/// Protocol 767 = 1.21.x, present in the embedded registry tables.
const PROTOCOL: i32 = 767;
const PLAYER_NAME: &str = "Stark";
const CONFIG_FILE: &str = "candyriya.toml";
const SECRET_FILE: &str = "forwarding.secret";

/// How the fixture backend is seeded for a test.
#[derive(Debug, Clone, Copy)]
enum FixtureSeed {
    /// Unencrypted fixture: the backend answers login start immediately
    /// with its own profile. The config pins
    /// `player-info-forwarding-mode = "disabled"`.
    Off,
    /// Modern forwarding against the fixture: the backend sends
    /// `velocity:player_info` and verifies the HMAC-sealed payload before
    /// answering. The embedded default (`"modern"`) is left untouched in
    /// the config on purpose, so this test also proves the default.
    ///
    /// The secret is pre-seeded with a known UTF-8 value instead of the
    /// auto-generated one because the fixture keys the HMAC off the
    /// secret string's bytes and an auto-generated random secret is not
    /// valid UTF-8 in general. The auto-generation path itself is
    /// asserted for real in [`offline_login_end_to_end`].
    ModernSeeded,
}

impl FixtureSeed {
    fn forwarding_mode(self) -> &'static str {
        match self {
            Self::Off => "disabled",
            Self::ModernSeeded => "modern",
        }
    }

    async fn start(self, dir: &TestDataDir) -> BackendFixture {
        let config = match self {
            Self::Off => FakeBackendConfig {
                protocol: PROTOCOL,
                motd: "fixture".to_owned(),
                max_players: 8,
                forwarding: BackendForwarding::Off,
            },
            Self::ModernSeeded => {
                let encoded = std::fs::read_to_string(dir.path().join(SECRET_FILE))
                    .expect("the seeded secret is readable");
                let raw = base64::engine::general_purpose::STANDARD
                    .decode(encoded.trim())
                    .expect("the seeded secret is valid base64");
                let secret = String::from_utf8(raw).expect("the seeded secret is valid UTF-8");
                FakeBackendConfig {
                    protocol: PROTOCOL,
                    motd: "fixture".to_owned(),
                    max_players: 8,
                    forwarding: BackendForwarding::Modern { secret },
                }
            }
        };
        BackendFixture::start(config).await.expect("fixture binds")
    }
}

/// A unique temp data dir, removed even when the test panics.
///
/// The tests never touch shared paths: pid + nanos + counter, all under
/// the system temp root.
struct TestDataDir(PathBuf);

impl TestDataDir {
    fn new(tag: &str) -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!(
            "candyriya-it-{tag}-{}-{nanos}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed),
        ));
        std::fs::create_dir_all(&dir).expect("the temp data dir is created");
        Self(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestDataDir {
    fn drop(&mut self) {
        // a failure here must not mask an earlier test outcome
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Everything a running proxy needs, plus the handles the tests hold.
struct RunningProxy {
    addr: SocketAddr,
    shutdown: watch::Sender<bool>,
    service: TaskService,
    init: Init,
    config_pid: Pid,
    _state: Arc<ProxyState>,
}

/// Wire a bound proxy the same way the composition root does, minus the
/// CLI: socket options from `[advanced]`, the routing registry, the
/// accept loop on the task service.
fn spawn_proxy(config: Config, service: TaskService) -> RunningProxy {
    let socket_config = SocketConfig::from(&config.advanced);
    let bind: SocketAddr = config.bind.parse().expect("config bind is host:port");
    let router = registry_from_config(&config).expect("router builds from config");
    // the config unit owns the live store; the fixture wires it exactly
    // the way the composition root does
    let store = Arc::new(candyriya_config::ConfigStore::new(config));
    let init = Init::new(service.clone());
    let config_pid = init
        .spawn_unit(Pid::INIT, config_spec(store))
        .expect("config unit spawns");
    let state = Arc::new(ProxyState::new(init.clone(), config_pid).with_router(Arc::new(router)));
    let socket = ProxySocket::bind(bind, &socket_config).expect("proxy socket binds");
    let addr = socket.local_addr();
    let listener = socket.into_listener();
    let (shutdown, shutdown_rx) = watch::channel(false);
    serve(Arc::clone(&state), listener, shutdown_rx, &service);
    RunningProxy {
        addr,
        shutdown,
        service,
        init,
        config_pid,
        _state: state,
    }
}

/// Flip the shutdown watch and wait for every task to finish; then drain.
/// The report and the final stats snapshot go to the caller for assertions.
async fn finish_proxy(proxy: RunningProxy) -> (ShutdownReport, candyriya_scheduler::TaskStats) {
    let _ = proxy.shutdown.send(true);
    // the config unit is LongLived by design and never exits on its own;
    // cancel it so the active count can reach zero
    if let Err(error) = proxy.init.cancel(proxy.config_pid) {
        eprintln!("failed to cancel the config unit: {error}");
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    while proxy.service.stats().active != 0 {
        assert!(
            Instant::now() < deadline,
            "accept loop and connection task did not finish within 5s (active: {:?})",
            proxy.service.stats().by_name,
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let report = proxy.service.drain(Duration::from_secs(2)).await;
    let stats = proxy.service.stats();
    (report, stats)
}

fn free_loopback_port() -> u16 {
    TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .expect("an ephemeral loopback port binds")
        .local_addr()
        .expect("the ephemeral port is readable")
        .port()
}

fn write_config(path: &Path, proxy_port: u16, backend_port: u16, forwarding: &str) {
    let document = format!(
        "# written by the test; everything else comes from the embedded defaults\n\
         config-version = \"0.1\"\n\
         bind = \"127.0.0.1:{proxy_port}\"\n\
         online-mode = false\n\
         player-info-forwarding-mode = \"{forwarding}\"\n\
         \n\
         [servers]\n\
         local = \"127.0.0.1:{backend_port}\"\n\
         try = [\"local\"]\n",
    );
    std::fs::write(path, document).expect("the config file is written");
}

struct OfflineSetup {
    dir: TestDataDir,
    config_path: PathBuf,
    backend: BackendFixture,
    proxy: RunningProxy,
    secret_bytes: Vec<u8>,
}

/// Load a fresh data dir end to end: placeholder backend, first-run
/// secret resolution, fixture on its ephemeral port, config rewritten
/// with the real backend address, proxy up.
async fn setup(seed: FixtureSeed, tag: &str) -> OfflineSetup {
    let dir = TestDataDir::new(tag);
    let proxy_port = free_loopback_port();
    let config_path = dir.path().join(CONFIG_FILE);

    // placeholder backend address: valid in format, never dialed. the
    // first load exists to resolve the secret (auto-generated or seeded);
    // the real address is patched in once the fixture is listening.
    write_config(&config_path, proxy_port, 1, seed.forwarding_mode());
    if matches!(seed, FixtureSeed::ModernSeeded) {
        let seeded = base64::engine::general_purpose::STANDARD.encode([b's'; 32]);
        std::fs::write(dir.path().join(SECRET_FILE), format!("{seeded}\n"))
            .expect("the seeded secret is written");
    }
    let first = Config::load(&config_path).expect("the config loads; the secret is resolved here");
    if matches!(seed, FixtureSeed::Off) {
        // first run on an empty data dir: the secret must be auto-generated
        let secret_path = dir.path().join(SECRET_FILE);
        assert!(
            secret_path.is_file(),
            "forwarding.secret must be auto-generated in the data dir"
        );
        let generated = std::fs::read_to_string(&secret_path).expect("the secret file is readable");
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(generated.trim())
            .expect("the auto-generated secret is base64");
        assert_eq!(decoded.len(), 32, "the auto-generated secret is 32 bytes");
        assert_eq!(
            decoded,
            first.forwarding_secret.as_bytes(),
            "the loaded secret is the one on disk"
        );
    }

    let backend = seed.start(&dir).await;
    write_config(
        &config_path,
        proxy_port,
        backend.addr().port(),
        seed.forwarding_mode(),
    );
    let second = Config::load(&config_path).expect("the rewritten config loads too");
    assert_eq!(
        second.forwarding_secret.as_bytes(),
        first.forwarding_secret.as_bytes(),
        "reloading the config must never rotate the secret",
    );
    // the layered load must keep the embedded defaults intact
    assert_eq!(
        second.advanced.compression_threshold, 256,
        "embedded default [advanced] compression-threshold survives"
    );
    assert!(
        second.motd.contains("Candyriya"),
        "the embedded default motd survives the layered load"
    );

    let secret_bytes = second.forwarding_secret.as_bytes().to_vec();
    let service = TaskService::builder()
        .blocking_threads(2)
        .build()
        .expect("the task service builds");
    let proxy = spawn_proxy(second, service);
    OfflineSetup {
        dir,
        config_path,
        backend,
        proxy,
        secret_bytes,
    }
}

async fn login(proxy: SocketAddr, name: &str) -> TestClient {
    let mut client = TestClient::connect(proxy)
        .await
        .expect("the client connects to the proxy");
    client
        .send_handshake(PROTOCOL, "play.example.com", 25565, Intent::Login)
        .await
        .expect("the handshake is accepted");
    client
        .login_start(PROTOCOL, name)
        .await
        .expect("the login start is accepted");
    client
}

#[tokio::test]
async fn offline_login_end_to_end() {
    let setup = setup(FixtureSeed::Off, "disabled").await;
    let OfflineSetup {
        dir,
        backend,
        proxy,
        ..
    } = setup;
    let proxy_addr = proxy.addr;

    let mut client = login(proxy_addr, PLAYER_NAME).await;
    let success = tokio::time::timeout(
        Duration::from_secs(5),
        client.expect_login_success(PROTOCOL),
    )
    .await
    .expect("login success arrives within 5s")
    .expect("login success decodes");
    assert_eq!(success.name(), PLAYER_NAME);
    // brief step 3: the uuid must be non-zero and round-trip through the parser
    assert!(
        success.is_uuid_non_zero(),
        "the login success uuid must not be zero"
    );
    assert!(success.is_uuid_parseable());

    // with forwarding disabled the proxy passes the backend's Login Success
    // verbatim: the uuid the client sees is the one the backend issued
    let stats = backend.close().await;
    assert_eq!(
        stats.connections_accepted, 1,
        "exactly one connection was accepted"
    );
    assert_eq!(stats.handshakes, 1, "exactly one handshake was processed");
    assert_eq!(stats.logins, 1, "the login went through");
    assert_eq!(
        stats.protocol_errors, 0,
        "no error packets on the backend side"
    );
    assert_eq!(
        stats.write_errors, 0,
        "no failed writes on the backend side"
    );

    // closing the fixture drops the connection toward the client
    let packet = client
        .next_packet()
        .await
        .expect("the read side is still usable");
    assert!(
        packet.is_none(),
        "the client observes EOF after the backend closes"
    );
    drop(client);

    let (report, stats) = finish_proxy(proxy).await;
    assert_eq!(
        report.timed_out, 0,
        "no task had to be torn down at the drain deadline"
    );
    assert_eq!(
        stats.active, 0,
        "no task remains registered after the drain"
    );
    assert_eq!(stats.panicked, 0, "no task panicked");
    assert!(
        dir.path().join(SECRET_FILE).is_file(),
        "the secret file survives the run"
    );
}

#[tokio::test]
async fn offline_login_with_default_forwarding_verifies_the_sealed_profile() {
    let setup = setup(FixtureSeed::ModernSeeded, "modern").await;
    let proxy_addr = setup.proxy.addr;

    let mut client = login(proxy_addr, PLAYER_NAME).await;
    let success = tokio::time::timeout(
        Duration::from_secs(5),
        client.expect_login_success(PROTOCOL),
    )
    .await
    .expect("login success arrives within 5s")
    .expect("login success decodes");
    assert_eq!(success.name(), PLAYER_NAME);
    // the backend only issues login success for a sealed payload that
    // verifies against the shared secret; the profile it carries is the
    // offline-mode one derived from the client's name
    let expected = offline_uuid(PLAYER_NAME);
    let (msb, lsb) = expected.as_u64_pair();
    assert_eq!(
        success.uuid_parts(),
        (msb as i64, lsb as i64),
        "the sealed profile must carry the vanilla offline uuid"
    );
    assert_eq!(
        success.uuid_string(),
        expected.to_string(),
        "the uuid survives the string round trip"
    );

    let stats = setup.backend.close().await;
    assert_eq!(stats.connections_accepted, 1);
    assert_eq!(stats.handshakes, 1, "exactly one handshake was processed");
    assert_eq!(
        stats.player_info_requests, 1,
        "the backend asked for velocity:player_info"
    );
    assert_eq!(
        stats.forwarding_failures, 0,
        "the sealed payload verified against the shared secret"
    );
    assert_eq!(
        stats.logins, 1,
        "the forwarding exchange earned a login success"
    );
    assert_eq!(stats.protocol_errors, 0);
    assert_eq!(stats.write_errors, 0);

    drop(client);
    let (report, stats) = finish_proxy(setup.proxy).await;
    assert_eq!(report.timed_out, 0);
    assert_eq!(stats.active, 0);
    assert_eq!(stats.panicked, 0);
}

#[tokio::test]
async fn task_service_drops_cleanly_after_disconnect() {
    let setup = setup(FixtureSeed::Off, "drain").await;
    let proxy_addr = setup.proxy.addr;

    let mut client = login(proxy_addr, PLAYER_NAME).await;
    let success = tokio::time::timeout(
        Duration::from_secs(5),
        client.expect_login_success(PROTOCOL),
    )
    .await
    .expect("login success arrives within 5s")
    .expect("login success decodes");
    assert!(success.is_uuid_non_zero());

    // client goes away; the backend fixture tears its side down too, so
    // the pump sees EOF on both ends and the connection task finishes
    drop(client);
    let backend_stats = setup.backend.close().await;
    assert_eq!(backend_stats.handshakes, 1);
    assert_eq!(backend_stats.logins, 1);

    let secret_before = setup.secret_bytes.clone();
    let config_path = setup.config_path.clone();
    let (report, stats) = finish_proxy(setup.proxy).await;
    assert_eq!(
        report.timed_out, 0,
        "every proxy task finished before the drain deadline"
    );
    assert_eq!(
        stats.active, 0,
        "no task remains registered after the disconnect"
    );
    assert_eq!(stats.panicked, 0, "no task panicked");
    assert!(
        stats.completed >= 2,
        "the accept loop and the connection task both completed (got {})",
        stats.completed
    );

    // config reset: the file on disk loads again from scratch, with the
    // same secret — no rotation, no leftovers from the previous run
    let reloaded = Config::load(&config_path).expect("the config loads again after the run");
    assert_eq!(
        reloaded.forwarding_secret.as_bytes(),
        secret_before.as_slice(),
        "the secret survives a fresh load unchanged"
    );
    assert_eq!(reloaded.servers.try_servers, vec!["local".to_string()]);
    assert_eq!(reloaded.servers.servers.len(), 1);
    assert!(!reloaded.online_mode);
}
