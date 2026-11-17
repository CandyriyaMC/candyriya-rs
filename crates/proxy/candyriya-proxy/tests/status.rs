//! Integration tests for the status intercept.
//!
//! Everything here runs on loopback; no external network is touched.
//! Two styles of test live side by side:
//!
//! - **Full-pipeline** tests drive the real accept loop through
//!   [`serve`], speak the actual wire protocol (either the hand-rolled
//!   client in this file or `candyriya_it::TestClient`), and assert what
//!   a Minecraft client would see: packets on the wire, exclusive of any
//!   backend.
//! - **Unit-adjacent** tests of the response body live in the module's
//!   `#[cfg(test)]` block in `src/status.rs`; this file covers the
//!   wire-facing behaviour.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::BytesMut;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::watch;
use tokio_util::codec::Decoder;

use candyriya_config::Config;
use candyriya_protocol::{Packet, PacketCodec};
use candyriya_scheduler::TaskService;

use candyriya_it::TestClient;
use candyriya_protocol::Intent;
use candyriya_proxy::{ProxySocket, ProxyState, SocketConfig, serve};

mod common;

// ---------------------------------------------------------------------------
// shared fixtures
// ---------------------------------------------------------------------------

fn loopback_any() -> SocketAddr {
    "127.0.0.1:0".parse().expect("valid literal")
}

fn test_service() -> TaskService {
    TaskService::builder()
        .blocking_threads(2)
        .build()
        .expect("task service must build")
}

fn default_state() -> Arc<ProxyState> {
    Arc::new(common::state(Config::default()))
}

fn default_proxy() -> (
    SocketAddr,
    watch::Sender<bool>,
    TaskService,
    Arc<ProxyState>,
) {
    let state = default_state();
    let service = test_service();

    let socket = ProxySocket::bind(loopback_any(), &SocketConfig::default()).unwrap();
    let addr = socket.local_addr();
    let listener = socket.into_listener();
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    serve(Arc::clone(&state), listener, shutdown_rx, &service);
    (addr, shutdown_tx, service, state)
}

/// VarInt encoder for hand-built frames (independent of the production one).
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

fn handshake_payload(protocol: i32, address: &str, port: u16, intent: i32) -> Vec<u8> {
    let mut out = varint(protocol);
    out.extend_from_slice(&varint(address.len() as i32)); // test addresses are ASCII
    out.extend_from_slice(address.as_bytes());
    out.extend_from_slice(&port.to_be_bytes());
    out.extend_from_slice(&varint(intent));
    out
}

fn handshake_frame(protocol: i32, address: &str, port: u16, intent: i32) -> Vec<u8> {
    frame(0x00, &handshake_payload(protocol, address, port, intent))
}

/// Reads exactly one frame with a 3 s ceiling. `Ok(None)` is a clean EOF.
async fn read_wire_frame(stream: &mut TcpStream, buf: &mut BytesMut) -> Option<Packet> {
    let read = async {
        let mut codec = PacketCodec::new();
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
    };
    tokio::time::timeout(Duration::from_secs(3), read)
        .await
        .expect("frame must arrive within 3 s")
}

/// Expects the peer to close within `within`; returns when the read hits EOF.
async fn expect_clean_close(stream: &mut TcpStream, within: Duration) {
    let mut buf = [0_u8; 64];
    let mut total = 0_usize;
    let deadline = Instant::now() + within;
    loop {
        match tokio::time::timeout(
            deadline.saturating_duration_since(Instant::now()),
            stream.read(&mut buf),
        )
        .await
        {
            Ok(Ok(0)) => return,
            Ok(Ok(n)) => total += n,
            Ok(Err(_)) => return, // reset also counts as closed
            Err(_) => panic!("connection must close within {within:?} (got {total} stray bytes)"),
        }
        if total > 0 && Instant::now() >= deadline {
            panic!("connection must close within {within:?} (got {total} stray bytes)");
        }
    }
}

fn png_data_uri(bytes: &[u8]) -> String {
    use base64::Engine;
    format!(
        "data:image/png;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(bytes)
    )
}

