//! Offline-mode UUID derivation. No network anywhere in this file.

use candyriya_player::offline_uuid;
use uuid::Uuid;

/// Worked example from the Minecraft Wiki: UUID ("OfflinePlayer:Notch",
/// as used on the "Java Edition protocol/Packets" page). The algorithm is
/// UUID version 3: MD5 of the UTF-8 string `OfflinePlayer:<name>`, with the
/// version nibble set to 3 and the variant bits set to RFC 4122.
#[test]
fn wiki_worked_example_notch() {
    assert_eq!(
        offline_uuid("Notch"),
        Uuid::parse_str("b50ad385-829d-3141-a216-7e7d7539ba7f").expect("valid literal"),
    );
}

/// Additional name-derived vectors, verified against an independent
/// MD5+variant implementation (not this crate's code path).
#[test]
fn additional_known_vectors() {
    let cases = [
        ("jeb_", "a762f560-4fce-3236-812a-b80efff0b62b"),
        ("Dinnerbone", "4d258a81-2358-3084-8166-05b9faccad80"),
    ];
    for (name, expected) in cases {
        assert_eq!(offline_uuid(name).to_string(), expected, "name {name:?}",);
    }
}

#[test]
fn version_nibble_is_three_and_rfc4122_variant() {
    for name in ["Notch", "jeb_", "Dinnerbone", "Herobrine", "a", "16"] {
        let bytes = *offline_uuid(name).as_bytes();
        assert_eq!(bytes[6] >> 4, 3, "version nibble for {name:?}");
        // RFC 4122 variant: two most significant bits are 10.
        assert_eq!(bytes[8] >> 6, 0b10, "variant bits for {name:?}");
    }
}

#[test]
fn derivation_is_deterministic() {
    assert_eq!(offline_uuid("Notch"), offline_uuid("Notch"));
}

#[test]
fn distinct_names_yield_distinct_uuids() {
    let a = offline_uuid("Notch");
    let b = offline_uuid("jeb_");
    assert_ne!(a, b);
}

/// Case sensitivity: Mojang names are case-insensitive on login but the
/// derivation is byte-exact over the exact spelling, so differing case
/// differs — this locks that contract in so nobody "fixes" it silently.
#[test]
fn derivation_is_case_sensitive() {
    assert_ne!(offline_uuid("Notch"), offline_uuid("notch"));
}
