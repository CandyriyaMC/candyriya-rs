//! The ABI contract, round-tripped: ProxyVersion, events, envelope aliases.

use candyriya_plugin_api::{EventKind, EventPayload, PlayerRef, PluginEvent, ProxyVersion};

fn full_payload() -> EventPayload {
    EventPayload::new(EventKind::SwitchServer {
        uuid: "069a79f4-44e9-4726-a5be-fca90e38aaf5".to_owned(),
        name: "SomePlayer".to_owned(),
        from_server: "lobby".to_owned(),
        to_server: "hub".to_owned(),
    })
}

#[test]
fn proxy_version_roundtrip() {
    let version = ProxyVersion::current();
    let json = serde_json::to_string(&version).expect("serialize");
    assert_eq!(
        json,
        r#"{"api_version":"0.1","implementation":"candyriya"}"#
    );
    let back: ProxyVersion = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(back, version);
}

#[test]
fn event_payload_roundtrip_full() {
    let payload = full_payload();
    let json = serde_json::to_string(&payload).expect("serialize");
    assert_eq!(
        json,
        r#"{"kind":"switch_server","uuid":"069a79f4-44e9-4726-a5be-fca90e38aaf5","name":"SomePlayer","from_server":"lobby","to_server":"hub"}"#
    );
    let back: EventPayload = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(back, payload);
}

#[test]
fn event_payload_roundtrip_login() {
    let payload = EventPayload::new(EventKind::Login {
        uuid: "069a79f4-44e9-4726-a5be-fca90e38aaf5".to_owned(),
        name: "SomePlayer".to_owned(),
        protocol_version: 769,
    });
    let json = serde_json::to_string(&payload).expect("serialize");
    assert_eq!(
        json,
        r#"{"kind":"login","uuid":"069a79f4-44e9-4726-a5be-fca90e38aaf5","name":"SomePlayer","protocol_version":769}"#
    );
    let back: EventPayload = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(back, payload);
}

#[test]
fn event_payload_disconnect_keeps_carrier_free_reason() {
    // `reason` is a plain string by design: it is carrier-free at the
    // WASM edge and the proxy renders it host-side
    let payload: EventPayload = serde_json::from_str(
        r#"{"kind":"disconnect","uuid":"069a79f4-44e9-4726-a5be-fca90e38aaf5","name":"SomePlayer","server":"lobby","reason":"kicked by admin"}"#,
    )
    .expect("disconnect payload must parse");
    assert_eq!(
        payload.kind,
        EventKind::Disconnect {
            uuid: "069a79f4-44e9-4726-a5be-fca90e38aaf5".to_owned(),
            name: "SomePlayer".to_owned(),
            server: "lobby".to_owned(),
            reason: "kicked by admin".to_owned(),
        }
    );
}

#[test]
fn plugin_event_alias_is_the_same_envelope() {
    // the plugin-facing spelling and the host-facing spelling are one type
    let event: PluginEvent = full_payload();
    let payload: EventPayload = event;
    assert_eq!(payload.kind.as_str(), "switch_server");
}

#[test]
fn every_known_kind_has_a_stable_wire_name() {
    let cases: [(EventKind, &str); 5] = [
        (
            EventKind::ConnectionAllowed {
                uuid: "069a79f4-44e9-4726-a5be-fca90e38aaf5".to_owned(),
                name: "SomePlayer".to_owned(),
                protocol: 769,
                address: "127.0.0.1:25565".to_owned(),
            },
            "connection_allowed",
        ),
        (
            EventKind::ConnectionKicked {
                uuid: "069a79f4-44e9-4726-a5be-fca90e38aaf5".to_owned(),
                name: "SomePlayer".to_owned(),
                reason: "server full".to_owned(),
            },
            "connection_kicked",
        ),
        (
            EventKind::Login {
                uuid: "069a79f4-44e9-4726-a5be-fca90e38aaf5".to_owned(),
                name: "SomePlayer".to_owned(),
                protocol_version: 769,
            },
            "login",
        ),
        (
            EventKind::SwitchServer {
                uuid: "069a79f4-44e9-4726-a5be-fca90e38aaf5".to_owned(),
                name: "SomePlayer".to_owned(),
                from_server: "lobby".to_owned(),
                to_server: "hub".to_owned(),
            },
            "switch_server",
        ),
        (
            EventKind::Disconnect {
                uuid: "069a79f4-44e9-4726-a5be-fca90e38aaf5".to_owned(),
                name: "SomePlayer".to_owned(),
                server: "lobby".to_owned(),
                reason: "quit".to_owned(),
            },
            "disconnect",
        ),
    ];
    for (kind, wire) in cases {
        assert_eq!(kind.as_str(), wire);
        let json = serde_json::to_string(&kind).expect("serialize");
        let prefix = format!("{{\"kind\":\"{wire}\"");
        assert!(json.starts_with(&prefix), "{json} lacks leading {prefix}");
    }
}

#[test]
fn unknown_kind_survives_roundtrip() {
    let payload: EventPayload = serde_json::from_str(r#"{"kind":"server_ping"}"#)
        .expect("an ABI plugin built earlier must tolerate future kinds");
    assert_eq!(payload.kind, EventKind::Unknown("server_ping".to_owned()));
    let json = serde_json::to_string(&payload).expect("serialize");
    assert_eq!(json, r#"{"kind":"server_ping"}"#);
}

#[test]
fn non_string_kind_is_rejected() {
    let result = serde_json::from_str::<EventKind>("42");
    assert!(result.is_err());
}

#[test]
fn player_ref_still_roundtrips() {
    let player = PlayerRef {
        uuid: "069a79f4-44e9-4726-a5be-fca90e38aaf5".to_owned(),
        name: "SomePlayer".to_owned(),
    };
    let json = serde_json::to_string(&player).expect("serialize");
    let back: PlayerRef = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(back, player);
}
