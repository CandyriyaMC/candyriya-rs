//! The ABI contract, round-tripped: ProxyVersion, events, envelope aliases.

use candyriya_plugin_api::{EventKind, EventPayload, PlayerRef, PluginEvent, ProxyVersion};

fn full_payload() -> EventPayload {
    EventPayload {
        kind: EventKind::SwitchServer,
        player: Some(PlayerRef {
            uuid: "069a79f4-44e9-4726-a5be-fca90e38aaf5".to_owned(),
            name: "SomePlayer".to_owned(),
        }),
        server: Some("lobby".to_owned()),
        reason: None,
    }
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
    // absent fields stay absent — no "reason": null on the wire
    assert_eq!(
        json,
        r#"{"kind":"switch_server","player":{"uuid":"069a79f4-44e9-4726-a5be-fca90e38aaf5","name":"SomePlayer"},"server":"lobby"}"#
    );
    let back: EventPayload = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(back, payload);
}

#[test]
fn event_payload_roundtrip_empty() {
    let payload = EventPayload::new(EventKind::Login);
    let json = serde_json::to_string(&payload).expect("serialize");
    assert_eq!(json, r#"{"kind":"login"}"#);
    let back: EventPayload = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(back, payload);
}

#[test]
fn event_payload_missing_optionals_deserialize() {
    let payload: EventPayload =
        serde_json::from_str(r#"{"kind":"disconnect","reason":"kicked by admin"}"#)
            .expect("reason-only payload must parse");
    assert_eq!(payload.kind, EventKind::Disconnect);
    assert_eq!(payload.player, None);
    assert_eq!(payload.server, None);
    assert_eq!(payload.reason.as_deref(), Some("kicked by admin"));
}

#[test]
fn plugin_event_alias_is_the_same_envelope() {
    // the plugin-facing spelling and the host-facing spelling are one type
    let event: PluginEvent = full_payload();
    let payload: EventPayload = event;
    assert_eq!(payload.kind, EventKind::SwitchServer);
}

#[test]
fn every_known_kind_has_a_stable_wire_name() {
    let cases = [
        (EventKind::ConnectionAllowed, "connection_allowed"),
        (EventKind::ConnectionKicked, "connection_kicked"),
        (EventKind::Login, "login"),
        (EventKind::SwitchServer, "switch_server"),
        (EventKind::Disconnect, "disconnect"),
    ];
    for (kind, wire) in cases {
        assert_eq!(kind.as_str(), wire);
        let json = serde_json::to_string(&kind).expect("serialize");
        assert_eq!(json, format!("\"{wire}\""));
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
