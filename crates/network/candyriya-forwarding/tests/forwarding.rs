//! Golden vectors and adversarial cases for the forwarding codecs.
//!
//! The golden payload below was laid out byte-by-byte and authenticated with
//! an independent HMAC-SHA256 computation (Python's `hmac` + `hashlib`), not
//! with this crate's code: it pins the on-the-wire contract, bugs included
//! are not possible to self-confirm here.

use candyriya_forwarding::{
    FORWARDING_VERSION, ForwardingError, clean_ipv6_scope, create_forwarding_data,
    create_legacy_forwarding_address, parse_legacy_forwarding_address, verify_forwarding_data,
};
use candyriya_player::{GameProfile, Property};
use uuid::Uuid;

const GOLDEN_SECRET: &[u8] = b"candyriya-golden-forwarding-secret";
const GOLDEN_ADDRESS: &str = "203.0.113.7";
const GOLDEN_UUID: &str = "b50ad385-829d-3141-a216-7e7d7539ba7f";
const GOLDEN_NAME: &str = "Notch";
const GOLDEN_PROPERTY_VALUE: &str = "dGVzdC12YWx1ZQ==";
const GOLDEN_PROPERTY_SIGNATURE: &str = "c2lnbmF0dXJl";

fn golden_profile() -> GameProfile {
    GameProfile {
        id: Uuid::parse_str(GOLDEN_UUID).expect("golden uuid is valid"),
        name: GOLDEN_NAME.to_owned(),
        properties: vec![
            Property::signed("textures", GOLDEN_PROPERTY_VALUE, GOLDEN_PROPERTY_SIGNATURE),
            Property::unsigned("hello", "world"),
        ],
    }
}

// payload = everything after the 32-byte signature, built by hand.
//   VarInt(1) | String("203.0.113.7") | 16 uuid bytes | String("Notch") |
//   VarInt(2) | String("textures") | String("dGVzdC12YWx1ZQ==") | 0x01 | String("c2lnbmF0dXJl") |
//   String("hello") | String("world") | 0x00
const GOLDEN_PAYLOAD: &[u8] = &[
    0x01, 0x0b, 0x32, 0x30, 0x33, 0x2e, 0x30, 0x2e, 0x31, 0x31, 0x33, 0x2e, 0x37, 0xb5, 0x0a, 0xd3,
    0x85, 0x82, 0x9d, 0x31, 0x41, 0xa2, 0x16, 0x7e, 0x7d, 0x75, 0x39, 0xba, 0x7f, 0x05, 0x4e, 0x6f,
    0x74, 0x63, 0x68, 0x02, 0x08, 0x74, 0x65, 0x78, 0x74, 0x75, 0x72, 0x65, 0x73, 0x10, 0x64, 0x47,
    0x56, 0x7a, 0x64, 0x43, 0x31, 0x32, 0x59, 0x57, 0x78, 0x31, 0x5a, 0x51, 0x3d, 0x3d, 0x01, 0x0c,
    0x63, 0x32, 0x6c, 0x6e, 0x62, 0x6d, 0x46, 0x30, 0x64, 0x58, 0x4a, 0x6c, 0x05, 0x68, 0x65, 0x6c,
    0x6c, 0x6f, 0x05, 0x77, 0x6f, 0x72, 0x6c, 0x64, 0x00,
];

// HMAC-SHA256(GOLDEN_SECRET, GOLDEN_PAYLOAD), computed with Python's
// hmac/hashlib over the payload above.
const GOLDEN_SIGNATURE: &[u8] = &[
    0x08, 0x8a, 0x8b, 0x0f, 0xad, 0xeb, 0x8c, 0x57, 0x88, 0x7a, 0x9a, 0x74, 0xf6, 0xf6, 0xe4, 0xa1,
    0x8c, 0x34, 0xf0, 0x0f, 0xfa, 0x52, 0xed, 0xa9, 0xf1, 0x8b, 0xcd, 0x50, 0x86, 0x36, 0x4c, 0x07,
];