// ---------------------------------------------------------------------------
// full status exchange
// ---------------------------------------------------------------------------

#[tokio::test]
async fn status_flow_answers_without_any_backend_configured() {
    let (addr, shutdown_tx, service, _state) = default_proxy();

    let mut client = TestClient::connect(addr).await.expect("connect");
    client
        .send_handshake(767, "mc.example.com", 25565, Intent::Status)
        .await
        .expect("handshake");
    let raw = client.status_request(767).await.expect("status response");
    let parsed: serde_json::Value = serde_json::from_str(&raw).expect("response is json");

    assert_eq!(parsed["version"]["name"], "candyriya");
    assert_eq!(parsed["version"]["protocol"], 767);
    assert_eq!(parsed["players"]["max"], 100);
    // a server-list ping is not a logged-in player: the roster is empty
    assert_eq!(
        parsed["players"]["online"]
            .as_u64()
            .expect("online is a number"),
        0,
        "a status ping does not count as an online player"
    );
    let description = parsed["description"].to_string();
    assert!(
        description.contains("A Candyriya network"),
        "motd component: {description}"
    );

    let favicon = parsed["favicon"].as_str().expect("favicon is a string");
    let body = favicon
        .strip_prefix("data:image/png;base64,")
        .expect("favicon is a png data URI");
    use base64::Engine;
    let png = base64::engine::general_purpose::STANDARD
        .decode(body)
        .expect("favicon base64 decodes");
    assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");

    // ping is echoed verbatim, then the connection closes cleanly
    let echo = client
        .status_ping(767, 0x0123_4567_89AB_CDEF)
        .await
        .expect("pong");
    assert_eq!(echo, 0x0123_4567_89AB_CDEF);
    assert_eq!(client.next_packet().await.expect("eof read"), None);

    shutdown_tx.send(true).expect("shutdown flip");
    let report = service.drain(Duration::from_secs(2)).await;
    assert_eq!(report.timed_out, 0);
}

#[tokio::test]
async fn status_flow_with_26x_protocol_echoes_777() {
    let (addr, shutdown_tx, service, _state) = default_proxy();

    let mut client = TestClient::connect(addr).await.expect("connect");
    client
        .send_handshake(777, "mc.example.com", 25565, Intent::Status)
        .await
        .expect("handshake");
    let raw = client.status_request(777).await.expect("status response");
    let parsed: serde_json::Value = serde_json::from_str(&raw).expect("response is json");

    assert_eq!(parsed["version"]["protocol"], 777);
    assert_eq!(parsed["version"]["name"], "candyriya");

    drop(client);
    shutdown_tx.send(true).expect("shutdown flip");
    let _ = service.drain(Duration::from_secs(2)).await;
}

#[tokio::test]
async fn status_flow_with_an_unknown_protocol_number_falls_back_cleanly() {
    let (addr, shutdown_tx, service, _state) = default_proxy();

    let mut client = TestClient::connect(addr).await.expect("connect");
    // deliberately not a known release: the registry must not reject it
    client
        .send_handshake(99_999, "mc.example.com", 25565, Intent::Status)
        .await
        .expect("handshake");
    let raw = client
        .status_request(99_999)
        .await
        .expect("status response");
    let parsed: serde_json::Value = serde_json::from_str(&raw).expect("response is json");

    assert_eq!(parsed["version"]["protocol"], 99_999);
    assert_eq!(parsed["version"]["name"], "candyriya");

    drop(client);
    shutdown_tx.send(true).expect("shutdown flip");
    let _ = service.drain(Duration::from_secs(2)).await;
}

