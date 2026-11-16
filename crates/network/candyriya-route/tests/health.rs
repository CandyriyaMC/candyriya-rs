//! Health checks against real, in-test TCP servers on 127.0.0.1.
//!
//! No external network is involved: servers are plain [`TcpListener`]s on the
//! loopback interface, spawned inside the test process.

use std::net::SocketAddr;
use std::time::Duration;

use bytes::{Bytes, BytesMut};
use candyriya_protocol::{
    DEFAULT_MAX_STRING_LENGTH, Handshake, Intent, Packet, PacketCodec, write_string,
};
use candyriya_route::{BackendRegistry, BackendState, DEFAULT_HEALTH_CHECK_TIMEOUT, RouteError};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_util::codec::{Decoder, Encoder};

fn registry_with(name: &str, addr: SocketAddr) -> BackendRegistry {
    BackendRegistry::new(vec![(name.to_string(), addr)], vec![name.to_string()])
        .expect("registry must build")
}

/// Spawns a loopback server that speaks just enough of the status protocol:
/// it reads the handshake and the status request, then replies with a
/// clientbound status response (packet `0x00`) carrying `status_json`.
async fn spawn_status_server(status_json: &str) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local addr");
    let json = status_json.to_owned();
    tokio::spawn(async move {
        while let Ok(accepted) = listener.accept().await {
            let (mut stream, _) = accepted;
            let json = json.clone();
            tokio::spawn(async move {
                let mut codec = PacketCodec::new();
                let mut pending = BytesMut::with_capacity(256);
                let mut packets_seen = 0u8;

                // build the response up front; it is the same for every check
                let mut payload = Vec::new();
                write_string(&mut payload, &json, DEFAULT_MAX_STRING_LENGTH).expect("fits");
                let mut out = BytesMut::new();
                codec
                    .encode(
                        Packet {
                            id: 0x00,
                            payload: Bytes::copy_from_slice(&payload),
                        },
                        &mut out,
                    )
                    .expect("response encodes");

                loop {
                    let read = stream.read_buf(&mut pending).await;
                    match read {
                        Ok(0) | Err(_) => return, // client hung up
                        Ok(_) => {}
                    }
                    loop {
                        match codec.decode(&mut pending) {
                            Ok(Some(packet)) => {
                                packets_seen += 1;
                                if packets_seen >= 2 {
                                    // handshake + status request seen; answer
                                    let _ = stream.write_all(&out).await;
                                    let _ = stream.shutdown().await;
                                    return;
                                }
                                let _ = packet; // packet is unused, ids already counted
                            }
                            Ok(None) => break,
                            Err(_) => return,
                        }
                    }
                }
            });
        }
    });
    addr
}

#[tokio::test]
async fn health_check_marks_unknown_backend_online() {
    let json = r#"{"version":{"name":"1.21.1","protocol":767},"players":{"max":20,"online":0},"description":{"text":"backend is up"}}"#;
    let addr = spawn_status_server(json).await;
    let registry = registry_with("lobby", addr);

    assert_eq!(registry.state("lobby"), Some(BackendState::Unknown));
    let outcomes = registry
        .health_check_with_timeout(Duration::from_secs(2))
        .await;

    assert_eq!(outcomes.len(), 1);
    let outcome = &outcomes[0];
    assert!(outcome.healthy());
    assert_eq!(outcome.state, BackendState::Online);
    assert_eq!(outcome.motd.as_deref(), Some(json));
    assert!(outcome.error.is_none());
    assert_eq!(registry.state("lobby"), Some(BackendState::Online));
}

#[tokio::test]
async fn health_check_on_closed_port_marks_backend_offline() {
    // reserve a port and release it so nothing is listening on it
    let probe = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = probe.local_addr().expect("local addr");
    drop(probe);

    let registry = registry_with("dead", addr);
    let outcomes = registry
        .health_check_with_timeout(Duration::from_secs(2))
        .await;

    assert_eq!(outcomes.len(), 1);
    let outcome = &outcomes[0];
    assert!(!outcome.healthy());
    assert_eq!(outcome.state, BackendState::Offline);
    assert!(outcome.error.is_some());
    assert_eq!(registry.state("dead"), Some(BackendState::Offline));
}

#[tokio::test]
async fn health_check_on_garbage_replies_marks_backend_offline() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local addr");
    tokio::spawn(async move {
        if let Ok((mut stream, _)) = listener.accept().await {
            let _ = stream
                .write_all(&[0xDE, 0xAD, 0xBE, 0xEF, 0xFF, 0xFF, 0xFF, 0x7F])
                .await;
            let _ = stream.shutdown().await;
        }
    });

    let registry = registry_with("garbage", addr);
    let outcomes = registry
        .health_check_with_timeout(Duration::from_secs(2))
        .await;
    assert_eq!(outcomes[0].state, BackendState::Offline);
}

#[tokio::test]
async fn health_check_on_wrong_packet_id_marks_backend_offline() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local addr");
    tokio::spawn(async move {
        if let Ok((mut stream, _)) = listener.accept().await {
            // read whatever the client sent so it does not get a reset, then
            // answer with packet id 0x01 (status pong) instead of 0x00
            let mut buf = [0u8; 512];
            let _ = stream.read(&mut buf).await;
            let mut codec = PacketCodec::new();
            let mut out = BytesMut::new();
            codec
                .encode(
                    Packet {
                        id: 0x01,
                        payload: Bytes::from_static(&[0u8; 8]),
                    },
                    &mut out,
                )
                .expect("encode");
            let _ = stream.write_all(&out).await;
            let _ = stream.shutdown().await;
        }
    });

    let registry = registry_with("wrongepoch", addr);
    let outcomes = registry
        .health_check_with_timeout(Duration::from_secs(2))
        .await;
    assert_eq!(outcomes[0].state, BackendState::Offline);
    assert!(matches!(
        outcomes[0].error,
        Some(RouteError::UnexpectedPacketId { id: 0x01 })
    ));
}