fn golden_sealed() -> Vec<u8> {
    let mut sealed = Vec::with_capacity(GOLDEN_SIGNATURE.len() + GOLDEN_PAYLOAD.len());
    sealed.extend_from_slice(GOLDEN_SIGNATURE);
    sealed.extend_from_slice(GOLDEN_PAYLOAD);
    sealed
}

#[test]
fn golden_payload_matches_python_hmac() {
    let created = create_forwarding_data(GOLDEN_SECRET, GOLDEN_ADDRESS, &golden_profile());
    assert_eq!(created, golden_sealed());

    // the payload part must be byte-identical to the hand-laid layout
    assert_eq!(&created[32..], GOLDEN_PAYLOAD);

    let (ip, profile) =
        verify_forwarding_data(GOLDEN_SECRET, &golden_sealed()).expect("golden payload verifies");
    assert_eq!(ip, GOLDEN_ADDRESS.as_bytes());
    assert_eq!(profile, golden_profile());
}

#[test]
fn golden_forwarding_version_is_one() {
    assert_eq!(FORWARDING_VERSION, 1);
}

#[test]
fn roundtrip_with_and_without_properties() {
    let profiles = vec![
        // no properties at all
        GameProfile::new(Uuid::parse_str(GOLDEN_UUID).unwrap(), "Notch"),
        // a single unsigned property
        GameProfile {
            id: Uuid::parse_str(GOLDEN_UUID).unwrap(),
            name: "Notch".to_owned(),
            properties: vec![Property::unsigned("hello", "world")],
        },
        // a single signed property
        GameProfile {
            id: Uuid::parse_str(GOLDEN_UUID).unwrap(),
            name: "Notch".to_owned(),
            properties: vec![Property::signed("textures", "dGVzdA==", "sig")],
        },
        // eight mixed properties
        GameProfile {
            id: Uuid::parse_str(GOLDEN_UUID).unwrap(),
            name: "Notch".to_owned(),
            properties: (0..8)
                .map(|i| {
                    if i % 2 == 0 {
                        Property::signed(format!("prop{i}"), format!("value{i}"), format!("sig{i}"))
                    } else {
                        Property::unsigned(format!("prop{i}"), format!("value{i}"))
                    }
                })
                .collect(),
        },
    ];

    for profile in profiles {
        let sealed = create_forwarding_data(GOLDEN_SECRET, "203.0.113.7", &profile);
        let (ip, decoded) = verify_forwarding_data(GOLDEN_SECRET, &sealed)
            .expect("create_forwarding_data output must verify");
        assert_eq!(ip, b"203.0.113.7");
        assert_eq!(decoded, profile);
    }
}

#[test]
fn tampering_with_any_byte_is_rejected() {
    let sealed = golden_sealed();
    for index in 0..sealed.len() {
        let mut tampered = sealed.clone();
        tampered[index] ^= 0x01;
        assert!(
            verify_forwarding_data(GOLDEN_SECRET, &tampered).is_err(),
            "flipping byte at index {index} must be rejected"
        );
    }
}

#[test]
fn wrong_secret_is_rejected() {
    let sealed = golden_sealed();
    let result = verify_forwarding_data(b"some-other-secret", &sealed);
    assert!(matches!(result, Err(ForwardingError::BadSignature)));
}

#[test]
fn every_truncation_is_rejected() {
    let sealed = golden_sealed();
    for len in 0..sealed.len() {
        let truncated = &sealed[..len];
        assert!(
            verify_forwarding_data(GOLDEN_SECRET, truncated).is_err(),
            "truncating to {len} bytes must be rejected"
        );
    }
}

#[test]
fn trailing_bytes_break_the_signature() {
    // the HMAC covers every byte after the signature, so a byte appended
    // to a sealed payload fails authentication before parsing even starts
    let mut sealed = create_forwarding_data(GOLDEN_SECRET, GOLDEN_ADDRESS, &golden_profile());
    sealed.push(0x00);
    let result = verify_forwarding_data(GOLDEN_SECRET, &sealed);
    assert!(matches!(result, Err(ForwardingError::BadSignature)));
}

