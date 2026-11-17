//! End-to-end flows over loopback: real TCP, real wire protocol, real
//! teardown. These tests are the contract of the harness — they pin down
//! every documented backend behavior, including the ones nobody would put
//! in production (unknown protocol numbers, unknown intents, ...).
//!
//! Nothing here touches the network beyond `127.0.0.1`.

use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use candyriya_it::{
    BackendFixture, BackendForwarding, FakeBackendConfig, Intent, LoginSuccess, TestClient,
    parse_uuid, uuid_from_name,
};
use candyriya_protocol_registry::Registry;

/// An MOTD hostile enough to break a naive JSON builder (quotes, backslash,
/// multibyte euro sign), so the escaping path is exercised through the wire.
const TEST_MOTD: &str = "it-works \"quoted\" \\backslash €";

/// Arbitrary play-state packet id. Play ids are deliberately not tracked by
/// the registry, so the client sends one explicitly and the backend ignores it.
const CHAT_PACKET_ID: i32 = 0x06;

fn config(protocol: i32) -> FakeBackendConfig {
    FakeBackendConfig {
        protocol,
        motd: TEST_MOTD.to_owned(),
        max_players: 42,
        forwarding: BackendForwarding::Off,
    }
}

fn registry() -> Registry {
    Registry::embedded().expect("embedded snapshot is valid")
}

async fn login(client: &mut TestClient, protocol: i32, name: &str) -> LoginSuccess {
    client
        .login_start(protocol, name)
        .await
        .expect("login start");
    client
        .expect_login_success(protocol)
        .await
        .expect("login success")
}

#[tokio::test]
async fn status_handshake_roundtrip_echoes_config() {
    let fixture = BackendFixture::start(config(767))
        .await
        .expect("fixture starts");
    let port = fixture.addr().port();
    let mut client = TestClient::connect(fixture.addr()).await.expect("connect");

    client
        .send_handshake(767, "127.0.0.1", port, Intent::Status)
        .await
        .expect("handshake");
    let raw = client.status_request(767).await.expect("status response");

    let json: serde_json::Value = serde_json::from_str(&raw).expect("status response is JSON");
    assert_eq!(json["description"], TEST_MOTD);
    assert_eq!(json["players"]["max"], 42);
    assert_eq!(json["players"]["online"], 0);
    assert_eq!(json["version"]["protocol"], 767);
    let expected_name = registry()
        .from_protocol(767)
        .expect("767 is in the table")
        .name;
    assert_eq!(json["version"]["name"], expected_name);

    // the favicon is an embedded 1x1 PNG; the magic bytes prove it is a real PNG
    let favicon = json["favicon"].as_str().expect("favicon is a string");
    let base64_body = favicon
        .strip_prefix("data:image/png;base64,")
        .expect("favicon is a data URI");
    let png = BASE64.decode(base64_body).expect("favicon base64 decodes");
    assert_eq!(&png[..8], &[0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]);

    // ping/pong round trip, payload preserved byte for byte
    for ts in [0i64, 1, -42, i64::MAX, i64::MIN] {
        assert_eq!(client.status_ping(767, ts).await.expect("ping"), ts);
    }

    let stats = fixture.close().await;
    assert_eq!(stats.handshakes, 1);
    assert_eq!(stats.status_requests, 1);
    assert_eq!(stats.pings, 5);
    assert_eq!(stats.logins, 0);
    assert_eq!(stats.protocol_errors, 0);
}

#[tokio::test]
async fn login_handshake_roundtrip_all_eras() {
    // 4 = 1.7.2, 47 = 1.8, 767 = 1.21.x, 777 = 26.3: the packet ids
    // (and only those) differ per version; the registry resolves them
    for protocol in [4, 47, 767, 777] {
        let fixture = BackendFixture::start(config(protocol))
            .await
            .expect("fixture starts");
        let port = fixture.addr().port();
        let mut client = TestClient::connect(fixture.addr()).await.expect("connect");

        client
            .send_handshake(protocol, "127.0.0.1", port, Intent::Login)
            .await
            .expect("handshake");
        let success = login(&mut client, protocol, "ItHarness").await;

        assert!(
            success.is_uuid_non_zero(),
            "uuid must not be zero (protocol {protocol})"
        );
        assert!(
            success.is_uuid_parseable(),
            "uuid must be parseable (protocol {protocol})"
        );
        assert_eq!(success.name(), "ItHarness");
        assert_eq!(success.uuid_parts(), uuid_from_name("ItHarness"));
        assert_eq!(
            parse_uuid(&success.uuid_string()).expect("canonical uuid parses"),
            success.uuid_parts()
        );
        // body layout: uuid | username | properties (0x00) | 16-byte session id
        assert_eq!(success.raw_extra()[0], 0x00);
        assert_eq!(success.raw_extra().len(), 17);

        let stats = fixture.close().await;
        assert_eq!(
            stats.logins, 1,
            "exactly one login per connection (protocol {protocol})"
        );
        assert_eq!(stats.protocol_errors, 0);
    }
}

