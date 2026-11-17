//! Integration tests for the visual packet field types: title, boss
//! bar and player info (tab list) packet bodies.
//!
//! No network, no runtime, no I/O: everything here is pure construction,
//! serialization and codec work. The golden byte strings are pinned
//! against the protocol spec as documented on the wiki (2026-11-17) and
//! against the `candyriya-protocol` VarInt/UUID/String primitives.

use candyriya_plugin_api::{
    AddPlayerData, BossBarAction, BossBarColor, BossBarDivision, BossBarFlag, BossBarFlags,
    BossBarPacket, GameProfileProperty, HoverEvent, PlayerInfoEntry, PlayerInfoRemove,
    PlayerInfoUpdate, TitleKind, TitlePacket, TitleTimes, VisualPacketError, text,
};
use lobsterchat::component::Component;
use uuid::Uuid;

fn uuid(bytes: [u8; 16]) -> Uuid {
    Uuid::from_bytes(bytes)
}

fn notch() -> Uuid {
    uuid([
        0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e,
        0x0f,
    ])
}

fn sample_entry(uuid: Uuid) -> PlayerInfoEntry {
    let mut entry = PlayerInfoEntry::new(uuid);
    entry.add = Some(AddPlayerData {
        name: "Notch".to_string(),
        properties: vec![GameProfileProperty::signed("textures", "dGV4", "c2ln")],
    });
    entry.update_game_mode = Some(1);
    entry.update_listed = Some(true);
    entry.update_latency = Some(42);
    entry.update_display_name = Some(Some(text("Notch")));
    entry.update_list_priority = Some(10);
    entry.update_hat = Some(true);
    entry
}

// ---------- component serialization ----------

#[test]
fn bossbar_title_with_color_and_hover_serializes_and_roundtrips() {
    let title = candyriya_plugin_api::ComponentBuilder::text("Kill them all")
        .color(candyriya_plugin_api::NamedColor::Gold)
        .hover_event(HoverEvent::show_text(Component::text("hover hint")))
        .build();
    let mut flags = BossBarFlags::DARKEN_SKY;
    flags.insert(BossBarFlag::PlayBossMusic);
    let packet = BossBarPacket {
        uuid: notch(),
        action: BossBarAction::Add {
            title,
            health: 0.75,
            color: BossBarColor::Purple,
            division: BossBarDivision::TenNotches,
            flags,
        },
    };

    // component JSON roundtrip through the ABI and back
    let json = serde_json::to_string(&packet).expect("boss bar packet must serialize");
    let parsed: BossBarPacket =
        serde_json::from_str(&json).expect("boss bar packet must deserialize");
    assert_eq!(parsed, packet);

    // and byte roundtrip through the codec
    let bytes = packet.to_vec().expect("boss bar packet must encode");
    let decoded = BossBarPacket::from_slice(&bytes).expect("boss bar packet must decode");
    assert_eq!(decoded, packet);
}

// ---------- title ----------

#[test]
fn title_times_math_fields() {
    let times = TitleTimes::from_millis(500, 3500, 1000);
    assert_eq!(times.fade_in_ticks, 10);
    assert_eq!(times.stay_ticks, 70);
    assert_eq!(times.fade_out_ticks, 20);
    assert_eq!(times.to_millis(), (500, 3500, 1000));
    assert_eq!(times.total_ticks(), 100);
    assert_eq!(times.total_millis(), 5000);

    // rounding to the nearest tick
    let rounded = TitleTimes::from_millis(26, 28, 75);
    assert_eq!(
        (
            rounded.fade_in_ticks,
            rounded.stay_ticks,
            rounded.fade_out_ticks
        ),
        (1, 1, 2)
    );

    let default = TitleTimes::default();
    assert_eq!(
        (
            default.fade_in_ticks,
            default.stay_ticks,
            default.fade_out_ticks
        ),
        (10, 70, 20)
    );

    // serde roundtrip
    let json = serde_json::to_string(&times).expect("times must serialize");
    let back: TitleTimes = serde_json::from_str(&json).expect("times must deserialize");
    assert_eq!(back, times);
}

