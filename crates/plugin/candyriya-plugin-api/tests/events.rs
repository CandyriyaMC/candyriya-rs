//! The event model checklist: structured kinds round-tripping with every
//! field intact, monotonic envelope ids, and the two response shapes a
//! plugin hands back to the proxy.

use candyriya_plugin_api::{
    ConnectAllowedEvent, DisconnectEvent, EventCancellation, EventEnvelope, EventIdRegistry,
    EventKind, KickEvent, LoginEvent, MinecraftEvent, OutcomeReport, ProxyEvent, SwitchServerEvent,
};

const NOTCH_UUID: &str = "069a79f4-44e9-4726-a5be-fca90e38aaf5";

fn login_kind() -> EventKind {
    EventKind::Login {
        uuid: NOTCH_UUID.to_owned(),
        name: "Notch".to_owned(),
        protocol_version: 769,
    }
}

fn envelope(id: u64, kind: EventKind) -> EventEnvelope {
    EventEnvelope::new(id, kind, 1_730_000_000_000)
}

#[test]
fn login_roundtrip_loses_no_fields() {
    let kind = login_kind();
    let json = serde_json::to_string(&kind).expect("serialize");
    assert_eq!(
        json,
        r#"{"kind":"login","uuid":"069a79f4-44e9-4726-a5be-fca90e38aaf5","name":"Notch","protocol_version":769}"#
    );
    let back: EventKind = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(back, kind);
}

#[test]
fn connection_kicked_roundtrip_keeps_reason() {
    let kind = EventKind::ConnectionKicked {
        uuid: NOTCH_UUID.to_owned(),
        name: "Notch".to_owned(),
        reason: "server full".to_owned(),
    };
    let json = serde_json::to_string(&kind).expect("serialize");
    assert_eq!(
        json,
        r#"{"kind":"connection_kicked","uuid":"069a79f4-44e9-4726-a5be-fca90e38aaf5","name":"Notch","reason":"server full"}"#
    );
    let back: EventKind = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(back, kind);
}

#[test]
fn disconnect_roundtrip_keeps_server_and_plain_reason() {
    let kind = EventKind::Disconnect {
        uuid: NOTCH_UUID.to_owned(),
        name: "Notch".to_owned(),
        server: "lobby".to_owned(),
        reason: "Connection reset".to_owned(),
    };
    let json = serde_json::to_string(&kind).expect("serialize");
    let back: EventKind = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(back, kind);
}

