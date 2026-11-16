//! Integration tests against the public API, using only the embedded
//! snapshot and public items. No network, no filesystem fixtures.

use candyriya_protocol_registry::{ConnectionState, Direction, PacketKind, PacketSlot, Registry};

fn registry() -> Registry {
    Registry::embedded().expect("the embedded snapshot is valid")
}

#[test]
fn table_is_sorted_and_names_unique() {
    let registry = registry();
    let versions = registry.versions();
    assert_eq!(versions.len(), 83);
    for pair in versions.windows(2) {
        assert!(
            pair[0].protocol <= pair[1].protocol,
            "protocol ids must be non-decreasing: {} then {}",
            pair[0].protocol,
            pair[1].protocol
        );
    }
    let unique: std::collections::HashSet<_> = versions.iter().map(|v| v.name).collect();
    assert_eq!(unique.len(), versions.len(), "release names must be unique");
}

#[test]
fn spot_checks_known_protocol_ids() {
    let registry = registry();
    let cases = [
        ("1.7.2", 4),
        ("1.7.6", 5),
        ("1.7.10", 5),
        ("1.8", 47),
        ("1.8.9", 47),
        ("1.9", 107),
        ("1.9.1", 108),
        ("1.9.2", 109),
        ("1.9.3", 110),
        ("1.9.4", 110),
        ("1.10.2", 210),
        ("1.11.2", 316),
        ("1.12", 335),
        ("1.12.1", 338),
        ("1.12.2", 340),
        ("1.13", 393),
        ("1.13.2", 404),
        ("1.14.4", 498),
        ("1.15.2", 578),
        ("1.16.5", 754),
        ("1.17", 755),
        ("1.17.1", 756),
        ("1.18.2", 758),
        ("1.19", 759),
        ("1.19.1", 760),
        ("1.19.2", 760),
        ("1.19.3", 761),
        ("1.19.4", 762),
        ("1.20", 763),
        ("1.20.1", 763),
        ("1.20.2", 764),
        ("1.20.3", 765),
        ("1.20.4", 765),
        ("1.20.5", 766),
        ("1.20.6", 766),
        ("1.21", 767),
        ("1.21.1", 767),
        ("1.21.2", 768),
        ("1.21.3", 768),
        ("1.21.4", 769),
        ("1.21.5", 770),
        ("1.21.6", 771),
        ("1.21.7", 772),
        ("1.21.8", 772),
        ("1.21.9", 773),
        ("1.21.10", 773),
        ("1.21.11", 774),
        ("26.1", 775),
        ("26.1.2", 775),
        ("26.2", 776),
        ("26.3", 777),
    ];
    for (name, protocol) in cases {
        assert_eq!(
            registry.protocol_id_of(name),
            Some(protocol),
            "protocol id mismatch for {name}"
        );
    }
}

#[test]
fn ambiguous_protocols_resolve_to_last_release_in_group() {
    let registry = registry();
    assert_eq!(registry.from_protocol(763).unwrap().name, "1.20.1");
    assert_eq!(registry.from_protocol(754).unwrap().name, "1.16.5");
    assert_eq!(registry.from_protocol(316).unwrap().name, "1.11.2");
    assert_eq!(registry.from_protocol(767).unwrap().name, "1.21.1");
    assert_eq!(registry.from_protocol(768).unwrap().name, "1.21.3");
    assert_eq!(registry.from_protocol(772).unwrap().name, "1.21.8");
    assert_eq!(registry.from_protocol(773).unwrap().name, "1.21.10");
    assert_eq!(registry.from_protocol(775).unwrap().name, "26.1.2");
    assert_eq!(registry.from_protocol(5).unwrap().name, "1.7.10");
}

#[test]
fn lookups_handle_boundaries() {
    let registry = registry();
    assert_eq!(registry.from_protocol(i32::MIN), None);
    assert_eq!(registry.from_protocol(-1), None);
    assert_eq!(registry.from_protocol(3), None); // older than 1.7.2
    assert_eq!(registry.from_protocol(778), None);
    assert_eq!(registry.from_name("26.3").unwrap().protocol, 777);
    assert_eq!(registry.from_name("26.4"), None);
    assert_eq!(registry.from_name(""), None);
    assert_eq!(registry.from_name("1.7.2 "), None); // exact match required
}

#[test]
fn latest_is_26_3() {
    let registry = registry();
    assert_eq!(registry.latest().name, "26.3");
    assert_eq!(registry.latest().protocol, 777);
    assert_eq!(registry.iter().count(), 83);
}