#[test]
fn title_times_encode_is_three_big_endian_ints() {
    let packet = TitlePacket::times(TitleTimes::new(10, 70, 20));
    let bytes = packet.to_vec().expect("times packet must encode");
    assert_eq!(
        bytes,
        [
            0x00, 0x00, 0x00, 0x0A, 0x00, 0x00, 0x00, 0x46, 0x00, 0x00, 0x00, 0x14
        ],
        "fade in / stay / fade out, each one big-endian Int"
    );

    let decoded =
        TitlePacket::from_slice(TitleKind::TitleTimes, &bytes).expect("times must decode");
    assert_eq!(decoded, packet);

    // negative i32s take the full four bytes
    let negative = TitlePacket::times(TitleTimes::new(-1, 0, -0x8000_0000));
    let bytes = negative.to_vec().expect("negative times must encode");
    assert_eq!(
        bytes,
        [
            0xFF, 0xFF, 0xFF, 0xFF, 0x00, 0x00, 0x00, 0x00, 0x80, 0x00, 0x00, 0x00
        ]
    );
}

#[test]
fn golden_bytes_simple_title() {
    // pins the whole pipeline: component -> JSON -> length-prefixed string
    let packet = TitlePacket::title(text("Hi"));
    let bytes = packet.to_vec().expect("title packet must encode");
    assert_eq!(
        bytes,
        [
            0x0D, b'{', b'"', b't', b'e', b'x', b't', b'"', b':', b'"', b'H', b'i', b'"', b'}'
        ],
        "varint(13) || {{\"text\":\"Hi\"}}"
    );

    // deserialization equals reserialized bytes
    let decoded = TitlePacket::from_slice(TitleKind::Title, &bytes).expect("title must decode");
    let reencoded = decoded.to_vec().expect("decoded title must re-encode");
    assert_eq!(reencoded, bytes);

    // and the decoded value equals the original packet
    assert_eq!(decoded, packet);
}

#[test]
fn title_clear_and_reset_roundtrip() {
    for (packet, byte) in [
        (TitlePacket::clear(), 0x00u8),
        (TitlePacket::reset(), 0x01u8),
    ] {
        let bytes = packet.to_vec().expect("clear/reset must encode");
        assert_eq!(bytes, [byte]);
        let decoded =
            TitlePacket::from_slice(packet.kind, &bytes).expect("clear/reset must decode");
        assert_eq!(decoded, packet);
    }

    // a byte that doesn't match the requested kind is rejected
    let err = TitlePacket::from_slice(TitleKind::Reset, &[0x00]).expect_err("mismatch must fail");
    assert!(matches!(err, VisualPacketError::ResetFlagMismatch { .. }));

    // an invalid boolean byte is rejected
    let err =
        TitlePacket::from_slice(TitleKind::Clear, &[0x07]).expect_err("invalid bool must fail");
    assert!(matches!(err, VisualPacketError::InvalidBoolean(0x07)));
}

#[test]
fn title_field_rules_enforced() {
    // title without text is rejected on encode and on validate
    let bare = TitlePacket {
        kind: TitleKind::Title,
        text: None,
        times: None,
    };
    assert!(matches!(
        bare.validate(),
        Err(VisualPacketError::MissingText { .. })
    ));
    assert!(bare.to_vec().is_err());

    // times attached to a non-times kind is rejected
    let weird = TitlePacket {
        kind: TitleKind::Subtitle,
        text: Some(text("x")),
        times: Some(TitleTimes::default()),
    };
    assert!(matches!(
        weird.validate(),
        Err(VisualPacketError::UnexpectedTimes { .. })
    ));

    // text attached to a times kind is rejected
    let weird = TitlePacket {
        kind: TitleKind::TitleTimes,
        text: Some(text("x")),
        times: Some(TitleTimes::default()),
    };
    assert!(matches!(
        weird.validate(),
        Err(VisualPacketError::UnexpectedText { .. })
    ));

    // clear/reset with payloads is rejected
    let weird = TitlePacket {
        kind: TitleKind::Clear,
        text: Some(text("x")),
        times: None,
    };
    assert!(matches!(
        weird.validate(),
        Err(VisualPacketError::UnexpectedText { .. })
    ));

    // decode rejects trailing garbage
    let err =
        TitlePacket::from_slice(TitleKind::TitleTimes, &[0; 13]).expect_err("trailing must fail");
    assert!(matches!(err, VisualPacketError::TrailingBytes(1)));
}

