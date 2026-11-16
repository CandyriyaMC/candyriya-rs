//! BungeeCord compatibility set: channel consts and the 18-subchannel
//! inventory, verified against the codec crate byte-for-byte.

use candyriya_messaging_bungeecord as bungee;
use candyriya_messaging_velocity::BungeeChannelSet;

#[test]
fn channel_name_consts_match_bungeecord_crate() {
    assert_eq!(
        BungeeChannelSet::MAIN_CHANNEL_NAME,
        bungee::BUNGEECORD_CHANNEL_NAME
    );
    assert_eq!(
        BungeeChannelSet::LEGACY_CHANNEL_NAME,
        bungee::BUNGEECORD_LEGACY_CHANNEL_NAME
    );
    assert_eq!(BungeeChannelSet::MAIN_CHANNEL_NAME, "bungeecord:main");
    assert_eq!(BungeeChannelSet::LEGACY_CHANNEL_NAME, "BungeeCord");
}

#[test]
fn compat_subchannels_are_exactly_the_bungeecord_inventory() {
    // identical values, identical order, element for element
    assert_eq!(
        BungeeChannelSet::COMPAT_SUBCHANNELS,
        bungee::SUBCHANNEL_NAMES,
        "the compat set must enumerate exactly the codec crate's subchannel inventory",
    );
}

#[test]
fn compat_subchannel_count_is_18() {
    assert_eq!(BungeeChannelSet::COMPAT_SUBCHANNEL_COUNT, 18);
    assert_eq!(BungeeChannelSet::COMPAT_SUBCHANNELS.len(), 18);
    assert_eq!(bungee::SUBCHANNEL_NAMES.len(), 18);
}

#[test]
fn compat_subchannels_contain_every_documented_subchannel() {
    // enumerate them explicitly so this test doubles as a spec checklist
    let expected = [
        "Connect",
        "ConnectOther",
        "IP",
        "IPOther",
        "PlayerCount",
        "PlayerList",
        "GetPlayerServer",
        "GetServers",
        "Message",
        "MessageRaw",
        "GetServer",
        "Forward",
        "ForwardToPlayer",
        "UUID",
        "UUIDOther",
        "ServerIP",
        "KickPlayer",
        "KickPlayerRaw",
    ];
    assert_eq!(BungeeChannelSet::COMPAT_SUBCHANNELS, expected);
    // no duplicates, all unique
    let mut seen = std::collections::HashSet::new();
    for subchannel in BungeeChannelSet::COMPAT_SUBCHANNELS {
        assert!(
            seen.insert(subchannel),
            "duplicate subchannel: {subchannel}"
        );
    }
}

#[test]
fn compat_channels_are_valid_identifiers_modern_first() {
    let channels = BungeeChannelSet::channels();
    assert_eq!(channels.len(), 2);
    assert_eq!(channels[0].as_str(), "bungeecord:main");
    assert_eq!(channels[1].as_str(), "BungeeCord");
    assert!(channels[0].is_modern());
    assert!(!channels[1].is_modern());
}

#[test]
fn compat_channels_match_bungeecord_static_identifiers() {
    let channels = BungeeChannelSet::channels();
    assert_eq!(&channels[0], &*bungee::BUNGEECORD_CHANNEL);
    assert_eq!(&channels[1], &*bungee::BUNGEECORD_LEGACY_CHANNEL);
}

#[test]
fn is_compat_channel_accepts_both_names_and_rejects_others() {
    let modern = candyriya_messaging::ChannelIdentifier::new("bungeecord:main").unwrap();
    let legacy = candyriya_messaging::ChannelIdentifier::new("BungeeCord").unwrap();
    let other = candyriya_messaging::ChannelIdentifier::new("candyriya:main").unwrap();
    let also_other = candyriya_messaging::ChannelIdentifier::new("bungeecord:other").unwrap();

    assert!(BungeeChannelSet::is_compat_channel(&modern));
    assert!(BungeeChannelSet::is_compat_channel(&legacy));
    assert!(!BungeeChannelSet::is_compat_channel(&other));
    assert!(!BungeeChannelSet::is_compat_channel(&also_other));
}

#[test]
fn re_exported_members_accept_the_same_inputs_as_the_bungeecord_crate() {
    // the compat set must not carry behavior that drifts from the codec crate
    let modern = candyriya_messaging::ChannelIdentifier::new("bungeecord:main").unwrap();
    let legacy = candyriya_messaging::ChannelIdentifier::new("BungeeCord").unwrap();
    assert_eq!(
        BungeeChannelSet::is_compat_channel(&modern),
        bungee::is_bungeecord_channel(&modern),
    );
    assert_eq!(
        BungeeChannelSet::is_compat_channel(&legacy),
        bungee::is_bungeecord_channel(&legacy),
    );
}
