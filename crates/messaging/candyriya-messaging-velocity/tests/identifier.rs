//! ChannelIdentifier rejection paths at this crate's boundary.
//!
//! The identifier type itself is defined and unit-tested in
//! `candyriya-messaging`; here we pin the exact rejection behavior the
//! registrar relies on, so a future loosening of the identifier rules
//! cannot silently change what channels this crate accepts.

use candyriya_messaging::{ChannelIdentifier, ChannelIdentifierError};

#[test]
fn empty_identifier_is_rejected() {
    assert_eq!(
        ChannelIdentifier::new(""),
        Err(ChannelIdentifierError::Empty)
    );
}

#[test]
fn identifier_longer_than_64_bytes_is_rejected() {
    let long = "a".repeat(65);
    assert_eq!(
        ChannelIdentifier::new(&long),
        Err(ChannelIdentifierError::TooLong { len: 65, max: 64 }),
    );
    // boundary: exactly 64 bytes is fine
    assert!(ChannelIdentifier::new(&"a".repeat(64)).is_ok());
}

#[test]
fn multiple_colons_are_rejected() {
    assert_eq!(
        ChannelIdentifier::new("a:b:c"),
        Err(ChannelIdentifierError::MultipleColons { count: 2 }),
    );
    assert_eq!(
        ChannelIdentifier::new(":a:b"),
        Err(ChannelIdentifierError::MultipleColons { count: 2 }),
    );
}

#[test]
fn empty_segments_are_rejected() {
    assert_eq!(
        ChannelIdentifier::new(":key"),
        Err(ChannelIdentifierError::EmptyNamespace),
    );
    assert_eq!(
        ChannelIdentifier::new("namespace:"),
        Err(ChannelIdentifierError::EmptyKey),
    );
}

#[test]
fn modern_segments_reject_invalid_characters() {
    // uppercase is not allowed in the modern form
    assert!(matches!(
        ChannelIdentifier::new("Custom:main"),
        Err(ChannelIdentifierError::InvalidCharacter { .. }),
    ));
    assert!(matches!(
        ChannelIdentifier::new("custom:Main"),
        Err(ChannelIdentifierError::InvalidCharacter { .. }),
    ));
    // whitespace is not allowed anywhere
    assert!(matches!(
        ChannelIdentifier::new("custom:ma in"),
        Err(ChannelIdentifierError::InvalidCharacter { .. }),
    ));
    // '@' is not allowed
    assert!(matches!(
        ChannelIdentifier::new("custom:ma@in"),
        Err(ChannelIdentifierError::InvalidCharacter { .. }),
    ));
}

#[test]
fn legacy_names_allow_mixed_case_but_not_other_characters() {
    assert!(ChannelIdentifier::new("BungeeCord").is_ok());
    assert!(
        ChannelIdentifier::new("MC|Brand").is_err(),
        "'|' is rejected"
    );
    assert!(
        ChannelIdentifier::new("legacy name").is_err(),
        "space is rejected"
    );
}

#[test]
fn well_formed_identifiers_are_accepted() {
    for ok in [
        "bungeecord:main",
        "BungeeCord",
        "candyriya:main",
        "minecraft:register",
        "a:b",
        "namespace:a/b.c-d_e",
    ] {
        assert!(
            ChannelIdentifier::new(ok).is_ok(),
            "expected {ok:?} to be accepted",
        );
    }
}

#[test]
fn identifiers_are_ord_and_hash_consistent() {
    // the registrar keeps an ordered set; Ord must agree with Eq
    let a = ChannelIdentifier::new("custom:a").unwrap();
    let b = ChannelIdentifier::new("custom:b").unwrap();
    assert!(a < b);
    assert_eq!(a, ChannelIdentifier::new("custom:a").unwrap());
}