// ---------- boss bar ----------

#[test]
fn bossbar_flag_serde_names_match_games() {
    // names pinned against Adventure/Paper (darken_sky, play_boss_music, create_fog)
    let cases = [
        (BossBarFlag::DarkenSky, "darken_sky", 0x01u8),
        (BossBarFlag::PlayBossMusic, "play_boss_music", 0x02u8),
        (BossBarFlag::CreateFog, "create_fog", 0x04u8),
    ];
    for (flag, name, bit) in cases {
        assert_eq!(flag.bit(), bit);
        assert_eq!(flag.as_str(), name);
        let json = serde_json::to_string(&flag).expect("flag must serialize");
        assert_eq!(json, format!("\"{name}\""));
        let back: BossBarFlag = serde_json::from_str(&json).expect("flag must deserialize");
        assert_eq!(back, flag);
        assert_eq!(BossBarFlag::from_bit(bit), Some(flag));
    }
    assert_eq!(BossBarFlag::from_bit(0x08), None);
}

#[test]
fn bossbar_flags_bit_mask_behavior() {
    let mut flags = BossBarFlags::NONE;
    assert!(flags.is_empty());
    flags.insert(BossBarFlag::DarkenSky);
    flags.insert(BossBarFlag::CreateFog);
    assert_eq!(flags.bits(), 0x05);
    assert!(flags.contains(BossBarFlag::DarkenSky));
    assert!(!flags.contains(BossBarFlag::PlayBossMusic));
    assert_eq!(flags.to_string(), "darken_sky+create_fog");
    let known: Vec<_> = flags.flags().collect();
    assert_eq!(known, [BossBarFlag::DarkenSky, BossBarFlag::CreateFog]);

    flags.remove(BossBarFlag::DarkenSky);
    assert_eq!(flags.bits(), 0x04);

    // unknown bits survive the roundtrip but yield no known flags
    let raw = BossBarFlags::from_bits(0xE0);
    assert_eq!(raw.bits(), 0xE0);
    assert_eq!(raw.flags().count(), 0);
    assert_eq!(raw.to_string(), "0xE0");

    // serde is transparent: the raw byte travels as-is
    let json = serde_json::to_string(&BossBarFlags::CREATE_FOG).expect("flags must serialize");
    assert_eq!(json, "4");
    let back: BossBarFlags = serde_json::from_str("4").expect("flags must deserialize");
    assert_eq!(back, BossBarFlags::CREATE_FOG);
}

#[test]
fn bossbar_add_health_rules() {
    let base = |health| BossBarPacket {
        uuid: notch(),
        action: BossBarAction::Add {
            title: text("boss"),
            health,
            color: BossBarColor::Red,
            division: BossBarDivision::NoDivision,
            flags: BossBarFlags::NONE,
        },
    };
    // 0.0 and 1.0 are legal, 0.5 too
    for health in [0.0f32, 0.5, 1.0] {
        assert!(base(health).to_vec().is_ok(), "health {health} must encode");
    }
    // out of range or non-finite is rejected
    for health in [1.5f32, -0.5, f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        let err = base(health).to_vec().expect_err("health must be rejected");
        assert!(matches!(err, VisualPacketError::HealthOutOfRange(_)));
    }
}

