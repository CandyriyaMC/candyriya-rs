//! Validation matrix for `ChannelIdentifier`.

use candyriya_messaging::{ChannelIdentifier, ChannelIdentifierError, ChannelSegment};

fn ok(s: &str) -> ChannelIdentifier {
    ChannelIdentifier::new(s).unwrap_or_else(|e| panic!("{s:?} should be valid: {e}"))
}

fn err(s: &str) -> ChannelIdentifierError {
    ChannelIdentifier::new(s).unwrap_err()
}

#[test]
fn legacy_names_are_valid() {
    let id = ok("BungeeCord");
    assert_eq!(id.as_str(), "BungeeCord");
    assert_eq!(id.parts(), None);
    assert!(!id.is_modern());

    ok("Bungee");
    ok("bunge_cord");
    ok("bungee-cord");
    ok("bungee.cord");
    ok("-");
    ok("a");
}

#[test]
fn modern_keys_are_valid() {
    let id = ok("bungeecord:main");
    assert_eq!(id.parts(), Some(("bungeecord", "main")));
    assert!(id.is_modern());

    ok("candyriya:main");
    ok("minecraft:register");
    ok("a:b");
    ok("ns:key/with/slashes");
    ok("na-me:ke_y.with/dots");
    ok("0:0");
}

#[test]
fn empty_is_rejected() {
    assert_eq!(err(""), ChannelIdentifierError::Empty);
}

#[test]
fn whitespace_and_control_chars_are_rejected() {
    assert_eq!(
        err("a b"),
        ChannelIdentifierError::InvalidCharacter {
            segment: ChannelSegment::Legacy,
            character: ' ',
            allowed: "ASCII letters, digits, '_', '.' and '-'",
        }
    );
    assert!(matches!(
        err("bungee\tcord"),
        ChannelIdentifierError::InvalidCharacter { .. }
    ));
    assert!(matches!(
        err("bungee\ncord"),
        ChannelIdentifierError::InvalidCharacter { .. }
    ));
    assert!(matches!(
        err("bungee\0cord"),
        ChannelIdentifierError::InvalidCharacter { .. }
    ));
}

#[test]
fn double_colons_are_rejected() {
    assert_eq!(
        err("a:b:c"),
        ChannelIdentifierError::MultipleColons { count: 2 }
    );
    assert_eq!(
        err("a::b"),
        ChannelIdentifierError::MultipleColons { count: 2 }
    );
    assert_eq!(
        err("::"),
        ChannelIdentifierError::MultipleColons { count: 2 }
    );
    assert_eq!(
        err("a:b:c:d"),
        ChannelIdentifierError::MultipleColons { count: 3 }
    );
}

#[test]
fn empty_segments_are_rejected() {
    assert_eq!(err(":main"), ChannelIdentifierError::EmptyNamespace);
    assert_eq!(err("bungeecord:"), ChannelIdentifierError::EmptyKey);
}

#[test]
fn modern_segments_are_lowercase_only() {
    assert_eq!(
        err("Bungee:main"),
        ChannelIdentifierError::InvalidCharacter {
            segment: ChannelSegment::Namespace,
            character: 'B',
            allowed: "a-z, 0-9, '_', '.', '/' and '-'",
        }
    );
    assert_eq!(
        err("bungee:Main"),
        ChannelIdentifierError::InvalidCharacter {
            segment: ChannelSegment::Key,
            character: 'M',
            allowed: "a-z, 0-9, '_', '.', '/' and '-'",
        }
    );
    assert!(matches!(
        err("bungee:mai n"),
        ChannelIdentifierError::InvalidCharacter {
            segment: ChannelSegment::Key,
            ..
        }
    ));
}

#[test]
fn modern_rejects_out_of_set_chars() {
    assert!(matches!(
        err("bungee:ma!n"),
        ChannelIdentifierError::InvalidCharacter {
            segment: ChannelSegment::Key,
            ..
        }
    ));
    assert!(matches!(
        err("bung ee:main"),
        ChannelIdentifierError::InvalidCharacter {
            segment: ChannelSegment::Namespace,
            ..
        }
    ));
    // non-ASCII anywhere in a modern key is out
    assert!(matches!(
        err("bünge:main"),
        ChannelIdentifierError::InvalidCharacter { .. }
    ));
    // '/' is a legal modern-key character but never a legacy one
    assert!(matches!(
        err("a/b"),
        ChannelIdentifierError::InvalidCharacter {
            segment: ChannelSegment::Legacy,
            ..
        }
    ));
    // legacy rejects ':' too (they cannot appear: they would split the string), so just double-check '+'
    assert!(matches!(
        err("a+b"),
        ChannelIdentifierError::InvalidCharacter {
            segment: ChannelSegment::Legacy,
            ..
        }
    ));
}

#[test]
fn length_cap_is_64_bytes() {
    let exactly = "a".repeat(64);
    ok(&exactly); // 64 bytes, fine

    let too_long = "a".repeat(65);
    assert_eq!(
        err(&too_long),
        ChannelIdentifierError::TooLong { len: 65, max: 64 }
    );

    let modern_ok = format!("{}:main", "a".repeat(64 - ":main".len()));
    ok(&modern_ok); // namespace fills exactly 64 bytes in total

    let modern_too_long = format!("{}:main", "a".repeat(64));
    assert!(matches!(
        err(&modern_too_long),
        ChannelIdentifierError::TooLong { .. }
    ));

    // 64 é's: 128 bytes, rejected by the byte-based length check
    let multibyte = "é".repeat(64);
    assert!(matches!(
        err(&multibyte),
        ChannelIdentifierError::TooLong { .. }
    ));
}

#[test]
fn from_str_matches_new() {
    let via_new = ChannelIdentifier::new("bungeecord:main").unwrap();
    let via_parse: ChannelIdentifier = "bungeecord:main".parse().unwrap();
    assert_eq!(via_new, via_parse);

    let bad: Result<ChannelIdentifier, _> = "a b".parse();
    assert!(bad.is_err());
}

#[test]
fn display_roundtrips() {
    for s in ["BungeeCord", "bungeecord:main", "candyriya:main"] {
        let id = ok(s);
        assert_eq!(id.to_string(), s);
        assert_eq!(id.as_ref(), s);
    }
}

#[test]
fn error_messages_are_actionable() {
    let e = err("a:b:c").to_string();
    assert!(
        e.contains("2 colons"),
        "message should count the colons: {e}"
    );

    let e = err("Bungee:main").to_string();
    assert!(
        e.contains('B'),
        "message should name the offending character: {e}"
    );
    assert!(
        e.contains("namespace"),
        "message should name the segment: {e}"
    );

    let e = err(&"a".repeat(65)).to_string();
    assert!(
        e.contains("65"),
        "message should state the measured length: {e}"
    );

    let e = ChannelIdentifier::new("").unwrap_err().to_string();
    assert!(
        e.contains("empty"),
        "message should say the identifier is empty: {e}"
    );
}