#[test]
fn feature_flags_follow_the_data() {
    let registry = registry();
    let v120 = registry.from_name("1.20").unwrap();
    let v1202 = registry.from_name("1.20.2").unwrap();
    let v1204 = registry.from_name("1.20.4").unwrap();
    let v1205 = registry.from_name("1.20.5").unwrap();
    let v121 = registry.from_name("1.21").unwrap();
    let v263 = registry.from_name("26.3").unwrap();

    assert!(!v120.has_configuration_state);
    assert!(v1202.has_configuration_state);
    assert!(v1204.has_configuration_state);
    assert!(v1205.has_configuration_state);
    assert!(v121.has_configuration_state);
    assert!(v263.has_configuration_state);

    assert!(!v120.has_transfer_intent);
    assert!(!v1202.has_transfer_intent);
    assert!(!v1204.has_transfer_intent);
    assert!(v1205.has_transfer_intent);
    assert!(v121.has_transfer_intent);
    assert!(v263.has_transfer_intent);
}

#[test]
fn version_info_display() {
    let registry = registry();
    assert_eq!(
        registry.from_name("1.12.2").unwrap().to_string(),
        "1.12.2 (protocol 340)"
    );
    assert_eq!(
        registry.from_name("26.3").unwrap().to_string(),
        "26.3 (protocol 777)"
    );
}

#[test]
fn classify_1_7_2_has_no_compression_or_plugin_packets() {
    let registry = registry();
    let v = registry.from_name("1.7.2").unwrap();
    let serverbound = Direction::Serverbound;
    let clientbound = Direction::Clientbound;

    // status, constant across every release since 1.7.2
    assert_eq!(
        registry.classify(ConnectionState::Status, serverbound, v, 0),
        PacketKind::StatusRequest
    );
    assert_eq!(
        registry.classify(ConnectionState::Status, serverbound, v, 1),
        PacketKind::StatusPing
    );
    assert_eq!(
        registry.classify(ConnectionState::Status, clientbound, v, 0),
        PacketKind::StatusResponse
    );
    assert_eq!(
        registry.classify(ConnectionState::Status, clientbound, v, 1),
        PacketKind::StatusPong
    );

    // login, core packets present
    assert_eq!(
        registry.classify(ConnectionState::Login, serverbound, v, 0),
        PacketKind::LoginStart
    );
    assert_eq!(
        registry.classify(ConnectionState::Login, serverbound, v, 1),
        PacketKind::EncryptionResponse
    );
    assert_eq!(
        registry.classify(ConnectionState::Login, clientbound, v, 0),
        PacketKind::LoginDisconnect
    );
    assert_eq!(
        registry.classify(ConnectionState::Login, clientbound, v, 1),
        PacketKind::EncryptionRequest
    );
    assert_eq!(
        registry.classify(ConnectionState::Login, clientbound, v, 2),
        PacketKind::LoginSuccess
    );

    // compression arrived in 1.8, plugin messages in 1.13: both Unknown here
    assert_eq!(
        registry.classify(ConnectionState::Login, clientbound, v, 3),
        PacketKind::Unknown(3)
    );
    assert_eq!(
        registry.classify(ConnectionState::Login, clientbound, v, 4),
        PacketKind::Unknown(4)
    );
    assert_eq!(
        registry.classify(ConnectionState::Login, serverbound, v, 2),
        PacketKind::Unknown(2)
    );

    // wire ids with no slot anywhere are Unknown
    assert_eq!(
        registry.classify(ConnectionState::Status, serverbound, v, 9),
        PacketKind::Unknown(9)
    );
}

#[test]
fn classify_1_8_has_compression_but_no_plugin_messages() {
    let registry = registry();
    let v = registry.from_name("1.8.9").unwrap();
    assert_eq!(
        registry.classify(ConnectionState::Login, Direction::Clientbound, v, 3),
        PacketKind::SetCompression
    );
    assert_eq!(
        registry.classify(ConnectionState::Login, Direction::Clientbound, v, 4),
        PacketKind::Unknown(4)
    );
    assert_eq!(
        registry.classify(ConnectionState::Login, Direction::Serverbound, v, 2),
        PacketKind::Unknown(2)
    );
    assert_eq!(
        registry.packet_id(v.protocol, PacketSlot::LoginClientboundSetCompression),
        Some(3)
    );
    assert_eq!(
        registry.packet_id(v.protocol, PacketSlot::LoginServerboundPluginResponse),
        None
    );
}