#[tokio::test]
async fn status_response_frame_is_id_zero_with_a_length_prefixed_json_string() {
    let (addr, shutdown_tx, service, _state) = default_proxy();

    let mut stream = TcpStream::connect(addr).await.expect("connect");
    stream
        .write_all(&handshake_frame(767, "mc.example.com", 25565, 1))
        .await
        .expect("handshake write");
    stream
        .write_all(&frame(0x00, &[]))
        .await
        .expect("request write");

    let mut buf = BytesMut::new();
    let packet = read_wire_frame(&mut stream, &mut buf)
        .await
        .expect("status response");
    assert_eq!(packet.id, 0x00);
    // the body is a length-prefixed String carrying the JSON document,
    // exactly what a vanilla client is allowed to assume
    let mut cursor: &[u8] = &packet.payload;
    let json =
        candyriya_protocol::read_string(&mut cursor, candyriya_protocol::DEFAULT_MAX_STRING_LENGTH)
            .expect("status response payload is a string");
    assert!(cursor.is_empty(), "no trailing bytes after the string");
    let parsed: serde_json::Value = serde_json::from_str(&json).expect("body is json");
    assert_eq!(parsed["version"]["protocol"], 767);

    drop(stream);
    shutdown_tx.send(true).expect("shutdown flip");
    let _ = service.drain(Duration::from_secs(2)).await;
}

#[tokio::test]
async fn pipelined_handshake_request_and_ping_in_one_segment() {
    let (addr, shutdown_tx, service, _state) = default_proxy();

    let mut stream = TcpStream::connect(addr).await.expect("connect");
    let mut segment = handshake_frame(767, "play.example.net", 25565, 1);
    segment.extend_from_slice(&frame(0x00, &[]));
    segment.extend_from_slice(&frame(0x01, b"\x01\x02\x03\x04\x05\x06\x07\x08"));
    stream.write_all(&segment).await.expect("pipelined write");

    let mut buf = BytesMut::new();
    let response = read_wire_frame(&mut stream, &mut buf)
        .await
        .expect("status response");
    assert_eq!(response.id, 0x00);
    let pong = read_wire_frame(&mut stream, &mut buf).await.expect("pong");
    assert_eq!(pong.id, 0x01);
    assert_eq!(pong.payload.as_ref(), b"\x01\x02\x03\x04\x05\x06\x07\x08");

    // after the pong the proxy closes the connection
    expect_clean_close(&mut stream, Duration::from_secs(2)).await;

    shutdown_tx.send(true).expect("shutdown flip");
    let _ = service.drain(Duration::from_secs(2)).await;
}

// ---------------------------------------------------------------------------
// rejection paths: nobody loves a close without a word
// ---------------------------------------------------------------------------

#[tokio::test]
async fn wrong_status_request_packet_id_is_closed_without_a_word() {
    let (addr, shutdown_tx, service, _state) = default_proxy();

    let mut stream = TcpStream::connect(addr).await.expect("connect");
    stream
        .write_all(&handshake_frame(767, "mc.example.com", 25565, 1))
        .await
        .expect("handshake write");
    stream
        .write_all(&frame(0x2A, &[]))
        .await
        .expect("garbage write");

    expect_clean_close(&mut stream, Duration::from_secs(2)).await;

    shutdown_tx.send(true).expect("shutdown flip");
    let _ = service.drain(Duration::from_secs(2)).await;
}

#[tokio::test]
async fn status_request_with_a_non_empty_payload_is_closed() {
    let (addr, shutdown_tx, service, _state) = default_proxy();

    let mut stream = TcpStream::connect(addr).await.expect("connect");
    stream
        .write_all(&handshake_frame(767, "mc.example.com", 25565, 1))
        .await
        .expect("handshake write");
    stream
        .write_all(&frame(0x00, b"x"))
        .await
        .expect("bad request write");

    expect_clean_close(&mut stream, Duration::from_secs(2)).await;

    shutdown_tx.send(true).expect("shutdown flip");
    let _ = service.drain(Duration::from_secs(2)).await;
}

