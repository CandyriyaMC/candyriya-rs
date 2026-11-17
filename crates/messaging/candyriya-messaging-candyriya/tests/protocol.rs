//! Behavioural tests for the `candyriya:main` codec: roundtrips, dispatch
//! rules, error paths and the size cap.

use bytes::Bytes;
use candyriya_messaging_candyriya::{
    CANDYRIYA_PROTOCOL_VERSION, CandyriyaMessageError, ClientboundMessage, Direction,
    MAX_PACKET_BYTES, ServerboundMessage,
};

fn all_serverbound() -> Vec<ServerboundMessage> {
    vec![
        ServerboundMessage::Transfer {
            target_server: "pvp".to_owned(),
        },
        ServerboundMessage::TransferOther {
            target_player: "Notch".to_owned(),
            target_server: "lobby".to_owned(),
        },
        ServerboundMessage::GetServer {},
        ServerboundMessage::GetServers {},
        ServerboundMessage::ServerInfo {
            target_server: "lobby".to_owned(),
        },
        ServerboundMessage::KickPlayer {
            target_player: "griefer".to_owned(),
            reason: r#"{"text":"bye"}"#.to_owned(),
        },
        ServerboundMessage::Message {
            target_player: "Notch".to_owned(),
            message: "<green>hello".to_owned(),
        },
        ServerboundMessage::Broadcast {
            message: "<aqua>restart in 5m".to_owned(),
        },
        ServerboundMessage::UUID {
            target_player: "Notch".to_owned(),
        },
        ServerboundMessage::PlayerOnServer {
            target_player: "Notch".to_owned(),
        },
        ServerboundMessage::Transferred {},
    ]
}

fn all_clientbound() -> Vec<ClientboundMessage> {
    vec![
        ClientboundMessage::TransferResult {
            ok: true,
            error: String::new(),
        },
        ClientboundMessage::TransferResult {
            ok: false,
            error: "no such server".to_owned(),
        },
        ClientboundMessage::TransferOtherResult {
            target_player: "Notch".to_owned(),
            ok: false,
            error: "player not online".to_owned(),
        },
        ClientboundMessage::TransferOtherResult {
            target_player: "Notch".to_owned(),
            ok: true,
            error: String::new(),
        },
        ClientboundMessage::Server {
            name: "lobby".to_owned(),
        },
        ClientboundMessage::Servers {
            names: "lobby, pvp, factions".to_owned(),
        },
        ClientboundMessage::Servers {
            names: String::new(),
        },
        ClientboundMessage::ServerInfo {
            target_server: "lobby".to_owned(),
            online: true,
            players: 12,
            max_players: 100,
            latency_ms: 33,
        },
        ClientboundMessage::ServerInfo {
            target_server: "broken".to_owned(),
            online: false,
            players: 0,
            max_players: 0,
            latency_ms: -1,
        },
        ClientboundMessage::KickResult { ok: true },
        ClientboundMessage::MessageResult { ok: false },
        ClientboundMessage::BroadcastResult { ok: true },
        ClientboundMessage::UUID {
            target_player: "Notch".to_owned(),
            uuid: "069a79f4-44e9-4726-a5be-fca90e38aaf5".to_owned(),
        },
        ClientboundMessage::UUID {
            target_player: "ghost".to_owned(),
            uuid: String::new(),
        },
        ClientboundMessage::PlayerOnServer {
            target_player: "Notch".to_owned(),
            server: "pvp".to_owned(),
        },
        ClientboundMessage::PlayerOnServer {
            target_player: "ghost".to_owned(),
            server: String::new(),
        },
        ClientboundMessage::TransferNotify {},
    ]
}

#[test]
fn roundtrip_every_serverbound_packet() {
    for packet in all_serverbound() {
        let bytes = packet.encode(Direction::Serverbound).unwrap();
        let decoded = ServerboundMessage::decode(Direction::Serverbound, bytes).unwrap();
        assert_eq!(decoded, packet);
    }
}