#[tokio::test]
async fn health_check_times_out_when_backend_is_silent() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local addr");
    tokio::spawn(async move {
        // accept and then do nothing at all — a health check must time out
        if let Ok((stream, _)) = listener.accept().await {
            let _hold_on = stream; // keep the socket open, never respond
            tokio::time::sleep(Duration::from_secs(30)).await;
        }
    });

    let registry = registry_with("silent", addr);
    let outcomes = registry
        .health_check_with_timeout(Duration::from_millis(150))
        .await;
    assert_eq!(outcomes[0].state, BackendState::Offline);
    assert!(matches!(outcomes[0].error, Some(RouteError::PingTimeout)));
}

#[tokio::test]
async fn health_check_updates_last_check() {
    let addr = spawn_status_server(r#"{"description":{"text":"hello"}}"#).await;
    let registry = registry_with("lobby", addr);
    let before = registry.get("lobby").expect("registered").last_check();
    tokio::time::sleep(Duration::from_millis(20)).await;
    let outcomes = registry
        .health_check_with_timeout(Duration::from_secs(2))
        .await;
    assert_eq!(outcomes[0].state, BackendState::Online);
    let after = registry.get("lobby").expect("registered").last_check();
    assert!(after > before);
}

#[tokio::test]
async fn offline_backend_can_come_back_online() {
    let json = r#"{"description":{"text":"back"}}"#;
    let addr = spawn_status_server(json).await;
    let registry = registry_with("flaky", addr);

    // first pass: server exists but the backend was manually marked offline
    registry
        .set_state("flaky", BackendState::Offline)
        .expect("known backend");
    let outcomes = registry
        .health_check_with_timeout(Duration::from_secs(2))
        .await;
    assert_eq!(outcomes[0].state, BackendState::Online);
    assert_eq!(registry.state("flaky"), Some(BackendState::Online));
}

#[tokio::test]
async fn default_timeout_is_two_seconds() {
    assert_eq!(DEFAULT_HEALTH_CHECK_TIMEOUT, Duration::from_secs(2));
}

#[tokio::test]
async fn health_check_backend_by_name_reports_unknown_backend() {
    let registry = registry_with("lobby", addr_from_unused_port());
    let result = registry
        .health_check_backend("ghost", Duration::from_secs(1))
        .await;
    assert!(matches!(result, Err(RouteError::UnknownBackend(name)) if name == "ghost"));
}

fn addr_from_unused_port() -> SocketAddr {
    use std::net::{IpAddr, Ipv4Addr};
    SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 9) // discard port, never connected
}

#[tokio::test]
async fn two_backends_are_checked_independently() {
    let json = r#"{"description":{"text":"up"}}"#;
    let good = spawn_status_server(json).await;
    let probe = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let bad = probe.local_addr().expect("local addr");
    drop(probe);

    let registry = BackendRegistry::new(
        vec![("good".to_string(), good), ("bad".to_string(), bad)],
        vec!["good".to_string(), "bad".to_string()],
    )
    .expect("registry must build");

    let outcomes = registry
        .health_check_with_timeout(Duration::from_secs(2))
        .await;
    assert_eq!(outcomes.len(), 2);
    let state_of = |name: &str| {
        outcomes
            .iter()
            .find(|o| o.name == name)
            .expect("outcome")
            .state
    };
    assert_eq!(state_of("good"), BackendState::Online);
    assert_eq!(state_of("bad"), BackendState::Offline);
}

#[test]
fn status_handshake_uses_status_intent_and_negative_protocol() {
    // backend-side sanity check of what the client sends: encode the same
    // handshake the health checker builds and decode it back
    let handshake = Handshake {
        protocol: -1,
        server_address: "127.0.0.1".to_string(),
        server_port: 25565,
        intent: Intent::Status,
    };
    let payload = handshake.encode_payload().expect("encode");
    let mut slice: &[u8] = &payload;
    let decoded = Handshake::decode_payload(&mut slice).expect("decode");
    assert_eq!(decoded.intent, Intent::Status);
    assert_eq!(decoded.protocol, -1);
}

#[tokio::test]
async fn status_server_helper_roundtrip() {
    // belt and braces: the in-test server produces exactly the framing the
    // health checker expects to parse
    let addr = spawn_status_server(r#"{"description":{"text":"ping"}}"#).await;
    let mut stream = TcpStream::connect(addr).await.expect("connect");
    let handshake = Handshake {
        protocol: -1,
        server_address: "127.0.0.1".to_string(),
        server_port: addr.port(),
        intent: Intent::Status,
    };
    let mut codec = PacketCodec::new();
    let mut out = BytesMut::new();
    codec
        .encode(
            Packet {
                id: Handshake::PACKET_ID,
                payload: handshake.encode_payload().expect("encode"),
            },
            &mut out,
        )
        .expect("encode");
    codec
        .encode(
            Packet {
                id: 0x00,
                payload: Bytes::new(),
            },
            &mut out,
        )
        .expect("encode");
    stream.write_all(&out).await.expect("write");

    let mut pending = BytesMut::new();
    let mut response = None;
    while response.is_none() {
        stream.read_buf(&mut pending).await.expect("read");
        if let Ok(Some(packet)) = codec.decode(&mut pending) {
            response = Some(packet);
        }
    }
    let response = response.expect("a packet");
    assert_eq!(response.id, 0x00);
}