#[test]
fn bossbar_wire_roundtrip_and_golden() {
    // golden: uuid + discriminant 0x01 (remove) and nothing else
    let packet = BossBarPacket {
        uuid: notch(),
        action: BossBarAction::Remove,
    };
    let bytes = packet.to_vec().expect("remove must encode");
    let mut golden = notch().into_bytes().to_vec();
    golden.push(0x01);
    assert_eq!(bytes, golden);
    assert_eq!(bytes.len(), 17);

    let decoded = BossBarPacket::from_slice(&bytes).expect("remove must decode");
    assert_eq!(decoded, packet);

    // golden: update_health layout = uuid || varint(2) || float BE
    let health_packet = BossBarPacket {
        uuid: notch(),
        action: BossBarAction::UpdateHealth { health: 0.5 },
    };
    let bytes = health_packet.to_vec().expect("health must encode");
    let mut golden = notch().into_bytes().to_vec();
    golden.push(0x02);
    golden.extend_from_slice(&0.5f32.to_be_bytes());
    assert_eq!(bytes, golden);
    let decoded = BossBarPacket::from_slice(&bytes).expect("health must decode");
    assert_eq!(decoded, health_packet);

    // every action discriminant roundtrips
    let full = BossBarAction::Add {
        title: text("raid"),
        health: 0.25,
        color: BossBarColor::Blue,
        division: BossBarDivision::TwentyNotches,
        flags: BossBarFlags::DARKEN_SKY,
    };
    for action in [
        full,
        BossBarAction::Remove,
        BossBarAction::UpdateHealth { health: 0.25 },
        BossBarAction::UpdateTitle {
            title: text("raid"),
        },
        BossBarAction::UpdateStyle {
            color: BossBarColor::Green,
            division: BossBarDivision::SixNotches,
        },
        BossBarAction::UpdateFlags {
            flags: BossBarFlags::PLAY_BOSS_MUSIC,
        },
    ] {
        let packet = BossBarPacket {
            uuid: notch(),
            action,
        };
        let bytes = packet.to_vec().expect("action must encode");
        let decoded = BossBarPacket::from_slice(&bytes).expect("action must decode");
        assert_eq!(decoded, packet);
    }
}

#[test]
fn bossbar_decode_rejects_bad_discriminants_and_health() {
    let mut buf = notch().into_bytes().to_vec();
    buf.push(0x06); // unknown action
    let err = BossBarPacket::from_slice(&buf).expect_err("unknown action must fail");
    assert!(matches!(
        err,
        VisualPacketError::UnknownDiscriminant {
            what: "boss bar action",
            found: 6
        }
    ));

    // unknown color id inside the style action
    let mut buf = notch().into_bytes().to_vec();
    buf.extend_from_slice(&[0x04, 0x07, 0x00]); // update_style, color=7, division=0
    let err = BossBarPacket::from_slice(&buf).expect_err("unknown color must fail");
    assert!(matches!(
        err,
        VisualPacketError::UnknownDiscriminant {
            what: "boss bar color",
            found: 7
        }
    ));

    // NaN health on the wire is rejected
    let mut buf = notch().into_bytes().to_vec();
    buf.push(0x02);
    buf.extend_from_slice(&f32::NAN.to_be_bytes());
    let err = BossBarPacket::from_slice(&buf).expect_err("NaN health must fail");
    assert!(matches!(err, VisualPacketError::HealthNotFinite));

    // finite out-of-range health passes through on decode (documented)
    let mut buf = notch().into_bytes().to_vec();
    buf.push(0x02);
    buf.extend_from_slice(&2.5f32.to_be_bytes());
    let packet = BossBarPacket::from_slice(&buf).expect("finite health must decode");
    match packet.action {
        BossBarAction::UpdateHealth { health } => assert_eq!(health, 2.5),
        other => panic!("expected UpdateHealth, got {other:?}"),
    }

    // trailing bytes are rejected
    let mut buf = notch().into_bytes().to_vec();
    buf.extend_from_slice(&[0x01, 0x00]); // remove + one stray byte
    let err = BossBarPacket::from_slice(&buf).expect_err("trailing must fail");
    assert!(matches!(err, VisualPacketError::TrailingBytes(1)));
}

