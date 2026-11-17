//! Integration tests for the login pipeline.
//!
//! Everything here runs on loopback; no external network is touched (the
//! Mojang session server is replaced by a raw-HTTP loopback fixture for the
//! online-mode case). Two styles are used, same split as the status
//! intercept: tests through the real accept loop through [`serve`] for the
//! end-to-end shape, and a hand-rolled encrypted client for the online
//! handshake.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, mpsc};
use std::time::Duration;

use bytes::BytesMut;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::watch;
use tokio_util::codec::Decoder;

use candyriya_config::{Config, ForwardingMode};
use candyriya_it::{
    BackendFixture, BackendForwarding, FakeBackendConfig, Intent, TestClient, uuid_from_name,
};
use candyriya_player::offline_uuid;
use candyriya_protocol::{Packet, PacketCodec, read_string, read_uuid, write_string, write_uuid};
use candyriya_protocol_registry::{PacketSlot, Registry};
use candyriya_scheduler::TaskService;

use candyriya_proxy::{ProxySocket, ProxyState, SocketConfig, registry_from_config, serve};

mod common;

// ---------------------------------------------------------------------------
// shared helpers
// ---------------------------------------------------------------------------

const PROTOCOL: i32 = 767; // 1.21.x, present in the embedded registry tables

/// The 32 bytes the proxy and the fixture share in the modern-forwarding tests.
const FORWARDING_SECRET_BYTES: [u8; 32] = [b's'; 32];

fn loopback_any() -> SocketAddr {
    "127.0.0.1:0".parse().expect("valid literal")
}

fn test_service() -> TaskService {
    TaskService::builder()
        .blocking_threads(2)
        .build()
        .expect("task service must build")
}

/// An offline-mode config that routes to nothing yet; servers are filled per test.
fn offline_config() -> Config {
    Config {
        online_mode: false,
        forwarding_secret: candyriya_config::ForwardingSecret::new(
            FORWARDING_SECRET_BYTES.to_vec(),
        ),
        ..Config::default()
    }
}