#[tokio::test]
async fn ping_with_a_seven_byte_payload_is_closed_without_a_pong() {
    let (addr, shutdown_tx, service, _state) = default_proxy();

    let mut stream = TcpStream::connect(addr).await.expect("connect");
    stream
        .write_all(&handshake_frame(767, "mc.example.com", 25565, 1))
        .await
        .expect("handshake write");
    stream
        .write_all(&frame(0x00, &[]))
        .await
        .expect("request write");

    let mut buf = BytesMut::new();
    let response = read_wire_frame(&mut stream, &mut buf)
        .await
        .expect("status response");
    assert_eq!(response.id, 0x00);

    // a ping whose payload is one byte short of the spec must not be echoed
    stream
        .write_all(&frame(0x01, b"\x01\x02\x03\x04\x05\x06\x07"))
        .await
        .expect("bad ping");
    expect_clean_close(&mut stream, Duration::from_secs(2)).await;

    shutdown_tx.send(true).expect("shutdown flip");
    let _ = service.drain(Duration::from_secs(2)).await;
}

#[tokio::test]
async fn ping_with_the_wrong_packet_id_is_closed_without_a_pong() {
    let (addr, shutdown_tx, service, _state) = default_proxy();

    let mut stream = TcpStream::connect(addr).await.expect("connect");
    stream
        .write_all(&handshake_frame(767, "mc.example.com", 25565, 1))
        .await
        .expect("handshake write");
    stream
        .write_all(&frame(0x00, &[]))
        .await
        .expect("request write");

    let mut buf = BytesMut::new();
    let response = read_wire_frame(&mut stream, &mut buf)
        .await
        .expect("status response");
    assert_eq!(response.id, 0x00);

    stream
        .write_all(&frame(0x7F, b"\x01\x02\x03\x04\x05\x06\x07\x08"))
        .await
        .expect("bad ping id");
    expect_clean_close(&mut stream, Duration::from_secs(2)).await;

    shutdown_tx.send(true).expect("shutdown flip");
    let _ = service.drain(Duration::from_secs(2)).await;
}

#[tokio::test]
async fn a_client_that_vanishes_after_the_handshake_times_out() {
    let config = Config {
        advanced: candyriya_config::AdvancedConfig {
            handshake_timeout: 50,
            ..Default::default()
        },
        ..Default::default()
    };
    let state = Arc::new(common::state(config));
    let service = test_service();

    let socket = ProxySocket::bind(loopback_any(), &SocketConfig::default()).unwrap();
    let addr = socket.local_addr();
    let listener = socket.into_listener();
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    serve(Arc::clone(&state), listener, shutdown_rx, &service);

    let mut stream = TcpStream::connect(addr).await.expect("connect");
    stream
        .write_all(&handshake_frame(767, "mc.example.com", 25565, 1))
        .await
        .expect("handshake write");
    // no status request is ever sent: the proxy must drop the connection
    expect_clean_close(&mut stream, Duration::from_secs(2)).await;

    shutdown_tx.send(true).expect("shutdown flip");
    let _ = service.drain(Duration::from_secs(2)).await;
}

#[tokio::test]
async fn a_client_that_vanishes_during_the_status_request_read_times_out() {
    let config = Config {
        advanced: candyriya_config::AdvancedConfig {
            handshake_timeout: 50,
            ..Default::default()
        },
        ..Default::default()
    };
    let state = Arc::new(common::state(config));
    let service = test_service();

    let socket = ProxySocket::bind(loopback_any(), &SocketConfig::default()).unwrap();
    let addr = socket.local_addr();
    let listener = socket.into_listener();
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    serve(Arc::clone(&state), listener, shutdown_rx, &service);

    let mut stream = TcpStream::connect(addr).await.expect("connect");
    // half a frame, then silence
    stream
        .write_all(&[0x01])
        .await
        .expect("partial frame write");
    expect_clean_close(&mut stream, Duration::from_secs(2)).await;

    shutdown_tx.send(true).expect("shutdown flip");
    let _ = service.drain(Duration::from_secs(2)).await;
}