#[test]
fn roundtrip_every_clientbound_packet() {
    for packet in all_clientbound() {
        let bytes = packet.encode(Direction::Clientbound).unwrap();
        let decoded = ClientboundMessage::decode(Direction::Clientbound, bytes).unwrap();
        assert_eq!(decoded, packet);
    }
}

#[test]
fn packet_id_values_match_the_schema() {
    let expected_serverbound: &[(ServerboundMessage, i32)] = &[
        (
            ServerboundMessage::Transfer {
                target_server: String::new(),
            },
            0x01,
        ),
        (
            ServerboundMessage::TransferOther {
                target_player: String::new(),
                target_server: String::new(),
            },
            0x02,
        ),
        (ServerboundMessage::GetServer {}, 0x03),
        (ServerboundMessage::GetServers {}, 0x04),
        (
            ServerboundMessage::ServerInfo {
                target_server: String::new(),
            },
            0x05,
        ),
        (
            ServerboundMessage::KickPlayer {
                target_player: String::new(),
                reason: String::new(),
            },
            0x06,
        ),
        (
            ServerboundMessage::Message {
                target_player: String::new(),
                message: String::new(),
            },
            0x07,
        ),
        (
            ServerboundMessage::Broadcast {
                message: String::new(),
            },
            0x08,
        ),
        (
            ServerboundMessage::UUID {
                target_player: String::new(),
            },
            0x09,
        ),
        (
            ServerboundMessage::PlayerOnServer {
                target_player: String::new(),
            },
            0x0A,
        ),
        (ServerboundMessage::Transferred {}, 0x0B),
    ];
    for (packet, id) in expected_serverbound {
        assert_eq!(packet.packet_id(), *id);
    }

    let expected_clientbound: &[(ClientboundMessage, i32)] = &[
        (
            ClientboundMessage::TransferResult {
                ok: true,
                error: String::new(),
            },
            0x81,
        ),
        (
            ClientboundMessage::TransferOtherResult {
                target_player: String::new(),
                ok: true,
                error: String::new(),
            },
            0x82,
        ),
        (
            ClientboundMessage::Server {
                name: String::new(),
            },
            0x83,
        ),
        (
            ClientboundMessage::Servers {
                names: String::new(),
            },
            0x84,
        ),
        (
            ClientboundMessage::ServerInfo {
                target_server: String::new(),
                online: true,
                players: 0,
                max_players: 0,
                latency_ms: 0,
            },
            0x85,
        ),
        (ClientboundMessage::KickResult { ok: true }, 0x86),
        (ClientboundMessage::MessageResult { ok: true }, 0x87),
        (ClientboundMessage::BroadcastResult { ok: true }, 0x88),
        (
            ClientboundMessage::UUID {
                target_player: String::new(),
                uuid: String::new(),
            },
            0x89,
        ),
        (
            ClientboundMessage::PlayerOnServer {
                target_player: String::new(),
                server: String::new(),
            },
            0x8A,
        ),
        (ClientboundMessage::TransferNotify {}, 0x8B),
    ];
    for (packet, id) in expected_clientbound {
        assert_eq!(packet.packet_id(), *id);
    }
}

#[test]
fn response_id_is_request_id_with_high_bit_set() {
    // pins the pairing rule from the schema: every reply id == request id | 0x80
    let pairs: [(i32, i32); 11] = [
        (0x01, 0x81),
        (0x02, 0x82),
        (0x03, 0x83),
        (0x04, 0x84),
        (0x05, 0x85),
        (0x06, 0x86),
        (0x07, 0x87),
        (0x08, 0x88),
        (0x09, 0x89),
        (0x0A, 0x8A),
        (0x0B, 0x8B),
    ];
    for (request, response) in pairs {
        assert_eq!(
            request | 0x80,
            response,
            "pairing broken for {request:#04x}"
        );
    }

    // and every packet id in the enums satisfies the rule
    for packet in all_serverbound() {
        assert_eq!(
            packet.packet_id() & 0x80,
            0,
            "serverbound id must be in 0x01..=0x0F"
        );
    }
    for packet in all_clientbound() {
        assert_eq!(
            packet.packet_id() & 0x80,
            0x80,
            "clientbound id must be in 0x80..=0x8F"
        );
    }
}