// ---------- tab list ----------

#[test]
fn tablist_add_full_packet_byte_layout() {
    let entry = sample_entry(notch());
    let mask = entry.action_mask().bits();
    assert_eq!(mask, 0x01 | 0x04 | 0x08 | 0x10 | 0x20 | 0x40 | 0x80);

    let update = PlayerInfoUpdate::from_entries(vec![entry]).expect("entry must build an update");
    let bytes = update.to_vec().expect("update must encode");

    // actions byte first, then the varint player count, then 16 uuid bytes
    assert_eq!(bytes[0], mask);
    assert_eq!(bytes[1], 0x01);
    assert_eq!(&bytes[2..18], &notch().into_bytes());

    // then the per-player payloads in fixed mask-bit order.
    let payload = &bytes[18..];
    // 0x01 add: varint name len + name ("Notch"), properties varint(1),
    // property name "textures", value "dGV4", signature bool + string
    let mut pos = 0;
    let name_len = payload[pos] as usize;
    pos += 1;
    assert_eq!(&payload[pos..pos + name_len], b"Notch");
    pos += name_len;
    assert_eq!(payload[pos], 0x01, "one property");
    pos += 1;
    let prop_name_len = payload[pos] as usize;
    pos += 1;
    assert_eq!(&payload[pos..pos + prop_name_len], b"textures");
    pos += prop_name_len;
    let value_len = payload[pos] as usize;
    pos += 1;
    assert_eq!(&payload[pos..pos + value_len], b"dGV4");
    pos += value_len;
    assert_eq!(payload[pos], 0x01, "signature present");
    pos += 1;
    let sig_len = payload[pos] as usize;
    pos += 1;
    assert_eq!(&payload[pos..pos + sig_len], b"c2ln");
    pos += sig_len;
    // 0x04 update game mode: varint(1)
    assert_eq!(payload[pos], 0x01);
    pos += 1;
    // 0x08 update listed: bool(1)
    assert_eq!(payload[pos], 0x01);
    pos += 1;
    // 0x10 update latency: varint(42)
    assert_eq!(payload[pos], 42);
    pos += 1;
    // 0x20 update display name: bool(1) + component string
    assert_eq!(payload[pos], 0x01);
    pos += 1;
    let comp_len = payload[pos] as usize;
    pos += 1;
    let comp_json =
        std::str::from_utf8(&payload[pos..pos + comp_len]).expect("component JSON is UTF-8");
    assert!(comp_json.contains("\"text\":\"Notch\""));
    pos += comp_len;
    // 0x40 update list priority: varint(10)
    assert_eq!(payload[pos], 10);
    pos += 1;
    // 0x80 update hat: bool(1)
    assert_eq!(payload[pos], 0x01);
    pos += 1;
    assert_eq!(pos, payload.len(), "every payload byte accounted for");
}

#[test]
fn tablist_roundtrip_mixed_entries() {
    // one add-only entry, one latency+hat entry: masks must agree, so
    // both carry the union... this specific pair is expected to fail
    let add_only = {
        let mut entry = PlayerInfoEntry::new(notch());
        entry.add = Some(AddPlayerData {
            name: "Notch".to_string(),
            properties: vec![],
        });
        entry
    };
    let hat_only = {
        let mut entry = PlayerInfoEntry::new(uuid([0xAA; 16]));
        entry.update_hat = Some(true);
        entry
    };
    let mismatch = PlayerInfoUpdate::from_entries(vec![add_only.clone(), hat_only.clone()])
        .expect_err("disagreeing masks must fail");
    assert!(matches!(
        mismatch,
        VisualPacketError::MaskMismatch(0x80, 0x01)
    ));

    // uniform masks across two players encode and roundtrip cleanly
    let latency_a = {
        let mut entry = PlayerInfoEntry::new(notch());
        entry.update_latency = Some(1);
        entry
    };
    let latency_b = {
        let mut entry = PlayerInfoEntry::new(uuid([0xBB; 16]));
        entry.update_latency = Some(2);
        entry
    };
    let update =
        PlayerInfoUpdate::from_entries(vec![latency_a, latency_b]).expect("uniform mask ok");
    let bytes = update.to_vec().expect("update must encode");
    assert_eq!(bytes[0], 0x10);
    assert_eq!(bytes[1], 0x02); // two players
    let decoded = PlayerInfoUpdate::from_slice(&bytes).expect("update must decode");
    assert_eq!(decoded, update);
}