#[tokio::test]
async fn backend_close_proceeds_client_sees_socket_close() {
    let fixture = BackendFixture::start(config(767))
        .await
        .expect("fixture starts");
    let port = fixture.addr().port();
    let mut client = TestClient::connect(fixture.addr()).await.expect("connect");

    client
        .send_handshake(767, "127.0.0.1", port, Intent::Login)
        .await
        .expect("handshake");
    let success = login(&mut client, 767, "ItHarness").await;
    assert!(success.is_uuid_non_zero());

    // teardown must complete even while the client holds the connection open
    let stats = tokio::time::timeout(Duration::from_secs(3), fixture.close())
        .await
        .expect("close() must not deadlock with an open client");
    assert_eq!(stats.logins, 1);

    // the backend is gone; the client observes end-of-stream, not a hang
    let eof = tokio::time::timeout(Duration::from_secs(2), client.next_packet())
        .await
        .expect("client must observe EOF after the backend went away")
        .expect("read must not error");
    assert!(eof.is_none(), "expected end-of-stream, got a packet");
}

#[tokio::test]
async fn unknown_protocol_number_is_accepted_and_echoed() {
    // 99999 is not a real protocol version; the backend accepts any number
    // and resolves packet ids from the newest known release (documented).
    let fixture = BackendFixture::start(config(99999))
        .await
        .expect("fixture starts");
    let port = fixture.addr().port();

    let mut status_client = TestClient::connect(fixture.addr()).await.expect("connect");
    status_client
        .send_handshake(99999, "127.0.0.1", port, Intent::Status)
        .await
        .expect("handshake");
    let raw = status_client
        .status_request(99999)
        .await
        .expect("status response");
    let json: serde_json::Value = serde_json::from_str(&raw).expect("status response is JSON");
    assert_eq!(json["version"]["protocol"], 99999);
    // no known release maps to 99999, so the release name is synthesized
    assert_eq!(json["version"]["name"], "protocol 99999");
    assert_eq!(
        status_client.status_ping(99999, 123).await.expect("ping"),
        123
    );

    let mut login_client = TestClient::connect(fixture.addr()).await.expect("connect");
    login_client
        .send_handshake(99999, "127.0.0.1", port, Intent::Login)
        .await
        .expect("handshake");
    let success = login(&mut login_client, 99999, "ItHarness").await;
    assert!(success.is_uuid_non_zero());
    assert_eq!(success.name(), "ItHarness");

    let stats = fixture.close().await;
    assert_eq!(stats.status_requests, 1);
    assert_eq!(stats.logins, 1);
    assert_eq!(stats.protocol_errors, 0);
}

#[tokio::test]
async fn multiple_sequential_connections_reuse_the_same_port() {
    let fixture = BackendFixture::start(config(767))
        .await
        .expect("fixture starts");
    let port = fixture.addr().port();

    for _ in 0..3 {
        let mut client = TestClient::connect(fixture.addr()).await.expect("connect");
        assert_eq!(client.peer_addr().expect("peer addr").port(), port);
        client
            .send_handshake(767, "127.0.0.1", port, Intent::Status)
            .await
            .expect("handshake");
        let raw = client.status_request(767).await.expect("status response");
        let json: serde_json::Value = serde_json::from_str(&raw).expect("status response is JSON");
        assert_eq!(json["players"]["max"], 42);
        drop(client); // disconnect; the backend reaps the connection and accepts the next one
    }

    let mut client = TestClient::connect(fixture.addr()).await.expect("connect");
    assert_eq!(client.peer_addr().expect("peer addr").port(), port);
    client
        .send_handshake(767, "127.0.0.1", port, Intent::Login)
        .await
        .expect("handshake");
    let success = login(&mut client, 767, "MultiStep").await;
    assert_eq!(success.name(), "MultiStep");

    let stats = fixture.close().await;
    assert_eq!(stats.connections_accepted, 4);
    assert_eq!(stats.status_requests, 3);
    assert_eq!(stats.logins, 1);
    assert_eq!(stats.protocol_errors, 0);
}