// ---------------------------------------------------------------------------
// misconfigured MOTD: the exchange refuses to answer
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_motd_lobsterchat_would_truncate_is_an_error_not_a_half_message() {
    let config = Config {
        motd: "<rainbow>Candy</rainbow>".to_owned(),
        ..Default::default()
    };
    let state = Arc::new(common::state(config));
    let service = test_service();

    let socket = ProxySocket::bind(loopback_any(), &SocketConfig::default()).unwrap();
    let addr = socket.local_addr();
    let listener = socket.into_listener();
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    serve(Arc::clone(&state), listener, shutdown_rx, &service);

    let mut stream = TcpStream::connect(addr).await.expect("connect");
    stream
        .write_all(&handshake_frame(767, "mc.example.com", 25565, 1))
        .await
        .expect("handshake write");
    stream
        .write_all(&frame(0x00, &[]))
        .await
        .expect("request write");

    // no response is written; the connection just goes away
    expect_clean_close(&mut stream, Duration::from_secs(2)).await;

    shutdown_tx.send(true).expect("shutdown flip");
    let _ = service.drain(Duration::from_secs(2)).await;
}

#[tokio::test]
async fn an_empty_motd_is_an_error_not_a_blank_server_list_entry() {
    let config = Config {
        motd: "   ".to_owned(),
        ..Default::default()
    };
    let state = Arc::new(common::state(config));
    let service = test_service();

    let socket = ProxySocket::bind(loopback_any(), &SocketConfig::default()).unwrap();
    let addr = socket.local_addr();
    let listener = socket.into_listener();
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    serve(Arc::clone(&state), listener, shutdown_rx, &service);

    let mut stream = TcpStream::connect(addr).await.expect("connect");
    stream
        .write_all(&handshake_frame(767, "mc.example.com", 25565, 1))
        .await
        .expect("handshake write");
    stream
        .write_all(&frame(0x00, &[]))
        .await
        .expect("request write");

    expect_clean_close(&mut stream, Duration::from_secs(2)).await;

    shutdown_tx.send(true).expect("shutdown flip");
    let _ = service.drain(Duration::from_secs(2)).await;
}

// ---------------------------------------------------------------------------
// favicon paths
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_configured_favicon_file_is_served() {
    let dir = tempfile::tempdir().expect("tempdir");
    let icon_path = dir.path().join("icon.png");
    let custom_bytes: Vec<u8> = b"\x89PNG\r\n\x1a\n"
        .iter()
        .cloned()
        .chain(b"the-icon-file-contents".iter().cloned())
        .collect();
    std::fs::write(&icon_path, &custom_bytes).expect("icon write");

    let config = Config {
        motd_favicon: icon_path.to_str().expect("utf-8 path").to_owned(),
        ..Default::default()
    };
    let state = Arc::new(common::state(config));
    let service = test_service();

    let socket = ProxySocket::bind(loopback_any(), &SocketConfig::default()).unwrap();
    let addr = socket.local_addr();
    let listener = socket.into_listener();
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    serve(Arc::clone(&state), listener, shutdown_rx, &service);

    let mut client = TestClient::connect(addr).await.expect("connect");
    client
        .send_handshake(767, "mc.example.com", 25565, Intent::Status)
        .await
        .expect("handshake");
    let raw = client.status_request(767).await.expect("status response");
    let parsed: serde_json::Value = serde_json::from_str(&raw).expect("response is json");

    assert_eq!(parsed["favicon"], png_data_uri(&custom_bytes));

    drop(client);
    shutdown_tx.send(true).expect("shutdown flip");
    let _ = service.drain(Duration::from_secs(2)).await;
}