#[test]
fn tablist_empty_actions_rejected() {
    // entries that imply no actions at all are rejected
    let empty = PlayerInfoEntry::new(notch());
    let err = PlayerInfoUpdate::from_entries(vec![empty]).expect_err("empty mask must fail");
    assert!(matches!(err, VisualPacketError::EmptyActions));

    // and so is an empty entries vec
    let err = PlayerInfoUpdate::from_entries(vec![]).expect_err("empty entries must fail");
    assert!(matches!(err, VisualPacketError::EmptyCollection { .. }));

    // add with an empty name is rejected
    let mut entry = PlayerInfoEntry::new(notch());
    entry.add = Some(AddPlayerData {
        name: String::new(),
        properties: vec![],
    });
    let update = PlayerInfoUpdate {
        entries: vec![entry],
    };
    let err = update.to_vec().expect_err("empty name must fail");
    assert!(matches!(err, VisualPacketError::EmptyCollection { .. }));
}

#[test]
fn tablist_decode_rejects_garbage() {
    // actions byte with the Initialize Chat bit -> unsupported
    let bytes = [0x02u8, 0x00];
    let err = PlayerInfoUpdate::from_slice(&bytes).expect_err("initialize chat must be rejected");
    assert!(matches!(err, VisualPacketError::UnsupportedInitializeChat));

    // zero actions byte -> rejected
    let bytes = [0x00u8, 0x00];
    let err = PlayerInfoUpdate::from_slice(&bytes).expect_err("zero mask must be rejected");
    assert!(matches!(err, VisualPacketError::EmptyActions));

    // negative player count -> rejected
    let bytes = [0x10u8, 0xFF, 0xFF, 0xFF, 0xFF, 0x0F];
    let err = PlayerInfoUpdate::from_slice(&bytes).expect_err("negative count must be rejected");
    assert!(matches!(err, VisualPacketError::InvalidArrayCount { .. }));

    // all known mask bits roundtrip
    let mut entry = sample_entry(notch());
    entry.update_game_mode = Some(3);
    let update = PlayerInfoUpdate::from_entries(vec![entry]).expect("full mask ok");
    let bytes = update.to_vec().expect("update must encode");
    assert_eq!(bytes[0], 0xFD); // every action bit except Initialize Chat (0x02)
    let decoded = PlayerInfoUpdate::from_slice(&bytes).expect("full mask must decode");
    assert_eq!(decoded, update);

    // trailing bytes are rejected
    let mut bytes = update.to_vec().expect("update must encode");
    bytes.push(0x00);
    let err = PlayerInfoUpdate::from_slice(&bytes).expect_err("trailing must fail");
    assert!(matches!(err, VisualPacketError::TrailingBytes(1)));
}

#[test]
fn tablist_game_mode_range() {
    for mode in [0, 1, 2, 3] {
        let mut entry = PlayerInfoEntry::new(notch());
        entry.update_game_mode = Some(mode);
        let update = PlayerInfoUpdate::from_entries(vec![entry]).expect("valid mode");
        assert!(update.to_vec().is_ok());
    }
    for mode in [-1, 4, 7] {
        let mut entry = PlayerInfoEntry::new(notch());
        entry.update_game_mode = Some(mode);
        let update = PlayerInfoUpdate {
            entries: vec![entry],
        };
        let err = update.to_vec().expect_err("out-of-range mode must fail");
        assert!(matches!(err, VisualPacketError::InvalidGameMode(m) if m == mode));
    }
}