#[tokio::test]
async fn teardown_with_an_open_client_does_not_deadlock() {
    let fixture = BackendFixture::start(config(767))
        .await
        .expect("fixture starts");
    let port = fixture.addr().port();
    let mut client = TestClient::connect(fixture.addr()).await.expect("connect");

    client
        .send_handshake(767, "127.0.0.1", port, Intent::Login)
        .await
        .expect("handshake");
    let _ = login(&mut client, 767, "ItHarness").await;

    // the client never disconnects; close() must still return promptly
    let stats = tokio::time::timeout(Duration::from_secs(3), fixture.close())
        .await
        .expect("close() must not deadlock with an open client");
    assert_eq!(stats.logins, 1);

    let eof = tokio::time::timeout(Duration::from_secs(2), client.next_packet())
        .await
        .expect("client must observe EOF")
        .expect("read must not error");
    assert!(eof.is_none());
}

#[tokio::test]
async fn drop_teardown_does_not_orphan_the_connection() {
    let fixture = BackendFixture::start(config(767))
        .await
        .expect("fixture starts");
    let port = fixture.addr().port();
    let mut client = TestClient::connect(fixture.addr()).await.expect("connect");

    client
        .send_handshake(767, "127.0.0.1", port, Intent::Login)
        .await
        .expect("handshake");
    let _ = login(&mut client, 767, "ItHarness").await;

    drop(fixture); // sync Drop path: aborts the accept loop and the connection task

    let eof = tokio::time::timeout(Duration::from_secs(2), client.next_packet())
        .await
        .expect("drop must not orphan the connection task holding the socket open")
        .expect("read must not error");
    assert!(eof.is_none());
}

