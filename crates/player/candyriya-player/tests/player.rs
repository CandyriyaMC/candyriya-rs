//! Player session model: protocol-name resolution and lifecycle.
//! No network anywhere in this file (the registry loads its embedded snapshot).

use std::net::SocketAddr;

use candyriya_player::{ConnectionState, GameProfile, Player, offline_uuid};
use candyriya_protocol_registry::Registry;

fn client() -> SocketAddr {
    "192.0.2.10:51000".parse().expect("valid socket addr")
}

#[test]
fn new_player_starts_in_login_state() {
    let profile = GameProfile::new(offline_uuid("Notch"), "Notch");
    let player = Player::new(profile, client(), 4);
    assert_eq!(player.connection_state, ConnectionState::Login);
    assert_eq!(player.locale, None);
}

#[test]
fn protocol_name_resolves_from_registry_for_known_versions() {
    let registry = Registry::embedded().expect("embedded snapshot is valid");
    for version in [4, 340, 767] {
        let expected = registry
            .from_protocol(version)
            .expect("version in embedded snapshot")
            .name;
        let profile = GameProfile::new(offline_uuid("Notch"), "Notch");
        let player = Player::new(profile, client(), version);
        assert_eq!(player.protocol_name, expected, "protocol {version}");
        assert_eq!(player.protocol_version, version);
    }
}

#[test]
fn unknown_protocol_yields_documented_placeholder() {
    let profile = GameProfile::new(offline_uuid("Herobrine"), "Herobrine");
    let player = Player::new(profile, client(), 999_999);
    assert_eq!(
        player.protocol_name,
        candyriya_player::UNKNOWN_PROTOCOL_NAME
    );
    assert_eq!(player.protocol_version, 999_999);
}

#[test]
fn with_protocol_name_takes_explicit_value() {
    let profile = GameProfile::new(offline_uuid("jeb_"), "jeb_");
    let player = Player::with_protocol_name(profile, client(), 123, "custom-version");
    assert_eq!(player.protocol_name, "custom-version");
    assert_eq!(player.connection_state, ConnectionState::Login);
}

#[test]
fn connection_state_display_is_stable() {
    assert_eq!(ConnectionState::Login.to_string(), "login");
    assert_eq!(ConnectionState::Configuration.to_string(), "configuration");
    assert_eq!(ConnectionState::Play.to_string(), "play");
}