#[test]
fn ipv6_scope_is_stripped_end_to_end() {
    let profile = golden_profile();
    let sealed = create_forwarding_data(GOLDEN_SECRET, "fe80::1%eth0", &profile);
    let (ip, decoded) = verify_forwarding_data(GOLDEN_SECRET, &sealed).unwrap();
    assert_eq!(ip, b"fe80::1");
    assert_eq!(decoded, profile);
}

#[test]
fn clean_ipv6_scope_behaviour() {
    assert_eq!(clean_ipv6_scope("fe80::abcd%wlan0"), "fe80::abcd");
    assert_eq!(clean_ipv6_scope("::1"), "::1");
    assert_eq!(clean_ipv6_scope("127.0.0.1"), "127.0.0.1");
}

// ---------------------------------------------------------------- legacy

#[test]
fn legacy_format_exact_string_unsigned_property() {
    let profile = GameProfile {
        id: Uuid::parse_str(GOLDEN_UUID).unwrap(),
        name: "Notch".to_owned(),
        properties: vec![Property::unsigned("textures", "dGVzdC12YWx1ZQ==")],
    };
    let address = create_legacy_forwarding_address("mc.example.com", "203.0.113.7", &profile, None);
    assert_eq!(
        address,
        "mc.example.com\x00203.0.113.7\x00b50ad385829d3141a2167e7d7539ba7f\x00\
         [{\"name\":\"textures\",\"value\":\"dGVzdC12YWx1ZQ==\"}]"
    );
}

#[test]
fn legacy_format_exact_string_signed_property() {
    let profile = GameProfile {
        id: Uuid::parse_str(GOLDEN_UUID).unwrap(),
        name: "Notch".to_owned(),
        properties: vec![Property::signed(
            "textures",
            "dGVzdC12YWx1ZQ==",
            "c2lnbmF0dXJl",
        )],
    };
    let address = create_legacy_forwarding_address("mc.example.com", "203.0.113.7", &profile, None);
    assert_eq!(
        address,
        "mc.example.com\x00203.0.113.7\x00b50ad385829d3141a2167e7d7539ba7f\x00\
         [{\"name\":\"textures\",\"value\":\"dGVzdC12YWx1ZQ==\",\"signature\":\"c2lnbmF0dXJl\"}]"
    );
}

#[test]
fn legacy_format_exact_string_empty_properties() {
    let profile = GameProfile::new(Uuid::parse_str(GOLDEN_UUID).unwrap(), "Notch");
    let address = create_legacy_forwarding_address("mc.example.com", "203.0.113.7", &profile, None);
    assert_eq!(
        address,
        "mc.example.com\x00203.0.113.7\x00b50ad385829d3141a2167e7d7539ba7f\x00[]"
    );
}

#[test]
fn legacy_roundtrip_with_bungeeguard_token() {
    let profile = GameProfile {
        id: Uuid::parse_str(GOLDEN_UUID).unwrap(),
        name: "Notch".to_owned(),
        properties: vec![Property::signed(
            "textures",
            "dGVzdC12YWx1ZQ==",
            "c2lnbmF0dXJl",
        )],
    };
    let address = create_legacy_forwarding_address(
        "mc.example.com",
        "203.0.113.7",
        &profile,
        Some(b"test-token"),
    );

    // token appended as the last property, after the existing ones,
    // exactly one token property, with no signature key
    assert_eq!(
        address,
        "mc.example.com\x00203.0.113.7\x00b50ad385829d3141a2167e7d7539ba7f\x00\
         [{\"name\":\"textures\",\"value\":\"dGVzdC12YWx1ZQ==\",\"signature\":\"c2lnbmF0dXJl\"},\
         {\"name\":\"bungeeguard-token\",\"value\":\"test-token\"}]"
    );

    let (server, ip, uuid, properties) =
        parse_legacy_forwarding_address(&address).expect("roundtrip parses");
    assert_eq!(server, "mc.example.com");
    assert_eq!(ip, "203.0.113.7");
    assert_eq!(uuid, Uuid::parse_str(GOLDEN_UUID).unwrap());
    assert_eq!(
        properties,
        vec![
            Property::signed("textures", "dGVzdC12YWx1ZQ==", "c2lnbmF0dXJl"),
            Property::unsigned("bungeeguard-token", "test-token"),
        ]
    );
}