#[test]
fn unknown_packet_id_is_rejected() {
    // 0x0C..=0x0F are inside the allowed serverbound range but unassigned;
    // everything outside the range is rejected too
    for id in [
        0x00i32, 0x0C, 0x0F, 0x10, 0x7F, 0x80, 0x81, 0x8B, 0x8F, 0xFF,
    ] {
        let mut payload = vec![0x01];
        candyriya_protocol::write_varint(id, &mut payload);
        let result = ServerboundMessage::decode(Direction::Serverbound, Bytes::from(payload));
        assert!(
            matches!(
                result,
                Err(CandyriyaMessageError::UnknownPacketId {
                    direction: Direction::Serverbound,
                    ..
                })
            ),
            "serverbound id {id:#04x} must be rejected as unknown"
        );
    }
    for id in [0x00i32, 0x01, 0x7F, 0x80, 0x8C, 0x8F, 0x90, 0xFF] {
        let mut payload = vec![0x01];
        candyriya_protocol::write_varint(id, &mut payload);
        let result = ClientboundMessage::decode(Direction::Clientbound, Bytes::from(payload));
        assert!(
            matches!(
                result,
                Err(CandyriyaMessageError::UnknownPacketId {
                    direction: Direction::Clientbound,
                    ..
                })
            ),
            "clientbound id {id:#04x} must be rejected as unknown"
        );
    }
}

#[test]
fn unknown_packet_id_error_carries_direction_and_id() {
    let payload = Bytes::from_static(&[0x01, 0x7F]);
    let err = ServerboundMessage::decode(Direction::Serverbound, payload).unwrap_err();
    assert_eq!(
        err,
        CandyriyaMessageError::UnknownPacketId {
            direction: Direction::Serverbound,
            id: 0x7F,
        }
    );
}

#[test]
fn wrong_protocol_version_is_rejected() {
    for version in [0i32, 2, -1, 300, i32::MAX] {
        let mut payload = Vec::new();
        candyriya_protocol::write_varint(version, &mut payload);
        candyriya_protocol::write_varint(0x01, &mut payload);
        payload.extend_from_slice(&[0x03, 0x70, 0x76, 0x70]); // "pvp" (would be a valid Transfer)
        let err =
            ServerboundMessage::decode(Direction::Serverbound, Bytes::from(payload)).unwrap_err();
        assert_eq!(
            err,
            CandyriyaMessageError::ProtocolVersionMismatch {
                expected: CANDYRIYA_PROTOCOL_VERSION,
                found: version,
            }
        );
    }
}

#[test]
fn oversized_varint_in_version_field_is_rejected() {
    // six continuation bytes: never a valid 5-byte VarInt
    let payload = Bytes::from_static(&[0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x01, 0x01]);
    let err = ServerboundMessage::decode(Direction::Serverbound, payload).unwrap_err();
    assert_eq!(err, CandyriyaMessageError::VarIntTooBig);
}

#[test]
fn every_truncation_of_a_valid_packet_is_rejected() {
    let packet = ClientboundMessage::ServerInfo {
        target_server: "lobby".to_owned(),
        online: true,
        players: 12,
        max_players: 100,
        latency_ms: 33,
    };
    let full = packet.encode(Direction::Clientbound).unwrap();
    for cut in 0..full.len() {
        let result = ClientboundMessage::decode(Direction::Clientbound, full.slice(..cut));
        assert!(result.is_err(), "truncating to {cut} bytes must fail");
    }
    // and the full payload still decodes
    assert_eq!(
        ClientboundMessage::decode(Direction::Clientbound, full).unwrap(),
        packet
    );

    // same for a serverbound packet with a body
    let transfer = ServerboundMessage::Transfer {
        target_server: "pvp".to_owned(),
    };
    let full = transfer.encode(Direction::Serverbound).unwrap();
    for cut in 0..full.len() {
        let result = ServerboundMessage::decode(Direction::Serverbound, full.slice(..cut));
        assert!(result.is_err(), "truncating to {cut} bytes must fail");
    }
}

