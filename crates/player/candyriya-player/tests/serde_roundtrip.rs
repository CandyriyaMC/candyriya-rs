//! Serde roundtrips for the document types. No network anywhere in this file.

use candyriya_player::{GameProfile, OfflinePlayer, Property, offline_uuid};
use chrono::{DateTime, Utc};

fn sample_profile() -> GameProfile {
    GameProfile {
        id: offline_uuid("Notch"),
        name: "Notch".to_string(),
        properties: vec![
            Property::signed("textures", "dGV4dHVyZXM=", "c2lnbmF0dXJl"),
            Property::unsigned("legacy", "dHJ1ZQ=="),
        ],
    }
}

#[test]
fn game_profile_roundtrip_with_properties() {
    let profile = sample_profile();
    let json = serde_json::to_string(&profile).expect("profile serializes");
    let back: GameProfile = serde_json::from_str(&json).expect("profile deserializes");
    assert_eq!(profile, back);
}

#[test]
fn game_profile_roundtrip_without_properties() {
    let profile = GameProfile::new(offline_uuid("Herobrine"), "Herobrine");
    let json = serde_json::to_string(&profile).expect("profile serializes");
    let back: GameProfile = serde_json::from_str(&json).expect("profile deserializes");
    assert_eq!(profile, back);
}

#[test]
fn unsigned_property_omits_signature_key() {
    let profile = GameProfile {
        id: offline_uuid("jeb_"),
        name: "jeb_".to_string(),
        properties: vec![Property::unsigned("textures", "dmFsdWU=")],
    };
    let json = serde_json::to_string(&profile).expect("profile serializes");
    assert!(
        !json.contains("signature"),
        "unsigned property leaks signature key: {json}"
    );

    let back: GameProfile = serde_json::from_str(&json).expect("profile deserializes");
    assert_eq!(back.properties[0].signature, None);
    assert_eq!(profile, back);
}

#[test]
fn missing_signature_key_deserializes_as_none() {
    // Hand-written JSON without a signature key, as Mojang emits it.
    let json = r#"{"id":"b50ad385-829d-3141-a216-7e7d7539ba7f","name":"Notch","properties":[{"name":"textures","value":"dGV4dHVyZXM="}]}"#;
    let profile: GameProfile = serde_json::from_str(json).expect("profile deserializes");
    assert_eq!(profile.properties[0].signature, None);
}

#[test]
fn missing_properties_key_defaults_to_empty() {
    let json = r#"{"id":"b50ad385-829d-3141-a216-7e7d7539ba7f","name":"Notch"}"#;
    let profile: GameProfile = serde_json::from_str(json).expect("profile deserializes");
    assert!(profile.properties.is_empty());
}

#[test]
fn offline_player_roundtrip_iso_dates() {
    let first_seen = DateTime::from_timestamp(1_700_000_000, 0).expect("valid timestamp");
    let last_seen = DateTime::from_timestamp(1_700_100_000, 0).expect("valid timestamp");
    let player = OfflinePlayer {
        uuid: offline_uuid("Notch"),
        name: "Notch".to_string(),
        properties: vec![Property::signed("textures", "dGV4dHVyZXM=", "c2lnbmF0dXJl")],
        first_seen,
        last_seen,
    };

    let json = serde_json::to_string(&player).expect("offline player serializes");

    // chrono's default serde writes RFC 3339 / ISO 8601 UTC instants.
    let value: serde_json::Value = serde_json::from_str(&json).expect("json parses");
    for (key, expected) in [("first_seen", first_seen), ("last_seen", last_seen)] {
        let raw = value[key].as_str().expect("timestamp is a string");
        let parsed = DateTime::parse_from_rfc3339(raw).expect("timestamp parses as RFC 3339");
        assert_eq!(parsed.with_timezone(&Utc), expected, "{key} roundtrips");
    }

    let back: OfflinePlayer = serde_json::from_str(&json).expect("offline player deserializes");
    assert_eq!(player, back);
}

#[test]
fn offline_player_serialized_dates_are_iso_8601() {
    let at = DateTime::from_timestamp(1_700_000_000, 0).expect("valid timestamp");
    let player = OfflinePlayer {
        uuid: offline_uuid("jeb_"),
        name: "jeb_".to_string(),
        properties: Vec::new(),
        first_seen: at,
        last_seen: at,
    };
    let value: serde_json::Value = serde_json::to_value(&player).expect("serializes");
    for key in ["first_seen", "last_seen"] {
        let raw = value[key].as_str().expect("timestamp is a string");
        assert!(
            raw.contains('T'),
            "{key} lacks the ISO 8601 date/time separator: {raw}"
        );
        assert!(
            raw.ends_with('Z'),
            "{key} is not a UTC ('Z') instant: {raw}"
        );
    }
}
