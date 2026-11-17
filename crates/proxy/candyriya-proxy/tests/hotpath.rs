//! Integration tests for the play-state hot path.
//!
//! After the Login Success is delivered, the proxy stops parsing packets
//! and copies bytes bidirectionally. These tests drive full sessions
//! through the real accept loop and the loopback `BackendFixture`:
//! byte-exact copies in both directions, clean teardown on either side,
//! backpressure while the client reads nothing, and parallel sessions.
//!
//! Play-state packet ids are deliberately not tracked by the registry;
//! the ids used below are arbitrary and only meaningful as "the id that
//! came in is the id that goes out".

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use bytes::BytesMut;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::watch;
use tokio_util::codec::Decoder;

use candyriya_config::{Config, ForwardingMode};
use candyriya_it::{
    BackendFixture, BackendForwarding, FakeBackendConfig, Intent, TestClient, uuid_from_name,
};
use candyriya_protocol::{Packet, PacketCodec, read_string, write_string};
use candyriya_protocol_registry::{PacketSlot, Registry};
use candyriya_scheduler::TaskService;

use candyriya_proxy::{ProxySocket, ProxyState, SocketConfig, registry_from_config, serve};

const PROTOCOL: i32 = 767; // 1.21.x, present in the embedded registry tables

/// The 32 bytes the proxy and the modern-forwarding fixtures share.
const FORWARDING_SECRET_BYTES: [u8; 32] = [b's'; 32];

/// Arbitrary serverbound play packet id (the client never speaks to the
/// registry about play state; the fixture accepts any id).
const CLIENT_PLAY_ID: i32 = 0x71;
/// Arbitrary clientbound play packet id (used for the backend -> client
/// direction).
const BACKEND_PLAY_ID: i32 = 0x70;

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

fn start_proxy(config: Config) -> (SocketAddr, watch::Sender<bool>, TaskService) {
    let router = registry_from_config(&config).expect("router from config must build");
    let state = Arc::new(ProxyState::new(Arc::new(config)).with_router(Arc::new(router)));
    start_proxy_with_state(state)
}