#[test]
fn classify_1_12_2_matrix() {
    let registry = registry();
    let v = registry.from_name("1.12.2").unwrap();
    let sb = Direction::Serverbound;
    let cb = Direction::Clientbound;
    assert_eq!(
        registry.classify(ConnectionState::Login, sb, v, 0),
        PacketKind::LoginStart
    );
    assert_eq!(
        registry.classify(ConnectionState::Login, sb, v, 1),
        PacketKind::EncryptionResponse
    );
    assert_eq!(
        registry.classify(ConnectionState::Login, cb, v, 0),
        PacketKind::LoginDisconnect
    );
    assert_eq!(
        registry.classify(ConnectionState::Login, cb, v, 1),
        PacketKind::EncryptionRequest
    );
    assert_eq!(
        registry.classify(ConnectionState::Login, cb, v, 2),
        PacketKind::LoginSuccess
    );
    assert_eq!(
        registry.classify(ConnectionState::Login, cb, v, 3),
        PacketKind::SetCompression
    );
    // 1.13 login plugin messages not present yet
    assert_eq!(
        registry.classify(ConnectionState::Login, cb, v, 4),
        PacketKind::Unknown(4)
    );
    assert_eq!(
        registry.classify(ConnectionState::Login, sb, v, 2),
        PacketKind::Unknown(2)
    );
}

#[test]
fn classify_1_19_and_1_19_3_have_plugin_messages() {
    let registry = registry();
    for name in ["1.19", "1.19.1", "1.19.2", "1.19.3", "1.19.4"] {
        let v = registry.from_name(name).unwrap();
        let sb = Direction::Serverbound;
        let cb = Direction::Clientbound;
        assert_eq!(
            registry.classify(ConnectionState::Login, sb, v, 0),
            PacketKind::LoginStart,
            "{name}"
        );
        assert_eq!(
            registry.classify(ConnectionState::Login, sb, v, 1),
            PacketKind::EncryptionResponse,
            "{name}"
        );
        assert_eq!(
            registry.classify(ConnectionState::Login, sb, v, 2),
            PacketKind::LoginPluginResponse,
            "{name}"
        );
        assert_eq!(
            registry.classify(ConnectionState::Login, cb, v, 0),
            PacketKind::LoginDisconnect,
            "{name}"
        );
        assert_eq!(
            registry.classify(ConnectionState::Login, cb, v, 1),
            PacketKind::EncryptionRequest,
            "{name}"
        );
        assert_eq!(
            registry.classify(ConnectionState::Login, cb, v, 2),
            PacketKind::LoginSuccess,
            "{name}"
        );
        assert_eq!(
            registry.classify(ConnectionState::Login, cb, v, 3),
            PacketKind::SetCompression,
            "{name}"
        );
        assert_eq!(
            registry.classify(ConnectionState::Login, cb, v, 4),
            PacketKind::LoginPluginRequest,
            "{name}"
        );
    }
}

#[test]
fn classify_1_20_2_has_plugin_messages() {
    let registry = registry();
    let v = registry.from_name("1.20.2").unwrap();
    let sb = Direction::Serverbound;
    let cb = Direction::Clientbound;
    assert_eq!(
        registry.classify(ConnectionState::Login, sb, v, 2),
        PacketKind::LoginPluginResponse
    );
    assert_eq!(
        registry.classify(ConnectionState::Login, cb, v, 4),
        PacketKind::LoginPluginRequest
    );
    // login_acknowledged (serverbound 0x03) is not a slot; the proxy does not need it
    assert_eq!(
        registry.classify(ConnectionState::Login, sb, v, 3),
        PacketKind::Unknown(3)
    );
    // the configuration state is deliberately not classified
    assert_eq!(
        registry.classify(ConnectionState::Configuration, sb, v, 0),
        PacketKind::Unknown(0)
    );
    assert_eq!(
        registry.classify(ConnectionState::Configuration, cb, v, 0),
        PacketKind::Unknown(0)
    );
}

