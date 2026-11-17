//! Golden byte vectors, computed by hand from the protocol tables in
//! `schema` (see crate docs). Each vector is written out byte by byte so a
//! regression in the encoder fails loudly with a readable diff.
//!
//! Hand derivation (protocol version 1 encodes as `0x01`):
//!
//! Transfer "pvp":
//!   `01` version | `01` id (Transfer) | `03` string len 3 | `70 76 70` "pvp"
//!
//! UUID "Notch":
//!   `01` version | `09` id (UUID) | `05` string len 5 | `4E 6F 74 63 68` "Notch"
//!
//! ServerInfo "lobby", online, 12/100/33:
//!   `01` version | `85 01` id (VarInt 133 = 0x85) | `05` len | `6C 6F 62 62 79` "lobby"
//!   | `01` online=true | `0C` players=12 | `64` max_players=100 | `21` latency_ms=33
//!
//! Broadcast "<aqua>hi":
//!   `01` version | `08` id (Broadcast) | `08` len 8 | `3C 61 71 75 61 3E 68 69` "<aqua>hi"

use bytes::Bytes;
use candyriya_messaging_candyriya::{ClientboundMessage, Direction, ServerboundMessage};

#[test]
fn golden_transfer() {
    let packet = ServerboundMessage::Transfer {
        target_server: "pvp".to_owned(),
    };
    let expected: &[u8] = &[0x01, 0x01, 0x03, 0x70, 0x76, 0x70];

    let encoded = packet.encode(Direction::Serverbound).unwrap();
    assert_eq!(encoded.as_ref(), expected, "encode golden mismatch");

    let decoded =
        ServerboundMessage::decode(Direction::Serverbound, Bytes::from_static(expected)).unwrap();
    assert_eq!(decoded, packet, "decode golden mismatch");
}

#[test]
fn golden_uuid() {
    let packet = ServerboundMessage::UUID {
        target_player: "Notch".to_owned(),
    };
    let expected: &[u8] = &[0x01, 0x09, 0x05, 0x4E, 0x6F, 0x74, 0x63, 0x68];

    let encoded = packet.encode(Direction::Serverbound).unwrap();
    assert_eq!(encoded.as_ref(), expected, "encode golden mismatch");

    let decoded =
        ServerboundMessage::decode(Direction::Serverbound, Bytes::from_static(expected)).unwrap();
    assert_eq!(decoded, packet, "decode golden mismatch");
}

#[test]
fn golden_server_info() {
    let packet = ClientboundMessage::ServerInfo {
        target_server: "lobby".to_owned(),
        online: true,
        players: 12,
        max_players: 100,
        latency_ms: 33,
    };
    // VarInt(133) = 0b1000_0101 0x01 -> 0x85 0x01; VarInt(100) = 0x64 (single byte);
    // VarInt(12) = 0x0C; VarInt(33) = 0x21.
    let expected: &[u8] = &[
        0x01, // protocol version
        0x85, 0x01, // id, VarInt(133)
        0x05, // string len
        0x6C, 0x6F, 0x62, 0x62, 0x79, // "lobby"
        0x01, // online = true
        0x0C, // players = 12
        0x64, // max_players = 100
        0x21, // latency_ms = 33
    ];

    let encoded = packet.encode(Direction::Clientbound).unwrap();
    assert_eq!(encoded.as_ref(), expected, "encode golden mismatch");

    let decoded =
        ClientboundMessage::decode(Direction::Clientbound, Bytes::from_static(expected)).unwrap();
    assert_eq!(decoded, packet, "decode golden mismatch");
}

#[test]
fn golden_broadcast() {
    let packet = ServerboundMessage::Broadcast {
        message: "<aqua>hi".to_owned(),
    };
    let expected: &[u8] = &[
        0x01, // protocol version
        0x08, // id (Broadcast)
        0x08, // string len
        0x3C, 0x61, 0x71, 0x75, 0x61, 0x3E, 0x68, 0x69, // "<aqua>hi"
    ];

    let encoded = packet.encode(Direction::Serverbound).unwrap();
    assert_eq!(encoded.as_ref(), expected, "encode golden mismatch");

    let decoded =
        ServerboundMessage::decode(Direction::Serverbound, Bytes::from_static(expected)).unwrap();
    assert_eq!(decoded, packet, "decode golden mismatch");
}

#[test]
fn golden_transferred_empty_body() {
    let packet = ServerboundMessage::Transferred {};
    let expected: &[u8] = &[0x01, 0x0B];

    let encoded = packet.encode(Direction::Serverbound).unwrap();
    assert_eq!(encoded.as_ref(), expected);
    let decoded =
        ServerboundMessage::decode(Direction::Serverbound, Bytes::from_static(expected)).unwrap();
    assert_eq!(decoded, packet);
}

#[test]
fn golden_transfer_notify_empty_body() {
    let packet = ClientboundMessage::TransferNotify {};
    // VarInt(0x8B = 139) is two bytes: 0x8B 0x01
    let expected: &[u8] = &[0x01, 0x8B, 0x01];

    let encoded = packet.encode(Direction::Clientbound).unwrap();
    assert_eq!(encoded.as_ref(), expected);
    let decoded =
        ClientboundMessage::decode(Direction::Clientbound, Bytes::from_static(expected)).unwrap();
    assert_eq!(decoded, packet);
}

#[test]
fn golden_transfer_result_field_order() {
    // ok before error; empty error string encodes as a zero length prefix
    let packet = ClientboundMessage::TransferResult {
        ok: true,
        error: String::new(),
    };
    // VarInt(0x81 = 129) -> 0x81 0x01
    let expected: &[u8] = &[0x01, 0x81, 0x01, 0x01, 0x00];

    let encoded = packet.encode(Direction::Clientbound).unwrap();
    assert_eq!(encoded.as_ref(), expected);
    let decoded =
        ClientboundMessage::decode(Direction::Clientbound, Bytes::from_static(expected)).unwrap();
    assert_eq!(decoded, packet);
}

#[test]
fn golden_transfer_other_result_field_order() {
    // target_player, then ok, then error (this order is on the wire)
    let packet = ClientboundMessage::TransferOtherResult {
        target_player: "p".to_owned(),
        ok: true,
        error: String::new(),
    };
    // VarInt(0x82 = 130) -> 0x82 0x01
    let expected: &[u8] = &[0x01, 0x82, 0x01, 0x01, 0x70, 0x01, 0x00];

    let encoded = packet.encode(Direction::Clientbound).unwrap();
    assert_eq!(encoded.as_ref(), expected);
    let decoded =
        ClientboundMessage::decode(Direction::Clientbound, Bytes::from_static(expected)).unwrap();
    assert_eq!(decoded, packet);
}