#[test]
fn truncation_inside_string_payload_reports_eof() {
    // Transfer to "pvp" with the last byte of the string missing
    let payload = Bytes::from_static(&[0x01, 0x01, 0x03, 0x70, 0x76]);
    let err = ServerboundMessage::decode(Direction::Serverbound, payload).unwrap_err();
    assert_eq!(
        err,
        CandyriyaMessageError::UnexpectedEof {
            field: "target_server",
        }
    );
}

#[test]
fn trailing_bytes_are_rejected() {
    let packet = ServerboundMessage::Transfer {
        target_server: "pvp".to_owned(),
    };
    let mut bytes = packet.encode(Direction::Serverbound).unwrap().to_vec();
    bytes.push(0x00);
    let err = ServerboundMessage::decode(Direction::Serverbound, Bytes::from(bytes)).unwrap_err();
    assert_eq!(err, CandyriyaMessageError::TrailingBytes { bytes: 1 });

    let mut bytes = packet.encode(Direction::Serverbound).unwrap().to_vec();
    bytes.extend_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]);
    let err = ServerboundMessage::decode(Direction::Serverbound, Bytes::from(bytes)).unwrap_err();
    assert_eq!(err, CandyriyaMessageError::TrailingBytes { bytes: 4 });
}

#[test]
fn empty_payload_is_rejected() {
    let err = ServerboundMessage::decode(Direction::Serverbound, Bytes::new()).unwrap_err();
    assert_eq!(err, CandyriyaMessageError::EmptyPayload);
    let err = ClientboundMessage::decode(Direction::Clientbound, Bytes::new()).unwrap_err();
    assert_eq!(err, CandyriyaMessageError::EmptyPayload);
}

#[test]
fn negative_string_length_prefix_is_rejected() {
    // VarInt(-1) as the string length prefix of a Transfer
    let payload = Bytes::from_static(&[0x01, 0x01, 0xFF, 0xFF, 0xFF, 0xFF, 0x0F]);
    let err = ServerboundMessage::decode(Direction::Serverbound, payload).unwrap_err();
    assert_eq!(err, CandyriyaMessageError::StringTooLong);
}

#[test]
fn string_length_prefix_over_byte_cap_is_rejected() {
    // declared byte length = 98302 = 32767 * 3 + 1, one above the UTF-8 byte cap
    let mut payload = vec![0x01, 0x01];
    candyriya_protocol::write_varint(98302, &mut payload);
    payload.extend_from_slice(b"x");
    let err = ServerboundMessage::decode(Direction::Serverbound, Bytes::from(payload)).unwrap_err();
    assert_eq!(err, CandyriyaMessageError::StringTooLong);
}

#[test]
fn string_over_utf16_unit_cap_is_rejected_on_encode() {
    // 16384 four-byte chars: 65536 bytes (within the byte cap) but
    // 32768 UTF-16 code units, one above the 32767-unit cap
    let message = "𝄞".repeat(16384);
    let packet = ServerboundMessage::Broadcast { message };
    let err = packet.encode(Direction::Serverbound).unwrap_err();
    assert_eq!(err, CandyriyaMessageError::StringTooLong);
}

// note: on decode the 32767-code-unit string cap is unreachable in practice —
// MAX_PACKET_BYTES (32767) is enforced before parsing, and no combination of
// multibyte sequences fits more than 32767 code units into 32767 bytes.
// The unit cap is enforced on encode (see string_over_utf16_unit_cap_is_rejected_on_encode)
// and by candyriya-protocol on read when the cap is reached exactly.

#[test]
fn invalid_utf8_in_string_is_rejected() {
    // Broadcast with a two-byte string that is not valid UTF-8
    let payload = Bytes::from_static(&[0x01, 0x08, 0x02, 0xFF, 0xFE]);
    let err = ServerboundMessage::decode(Direction::Serverbound, payload).unwrap_err();
    assert_eq!(err, CandyriyaMessageError::InvalidUtf8);
}

