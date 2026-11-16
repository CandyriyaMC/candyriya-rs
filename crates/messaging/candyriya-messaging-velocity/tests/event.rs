//! PluginMessageEvent / ForwardResult / MessageSource semantics.
//!
//! The 4 cases from the upstream documentation map onto the two source
//! variants like this:
//!
//! - Case 1 (client → proxy): event with `MessageSource::Player`
//! - Case 2 (proxy → backend): outbound, no event; source is not involved
//! - Case 3 (backend → proxy): event with `MessageSource::ServerBackend`
//! - Case 4 (proxy → client): outbound, no event; source is not involved

use bytes::Bytes;
use candyriya_messaging::ChannelIdentifier;
use candyriya_messaging_velocity::{ForwardResult, MessageSource, PluginMessageEvent};
use uuid::Uuid;

fn id(s: &str) -> ChannelIdentifier {
    ChannelIdentifier::new(s).expect("test channels must be valid identifiers")
}

fn player_source() -> MessageSource {
    MessageSource::Player {
        name: "Steve".to_string(),
        uuid: Some(Uuid::from_u128(0x1234_5678_9abc_def0_1234_5678_9abc_def0)),
    }
}

fn backend_source() -> MessageSource {
    MessageSource::ServerBackend {
        name: "lobby".to_string(),
    }
}

// ---- ForwardResult -------------------------------------------------------

#[test]
fn forward_result_handled_returns_copy_and_is_stable() {
    // calling handled() repeatedly must not consume or mutate anything
    let a = ForwardResult::handled();
    let b = ForwardResult::handled();
    assert_eq!(a, b);
    assert_eq!(a, ForwardResult::Handled);
    assert_eq!(b, ForwardResult::Handled);
    // Copy semantics: passing the value around never moves anything meaningful
    let copied = a;
    assert_eq!(a, copied);
}

#[test]
fn forward_result_forward_returns_copy_and_is_stable() {
    let a = ForwardResult::forward();
    let b = ForwardResult::forward();
    assert_eq!(a, b);
    assert_eq!(a, ForwardResult::Forward);
}

#[test]
fn forward_result_is_allowed_matches_variant() {
    assert!(!ForwardResult::Handled.is_allowed());
    assert!(ForwardResult::Forward.is_allowed());
    assert!(ForwardResult::Handled.is_handled());
    assert!(!ForwardResult::Forward.is_handled());
}

#[test]
fn forward_result_default_is_forward_like_upstream() {
    // upstream initializes PluginMessageEvent with ForwardResult.forward()
    assert_eq!(ForwardResult::default(), ForwardResult::Forward);
}

#[test]
fn forward_result_display_is_human_readable() {
    assert_ne!(
        ForwardResult::Handled.to_string(),
        ForwardResult::Forward.to_string()
    );
}

// ---- MessageSource -------------------------------------------------------

#[test]
fn player_source_carries_name_and_optional_uuid() {
    let source = MessageSource::Player {
        name: "Alex".to_string(),
        uuid: None,
    };
    assert_eq!(source.name(), "Alex");
    assert!(source.is_player());
    assert!(!source.is_backend());

    let uuid = Uuid::from_u128(1);
    let source = MessageSource::Player {
        name: "Alex".to_string(),
        uuid: Some(uuid),
    };
    match &source {
        MessageSource::Player { uuid: got, .. } => assert_eq!(*got, Some(uuid)),
        other => panic!("expected player source, got {other:?}"),
    }
}

#[test]
fn backend_source_carries_name() {
    let source = MessageSource::ServerBackend {
        name: "factions".to_string(),
    };
    assert_eq!(source.name(), "factions");
    assert!(source.is_backend());
    assert!(!source.is_player());
}

// ---- PluginMessageEvent: construction and defaults -----------------------

#[test]
fn event_defaults_to_forward_like_upstream() {
    let event =
        PluginMessageEvent::new(id("custom:main"), player_source(), Bytes::from_static(b"x"));
    assert_eq!(event.result(), ForwardResult::Forward);
    assert!(event.is_forwarded());
    assert!(!event.is_handled());
}

#[test]
fn event_for_known_channel_defaults_to_handled() {
    // the leak-prevention default for channels the proxy registered
    let event = PluginMessageEvent::new_for_known_channel(
        id("bungeecord:main"),
        backend_source(),
        Bytes::from_static(b"GetServers"),
    );
    assert_eq!(event.result(), ForwardResult::Handled);
    assert!(event.is_handled());
    assert!(!event.is_forwarded());
}