#[test]
fn player_info_remove_roundtrip_and_validation() {
    let remove = PlayerInfoRemove::new(vec![notch(), uuid([0xFF; 16])]).expect("non-empty list ok");
    let bytes = remove.to_vec().expect("remove must encode");
    assert_eq!(bytes[0], 0x02, "two uuids");
    assert_eq!(bytes.len(), 1 + 32);

    // golden bytes for a single known uuid
    let single = PlayerInfoRemove::new(vec![notch()]).expect("single uuid ok");
    let mut golden = vec![0x01u8];
    golden.extend_from_slice(&notch().into_bytes());
    assert_eq!(single.to_vec().expect("remove must encode"), golden);

    let decoded = PlayerInfoRemove::from_slice(&bytes).expect("remove must decode");
    assert_eq!(decoded, remove);

    // empty list is rejected on construction and on decode
    assert!(matches!(
        PlayerInfoRemove::new(vec![]).expect_err("empty must fail"),
        VisualPacketError::EmptyCollection { .. }
    ));
    assert!(matches!(
        PlayerInfoRemove::from_slice(&[0x00]).expect_err("empty must fail"),
        VisualPacketError::EmptyCollection { .. }
    ));

    // trailing bytes are rejected
    let mut bytes = bytes;
    bytes.push(0x00);
    assert!(matches!(
        PlayerInfoRemove::from_slice(&bytes).expect_err("trailing must fail"),
        VisualPacketError::TrailingBytes(1)
    ));
}

#[test]
fn entry_serde_omits_absent_actions() {
    let entry = PlayerInfoEntry::new(notch());
    let json = serde_json::to_string(&entry).expect("entry must serialize");
    assert_eq!(
        json,
        format!("{{\"uuid\":\"{notch}\"}}", notch = notch().hyphenated())
    );

    // display name double-option is distinguishable through the ABI
    let mut entry = PlayerInfoEntry::new(notch());
    entry.update_display_name = Some(None);
    let json = serde_json::to_string(&entry).expect("entry must serialize");
    assert!(json.contains("\"update_display_name\":null"));
    entry.update_display_name = Some(Some(text("n")));
    let json = serde_json::to_string(&entry).expect("entry must serialize");
    assert!(json.contains("\"text\":\"n\""));
    let back: PlayerInfoEntry = serde_json::from_str(&json).expect("entry must deserialize");
    assert_eq!(back, entry);
}

// ---------- robustness ----------

#[test]
fn decode_never_panics_on_garbage() {
    // deterministic xorshift-ish generator, no rand dependency needed
    let mut state = 0x243F_6A88_85A3_08D3u64;
    let mut next = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    for _ in 0..2000 {
        let len = (next() % 96) as usize;
        let bytes: Vec<u8> = (0..len).map(|_| (next() & 0xFF) as u8).collect();
        // every decoder must either error or return a valid value; never panic
        let _ = TitlePacket::from_slice(TitleKind::Title, &bytes)
            .or_else(|_| TitlePacket::from_slice(TitleKind::Subtitle, &bytes))
            .or_else(|_| TitlePacket::from_slice(TitleKind::ActionBar, &bytes))
            .or_else(|_| TitlePacket::from_slice(TitleKind::Clear, &bytes))
            .or_else(|_| TitlePacket::from_slice(TitleKind::Reset, &bytes))
            .or_else(|_| TitlePacket::from_slice(TitleKind::TitleTimes, &bytes));
        let _ = BossBarPacket::from_slice(&bytes);
        let _ = PlayerInfoUpdate::from_slice(&bytes);
        let _ = PlayerInfoRemove::from_slice(&bytes);
    }

    // and every truncation of a valid payload of each kind
    for valid in [
        TitlePacket::title(text("some longer title text"))
            .to_vec()
            .unwrap(),
        TitlePacket::times(TitleTimes::new(5, 50, 10))
            .to_vec()
            .unwrap(),
        BossBarPacket {
            uuid: notch(),
            action: BossBarAction::Add {
                title: text("x"),
                health: 1.0,
                color: BossBarColor::White,
                division: BossBarDivision::TwelveNotches,
                flags: BossBarFlags::from_bits(0x07),
            },
        }
        .to_vec()
        .unwrap(),
        PlayerInfoUpdate::from_entries(vec![sample_entry(notch())])
            .unwrap()
            .to_vec()
            .unwrap(),
        PlayerInfoRemove::new(vec![notch()])
            .unwrap()
            .to_vec()
            .unwrap(),
    ] {
        for cut in 0..valid.len() {
            let bytes = &valid[..cut];
            let _ = BossBarPacket::from_slice(bytes);
            let _ = PlayerInfoUpdate::from_slice(bytes);
            let _ = PlayerInfoRemove::from_slice(bytes);
            let _ = TitlePacket::from_slice(TitleKind::Title, bytes);
            let _ = TitlePacket::from_slice(TitleKind::TitleTimes, bytes);
        }
    }
}