#[test]
fn classify_1_20_5_1_21_1_and_26_3() {
    let registry = registry();
    for name in ["1.20.5", "1.20.6", "1.21", "1.21.1", "26.1", "26.2", "26.3"] {
        let v = registry.from_name(name).unwrap();
        let sb = Direction::Serverbound;
        let cb = Direction::Clientbound;
        assert_eq!(
            registry.classify(ConnectionState::Status, sb, v, 0),
            PacketKind::StatusRequest,
            "{name}"
        );
        assert_eq!(
            registry.classify(ConnectionState::Status, sb, v, 1),
            PacketKind::StatusPing,
            "{name}"
        );
        assert_eq!(
            registry.classify(ConnectionState::Status, cb, v, 0),
            PacketKind::StatusResponse,
            "{name}"
        );
        assert_eq!(
            registry.classify(ConnectionState::Status, cb, v, 1),
            PacketKind::StatusPong,
            "{name}"
        );
        assert_eq!(
            registry.classify(ConnectionState::Login, sb, v, 0),
            PacketKind::LoginStart,
            "{name}"
        );
        assert_eq!(
            registry.classify(ConnectionState::Login, sb, v, 1),
            PacketKind::EncryptionResponse,
            "{name}"
        );
        assert_eq!(
            registry.classify(ConnectionState::Login, sb, v, 2),
            PacketKind::LoginPluginResponse,
            "{name}"
        );
        assert_eq!(
            registry.classify(ConnectionState::Login, cb, v, 0),
            PacketKind::LoginDisconnect,
            "{name}"
        );
        assert_eq!(
            registry.classify(ConnectionState::Login, cb, v, 1),
            PacketKind::EncryptionRequest,
            "{name}"
        );
        assert_eq!(
            registry.classify(ConnectionState::Login, cb, v, 2),
            PacketKind::LoginSuccess,
            "{name}"
        );
        assert_eq!(
            registry.classify(ConnectionState::Login, cb, v, 3),
            PacketKind::SetCompression,
            "{name}"
        );
        assert_eq!(
            registry.classify(ConnectionState::Login, cb, v, 4),
            PacketKind::LoginPluginRequest,
            "{name}"
        );
        // cookie_request (clientbound 0x05) and cookie_response (serverbound 0x04) are not slots
        assert_eq!(
            registry.classify(ConnectionState::Login, cb, v, 5),
            PacketKind::Unknown(5),
            "{name}"
        );
        assert_eq!(
            registry.classify(ConnectionState::Login, sb, v, 4),
            PacketKind::Unknown(4),
            "{name}"
        );
        // a play-state id in the login classifier stays Unknown
        assert_eq!(
            registry.classify(ConnectionState::Play, sb, v, 0),
            PacketKind::Unknown(0),
            "{name}"
        );
    }
}

#[test]
fn handshake_serverbound_zero_is_handshake() {
    let registry = registry();
    for name in [
        "1.7.2", "1.12.2", "1.19.3", "1.20.2", "1.20.5", "1.21.1", "26.3",
    ] {
        let v = registry.from_name(name).unwrap();
        assert_eq!(
            registry.classify(ConnectionState::Handshaking, Direction::Serverbound, v, 0),
            PacketKind::Handshake,
            "{name}"
        );
        assert_eq!(
            registry.classify(ConnectionState::Handshaking, Direction::Serverbound, v, 1),
            PacketKind::Unknown(1),
            "{name}"
        );
        // no clientbound handshake exists
        assert_eq!(
            registry.classify(ConnectionState::Handshaking, Direction::Clientbound, v, 0),
            PacketKind::Unknown(0),
            "{name}"
        );
    }
}

#[test]
fn every_supported_release_classifies_its_core_login_set() {
    let registry = registry();
    for v in registry.versions() {
        let sb = Direction::Serverbound;
        let cb = Direction::Clientbound;
        assert_eq!(
            registry.classify(ConnectionState::Status, sb, v, 0),
            PacketKind::StatusRequest,
            "{}",
            v.name
        );
        assert_eq!(
            registry.classify(ConnectionState::Status, sb, v, 1),
            PacketKind::StatusPing,
            "{}",
            v.name
        );
        assert_eq!(
            registry.classify(ConnectionState::Status, cb, v, 0),
            PacketKind::StatusResponse,
            "{}",
            v.name
        );
        assert_eq!(
            registry.classify(ConnectionState::Status, cb, v, 1),
            PacketKind::StatusPong,
            "{}",
            v.name
        );
        assert_eq!(
            registry.classify(ConnectionState::Login, sb, v, 0),
            PacketKind::LoginStart,
            "{}",
            v.name
        );
        assert_eq!(
            registry.classify(ConnectionState::Login, sb, v, 1),
            PacketKind::EncryptionResponse,
            "{}",
            v.name
        );
        assert_eq!(
            registry.classify(ConnectionState::Login, cb, v, 0),
            PacketKind::LoginDisconnect,
            "{}",
            v.name
        );
        assert_eq!(
            registry.classify(ConnectionState::Login, cb, v, 1),
            PacketKind::EncryptionRequest,
            "{}",
            v.name
        );
        assert_eq!(
            registry.classify(ConnectionState::Login, cb, v, 2),
            PacketKind::LoginSuccess,
            "{}",
            v.name
        );
    }
}

#[test]
fn packet_kind_display() {
    assert_eq!(PacketKind::Handshake.to_string(), "handshake");
    assert_eq!(PacketKind::LoginStart.to_string(), "login_start");
    assert_eq!(PacketKind::Unknown(42).to_string(), "unknown(42)");
}

#[test]
fn registry_display_and_debug() {
    let registry = registry();
    let v = registry.from_name("1.7.2").unwrap();
    assert_eq!(v.to_string(), "1.7.2 (protocol 4)");
    // VersionInfo is Copy and Eq — cheap to stash alongside connections
    let copied = *v;
    assert_eq!(v, &copied);
}