fn start_proxy_with_state(
    state: Arc<ProxyState>,
) -> (SocketAddr, watch::Sender<bool>, TaskService) {
    let service = test_service();
    let socket = ProxySocket::bind(loopback_any(), &SocketConfig::default()).unwrap();
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

/// Standard client handshake: intent=login, protocol from [`PROTOCOL`].
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

/// Config routed at `backend` only.
fn config_for(backend: SocketAddr) -> Config {
    let mut config = offline_config();
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

/// `frame(id, payload)`: the exact bytes a vanilla peer would emit for
/// `packet id || payload` inside a frame.
fn frame(id: i32, payload: &[u8]) -> Vec<u8> {
    let mut body = varint(id);
    body.extend_from_slice(payload);
    let mut out = varint(body.len() as i32);
    out.extend_from_slice(&body);
    out
}

/// Poll `cond` until it holds, with a hard deadline. Fails the test on
/// timeout instead of hanging forever.
async fn wait_until(mut cond: impl FnMut() -> bool, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !cond() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn hot_path_copies_bytes_verbatim_both_ways() {
    let backend = BackendFixture::start(backend_config(BackendForwarding::Off))
        .await
        .expect("fixture starts");
    let (proxy, _shutdown, _service) = start_proxy(config_for(backend.addr()));

    let mut client = client_login(proxy, "Steve").await;
    client
        .expect_login_success(PROTOCOL)
        .await
        .expect("login success");

    // backend -> client: an arbitrary frame injected by the fixture must
    // reach the client byte-for-byte
    let incoming = frame(BACKEND_PLAY_ID, &chat_payload("hello from the server"));
    assert_eq!(
        backend.send_play_bytes(&incoming).await,
        1,
        "the play-state connection must be registered with the fixture"
    );
    let packet = client
        .next_packet()
        .await
        .expect("read")
        .expect("a packet must arrive");
    assert_eq!(packet.id, BACKEND_PLAY_ID);
    let mut body: &[u8] = &packet.payload;
    let message = read_string(&mut body, 256).expect("chat string");
    assert_eq!(message, "hello from the server");

    // client -> backend: the frame the fixture records must equal the
    // frame the client emitted, byte for byte
    client
        .send_chat_message(CLIENT_PLAY_ID, "hello from the player")
        .await
        .expect("chat message");
    let expected = frame(CLIENT_PLAY_ID, &chat_payload("hello from the player"));
    wait_until(
        || backend.play_bytes_recorded() >= expected.len() as u64,
        "the backend to record the client's frame",
    )
    .await;
    assert_eq!(backend.take_play_bytes(), expected);

    // a clean client close must tear down both ends
    drop(client);
    wait_until(
        || backend.stats().connections_closed >= 1,
        "the backend to observe the client going away",
    )
    .await;
}

#[tokio::test]
async fn client_close_tears_down_both_connections() {
    let backend = BackendFixture::start(backend_config(BackendForwarding::Off))
        .await
        .expect("fixture starts");
    let (proxy, _shutdown, _service) = start_proxy(config_for(backend.addr()));

    let client = client_login(proxy, "Steve").await;
    expect_success_and_drop(client).await;

    wait_until(
        || backend.stats().connections_closed >= 1,
        "the backend connection to close after the client dropped",
    )
    .await;
    let stats = backend.close().await;
    assert_eq!(stats.connections_closed, 1);
}

/// Log the client in, then drop it (TCP FIN) without playing along.
async fn expect_success_and_drop(mut client: TestClient) {
    client
        .expect_login_success(PROTOCOL)
        .await
        .expect("login success");
    drop(client);
}

#[tokio::test]
async fn backend_close_tears_down_both_connections() {
    let backend = BackendFixture::start(backend_config(BackendForwarding::Off))
        .await
        .expect("fixture starts");
    let (proxy, _shutdown, _service) = start_proxy(config_for(backend.addr()));

    let mut client = client_login(proxy, "Steve").await;
    client
        .expect_login_success(PROTOCOL)
        .await
        .expect("login success");

    backend.close().await;

    let packet = client.next_packet().await.expect("read");
    assert!(
        packet.is_none(),
        "the client must observe EOF once the backend is gone, got {packet:?}"
    );
}

#[tokio::test]
async fn client_can_burst_256_kib_chunks_without_reading() {
    let backend = BackendFixture::start(backend_config(BackendForwarding::Off))
        .await
        .expect("fixture starts");
    let (proxy, _shutdown, _service) = start_proxy(config_for(backend.addr()));

    let mut client = client_login(proxy, "Steve").await;
    client
        .expect_login_success(PROTOCOL)
        .await
        .expect("login success");

    // 4 MiB total, in 256 KiB chunks; the client never reads a byte back,
    // so the proxy may only forward what the kernel buffers absorb and
    // the fixture drains — the pump holds exactly one 8 KiB buffer per
    // direction, so there is nowhere for 4 MiB to accumulate.
    const CHUNK: usize = 256 * 1024;
    const CHUNKS: usize = 16;
    let payload = vec![0x5A; CHUNK];
    let one = frame(CLIENT_PLAY_ID, &payload);
    let mut expected = Vec::with_capacity(one.len() * CHUNKS);
    for _ in 0..CHUNKS {
        expected.extend_from_slice(&one);
        client
            .send_frame(CLIENT_PLAY_ID, &payload)
            .await
            .expect("send chunk");
    }

    wait_until(
        || backend.play_bytes_recorded() >= expected.len() as u64,
        "the backend to receive the full burst",
    )
    .await;
    assert_eq!(
        backend.play_bytes_dropped(),
        0,
        "the burst fits the record cap"
    );
    assert_eq!(backend.take_play_bytes(), expected);

    drop(client);
    wait_until(
        || backend.stats().connections_closed >= 1,
        "the backend to observe the client going away",
    )
    .await;
}

#[tokio::test]
async fn eight_parallel_sessions_stay_independent() {
    let backend = BackendFixture::start(backend_config(BackendForwarding::Off))
        .await
        .expect("fixture starts");
    let (proxy, _shutdown, _service) = start_proxy(config_for(backend.addr()));

    // eight players log in and hold their sessions at once
    let mut clients = Vec::with_capacity(8);
    let mut expected_total = 0u64;
    for i in 0..8u8 {
        let mut client = client_login(proxy, &format!("Player{i}")).await;
        client
            .expect_login_success(PROTOCOL)
            .await
            .expect("login success");
        // a distinct-sized frame per connection: byte totals are
        // order-agnostic and interleave-safe
        let payload = vec![b'p' + i; 32 + usize::from(i)];
        expected_total += frame(CLIENT_PLAY_ID, &payload).len() as u64;
        client
            .send_frame(CLIENT_PLAY_ID, &payload)
            .await
            .expect("send chunk");
        clients.push(client);
    }

    // one broadcast reaches all eight independent copy paths verbatim
    let broadcast = frame(BACKEND_PLAY_ID, &chat_payload("broadcast to everyone"));
    assert_eq!(backend.send_play_bytes(&broadcast).await, 8);

    for client in clients.iter_mut() {
        let packet = client
            .next_packet()
            .await
            .expect("read")
            .expect("a packet must arrive");
        assert_eq!(packet.id, BACKEND_PLAY_ID);
        let mut body: &[u8] = &packet.payload;
        assert_eq!(
            read_string(&mut body, 256).expect("chat string"),
            "broadcast to everyone"
        );
    }

    wait_until(
        || backend.play_bytes_recorded() >= expected_total,
        "the backend to receive all eight frames",
    )
    .await;
    assert_eq!(
        backend.play_bytes_recorded(),
        expected_total,
        "every byte must land exactly once, interleaved arbitrarily"
    );

    // tear all sessions down; every backend connection must close
    drop(clients);
    wait_until(
        || backend.stats().connections_closed >= 8,
        "all eight backend connections to close",
    )
    .await;
}

// ---------------------------------------------------------------------------
// online mode: the client link is AES-128-CFB8 across the login -> play
// transition; these helpers mirror the ones the login e2e test uses.
// ---------------------------------------------------------------------------

const PROTOCOL_ENCRYPTION_SHOULD_AUTHENTICATE: i32 = 766;

fn registry() -> Registry {
    Registry::embedded().expect("embedded snapshot is valid")
}

fn slot_id(slot: PacketSlot) -> i32 {
    registry()
        .packet_id(PROTOCOL, slot)
        .expect("slot has a packet id at 767")
}

/// VarInt-prefixed byte array writer.
fn write_byte_array_test(out: &mut BytesMut, bytes: &[u8]) {
    out.extend_from_slice(&varint(bytes.len() as i32));
    out.extend_from_slice(bytes);
}

/// VarInt-prefixed byte array reader, with a size cap.
fn read_byte_array_test(src: &mut &[u8]) -> Option<Vec<u8>> {
    let mut value: u64 = 0;
    let mut shift = 0u32;
    loop {
        let (&byte, rest) = src.split_first()?;
        *src = rest;
        value |= u64::from(byte & 0x7F) << shift;
        if byte & 0x80 == 0 {
            break;
        }
        shift += 7;
        if shift >= 64 {
            return None;
        }
    }
    let len = usize::try_from(value).ok()?;
    if src.len() < len {
        return None;
    }
    let (head, tail) = src.split_at(len);
    *src = tail;
    Some(head.to_vec())
}

/// Read one *unencrypted* frame; `Ok(None)` on clean EOF.
async fn read_wire_frame(stream: &mut TcpStream, buf: &mut BytesMut) -> Option<Packet> {
    let mut codec = PacketCodec;
    loop {
        match codec.decode(buf) {
            Ok(Some(packet)) => return Some(packet),
            Ok(None) => {
                if stream.read_buf(buf).await.expect("read") == 0 {
                    return None;
                }
            }
            Err(error) => panic!("wire frame rejected: {error}"),
        }
    }
}

/// Read one AES-CFB8 encrypted frame; `cipher` is the receive half of the
/// session cipher pair.
async fn read_frame_encrypted(
    stream: &mut TcpStream,
    buf: &mut BytesMut,
    cipher: &mut candyriya_encryption::AesCfb8,
) -> Option<Packet> {
    let mut codec = PacketCodec;
    loop {
        match codec.decode(buf) {
            Ok(Some(packet)) => return Some(packet),
            Ok(None) => {}
            Err(error) => panic!("encrypted wire frame rejected: {error}"),
        }
        let mut byte = [0u8; 1];
        if stream.read_exact(&mut byte).await.is_err() {
            return None;
        }
        cipher.decrypt_in_place(&mut byte);
        buf.extend_from_slice(&byte);
    }
}

/// A raw HTTP fake of the Mojang session server. Accepts exactly one
/// connection, captures the request, and answers with a canned profile.
fn fake_session_server(uuid: uuid::Uuid) -> (u16, mpsc::Receiver<String>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("local addr").port();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let (stream, _) = listener.accept().expect("accept one connection");
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("timeout");
        let mut buf = vec![0_u8; 8192];
        let mut n = 0;
        while n < buf.len() {
            match std::io::Read::read(&mut &stream, &mut buf[n..]) {
                Ok(0) => break,
                Ok(read) => {
                    n += read;
                    if buf[..n].windows(4).any(|window| window == b"\r\n\r\n") {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        let request = String::from_utf8_lossy(&buf[..n]).into_owned();
        let body = format!(
            r#"{{"id": "{uuid}", "name": "Steve", "properties": [{{"name": "textures", "value": "dGVzdA==", "signature": "c2lnSIG="}}]}}"#
        );
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        let mut stream = stream;
        use std::io::Write as _;
        stream
            .write_all(response.as_bytes())
            .expect("write response");
        tx.send(request).expect("hand the request back");
    });
    (port, rx)
}

/// Bridge between this workspace's `rand` (0.9) and the `rand_core` the
/// `rsa` crate parameterises on: PKCS#1 v1.5 padding needs fresh entropy
/// per encryption.
struct TestRng;

impl rsa::rand_core::TryRng for TestRng {
    type Error = core::convert::Infallible;

    fn try_next_u32(&mut self) -> Result<u32, Self::Error> {
        Ok(rand::RngCore::next_u32(&mut rand::rng()))
    }

    fn try_next_u64(&mut self) -> Result<u64, Self::Error> {
        Ok(rand::RngCore::next_u64(&mut rand::rng()))
    }

    fn try_fill_bytes(&mut self, dest: &mut [u8]) -> Result<(), Self::Error> {
        rand::RngCore::fill_bytes(&mut rand::rng(), dest);
        Ok(())
    }
}

impl rsa::rand_core::TryCryptoRng for TestRng {}

#[tokio::test]
async fn online_mode_play_state_keeps_the_cipher_chain() {
    use rsa::Pkcs1v15Encrypt;
    use rsa::pkcs8::DecodePublicKey;

    let profile_uuid = uuid::Uuid::parse_str("200f5b42-2f4e-4a2d-8a1a-3fd27f0d5e01").unwrap();
    let (mojang_port, mojang_requests) = fake_session_server(profile_uuid);

    let backend = BackendFixture::start(backend_config(BackendForwarding::Modern {
        secret: String::from_utf8(FORWARDING_SECRET_BYTES.to_vec()).unwrap(),
    }))
    .await
    .expect("fixture starts");

    let mut config = offline_config();
    config.online_mode = true;
    config.player_info_forwarding_mode = ForwardingMode::Modern;
    config.servers.servers = HashMap::from([server_addr("lobby", backend.addr())]);
    config.servers.try_servers = vec!["lobby".to_owned()];
    let router = registry_from_config(&config).expect("router from config must build");
    let state = ProxyState::new(Arc::new(config))
        .with_router(Arc::new(router))
        .with_auth(candyriya_auth::AuthConfig {
            mode: candyriya_auth::AuthMode::Online,
            session_server_url: format!("http://127.0.0.1:{mojang_port}/session/hasJoined"),
            timeout: Duration::from_secs(2),
        });
    let (proxy, _shutdown, _service) = start_proxy_with_state(Arc::new(state));

    let mut stream = TcpStream::connect(proxy).await.expect("connect");
    stream.set_nodelay(true).unwrap();

    // handshake + login start (same shapes the online login e2e uses)
    let mut handshake_payload = varint(PROTOCOL);
    handshake_payload.extend_from_slice(&varint("play.example.com".len() as i32));
    handshake_payload.extend_from_slice(b"play.example.com");
    handshake_payload.extend_from_slice(&25565u16.to_be_bytes());
    handshake_payload.extend_from_slice(&varint(2));
    stream
        .write_all(&frame(0x00, &handshake_payload))
        .await
        .unwrap();
    let start_id = slot_id(PacketSlot::LoginServerboundStart);
    let mut start_payload = BytesMut::new();
    write_string(&mut start_payload, "Steve", 16).unwrap();
    let (msb, lsb) = uuid_from_name("Steve");
    candyriya_protocol::write_uuid(msb, lsb, &mut start_payload);
    stream
        .write_all(&frame(start_id, &start_payload))
        .await
        .unwrap();

    // the proxy answers with the Encryption Request
    let mut buf = BytesMut::new();
    let encryption_request = read_wire_frame(&mut stream, &mut buf)
        .await
        .expect("encryption request");
    let mut cursor: &[u8] = &encryption_request.payload;
    let _server_id = candyriya_protocol::read_string(&mut cursor, 20).expect("server id");
    let public_key = read_byte_array_test(&mut cursor).expect("public key");
    let verify_token = read_byte_array_test(&mut cursor).expect("verify token");
    if PROTOCOL >= PROTOCOL_ENCRYPTION_SHOULD_AUTHENTICATE {
        assert_eq!(cursor, [0x01].as_slice(), "shouldAuthenticate for 1.20.5+");
    }

    // build the Encryption Response by hand
    let shared_secret: [u8; 16] = [0xA5; 16];
    let key = rsa::PublicKey::from_public_key_der(&public_key).expect("valid DER public key");
    let mut rng = TestRng;
    let encrypted_secret = key
        .encrypt(&mut rng, Pkcs1v15Encrypt, &shared_secret)
        .expect("secret encrypts");
    let encrypted_token = key
        .encrypt(&mut rng, Pkcs1v15Encrypt, &verify_token)
        .expect("token encrypts");
    let mut response_payload = BytesMut::new();
    write_byte_array_test(&mut response_payload, &encrypted_secret);
    write_byte_array_test(&mut response_payload, &encrypted_token);
    let response_id = slot_id(PacketSlot::LoginServerboundEncryptionResponse);
    stream
        .write_all(&frame(response_id, &response_payload))
        .await
        .unwrap();

    // the session cipher: one instance, both directions, chain carried
    // across the transition — exactly what the pump relies on
    let secret = candyriya_encryption::SharedSecret::from_bytes(shared_secret);
    let mut cipher = candyriya_encryption::AesCfb8::new(&secret);
    let mut buf = BytesMut::new();
    let success = read_frame_encrypted(&mut stream, &mut buf, &mut cipher)
        .await
        .expect("login success");
    assert_eq!(
        success.id,
        slot_id(PacketSlot::LoginClientboundLoginSuccess),
        "expected the login success"
    );
    let _ = mojang_requests
        .recv_timeout(Duration::from_secs(5))
        .expect("hasJoined was called");

    // ---- play state: the bytes must keep the CFB8 chain running

    // client -> backend: the frame arrives at the fixture byte-for-byte
    let raw = frame(CLIENT_PLAY_ID, &chat_payload("hello encrypted player"));
    let mut encrypted = raw.clone();
    cipher.encrypt_in_place(&mut encrypted);
    stream.write_all(&encrypted).await.unwrap();
    wait_until(
        || backend.play_bytes_recorded() >= raw.len() as u64,
        "the backend to record the encrypted client frame",
    )
    .await;
    assert_eq!(
        backend.take_play_bytes(),
        raw,
        "the proxy must decrypt before forwarding; the backend sees plain frames"
    );

    // backend -> client: the injected frame arrives byte-for-byte
    let incoming = frame(BACKEND_PLAY_ID, &chat_payload("hello encrypted server"));
    assert_eq!(backend.send_play_bytes(&incoming).await, 1);
    let mut got = vec![0u8; incoming.len()];
    stream.read_exact(&mut got).await.unwrap();
    cipher.decrypt_in_place(&mut got);
    assert_eq!(got, incoming);

    // tear down: EOF on the client side closes both ends
    drop(stream);
    wait_until(
        || backend.stats().connections_closed >= 1,
        "the backend to observe the client going away",
    )
    .await;
}