/// Start a proxy on an ephemeral port with the given config; returns the
/// address and the shutdown handle.
fn start_proxy(config: Config) -> (SocketAddr, watch::Sender<bool>, TaskService) {
    let router = registry_from_config(&config).expect("router from config must build");
    let state = Arc::new(common::state(config).with_router(Arc::new(router)));
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

fn registry() -> Registry {
    Registry::embedded().expect("embedded snapshot is valid")
}

/// `slot`'s packet id at 767.
fn slot_id(slot: PacketSlot) -> i32 {
    registry()
        .packet_id(PROTOCOL, slot)
        .expect("slot has a packet id at 767")
}

/// Build a backend config with the given forwarding behavior at 767.
fn backend_config(forwarding: BackendForwarding) -> FakeBackendConfig {
    FakeBackendConfig {
        protocol: PROTOCOL,
        motd: "fixture".to_owned(),
        max_players: 8,
        forwarding,
    }
}

/// The `servers` table string for a backend address.
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

// ---------------------------------------------------------------------------
// modern forwarding
// ---------------------------------------------------------------------------

#[tokio::test]
async fn modern_forwarding_seamlessly_logs_a_player_in() {
    // backend A: verifies the HMAC of the forwarding payload for real
    let backend = BackendFixture::start(backend_config(BackendForwarding::Modern {
        secret: String::from_utf8(FORWARDING_SECRET_BYTES.to_vec()).unwrap(),
    }))
    .await
    .expect("fixture starts");

    let mut config = offline_config();
    config.player_info_forwarding_mode = ForwardingMode::Modern;
    config.servers.servers = HashMap::from([server_addr("lobby", backend.addr())]);
    config.servers.try_servers = vec!["lobby".to_owned()];
    let (proxy, _shutdown, _service) = start_proxy(config);

    let mut client = client_login(proxy, "Steve").await;
    let success = client
        .expect_login_success(PROTOCOL)
        .await
        .expect("login success");
    assert_eq!(success.name(), "Steve");
    // the UUID the client sees is the one from the sealed forwarding payload:
    // the offline-mode proxy derives it with the vanilla offline algorithm
    assert_eq!(
        success.uuid_parts(),
        {
            let uuid = offline_uuid("Steve");
            (uuid.as_u64_pair().0 as i64, uuid.as_u64_pair().1 as i64)
        },
        "the sealed profile must carry the vanilla offline UUID"
    );

    let stats = backend.close().await;
    assert_eq!(
        stats.player_info_requests, 1,
        "the backend asked for velocity:player_info"
    );
    assert_eq!(
        stats.forwarding_failures, 0,
        "the sealed payload authenticated"
    );
    assert_eq!(stats.logins, 1, "the backend accepted the login");
}

// ---------------------------------------------------------------------------
// offline mode
// ---------------------------------------------------------------------------

#[tokio::test]
async fn offline_mode_login_succeeds_without_a_session_server() {
    let backend = BackendFixture::start(backend_config(BackendForwarding::Off))
        .await
        .expect("fixture starts");

    let mut config = offline_config();
    config.player_info_forwarding_mode = ForwardingMode::Disabled;
    config.servers.servers = HashMap::from([server_addr("lobby", backend.addr())]);
    config.servers.try_servers = vec!["lobby".to_owned()];
    let (proxy, _shutdown, _service) = start_proxy(config);

    let mut client = client_login(proxy, "Steve").await;
    let success = client
        .expect_login_success(PROTOCOL)
        .await
        .expect("login success");
    assert_eq!(success.name(), "Steve");
    // the UUID the client sees is the backend's own — the proxy forwards the
    // Login Success verbatim; its own offline derivation is unit-tested
    // in src/login.rs (resolve_profile)
    assert_eq!(success.uuid_parts(), uuid_from_name("Steve"));

    let stats = backend.close().await;
    assert_eq!(stats.logins, 1);
}

// ---------------------------------------------------------------------------
// online mode (fake session server on raw HTTP)
// ---------------------------------------------------------------------------

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

fn frame(id: i32, payload: &[u8]) -> Vec<u8> {
    let mut body = varint(id);
    body.extend_from_slice(payload);
    let mut out = varint(body.len() as i32);
    out.extend_from_slice(&body);
    out
}

fn raw_handshake(protocol: i32, address: &str, port: u16, intent: i32) -> Vec<u8> {
    let mut payload = varint(protocol);
    payload.extend_from_slice(&varint(address.len() as i32));
    payload.extend_from_slice(address.as_bytes());
    payload.extend_from_slice(&port.to_be_bytes());
    payload.extend_from_slice(&varint(intent));
    frame(0x00, &payload)
}

/// Read one frame; `Ok(None)` on clean EOF. Raw helper for the encrypted client.
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

/// A bridge between this workspace's `rand` (0.9) and the `rand_core` 0.10
/// the `rsa` crate parameterises on: the versions intentionally differ (see
/// candyriya-encryption's rng bridge). PKCS#1 v1.5 padding needs fresh
/// entropy per encryption, so the test supplies a CSPRNG.
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
        // read until the headers are terminated; a GET has no body
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

#[tokio::test]
async fn online_mode_verifies_with_the_session_server() {
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
    let state = common::state(config)
        .with_router(Arc::new(router))
        .with_auth(candyriya_auth::AuthConfig {
            mode: candyriya_auth::AuthMode::Online,
            session_server_url: format!("http://127.0.0.1:{mojang_port}/session/hasJoined"),
            timeout: Duration::from_secs(2),
        });
    let (proxy, _shutdown, _service) = start_proxy_with_state(Arc::new(state));

    let mut stream = TcpStream::connect(proxy).await.expect("connect");
    stream.set_nodelay(true).unwrap();

    // 1. handshake + login start (protocol >= 759 carries the UUID trailing field)
    stream
        .write_all(&raw_handshake(PROTOCOL, "play.example.com", 25565, 2))
        .await
        .unwrap();
    let mut payload = BytesMut::new();
    write_string(&mut payload, "Steve", 16).unwrap();
    let (msb, lsb) = uuid_from_name("Steve");
    write_uuid(msb, lsb, &mut payload);
    let start_id = slot_id(PacketSlot::LoginServerboundStart);
    stream.write_all(&frame(start_id, &payload)).await.unwrap();

    // 2. the proxy answers with the Encryption Request
    let mut buf = BytesMut::new();
    let encryption_request = read_wire_frame(&mut stream, &mut buf)
        .await
        .expect("encryption request");
    assert_eq!(
        encryption_request.id,
        slot_id(PacketSlot::LoginClientboundEncryptionRequest),
        "expected the encryption request"
    );
    let mut cursor: &[u8] = &encryption_request.payload;
    let server_id = read_string(&mut cursor, 20).expect("server id");
    assert_eq!(server_id, "", "server id is empty since 1.7");
    let public_key = read_byte_array_test(&mut cursor).expect("public key");
    let verify_token = read_byte_array_test(&mut cursor).expect("verify token");
    // protocol 767 >= 766: shouldAuthenticate must be present and set
    assert_eq!(
        cursor,
        [0x01].as_slice(),
        "shouldAuthenticate is on for 1.20.5+"
    );

    // 3. build the Encryption Response by hand
    let shared_secret: [u8; 16] = [0xA5; 16];
    let expected_server_id = candyriya_encryption::server_id_hash("", &shared_secret, &public_key);
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

    // 4. the proxy now verifies with the fake session server, then logs in to the backend
    let secret = candyriya_encryption::SharedSecret::from_bytes(shared_secret);
    let mut cipher_read = candyriya_encryption::AesCfb8::new(&secret);
    // the proxy->client link switched to AES-CFB8 as well
    let mut buf = BytesMut::new();
    let success = read_frame_encrypted(&mut stream, &mut buf, &mut cipher_read)
        .await
        .expect("login success");
    assert_eq!(
        success.id,
        slot_id(PacketSlot::LoginClientboundLoginSuccess),
        "expected the login success"
    );

    let mut body: &[u8] = &success.payload;
    let (msb, lsb) = read_uuid(&mut body).expect("uuid");
    assert_eq!(
        (msb as u64, lsb as u64),
        profile_uuid.as_u64_pair(),
        "the UUID must come from the session-server JSON, not from offline derivation"
    );

    // 5. the fake session server saw exactly one request with the right hash
    let request = mojang_requests
        .recv_timeout(Duration::from_secs(5))
        .expect("hasJoined was called");
    assert!(
        request.starts_with("GET /session/hasJoined?username=Steve&serverId="),
        "path: {request}"
    );
    assert!(
        request.contains(&format!("serverId={expected_server_id}")),
        "hash must match the proxy's derivation: {request}"
    );

    let stats = backend.close().await;
    assert_eq!(stats.logins, 1);
}

/// Local helpers for the raw online-mode client.
fn read_byte_array_test(src: &mut &[u8]) -> Option<Vec<u8>> {
    let mut value: u64 = 0;
    let mut shift = 0u32;
    loop {
        let (&byte, rest) = match src.split_first() {
            Some((byte, rest)) => (byte, rest),
            None => return None,
        };
        // always advance, continuation or not
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

fn write_byte_array_test(out: &mut BytesMut, bytes: &[u8]) {
    let mut len = bytes.len() as u32;
    loop {
        let mut byte = (len & 0x7F) as u8;
        len >>= 7;
        if len != 0 {
            byte |= 0x80;
        }
        out.extend_from_slice(&[byte]);
        if len == 0 {
            break;
        }
    }
    out.extend_from_slice(bytes);
}

/// Read one frame whose bytes are AES-CFB8 encrypted. `cipher` is the
/// receive half of the session cipher pair.
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
        let mut byte = [0_u8; 1];
        if stream.read_exact(&mut byte).await.is_err() {
            return None;
        }
        cipher.decrypt_in_place(&mut byte);
        buf.extend_from_slice(&byte);
    }
}

// ---------------------------------------------------------------------------
// kick fallback
// ---------------------------------------------------------------------------

#[tokio::test]
async fn kick_from_the_first_backend_falls_back_to_the_second() {
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
    let (proxy, _shutdown, _service) = start_proxy(config);

    let mut client = client_login(proxy, "Steve").await;
    let success = client
        .expect_login_success(PROTOCOL)
        .await
        .expect("login success from the fallback");
    assert_eq!(success.name(), "Steve");

    let bad_stats = bad.close().await;
    let good_stats = good.close().await;
    assert_eq!(
        bad_stats.forced_disconnects, 1,
        "the kicking backend was tried first"
    );
    assert_eq!(
        good_stats.logins, 1,
        "the fallback backend served the login"
    );
}

#[tokio::test]
async fn no_viable_backend_disconnects_the_client() {
    let bad = BackendFixture::start(backend_config(BackendForwarding::Disconnect {
        reason: "maintenance".to_owned(),
    }))
    .await
    .expect("fixture starts");

    let mut config = offline_config();
    config.player_info_forwarding_mode = ForwardingMode::Disabled;
    config.servers.servers = HashMap::from([server_addr("bad", bad.addr())]);
    config.servers.try_servers = vec!["bad".to_owned()];
    let (proxy, _shutdown, _service) = start_proxy(config);

    let mut client = client_login(proxy, "Steve").await;
    let packet = client.next_packet().await.expect("read").expect("a packet");
    let disconnect_id = slot_id(PacketSlot::LoginClientboundDisconnect);
    assert_eq!(
        packet.id, disconnect_id,
        "the client must receive a login disconnect when the chain is exhausted"
    );
    let mut body: &[u8] = &packet.payload;
    let reason = candyriya_protocol::read_string(&mut body, 32767).expect("reason is utf-8");
    assert!(
        reason.contains("maintenance"),
        "the backend's kick reason is passed through: {reason}"
    );

    let stats = bad.close().await;
    assert_eq!(stats.forced_disconnects, 1);
    assert_eq!(stats.logins, 0);
}

// ---------------------------------------------------------------------------
// routing
// ---------------------------------------------------------------------------

#[tokio::test]
async fn forced_host_wins_over_the_try_chain() {
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
    config.servers.try_servers = vec!["bad".to_owned()];
    config.forced_hosts =
        HashMap::from([("forced.example.com".to_owned(), vec!["good".to_owned()])]);
    let (proxy, _shutdown, _service) = start_proxy(config);

    let mut client = TestClient::connect(proxy).await.expect("connect");
    client
        .send_handshake(PROTOCOL, "forced.example.com", 25565, Intent::Login)
        .await
        .expect("handshake");
    client
        .login_start(PROTOCOL, "Steve")
        .await
        .expect("login start");

    // the forced host routes to the good backend even though the try chain is bad
    let success = client
        .expect_login_success(PROTOCOL)
        .await
        .expect("login success via forced host");
    assert_eq!(success.name(), "Steve");

    let good_stats = good.close().await;
    let bad_stats = bad.close().await;
    assert_eq!(good_stats.logins, 1);
    assert_eq!(
        bad_stats.forced_disconnects, 0,
        "the kicking backend was never tried"
    );
}

#[tokio::test]
async fn non_forced_host_falls_back_to_the_try_chain() {
    let bad = BackendFixture::start(backend_config(BackendForwarding::Disconnect {
        reason: "maintenance".to_owned(),
    }))
    .await
    .expect("fixture starts");

    let mut config = offline_config();
    config.player_info_forwarding_mode = ForwardingMode::Disabled;
    config.servers.servers = HashMap::from([server_addr("bad", bad.addr())]);
    config.servers.try_servers = vec!["bad".to_owned()];
    config.forced_hosts =
        HashMap::from([("forced.example.com".to_owned(), vec!["bad".to_owned()])]);
    let (proxy, _shutdown, _service) = start_proxy(config);

    let mut client = TestClient::connect(proxy).await.expect("connect");
    client
        .send_handshake(PROTOCOL, "plain.example.com", 25565, Intent::Login)
        .await
        .expect("handshake");
    client
        .login_start(PROTOCOL, "Steve")
        .await
        .expect("login start");

    let packet = client.next_packet().await.expect("read").expect("a packet");
    assert_eq!(
        packet.id,
        slot_id(PacketSlot::LoginClientboundDisconnect),
        "a non-matching host falls back to the try chain and gets kicked"
    );
}

// ---------------------------------------------------------------------------
// concurrency
// ---------------------------------------------------------------------------

#[tokio::test]
async fn two_parallel_connections_get_independent_sessions() {
    let backend = BackendFixture::start(backend_config(BackendForwarding::Off))
        .await
        .expect("fixture starts");

    let mut config = offline_config();
    config.player_info_forwarding_mode = ForwardingMode::Disabled;
    config.servers.servers = HashMap::from([server_addr("lobby", backend.addr())]);
    config.servers.try_servers = vec!["lobby".to_owned()];
    let (proxy, _shutdown, _service) = start_proxy(config);

    let first = tokio::spawn(async move {
        let mut client = client_login(proxy, "Alex").await;
        client
            .expect_login_success(PROTOCOL)
            .await
            .map(|s| s.name().to_owned())
    });
    let second = tokio::spawn(async move {
        let mut client = client_login(proxy, "Steve").await;
        client
            .expect_login_success(PROTOCOL)
            .await
            .map(|s| s.name().to_owned())
    });
    let (first, second) = tokio::join!(first, second);
    assert_eq!(first.expect("task").expect("login").as_str(), "Alex");
    assert_eq!(second.expect("task").expect("login").as_str(), "Steve");

    let stats = backend.close().await;
    assert_eq!(stats.logins, 2, "both parallel connections were served");
}
