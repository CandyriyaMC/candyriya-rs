//! Integration tests for the connection units.
//!
//! After the Login Success is delivered the session lives inside the
//! `conn-<n>` unit: pump counters, the cooperative park gate and the
//! tree's exit path are all observable from outside. These tests drive
//! full sessions through the real accept loop and the loopback
//! `BackendFixture`, exactly like the play-state hot-path tests, and
//! pin down what ps will later be able to see: the unit exists under
//! `players`, its counters are exact, freezing parks it, and the
//! tree-wide cancel sweep ends it with the client observing an EOF.
//!
//! Play-state packet ids are arbitrary (the pump does not parse).

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::BytesMut;
use tokio::sync::watch;

use candyriya_config::{Config, ForwardingMode};
use candyriya_it::{BackendFixture, BackendForwarding, FakeBackendConfig, Intent, TestClient};
use candyriya_protocol::write_string;
use candyriya_scheduler::TaskService;

use candyriya_init::{ConnectionInfo, ExitStatus, Init, Pid, ReapOutcome, UnitKind, UnitState};
use candyriya_proxy::{ProxySocket, ProxyState, SocketConfig, registry_from_config, serve};

mod common;

const PROTOCOL: i32 = 767; // 1.21.x, present in the embedded registry tables

/// Arbitrary serverbound play packet id.
const CLIENT_PLAY_ID: i32 = 0x71;
/// Arbitrary clientbound play packet id.
const BACKEND_PLAY_ID: i32 = 0x70;