#[tokio::test]
async fn chat_message_is_met_with_silence() {
    let fixture = BackendFixture::start(config(767))
        .await
        .expect("fixture starts");
    let port = fixture.addr().port();
    let mut client = TestClient::connect(fixture.addr()).await.expect("connect");

    client
        .send_handshake(767, "127.0.0.1", port, Intent::Login)
        .await
        .expect("handshake");
    let _ = login(&mut client, 767, "ItHarness").await;

    client
        .send_chat_message(CHAT_PACKET_ID, "hello backend")
        .await
        .expect("chat message sends");

    // the backend must not answer play traffic at all
    let silence = tokio::time::timeout(Duration::from_millis(300), client.next_packet()).await;
    assert!(silence.is_err(), "backend must not respond to play traffic");

    // synchronize with the fixture actually consuming the frame instead of
    // racing the close() abort against the read loop
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while fixture.stats().discarded_frames == 0 {
        assert!(
            std::time::Instant::now() < deadline,
            "fixture must read the play frame"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    drop(client);
    let stats = fixture.close().await;
    assert_eq!(
        stats.discarded_frames, 1,
        "the play frame was read and discarded"
    );
}

#[tokio::test]
async fn unknown_intent_closes_connection_without_a_response() {
    let fixture = BackendFixture::start(config(767))
        .await
        .expect("fixture starts");
    let port = fixture.addr().port();
    let mut client = TestClient::connect(fixture.addr()).await.expect("connect");

    client
        .send_handshake(767, "127.0.0.1", port, Intent::Unknown(7))
        .await
        .expect("handshake");

    // documented behavior: no response packet, the connection just closes
    let eof = tokio::time::timeout(Duration::from_secs(2), client.next_packet())
        .await
        .expect("unknown intent must be refused promptly")
        .expect("read must not error");
    assert!(eof.is_none(), "expected end-of-stream, got a packet");

    let stats = fixture.close().await;
    assert_eq!(stats.protocol_errors, 1);
}

#[tokio::test]
async fn unknown_packet_in_status_state_closes_connection() {
    let fixture = BackendFixture::start(config(767))
        .await
        .expect("fixture starts");
    let port = fixture.addr().port();
    let mut client = TestClient::connect(fixture.addr()).await.expect("connect");

    client
        .send_handshake(767, "127.0.0.1", port, Intent::Status)
        .await
        .expect("handshake");
    // 0x2A is not a status-state packet id on any supported version
    client.send_frame(0x2a, &[]).await.expect("frame sends");

    let eof = tokio::time::timeout(Duration::from_secs(2), client.next_packet())
        .await
        .expect("unexpected status packet must be refused promptly")
        .expect("read must not error");
    assert!(eof.is_none(), "expected end-of-stream, got a packet");

    let stats = fixture.close().await;
    assert_eq!(stats.protocol_errors, 1);
}

#[tokio::test]
async fn disconnect_mode_kicks_any_login_start() {
    let fixture = BackendFixture::start(FakeBackendConfig {
        protocol: 767,
        motd: TEST_MOTD.to_owned(),
        max_players: 42,
        forwarding: BackendForwarding::Disconnect {
            reason: "maintenance".to_owned(),
        },
    })
    .await
    .expect("fixture starts");
    let port = fixture.addr().port();
    let mut client = TestClient::connect(fixture.addr()).await.expect("connect");

    client
        .send_handshake(767, "127.0.0.1", port, Intent::Login)
        .await
        .expect("handshake");
    client.login_start(767, "Steve").await.expect("login start");

    // the kick is a login disconnect whose JSON reason carries the configured text
    let registry = Registry::embedded().expect("embedded snapshot is valid");
    let disconnect_id = registry.packet_id(
        767,
        candyriya_protocol_registry::PacketSlot::LoginClientboundDisconnect,
    );
    let packet = client.next_packet().await.expect("read").expect("a packet");
    assert_eq!(
        Some(packet.id),
        disconnect_id,
        "a login disconnect must be the kick packet"
    );
    let mut body: &[u8] = &packet.payload;
    let reason = candyriya_protocol::read_string(&mut body, 32767).expect("reason is utf-8");
    assert!(
        reason.contains("maintenance"),
        "reason must carry the configured text: {reason}"
    );

    let stats = fixture.close().await;
    assert_eq!(stats.forced_disconnects, 1);
    assert_eq!(stats.logins, 0);
}

#[tokio::test]
async fn modern_forwarding_rejects_a_forged_plugin_response() {
    let fixture = BackendFixture::start(FakeBackendConfig {
        protocol: 767,
        motd: TEST_MOTD.to_owned(),
        max_players: 42,
        forwarding: BackendForwarding::Modern {
            secret: "s3cret-s3cret-s3cret-s3cret-s3cret-!!".to_owned(),
        },
    })
    .await
    .expect("fixture starts");
    let port = fixture.addr().port();
    let mut client = TestClient::connect(fixture.addr()).await.expect("connect");

    client
        .send_handshake(767, "127.0.0.1", port, Intent::Login)
        .await
        .expect("handshake");
    client.login_start(767, "Steve").await.expect("login start");

    // the backend asks for the player info; answer with a forged payload
    let registry = Registry::embedded().expect("embedded snapshot is valid");
    let request = client.next_packet().await.expect("read").expect("a packet");
    let request_kind = registry.classify(
        candyriya_protocol_registry::ConnectionState::Login,
        candyriya_protocol_registry::Direction::Clientbound,
        registry.from_protocol(767).expect("767 is in the table"),
        request.id,
    );
    assert_eq!(
        request_kind,
        candyriya_protocol_registry::PacketKind::LoginPluginRequest,
        "the backend must ask for velocity:player_info"
    );

    let response_id = registry
        .packet_id(
            767,
            candyriya_protocol_registry::PacketSlot::LoginServerboundPluginResponse,
        )
        .expect("plugin response id exists on 767");
    let mut payload = Vec::new();
    payload.push(0x01); // message id 1
    payload.push(0x01); // success flag
    payload.extend_from_slice(b"forged-not-hmac");
    client
        .send_frame(response_id, &payload)
        .await
        .expect("response sends");

    let packet = client.next_packet().await.expect("read").expect("a packet");
    let disconnect_id = registry
        .packet_id(
            767,
            candyriya_protocol_registry::PacketSlot::LoginClientboundDisconnect,
        )
        .expect("login disconnect id exists on 767");
    assert_eq!(
        packet.id, disconnect_id,
        "a forged forwarding payload must be rejected with a login disconnect"
    );

    let stats = fixture.close().await;
    assert_eq!(stats.player_info_requests, 1);
    assert_eq!(stats.forwarding_failures, 1);
    assert_eq!(stats.logins, 0);
}