#[test]
fn invalid_boolean_byte_is_rejected() {
    // TransferResult with ok = 0x02
    let payload = Bytes::from_static(&[0x01, 0x81, 0x01, 0x02, 0x00]);
    let err = ClientboundMessage::decode(Direction::Clientbound, payload).unwrap_err();
    assert_eq!(err, CandyriyaMessageError::InvalidBoolean(0x02));
}

#[test]
fn decode_with_wrong_direction_is_rejected() {
    let serverbound = ServerboundMessage::Transfer {
        target_server: "pvp".to_owned(),
    }
    .encode(Direction::Serverbound)
    .unwrap();
    // a serverbound payload decoded as clientbound (and vice versa)
    let err = ClientboundMessage::decode(Direction::Serverbound, serverbound.clone()).unwrap_err();
    assert_eq!(
        err,
        CandyriyaMessageError::DirectionMismatch {
            expected: Direction::Clientbound,
            found: Direction::Serverbound,
        }
    );
    let clientbound = ClientboundMessage::TransferResult {
        ok: true,
        error: String::new(),
    }
    .encode(Direction::Clientbound)
    .unwrap();
    let err = ServerboundMessage::decode(Direction::Clientbound, clientbound).unwrap_err();
    assert_eq!(
        err,
        CandyriyaMessageError::DirectionMismatch {
            expected: Direction::Serverbound,
            found: Direction::Clientbound,
        }
    );
}

#[test]
fn encode_with_wrong_direction_is_rejected() {
    let packet = ServerboundMessage::GetServer {};
    let err = packet.encode(Direction::Clientbound).unwrap_err();
    assert_eq!(
        err,
        CandyriyaMessageError::DirectionMismatch {
            expected: Direction::Serverbound,
            found: Direction::Clientbound,
        }
    );
    let packet = ClientboundMessage::TransferNotify {};
    let err = packet.encode(Direction::Serverbound).unwrap_err();
    assert_eq!(
        err,
        CandyriyaMessageError::DirectionMismatch {
            expected: Direction::Clientbound,
            found: Direction::Serverbound,
        }
    );
}

#[test]
fn max_packet_bytes_encode_boundary() {
    // version(1) + id(1) + prefix(3 for 32762) + 32762 = exactly 32767
    let message = "a".repeat(32762);
    let packet = ServerboundMessage::Broadcast { message };
    let bytes = packet.encode(Direction::Serverbound).unwrap();
    assert_eq!(bytes.len(), MAX_PACKET_BYTES);
    let decoded = ServerboundMessage::decode(Direction::Serverbound, bytes).unwrap();
    match decoded {
        ServerboundMessage::Broadcast { message } => assert_eq!(message.len(), 32762),
        other => panic!("unexpected packet: {other:?}"),
    }

    // one byte longer crosses the cap
    let message = "a".repeat(32763);
    let packet = ServerboundMessage::Broadcast { message };
    let err = packet.encode(Direction::Serverbound).unwrap_err();
    assert_eq!(
        err,
        CandyriyaMessageError::PacketTooLarge {
            actual: MAX_PACKET_BYTES + 1,
            max: MAX_PACKET_BYTES,
        }
    );
}

#[test]
fn max_packet_bytes_decode_boundary() {
    let message = "a".repeat(32762);
    let packet = ServerboundMessage::Broadcast { message };
    let bytes = packet.encode(Direction::Serverbound).unwrap();
    assert_eq!(bytes.len(), MAX_PACKET_BYTES);
    // decode of exactly MAX_PACKET_BYTES works
    assert!(ServerboundMessage::decode(Direction::Serverbound, bytes.clone()).is_ok());

    // 32768 bytes rejected before parsing, even if the tail looks valid
    let mut oversized = bytes.to_vec();
    oversized.push(0x00);
    let err =
        ServerboundMessage::decode(Direction::Serverbound, Bytes::from(oversized)).unwrap_err();
    assert_eq!(
        err,
        CandyriyaMessageError::PacketTooLarge {
            actual: MAX_PACKET_BYTES + 1,
            max: MAX_PACKET_BYTES,
        }
    );
}