#[test]
fn bungeeguard_token_is_appended_to_empty_profile_too() {
    let profile = GameProfile::new(Uuid::parse_str(GOLDEN_UUID).unwrap(), "Notch");
    let address = create_legacy_forwarding_address(
        "mc.example.com",
        "203.0.113.7",
        &profile,
        Some(b"token123"),
    );
    let (_, _, _, properties) = parse_legacy_forwarding_address(&address).unwrap();
    assert_eq!(
        properties,
        vec![Property::unsigned("bungeeguard-token", "token123")]
    );
}

#[test]
fn legacy_roundtrip_without_properties_json_part() {
    // BungeeCord omits the properties part entirely when the profile has none;
    // the parser must still accept that shape.
    let address = "mc.example.com\x00203.0.113.7\x00b50ad385829d3141a2167e7d7539ba7f";
    let (server, ip, uuid, properties) = parse_legacy_forwarding_address(address).unwrap();
    assert_eq!(server, "mc.example.com");
    assert_eq!(ip, "203.0.113.7");
    assert_eq!(uuid, Uuid::parse_str(GOLDEN_UUID).unwrap());
    assert!(properties.is_empty());
}

#[test]
fn legacy_roundtrip_various_property_shapes() {
    let cases: Vec<GameProfile> = vec![
        GameProfile::new(Uuid::parse_str(GOLDEN_UUID).unwrap(), "Notch"),
        GameProfile {
            id: Uuid::parse_str(GOLDEN_UUID).unwrap(),
            name: "Notch".to_owned(),
            properties: vec![
                Property::unsigned("a", "1"),
                Property::signed("b", "2", "sig"),
                Property::unsigned("c", "3"),
            ],
        },
        GameProfile {
            id: Uuid::parse_str(GOLDEN_UUID).unwrap(),
            name: "Notch".to_owned(),
            properties: (0..8)
                .map(|i| Property::unsigned(format!("prop{i}"), format!("value{i}")))
                .collect(),
        },
    ];

    for profile in cases {
        let address =
            create_legacy_forwarding_address("host.example", "198.51.100.3", &profile, None);
        let (server, ip, uuid, properties) = parse_legacy_forwarding_address(&address).unwrap();
        assert_eq!(server, "host.example");
        assert_eq!(ip, "198.51.100.3");
        assert_eq!(uuid, profile.id);
        assert_eq!(properties, profile.properties);
    }
}

#[test]
fn legacy_rejects_malformed_addresses() {
    // too few parts
    assert!(parse_legacy_forwarding_address("onlyone").is_err());
    assert!(parse_legacy_forwarding_address("a\x00b").is_err());
    // too many parts
    assert!(parse_legacy_forwarding_address("a\x00b\x00c\x00d\x00e\x00f").is_err());
    assert!(parse_legacy_forwarding_address("a\x00b\x00c\x00d\x00e").is_err());
    // bad uuid
    assert!(parse_legacy_forwarding_address("host\x00ip\x00not-a-uuid").is_err());
    assert!(
        parse_legacy_forwarding_address("host\x00203.0.113.7\x00ZZ50ad385829d3141a2167e7d7539ba7f")
            .is_err()
    );
    // bad properties JSON
    assert!(
        parse_legacy_forwarding_address(
            "host\x00203.0.113.7\x00b50ad385829d3141a2167e7d7539ba7f\x00nope"
        )
        .is_err()
    );
}

#[test]
fn legacy_uuid_is_undashed_lowercase_hex() {
    let profile = GameProfile::new(Uuid::parse_str(GOLDEN_UUID).unwrap(), "Notch");
    let address = create_legacy_forwarding_address("mc.example.com", "203.0.113.7", &profile, None);
    let uuid_part = address.split('\x00').nth(2).expect("third part exists");
    assert_eq!(uuid_part.len(), 32);
    assert!(
        uuid_part
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
    );
}