#[tokio::test]
async fn a_configured_favicon_is_served_from_the_cachegirl_cache() {
    let dir = tempfile::tempdir().expect("tempdir");
    let icon_path = dir.path().join("icon.png");
    let icon_a: Vec<u8> = b"\x89PNG\r\n\x1a\n"
        .iter()
        .cloned()
        .chain(b"favicon-a".iter().cloned())
        .collect();
    std::fs::write(&icon_path, &icon_a).expect("icon write");

    let cache_root = tempfile::tempdir().expect("cache root");
    let manager = candyriya_cache::CacheManager::new(cache_root.path()).expect("cache manager");
    let favicon_cache = Arc::new(
        candyriya_proxy::FaviconCache::new(&manager, Duration::from_secs(60))
            .expect("favicon cache"),
    );

    let config = Config {
        motd_favicon: icon_path.to_str().expect("utf-8 path").to_owned(),
        ..Default::default()
    };
    let mut state = common::state(config);
    state.favicon_cache = Some(favicon_cache);
    let state = Arc::new(state);
    let service = test_service();

    let socket = ProxySocket::bind(loopback_any(), &SocketConfig::default()).unwrap();
    let addr = socket.local_addr();
    let listener = socket.into_listener();
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    serve(Arc::clone(&state), listener, shutdown_rx, &service);

    async fn ask(addr: SocketAddr) -> serde_json::Value {
        let mut client = TestClient::connect(addr).await.expect("connect");
        client
            .send_handshake(767, "mc.example.com", 25565, Intent::Status)
            .await
            .expect("handshake");
        let raw = client.status_request(767).await.expect("status response");
        serde_json::from_str(&raw).expect("response is json")
    }

    let first = ask(addr).await;
    assert_eq!(first["favicon"], png_data_uri(&icon_a));

    // swap the file on disk; the cache must keep serving what was cached
    let icon_b: Vec<u8> = b"\x89PNG\r\n\x1a\n"
        .iter()
        .cloned()
        .chain(b"favicon-b".iter().cloned())
        .collect();
    std::fs::write(&icon_path, &icon_b).expect("icon rewrite");
    let second = ask(addr).await;
    assert_eq!(second["favicon"], png_data_uri(&icon_a));

    shutdown_tx.send(true).expect("shutdown flip");
    let _ = service.drain(Duration::from_secs(2)).await;
    manager.shutdown().expect("cache shutdown");
}

#[tokio::test]
async fn players_max_reflects_show_max_players() {
    let config = Config {
        show_max_players: 42,
        ..Default::default()
    };
    let state = Arc::new(common::state(config));
    let service = test_service();

    let socket = ProxySocket::bind(loopback_any(), &SocketConfig::default()).unwrap();
    let addr = socket.local_addr();
    let listener = socket.into_listener();
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    serve(Arc::clone(&state), listener, shutdown_rx, &service);

    let mut client = TestClient::connect(addr).await.expect("connect");
    client
        .send_handshake(767, "mc.example.com", 25565, Intent::Status)
        .await
        .expect("handshake");
    let raw = client.status_request(767).await.expect("status response");
    let parsed: serde_json::Value = serde_json::from_str(&raw).expect("response is json");

    assert_eq!(parsed["players"]["max"], 42);

    drop(client);
    shutdown_tx.send(true).expect("shutdown flip");
    let _ = service.drain(Duration::from_secs(2)).await;
}

#[tokio::test]
async fn a_silent_login_client_times_out_and_closes() {
    // a login-intent handshake with no login start followed: the login
    // pipeline waits for the packet the client owes it, until the login
    // deadline ([advanced] connection-timeout + 5 s of headroom) expires,
    // then closes both sides. Config pins connection-timeout to 100 ms
    // so the test does not pay the headroom's default.
    let mut config = Config::default();
    config.advanced.connection_timeout = 100;
    let state = Arc::new(common::state(config));
    let service = test_service();

    let socket = ProxySocket::bind(loopback_any(), &SocketConfig::default()).unwrap();
    let addr = socket.local_addr();
    let listener = socket.into_listener();
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    serve(Arc::clone(&state), listener, shutdown_rx, &service);

    let mut stream = TcpStream::connect(addr).await.expect("connect");
    stream
        .write_all(&handshake_frame(767, "play.example.net", 25565, 2))
        .await
        .expect("login handshake write");

    expect_clean_close(&mut stream, Duration::from_secs(8)).await;

    shutdown_tx.send(true).expect("shutdown flip");
    let _ = service.drain(Duration::from_secs(2)).await;
}

#[test]
fn frame_helpers_are_wire_exact() {
    // the golden status-flow frame from the protocol crate docs: a 2-byte
    // status request must come out byte for byte
    assert_eq!(frame(0x00, &[]), [0x01, 0x00]);
    assert_eq!(
        frame(0x01, &[0xAA; 8]),
        [0x09, 0x01, 0xAA, 0xAA, 0xAA, 0xAA, 0xAA, 0xAA, 0xAA, 0xAA]
    );
}