#[test]
fn switch_server_roundtrip_keeps_from_and_to() {
    let kind = EventKind::SwitchServer {
        uuid: NOTCH_UUID.to_owned(),
        name: "Notch".to_owned(),
        from_server: "lobby".to_owned(),
        to_server: "hub".to_owned(),
    };
    let json = serde_json::to_string(&kind).expect("serialize");
    assert!(json.contains(r#""from_server":"lobby""#));
    assert!(json.contains(r#""to_server":"hub""#));
    let back: EventKind = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(back, kind);
}

#[test]
fn connection_allowed_roundtrip_forwards_address_and_protocol() {
    let kind = EventKind::ConnectionAllowed {
        uuid: NOTCH_UUID.to_owned(),
        name: "Notch".to_owned(),
        protocol: 769,
        address: "192.0.2.1:51234".to_owned(),
    };
    let json = serde_json::to_string(&kind).expect("serialize");
    assert!(json.contains(r#""protocol":769"#));
    assert!(json.contains(r#""address":"192.0.2.1:51234""#));
    let back: EventKind = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(back, kind);
}

#[test]
fn unknown_fields_are_tolerated_and_dropped() {
    // a newer host added fields this ABI does not know
    let kind: EventKind = serde_json::from_str(
        r#"{"kind":"login","uuid":"069a79f4-44e9-4726-a5be-fca90e38aaf5","name":"Notch","protocol_version":769,"future_field":{"nested":[1,2,3]}}"#,
    )
    .expect("unknown fields must not break parsing");
    assert_eq!(kind, login_kind());
    let json = serde_json::to_string(&kind).expect("serialize");
    assert!(!json.contains("future_field"));
}

#[test]
fn missing_required_field_is_an_error() {
    let result = serde_json::from_str::<EventKind>(
        r#"{"kind":"login","uuid":"069a79f4-44e9-4726-a5be-fca90e38aaf5","name":"Notch"}"#,
    );
    let err = result.expect_err("missing protocol_version must fail");
    assert!(err.to_string().contains("protocol_version"));
}

#[test]
fn envelope_roundtrip_and_exact_wire_pin() {
    let env = envelope(1, login_kind());
    let json = serde_json::to_string(&env).expect("serialize");
    assert_eq!(
        json,
        r#"{"id":1,"kind":"login","uuid":"069a79f4-44e9-4726-a5be-fca90e38aaf5","name":"Notch","protocol_version":769,"occurred_at_unix_ms":1730000000000}"#
    );
    let back: EventEnvelope = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(back, env);
}

#[test]
fn event_id_registry_is_monotonic() {
    let registry = EventIdRegistry::new();
    let ids: Vec<u64> = (0..5).map(|_| registry.next_id()).collect();
    assert_eq!(ids, vec![1, 2, 3, 4, 5]);

    let first = envelope(registry.next_id(), login_kind());
    let second = envelope(registry.next_id(), login_kind());
    assert!(first.id < second.id);
}

#[test]
fn cancellation_serializes_with_spec_key_cancelled() {
    let cancellation = EventCancellation {
        uuid: NOTCH_UUID.to_owned(),
        event_id: 7,
        cancelled: true,
    };
    let json = serde_json::to_string(&cancellation).expect("serialize");
    assert!(json.contains(r#""cancelled":true"#));
    let value: serde_json::Value = serde_json::from_str(&json).expect("valid json");
    assert_eq!(value["cancelled"], serde_json::Value::Bool(true));
    assert_eq!(value["event_id"], serde_json::json!(7));
    let back: EventCancellation = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(back, cancellation);
}

#[test]
fn outcome_report_parses_canned_host_bytes() {
    let canned = br#"{"cancelled":true,"cancel_reason":"not allowed here"}"#;
    let report: OutcomeReport = serde_json::from_slice(canned).expect("parse canned bytes");
    assert!(report.cancelled);
    assert_eq!(report.cancel_reason.as_deref(), Some("not allowed here"));

    // the wire document carries exactly two fields
    let value: serde_json::Value = serde_json::from_slice(canned).expect("valid json");
    assert_eq!(value.as_object().expect("object").len(), 2);

    // a plugin answering without a reason stays valid and elides the key
    let minimal = br#"{"cancelled":false}"#;
    let report: OutcomeReport = serde_json::from_slice(minimal).expect("parse minimal");
    assert!(!report.cancelled);
    assert_eq!(report.cancel_reason, None);
    let json = serde_json::to_string(&report).expect("serialize");
    assert_eq!(json, r#"{"cancelled":false}"#);
}

#[test]
fn proxy_event_cancel_and_outcome_flow() {
    let mut event = ProxyEvent::new(envelope(42, login_kind()));
    assert_eq!(event.event_id(), 42);
    assert!(!event.is_cancelled());
    assert_eq!(event.cancel_reason(), None);

    event.cancel(Some("lobby closed".to_owned()));
    assert!(event.is_cancelled());
    assert_eq!(event.cancel_reason(), Some("lobby closed"));

    let outcome = event.outcome();
    assert!(outcome.cancelled);
    assert_eq!(outcome.cancel_reason.as_deref(), Some("lobby closed"));

    let cancellation = event.cancellation().expect("login carries a uuid");
    assert_eq!(cancellation.uuid, NOTCH_UUID);
    assert_eq!(cancellation.event_id, 42);
    assert!(cancellation.cancelled);

    // builds back into a plain envelope without losing the original facts
    let env = event.clone().into_envelope();
    assert_eq!(env.id, 42);
    assert_eq!(env.kind, login_kind());
}

#[test]
fn proxy_event_cancellation_is_none_for_unknown_kind() {
    let event = ProxyEvent::new(envelope(9, EventKind::Unknown("server_ping".to_owned())));
    assert_eq!(event.cancellation(), None);
    // ...but the id is still there
    assert_eq!(event.event_id(), 9);
}

#[test]
fn proxy_event_serde_roundtrip_keeps_state() {
    let mut event = ProxyEvent::new(envelope(3, login_kind()));
    event.cancel(Some("maintenance".to_owned()));
    let json = serde_json::to_string(&event).expect("serialize");
    assert!(json.contains(r#""cancelled":true"#));
    assert!(json.contains(r#""cancel_reason":"maintenance""#));
    let back: ProxyEvent = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(back, event);
}

#[test]
fn game_event_aliases_name_the_common_dispatch_shape() {
    let login: LoginEvent = ProxyEvent::new(envelope(1, login_kind()));
    let kick: KickEvent = ProxyEvent::new(envelope(2, login_kind()));
    let disconnect: DisconnectEvent = ProxyEvent::new(envelope(3, login_kind()));
    let allowed: ConnectAllowedEvent = ProxyEvent::new(envelope(4, login_kind()));
    let switched: SwitchServerEvent = ProxyEvent::new(envelope(5, login_kind()));
    assert_eq!(login.event_id(), 1);
    assert_eq!(kick.event_id(), 2);
    assert_eq!(disconnect.event_id(), 3);
    assert_eq!(allowed.event_id(), 4);
    assert_eq!(switched.event_id(), 5);
}

#[test]
fn minecraft_event_ties_input_and_output_together() {
    let environment: <ProxyEvent as MinecraftEvent>::Input = envelope(11, login_kind());
    let answer: <ProxyEvent as MinecraftEvent>::Output = OutcomeReport::allowed();
    assert_eq!(environment.id, 11);
    assert!(!answer.cancelled);
}

#[test]
fn event_types_are_static_and_unpinned() {
    fn assert_static_unpinned<T: 'static + Unpin>() {}

    assert_static_unpinned::<EventKind>();
    assert_static_unpinned::<EventEnvelope>();
    assert_static_unpinned::<EventCancellation>();
    assert_static_unpinned::<OutcomeReport>();
    assert_static_unpinned::<ProxyEvent>();
}