#[test]
fn display_impls_are_useful_and_do_not_panic() {
    let title = TitlePacket::title(text("hello")).to_string();
    assert!(title.contains("title"));

    let times = TitleTimes::new(10, 70, 20).to_string();
    assert!(times.contains("fade_in=10t"));
    assert!(times.contains("stay=70t"));

    let add = BossBarAction::Add {
        title: text("boss"),
        health: 0.5,
        color: BossBarColor::Red,
        division: BossBarDivision::TenNotches,
        flags: BossBarFlags::DARKEN_SKY,
    }
    .to_string();
    assert!(add.contains("add("));
    assert!(add.contains("color=red"));

    let bar = BossBarPacket {
        uuid: notch(),
        action: BossBarAction::Remove,
    }
    .to_string();
    assert!(bar.contains("BossBar("));
    assert!(bar.contains("remove"));

    let entry = sample_entry(notch()).to_string();
    assert!(entry.contains("PlayerInfoEntry("));

    let update = PlayerInfoUpdate::from_entries(vec![sample_entry(notch())])
        .expect("entry must build an update")
        .to_string();
    assert!(update.contains("players=1"));
    assert!(update.contains("actions="));

    let remove = PlayerInfoRemove::new(vec![notch()])
        .expect("non-empty list ok")
        .to_string();
    assert!(remove.contains("players=1"));
}

#[test]
fn title_times_math_handles_extreme_inputs() {
    // the whole computation runs in i64, so nothing panics or wraps,
    // even at i32 extremes
    let times = TitleTimes::new(i32::MAX, i32::MAX, i32::MAX);
    assert_eq!(times.total_ticks(), i64::from(i32::MAX) * 3);
    assert_eq!(times.total_millis(), i64::from(i32::MAX) * 3 * 50);
    let (fi, st, fo) = times.to_millis();
    assert_eq!(fi, i64::from(i32::MAX) * 50);
    assert_eq!(st, i64::from(i32::MAX) * 50);
    assert_eq!(fo, i64::from(i32::MAX) * 50);

    let times = TitleTimes::new(i32::MIN, i32::MIN, i32::MIN);
    assert_eq!(times.total_ticks(), i64::from(i32::MIN) * 3);
    let (_, _, fo) = times.to_millis();
    assert_eq!(fo, i64::from(i32::MIN) * 50);

    // i32 milliseconds never overflow when converted to ticks: the
    // whole i32 range maps to ≈ ±42949673 ticks, comfortably inside i32
    let converted = TitleTimes::from_millis(i32::MAX, i32::MIN, 0);
    assert_eq!(converted.fade_in_ticks, 42949673);
    assert_eq!(converted.stay_ticks, -42949673);
    assert_eq!(converted.fade_out_ticks, 0);
}