fn start_proxy(state: Arc<ProxyState>) -> (SocketAddr, watch::Sender<bool>, TaskService) {
    let service = TaskService::builder()
        .blocking_threads(2)
        .build()
        .expect("task service must build");
    let socket = ProxySocket::bind(
        "127.0.0.1:0".parse().expect("valid literal"),
        &SocketConfig::default(),
    )
    .unwrap();
    let addr = socket.local_addr();
    let listener = socket.into_listener();
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    serve(Arc::clone(&state), listener, shutdown_rx, &service);
    (addr, shutdown_tx, service)
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

fn config_for(backend: SocketAddr) -> Config {
    let mut config = Config {
        online_mode: false,
        ..Config::default()
    };
    config.player_info_forwarding_mode = ForwardingMode::Disabled;
    config.servers.servers = HashMap::from([server_addr("lobby", backend)]);
    config.servers.try_servers = vec!["lobby".to_owned()];
    config
}

/// VarInt encoder (matches the wire format; independent on purpose).
fn varint(value: i32) -> Vec<u8> {
    let mut remaining = value as u32;
    let mut out = Vec::new();
    loop {
        let mut byte = (remaining & 0x7F) as u8;
        remaining >>= 7;
        if remaining != 0 {
            byte |= 0x80;
        }
        out.push(byte);
        if remaining == 0 {
            break;
        }
    }
    out
}

fn chat_payload(message: &str) -> Vec<u8> {
    let mut payload = BytesMut::new();
    write_string(&mut payload, message, 256).expect("message fits the cap");
    payload.to_vec()
}

fn frame(id: i32, payload: &[u8]) -> Vec<u8> {
    let mut body = varint(id);
    body.extend_from_slice(payload);
    let mut out = varint(body.len() as i32);
    out.extend_from_slice(&body);
    out
}

/// Poll `cond` until it holds, with a hard deadline.
async fn wait_until(mut cond: impl FnMut() -> bool, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !cond() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Log a client in; return it along with the tree, the players pid and
/// the shutdown handle, so the test can pin down the unit that runs the
/// session.
async fn logged_in_session(
    backend: &BackendFixture,
) -> (
    TestClient,
    Arc<ProxyState>,
    watch::Sender<bool>,
    TaskService,
    Init,
    Pid,
) {
    let config = config_for(backend.addr());
    let router = registry_from_config(&config).expect("router builds");
    let (state, init, players_pid) = common::state_with_players(config);
    let state = Arc::new(state.with_router(Arc::new(router)));
    let (proxy, shutdown, service) = start_proxy(Arc::clone(&state));
    let mut client = TestClient::connect(proxy).await.expect("connect");
    client
        .send_handshake(PROTOCOL, "play.example.com", 25565, Intent::Login)
        .await
        .expect("handshake");
    client
        .login_start(PROTOCOL, "Steve")
        .await
        .expect("login start");
    client
        .expect_login_success(PROTOCOL)
        .await
        .expect("login success");
    (client, state, shutdown, service, init, players_pid)
}

async fn wait_for_single_conn(tree: &Init) -> ConnectionInfo {
    wait_until(
        || tree.connections().len() == 1,
        "the connection unit to register",
    )
    .await;
    tree.connections()
        .into_iter()
        .next()
        .expect("one connection unit")
}

#[tokio::test]
async fn conn_unit_exists_under_players_with_the_right_parent() {
    let backend = BackendFixture::start(backend_config(BackendForwarding::Off))
        .await
        .expect("fixture starts");
    let (client, _state, _shutdown, _service, init, players_pid) =
        logged_in_session(&backend).await;

    let conn = wait_for_single_conn(&init).await;
    assert_eq!(conn.name, "conn-1");
    assert_eq!(conn.state, UnitState::Running);
    let unit = init.unit(conn.pid).expect("the unit read model exists");
    assert_eq!(unit.kind, UnitKind::Connection);
    assert_eq!(
        unit.ppid, players_pid,
        "the connection unit must hang under players, not under init"
    );
    assert_eq!(
        init.unit(players_pid).expect("players exists").kind,
        UnitKind::LongLived,
        "players is a group node"
    );

    drop(client);
    let _ = backend.close().await;
}

#[tokio::test]
async fn conn_unit_counters_increment_on_the_loopback_exchange() {
    let backend = BackendFixture::start(backend_config(BackendForwarding::Off))
        .await
        .expect("fixture starts");
    let (mut client, _state, _shutdown, _service, init, _players_pid) =
        logged_in_session(&backend).await;
    let conn = wait_for_single_conn(&init).await;

    // client -> backend: the frame lands once, counted once
    let outgoing_payload = chat_payload("counter check");
    let outgoing = frame(CLIENT_PLAY_ID, &outgoing_payload);
    client
        .send_chat_message(CLIENT_PLAY_ID, "counter check")
        .await
        .expect("send frame");
    wait_until(
        || backend.play_bytes_recorded() >= outgoing.len() as u64,
        "the backend to record the frame",
    )
    .await;
    wait_until(
        || {
            init.connection_info(conn.pid)
                .is_some_and(|info| info.bytes_rx == outgoing.len() as u64)
        },
        "the conn unit to count the client bytes",
    )
    .await;

    // backend -> client: the injected frame lands once, counted once
    let incoming = frame(BACKEND_PLAY_ID, &chat_payload("from the server"));
    assert_eq!(backend.send_play_bytes(&incoming).await, 1);
    client
        .next_packet()
        .await
        .expect("read")
        .expect("a packet arrives");
    wait_until(
        || {
            init.connection_info(conn.pid)
                .is_some_and(|info| info.bytes_tx == incoming.len() as u64)
        },
        "the conn unit to count the backend bytes",
    )
    .await;

    let info = init.connection_info(conn.pid).expect("the connection unit");
    assert_eq!(info.bytes_rx, outgoing.len() as u64);
    assert_eq!(info.bytes_tx, incoming.len() as u64);
    assert!(info.last_activity >= info.connected_at);

    drop(client);
    let _ = backend.close().await;
}

#[tokio::test]
async fn freeze_parks_the_connection_until_unfreeze() {
    let backend = BackendFixture::start(backend_config(BackendForwarding::Off))
        .await
        .expect("fixture starts");
    let (mut client, _state, _shutdown, _service, init, _players_pid) =
        logged_in_session(&backend).await;
    let conn = wait_for_single_conn(&init).await;

    // priming frame: forces the pump to cycle once, so after the freeze
    // the pump is guaranteed to reach its parked state at the loop
    // boundary and is not merely "slow to notice"
    let prime_payload = chat_payload("prime");
    let prime = frame(CLIENT_PLAY_ID, &prime_payload);
    client
        .send_chat_message(CLIENT_PLAY_ID, "prime")
        .await
        .expect("send prime");
    wait_until(
        || backend.play_bytes_recorded() >= prime.len() as u64,
        "the backend to record the prime frame",
    )
    .await;

    assert!(init.freeze(conn.pid).expect("freeze"));
    // let the pump land in its parked state at the loop boundary
    tokio::time::sleep(Duration::from_millis(100)).await;

    // client -> backend: bytes must NOT flow while frozen
    let frozen_payload = chat_payload("frozen");
    let frozen_frame = frame(CLIENT_PLAY_ID, &frozen_payload);
    client
        .send_chat_message(CLIENT_PLAY_ID, "frozen")
        .await
        .expect("send frame");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        backend.play_bytes_recorded(),
        prime.len() as u64,
        "a frozen connection must not forward client bytes"
    );
    assert_eq!(
        init.connection_info(conn.pid).expect("info").bytes_rx,
        prime.len() as u64,
        "a frozen pump must not count client bytes either"
    );

    // backend -> client: bytes must NOT flow while frozen either
    let server_frame = frame(BACKEND_PLAY_ID, &chat_payload("server waits"));
    assert_eq!(backend.send_play_bytes(&server_frame).await, 1);
    let read = tokio::time::timeout(Duration::from_millis(400), client.next_packet()).await;
    assert!(
        read.is_err(),
        "a frozen connection must deliver nothing to the client"
    );

    // unfreeze: both directions resume
    assert!(init.unfreeze(conn.pid).expect("unfreeze"));
    wait_until(
        || backend.play_bytes_recorded() >= (prime.len() + frozen_frame.len()) as u64,
        "the backend to receive the queued client frame after unfreeze",
    )
    .await;
    let packet = tokio::time::timeout(Duration::from_secs(2), client.next_packet())
        .await
        .expect("the client is not starved")
        .expect("read")
        .expect("a packet arrives after unfreeze");
    assert_eq!(packet.id, BACKEND_PLAY_ID);

    let info = init.connection_info(conn.pid).expect("info");
    assert_eq!(info.bytes_rx, (prime.len() + frozen_frame.len()) as u64);
    assert_eq!(info.bytes_tx, server_frame.len() as u64);
    assert!(!info.parked);

    drop(client);
    let _ = backend.close().await;
}

#[tokio::test]
async fn tree_wide_cancel_sweep_ends_a_live_connection_unit() {
    let backend = BackendFixture::start(backend_config(BackendForwarding::Off))
        .await
        .expect("fixture starts");
    let (mut client, _state, _shutdown, _service, init, players_pid) =
        logged_in_session(&backend).await;
    let conn = wait_for_single_conn(&init).await;

    let swept = init.cancel_all();
    assert!(swept.contains(&conn.pid), "the sweep reaches the conn unit");
    assert!(
        swept.contains(&players_pid),
        "the sweep reaches the players group"
    );

    // the client observes the disconnect instead of a hang
    let packet = tokio::time::timeout(Duration::from_secs(2), client.next_packet())
        .await
        .expect("the client is not left hanging")
        .expect("read");
    assert!(
        packet.is_none(),
        "the client must observe EOF once the tree cancels the conn"
    );

    // the conn unit exits as Cancelled, not as success
    wait_until(
        || {
            init.unit(conn.pid)
                .is_some_and(|unit| unit.state == UnitState::Zombie)
        },
        "the conn unit to exit",
    )
    .await;
    // conn zombies could sit under a still-running players; harvest
    // both places, then check the record from wherever it landed
    let mut status = None;
    for _ in 0..3 {
        for outcome in init
            .reap_children(players_pid)
            .into_iter()
            .chain(init.reap_children(Pid::INIT))
        {
            if let ReapOutcome::Reaped(record) = outcome
                && record.pid == conn.pid
            {
                status = Some(record.status);
            }
        }
        if status.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(status, Some(ExitStatus::Cancelled));

    let _ = backend.close().await;
}

#[tokio::test]
async fn no_detached_unit_survives_the_cancel_sweep() {
    let backend = BackendFixture::start(backend_config(BackendForwarding::Off))
        .await
        .expect("fixture starts");

    let config = config_for(backend.addr());
    let router = registry_from_config(&config).expect("router builds");
    let (state, init, players_pid) = common::state_with_players(config);
    let state = Arc::new(state.with_router(Arc::new(router)));
    let (proxy, shutdown, service) = start_proxy(state);

    let mut clients = Vec::new();
    for _ in 0..2 {
        let mut client = TestClient::connect(proxy).await.expect("connect");
        client
            .send_handshake(PROTOCOL, "play.example.com", 25565, Intent::Login)
            .await
            .expect("handshake");
        client
            .login_start(PROTOCOL, "Steve")
            .await
            .expect("login start");
        client
            .expect_login_success(PROTOCOL)
            .await
            .expect("login success");
        clients.push(client);
    }
    wait_until(
        || init.connections().len() == 2,
        "both connections to register",
    )
    .await;

    // cascade shape: flip the watch (the accept loop stops), then sweep
    let _ = shutdown.send(true);
    let swept = init.cancel_all();
    assert!(
        swept.len() >= 5,
        "config, route, players and both conns are swept, got {swept:?}"
    );

    // everything the sweep stopped is reaped into the ring; nothing detached remains
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let _ = init.reap_children(players_pid);
        let _ = init.reap_children(Pid::INIT);
        if (init.units().len() == 1 && service.stats().active == 0) || Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    assert_eq!(
        init.units().len(),
        1,
        "only the root remains: {:?}",
        init.units()
    );
    assert_eq!(service.stats().active, 0, "no task outlives the sweep");
    assert!(init.connections().is_empty());

    let report = service.drain(Duration::from_secs(5)).await;
    assert_eq!(report.timed_out, 0, "the drain finds nothing still running");

    drop(clients);
    let _ = backend.close().await;
}
