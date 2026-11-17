//! The `candyriya:main` channel identifier, exported like the other
//! messaging crates export theirs.

use candyriya_messaging::ChannelIdentifier;
use candyriya_messaging_candyriya::{
    CANDYRIYA_CHANNEL, CANDYRIYA_CHANNEL_NAME, CANDYRIYA_PROTOCOL_VERSION, Direction,
    MAX_PACKET_BYTES, is_candyriya_channel,
};

#[test]
fn channel_name_and_identifier_agree() {
    assert_eq!(CANDYRIYA_CHANNEL_NAME, "candyriya:main");
    assert_eq!(CANDYRIYA_CHANNEL.as_str(), CANDYRIYA_CHANNEL_NAME);
    assert!(is_candyriya_channel(&CANDYRIYA_CHANNEL));

    // validation rules apply to the exported static too
    let other = ChannelIdentifier::new("candyriya:other").unwrap();
    assert!(!is_candyriya_channel(&other));
}

#[test]
fn protocol_constants_have_expected_values() {
    assert_eq!(CANDYRIYA_PROTOCOL_VERSION, 1);
    assert_eq!(MAX_PACKET_BYTES, 32767);
}

#[test]
fn direction_display() {
    assert_eq!(Direction::Serverbound.as_str(), "serverbound");
    assert_eq!(Direction::Clientbound.as_str(), "clientbound");
    assert_eq!(format!("{}", Direction::Serverbound), "serverbound");
}
