//! Tests for the public common-type codecs (`codec` module): VarInt,
//! String, ByteArray, Boolean, Uuid.

use uuid::Uuid;

use candyriya_messaging_candyriya::CandyriyaMessageError;
use candyriya_messaging_candyriya::codec;

#[test]
fn varint_roundtrip_and_limits() {
    let mut buf = Vec::new();
    codec::write_varint(300, &mut buf);
    assert_eq!(buf, [0xAC, 0x02]); // spot-check against the Minecraft wiki example

    let mut cur: &[u8] = &buf;
    assert_eq!(codec::read_varint(&mut cur).unwrap(), 300);
    assert!(cur.is_empty());

    // six bytes of continuation -> too big
    let mut cur: &[u8] = &[0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x01];
    assert_eq!(
        codec::read_varint(&mut cur).unwrap_err(),
        CandyriyaMessageError::VarIntTooBig
    );
}

#[test]
fn string_roundtrip() {
    let mut buf = Vec::new();
    codec::write_string("hello € 𝄞", &mut buf).unwrap();
    let mut cur: &[u8] = &buf;
    assert_eq!(codec::read_string(&mut cur).unwrap(), "hello € 𝄞");

    let mut buf = Vec::new();
    codec::write_string("", &mut buf).unwrap();
    assert_eq!(buf, [0x00]);
    let mut cur: &[u8] = &buf;
    assert_eq!(codec::read_string(&mut cur).unwrap(), "");
}

#[test]
fn byte_array_roundtrip() {
    let payload = b"raw bytes \x00\x01\x02";
    let mut buf = Vec::new();
    codec::write_byte_array(payload, &mut buf).unwrap();
    assert_eq!(buf[0] as usize, payload.len()); // single-byte varint prefix

    let mut cur: &[u8] = &buf;
    assert_eq!(codec::read_byte_array(&mut cur).unwrap(), payload);
    assert!(cur.is_empty());

    // empty array roundtrips
    let mut buf = Vec::new();
    codec::write_byte_array(b"", &mut buf).unwrap();
    let mut cur: &[u8] = &buf;
    assert_eq!(codec::read_byte_array(&mut cur).unwrap(), b"");
}

#[test]
fn byte_array_negative_length_prefix_is_rejected() {
    let mut cur: &[u8] = &[0xFF, 0xFF, 0xFF, 0xFF, 0x0F]; // VarInt(-1)
    assert_eq!(
        codec::read_byte_array(&mut cur).unwrap_err(),
        CandyriyaMessageError::ByteArrayTooLarge
    );
}

#[test]
fn byte_array_declared_length_over_remaining_is_rejected() {
    let mut cur: &[u8] = &[0x7F, 0xAA, 0xBB]; // declares 127 bytes, only 2 present
    assert_eq!(
        codec::read_byte_array(&mut cur).unwrap_err(),
        CandyriyaMessageError::UnexpectedEof {
            field: "byte_array"
        }
    );
}

#[test]
fn boolean_roundtrip_and_strictness() {
    let mut buf = Vec::new();
    codec::write_boolean(true, &mut buf);
    codec::write_boolean(false, &mut buf);
    assert_eq!(buf, [0x01, 0x00]);

    let mut cur: &[u8] = &buf;
    assert!(codec::read_boolean(&mut cur).unwrap());
    assert!(!codec::read_boolean(&mut cur).unwrap());

    let mut cur: &[u8] = &[0x02];
    assert_eq!(
        codec::read_boolean(&mut cur).unwrap_err(),
        CandyriyaMessageError::InvalidBoolean(0x02)
    );
    // 0xff is not truthy either (unlike Java's readBoolean)
    let mut cur: &[u8] = &[0xFF];
    assert_eq!(
        codec::read_boolean(&mut cur).unwrap_err(),
        CandyriyaMessageError::InvalidBoolean(0xFF)
    );
    // truncated
    let mut cur: &[u8] = &[];
    assert_eq!(
        codec::read_boolean(&mut cur).unwrap_err(),
        CandyriyaMessageError::UnexpectedEof { field: "boolean" }
    );
}

#[test]
fn uuid_wire_layout_msb_then_lsb_big_endian() {
    let value = Uuid::from_u128(0x0011_2233_4455_6677_8899_aabb_ccdd_eeff);
    let mut buf = Vec::new();
    codec::write_uuid(&value, &mut buf);
    assert_eq!(
        buf,
        [
            0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, // MSB i64 big-endian
            0x88, 0x99, 0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF, // LSB i64 big-endian
        ]
    );

    let mut cur: &[u8] = &buf;
    assert_eq!(codec::read_uuid(&mut cur).unwrap(), value);

    // the byte layout must match the (most significant i64, least significant i64)
    // pair encoding candyriya-protocol uses for the vanilla UUID data type
    let mut cur: &[u8] = &buf;
    let (msb, lsb) = candyriya_protocol::read_uuid(&mut cur).unwrap();
    assert_eq!(msb as u64, 0x0011_2233_4455_6677);
    assert_eq!(lsb as u64, 0x8899_AABB_CCDD_EEFF);
}

#[test]
fn uuid_short_payload_is_rejected() {
    let mut cur: &[u8] = &[0x00; 15];
    assert_eq!(
        codec::read_uuid(&mut cur).unwrap_err(),
        CandyriyaMessageError::UnexpectedEof { field: "uuid" }
    );
}

#[test]
fn error_messages_render_with_context() {
    let errors = [
        CandyriyaMessageError::EmptyPayload,
        CandyriyaMessageError::PacketTooLarge {
            actual: 32768,
            max: 32767,
        },
        CandyriyaMessageError::ProtocolVersionMismatch {
            expected: 1,
            found: 2,
        },
        CandyriyaMessageError::UnknownPacketId {
            direction: candyriya_messaging_candyriya::Direction::Serverbound,
            id: 0x7F,
        },
        CandyriyaMessageError::VarIntTooBig,
        CandyriyaMessageError::TrailingBytes { bytes: 3 },
    ];
    for error in errors {
        let rendered = error.to_string();
        assert!(!rendered.is_empty(), "{error:?} rendered empty");
    }
}
