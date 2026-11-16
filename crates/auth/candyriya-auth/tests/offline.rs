//! Offline-mode validator tests. No network, no sockets.

use candyriya_auth::{AuthError, OfflineSessionValidator, SessionValidator, validate_username};
use candyriya_player::offline_uuid;
use uuid::Uuid;

#[tokio::test]
async fn offline_derives_known_vectors_and_matches_player_crate() {
    let validator = OfflineSessionValidator;

    // The wiki worked example and the extra vectors from the player crate's
    // own test suite; the derivation must stay byte-identical to
    // `candyriya_player::offline_uuid`.
    let cases = [
        ("Notch", "b50ad385-829d-3141-a216-7e7d7539ba7f"),
        ("jeb_", "a762f560-4fce-3236-812a-b80efff0b62b"),
        ("Dinnerbone", "4d258a81-2358-3084-8166-05b9faccad80"),
    ];

    for (name, expected) in cases {
        let profile = validator
            .verify_client(name, "ignored-server-id")
            .await
            .unwrap();
        assert_eq!(
            profile.id,
            Uuid::parse_str(expected).unwrap(),
            "name {name:?}"
        );
        assert_eq!(
            profile.id,
            offline_uuid(name),
            "derivation mismatch for {name:?}"
        );
        assert_eq!(profile.name, name);
        assert!(
            profile.properties.is_empty(),
            "offline profiles carry no properties"
        );
    }
}

#[tokio::test]
async fn offline_preserves_case_verbatim() {
    let validator = OfflineSessionValidator;

    let lower = validator.verify_client("notch", "").await.unwrap();
    let upper = validator.verify_client("NOTCH", "").await.unwrap();
    let mixed = validator.verify_client("NoTcH", "").await.unwrap();

    assert_eq!(lower.name, "notch");
    assert_eq!(upper.name, "NOTCH");
    assert_eq!(mixed.name, "NoTcH");
    // the derivation is byte-exact over the spelling, so cases differ
    assert_ne!(lower.id, upper.id);
    assert_ne!(upper.id, mixed.id);
}

#[tokio::test]
async fn offline_ignores_server_id() {
    let validator = OfflineSessionValidator;

    let a = validator
        .verify_client("Notch", "-some-server-id")
        .await
        .unwrap();
    let b = validator.verify_client("Notch", "").await.unwrap();
    assert_eq!(a, b);
}

#[tokio::test]
async fn offline_rejects_invalid_usernames() {
    let validator = OfflineSessionValidator;

    for bad in [
        "",
        "ab",
        "name with spaces",
        "na-me",
        "thisnameisjustwaytoolong",
        "na\u{e4}me",
    ] {
        let err = validator.verify_client(bad, "").await.unwrap_err();
        assert!(
            matches!(err, AuthError::InvalidUsername(_)),
            "{bad:?} should be rejected, got: {err:?}"
        );
    }
}

#[tokio::test]
async fn validate_username_accepts_boundary_lengths() {
    assert!(validate_username("abc").is_ok());
    assert!(validate_username("0123456789abcdef").is_ok());
    assert!(validate_username("___").is_ok());
    assert!(validate_username("ab").is_err());
    assert!(validate_username("0123456789abcdefg").is_err());
}