#[test]
fn with_result_builder_overrides_either_default() {
    let flipped =
        PluginMessageEvent::new(id("custom:main"), player_source(), Bytes::from_static(b"x"))
            .with_result(ForwardResult::Handled);
    assert_eq!(flipped.result(), ForwardResult::Handled);

    let flipped_back = PluginMessageEvent::new_for_known_channel(
        id("custom:main"),
        player_source(),
        Bytes::from_static(b"x"),
    )
    .with_result(ForwardResult::Forward);
    assert_eq!(flipped_back.result(), ForwardResult::Forward);
}

#[test]
fn set_result_and_convenience_methods_mutate_result() {
    let mut event =
        PluginMessageEvent::new(id("custom:main"), player_source(), Bytes::from_static(b"x"));
    assert_eq!(event.result(), ForwardResult::Forward);

    event.handled();
    assert_eq!(event.result(), ForwardResult::Handled);

    event.forward();
    assert_eq!(event.result(), ForwardResult::Forward);

    event.set_result(ForwardResult::Handled);
    assert_eq!(event.result(), ForwardResult::Handled);
}

#[test]
fn handled_then_forward_then_handled_is_a_last_write_wins() {
    let mut event = PluginMessageEvent::new_for_known_channel(
        id("custom:main"),
        backend_source(),
        Bytes::from_static(b"y"),
    );
    event.forward();
    event.forward();
    event.handled();
    assert_eq!(event.result(), ForwardResult::Handled);
}

// ---- PluginMessageEvent: 4 cases construct correctly ----------------------

#[test]
fn case1_player_to_proxy_constructs() {
    let uuid = Uuid::from_u128(0xdead_beef);
    let event = PluginMessageEvent::new(
        id("custom:main"),
        MessageSource::Player {
            name: "Case1Mod".to_string(),
            uuid: Some(uuid),
        },
        Bytes::from_static(b"features=a,b"),
    );
    assert_eq!(event.identifier().as_str(), "custom:main");
    match event.source() {
        MessageSource::Player { name, uuid: got } => {
            assert_eq!(name, "Case1Mod");
            assert_eq!(*got, Some(uuid));
        }
        other => panic!("expected player source, got {other:?}"),
    }
    assert_eq!(event.data(), b"features=a,b");
}

#[test]
fn case3_backend_to_proxy_constructs() {
    let event = PluginMessageEvent::new(
        id("bungeecord:main"),
        MessageSource::ServerBackend {
            name: "lobby-1".to_string(),
        },
        Bytes::from_static(b"\0\x0cConnect"),
    );
    assert_eq!(event.identifier().as_str(), "bungeecord:main");
    match event.source() {
        MessageSource::ServerBackend { name } => assert_eq!(name, "lobby-1"),
        other => panic!("expected backend source, got {other:?}"),
    }
}

#[test]
fn case2_and_case4_outbound_directions_share_the_source_model() {
    // Cases 2 (proxy→backend) and 4 (proxy→player) are the outbound
    // directions: the proxy is the sender, so no source is attached by the
    // network layer. The event model keeps exactly two source variants,
    // and both construct fine on either outbound path.
    for source in [player_source(), backend_source()] {
        let event =
            PluginMessageEvent::new_for_known_channel(id("custom:main"), source, Bytes::new());
        assert!(matches!(
            event.source(),
            MessageSource::Player { .. } | MessageSource::ServerBackend { .. }
        ));
    }
}

#[test]
fn event_data_roundtrip_into_bytes() {
    let payload = Bytes::from_static(b"\x00\x03abc");
    let event = PluginMessageEvent::new(id("custom:main"), player_source(), payload.clone());
    assert_eq!(event.data(), payload.as_ref());
    assert_eq!(event.into_data(), payload);
}

#[test]
fn event_is_clone_and_debug() {
    let event = PluginMessageEvent::new_for_known_channel(
        id("custom:main"),
        backend_source(),
        Bytes::from_static(b"debug-me"),
    );
    let cloned = event.clone();
    assert_eq!(event, cloned);
    let rendered = format!("{event:?}");
    assert!(rendered.contains("custom:main"));
    assert!(rendered.contains("ServerBackend"));
}
