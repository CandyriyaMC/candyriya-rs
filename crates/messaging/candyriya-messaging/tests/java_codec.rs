//! Golden vectors and roundtrips for the Java modified-UTF-8 codec.
//!
//! The pinned sequences below were captured from OpenJDK 21
//! `DataOutputStream.writeUTF` (Temurin-21.0.12+8) and cross-checked with an
//! independent Python CESU-8 implementation; both agree byte-for-byte.

use candyriya_messaging::JavaCodecError;
use candyriya_messaging::java::*;

fn encoded(s: &str) -> Vec<u8> {
    let mut v = Vec::new();
    write_java_string(&mut v, s).expect("encode");
    v
}

fn decode(bytes: &[u8], max_bytes: usize) -> Result<String, JavaCodecError> {
    let mut cursor = bytes;
    read_java_string(&mut cursor, max_bytes)
}

#[test]
fn golden_empty() {
    assert_eq!(encoded(""), [0x00, 0x00]);
    assert_eq!(decode(&[0x00, 0x00], 65535).unwrap(), "");
}

#[test]
fn golden_ascii() {
    assert_eq!(encoded("abc"), [0x00, 0x03, 0x61, 0x62, 0x63]);
}

#[test]
fn golden_null_in_string() {
    // U+0000 is the one exception to shortest-form: always 0xC0 0x80
    assert_eq!(encoded("a\0b"), [0x00, 0x04, 0x61, 0xC0, 0x80, 0x62]);
    assert_eq!(
        decode(&[0x00, 0x04, 0x61, 0xC0, 0x80, 0x62], 64).unwrap(),
        "a\0b"
    );
}

#[test]
fn golden_two_byte_bmp() {
    assert_eq!(encoded("\u{00E9}"), [0x00, 0x02, 0xC3, 0xA9]); // e-acute
    assert_eq!(encoded("\u{0100}"), [0x00, 0x02, 0xC4, 0x80]); // first 2-byte code point
    assert_eq!(encoded("\u{07FF}"), [0x00, 0x02, 0xDF, 0xBF]); // last 2-byte code point
}

#[test]
fn golden_three_byte_bmp() {
    assert_eq!(encoded("\u{0800}"), [0x00, 0x03, 0xE0, 0xA0, 0x80]); // first 3-byte code point
    assert_eq!(encoded("\u{20AC}"), [0x00, 0x03, 0xE2, 0x82, 0xAC]); // euro sign
    assert_eq!(encoded("\u{FFFF}"), [0x00, 0x03, 0xEF, 0xBF, 0xBF]); // last BMP code point
}

#[test]
fn golden_astral_plane() {
    // U+1F600 grinning face: surrogate pair ED A0 BD ED B8 80
    assert_eq!(
        encoded("\u{1F600}"),
        [0x00, 0x06, 0xED, 0xA0, 0xBD, 0xED, 0xB8, 0x80]
    );
    // U+10000, the first astral code point
    assert_eq!(
        encoded("\u{10000}"),
        [0x00, 0x06, 0xED, 0xA0, 0x80, 0xED, 0xB0, 0x80]
    );
    // U+103FF, the code point the surrogate pair D800 DFFF encodes
    assert_eq!(
        encoded("\u{103FF}"),
        [0x00, 0x06, 0xED, 0xA0, 0x80, 0xED, 0xBF, 0xBF]
    );
    // U+10FFFF, the last Unicode scalar value
    assert_eq!(
        encoded("\u{10FFFF}"),
        [0x00, 0x06, 0xED, 0xAF, 0xBF, 0xED, 0xBF, 0xBF]
    );
}

#[test]
fn golden_mixed_bmp_astral_null() {
    assert_eq!(
        encoded("a\u{0}\u{20AC}\u{1F600}z"),
        [
            0x00, 0x0D, // u16 prefix: 13 bytes of payload
            0x61, 0xC0, 0x80, // 'a', U+0000
            0xE2, 0x82, 0xAC, // euro sign
            0xED, 0xA0, 0xBD, 0xED, 0xB8, 0x80, // grinning face
            0x7A, // 'z'
        ]
    );
}

#[test]
fn golden_length_prefix_is_u16_big_endian() {
    let bytes = encoded(&"a".repeat(256));
    assert_eq!(&bytes[..2], [0x01, 0x00]); // 256 = 0x0100, big-endian
    let bytes = encoded(&"a".repeat(300));
    assert_eq!(&bytes[..2], [0x01, 0x2C]); // 300 = 0x012C
    assert_eq!(bytes.len(), 302);
}

#[test]
fn golden_max_payload_65535_bytes() {
    let ok = encoded(&"a".repeat(MAX_UTF_BYTES));
    assert_eq!(&ok[..2], [0xFF, 0xFF]);
    assert_eq!(ok.len(), MAX_UTF_BYTES + 2);

    // 21845 euro signs * 3 bytes = exactly 65535 payload bytes
    let ok = encoded(&"\u{20AC}".repeat(21845));
    assert_eq!(&ok[..2], [0xFF, 0xFF]);
}

#[test]
fn encode_overlong_payload_fails_and_leaves_buf_untouched() {
    let mut buf = Vec::new();
    let err = write_java_string(&mut buf, &"a".repeat(65536)).unwrap_err();
    assert_eq!(
        err,
        JavaCodecError::EncodedStringTooLong { byte_len: 65536 }
    );
    assert!(
        buf.is_empty(),
        "the JVM throws before writing anything; so do we"
    );

    let err = write_java_string(&mut buf, &"\u{20AC}".repeat(21846)).unwrap_err();
    assert_eq!(
        err,
        JavaCodecError::EncodedStringTooLong { byte_len: 65538 }
    );
    assert!(buf.is_empty());
}

#[test]
fn roundtrip_all_ranges() {
    let cases = [
        "",
        "abc",
        "\0",
        "\0\0\0",
        "a\0b\0c",
        "\u{7F}",
        "\u{80}",
        "\u{7FF}",
        "\u{800}",
        "\u{20AC}",
        "\u{D7FF}",
        "\u{E000}",
        "\u{FFFF}",
        "\u{10000}",
        "\u{1F600}",
        "\u{103FF}",
        "\u{10FFFF}",
        "mixed a\0€😀z",
        "😀😀😀",
    ];
    for s in cases {
        let path = encoded(s);
        assert_eq!(
            decode(&path, MAX_UTF_BYTES).unwrap(),
            s,
            "roundtrip of {s:?}"
        );
    }
}

#[test]
fn roundtrip_reencode_is_byte_identical() {
    let s = "mixed a\0€😀z";
    let path = encoded(s);
    let decoded = decode(&path, MAX_UTF_BYTES).unwrap();
    assert_eq!(encoded(&decoded), path);
}

#[test]
fn read_rejects_declared_length_over_max() {
    // payload declares 4 bytes, caller allows only 3
    let err = decode(&[0x00, 0x04, 0x61, 0x62, 0x63, 0x64], 3).unwrap_err();
    assert_eq!(
        err,
        JavaCodecError::ExceedsMaxBytes {
            byte_len: 4,
            max_bytes: 3
        }
    );
    // declared == max is fine
    assert_eq!(
        decode(&[0x00, 0x04, 0x61, 0x62, 0x63, 0x64], 4).unwrap(),
        "abcd"
    );
}

#[test]
fn read_rejects_truncated_prefix() {
    assert_eq!(
        decode(&[], 64).unwrap_err(),
        JavaCodecError::Truncated {
            needed: 2,
            remaining: 0
        }
    );
    assert_eq!(
        decode(&[0x00], 64).unwrap_err(),
        JavaCodecError::Truncated {
            needed: 2,
            remaining: 1
        }
    );
}

#[test]
fn read_rejects_truncated_payload() {
    // declares 5 payload bytes, only 3 present
    assert_eq!(
        decode(&[0x00, 0x05, 0x61, 0x62, 0x63], 64).unwrap_err(),
        JavaCodecError::Truncated {
            needed: 5,
            remaining: 3
        }
    );
}

#[test]
fn read_rejects_truncated_multibyte_sequence() {
    // 0xC2 starts a 2-byte sequence, only 1 payload byte present
    assert_eq!(
        decode(&[0x00, 0x01, 0xC2], 64).unwrap_err(),
        JavaCodecError::TruncatedSequence {
            offset: 0,
            needed: 2,
            remaining: 1
        }
    );
    // 0xE0 0xA0 starts a 3-byte sequence, only 2 payload bytes present
    assert_eq!(
        decode(&[0x00, 0x02, 0xE0, 0xA0], 64).unwrap_err(),
        JavaCodecError::TruncatedSequence {
            offset: 0,
            needed: 3,
            remaining: 2
        }
    );
    // high surrogate with no room for the low surrogate half
    assert_eq!(
        decode(&[0x00, 0x03, 0xED, 0xA0, 0xBD], 64).unwrap_err(),
        JavaCodecError::TruncatedSequence {
            offset: 0,
            needed: 6,
            remaining: 3
        }
    );
}

#[test]
fn read_rejects_invalid_start_bytes() {
    for byte in [0x00, 0x80, 0xBF, 0xF0, 0xF5, 0xFF] {
        assert_eq!(
            decode(&[0x00, 0x01, byte], 64).unwrap_err(),
            JavaCodecError::InvalidStartByte { offset: 0, byte },
            "start byte 0x{byte:02X}"
        );
    }
}

#[test]
fn read_rejects_bad_continuation() {
    assert_eq!(
        decode(&[0x00, 0x02, 0xC2, 0x41], 64).unwrap_err(),
        JavaCodecError::InvalidContinuation {
            offset: 1,
            byte: 0x41
        }
    );
    assert_eq!(
        decode(&[0x00, 0x03, 0xE0, 0xA0, 0x41], 64).unwrap_err(),
        JavaCodecError::InvalidContinuation {
            offset: 2,
            byte: 0x41
        }
    );
    assert_eq!(
        decode(&[0x00, 0x03, 0xE0, 0x28, 0xA0], 64).unwrap_err(),
        JavaCodecError::InvalidContinuation {
            offset: 1,
            byte: 0x28
        }
    );
}

#[test]
fn read_rejects_overlong_encodings() {
    // 0xC0 followed by a continuation byte other than 0x80: overlong U+0001..U+003F
    assert_eq!(
        decode(&[0x00, 0x02, 0xC0, 0x81], 64).unwrap_err(),
        JavaCodecError::OverlongEncoding { offset: 0 }
    );
    // 0xC1 anything: overlong U+0040..U+007F
    assert_eq!(
        decode(&[0x00, 0x02, 0xC1, 0x80], 64).unwrap_err(),
        JavaCodecError::OverlongEncoding { offset: 0 }
    );
    // 0xE0 0x80 0x80: overlong U+0000 (must be 0xC0 0x80 in modified UTF-8)
    assert_eq!(
        decode(&[0x00, 0x03, 0xE0, 0x80, 0x80], 64).unwrap_err(),
        JavaCodecError::OverlongEncoding { offset: 0 }
    );
    // 0xE0 0x9F 0xBF: overlong U+07FF (must be the two-byte form)
    assert_eq!(
        decode(&[0x00, 0x03, 0xE0, 0x9F, 0xBF], 64).unwrap_err(),
        JavaCodecError::OverlongEncoding { offset: 0 }
    );
    // 0xE1 0x80 0x80 is boundary-legal: decodes to U+1000 (cp >= 0x800)
    assert_eq!(
        decode(&[0x00, 0x03, 0xE1, 0x80, 0x80], 64).unwrap(),
        "\u{1000}"
    );
}

#[test]
fn read_rejects_lone_high_surrogate() {
    // 0xED 0xA0 0xBD is D83D, a high surrogate; 'a' (0x61) is not 0xED
    assert_eq!(
        decode(&[0x00, 0x06, 0xED, 0xA0, 0xBD, 0x61, 0x62, 0x63], 64).unwrap_err(),
        JavaCodecError::LoneSurrogate {
            offset: 0,
            surrogate: 0xD83D
        }
    );
    // high surrogate followed by another high surrogate instead of a low one
    assert_eq!(
        decode(&[0x00, 0x06, 0xED, 0xA0, 0xBD, 0xED, 0xA0, 0xBD], 64).unwrap_err(),
        JavaCodecError::LoneSurrogate {
            offset: 0,
            surrogate: 0xD83D
        }
    );
}

#[test]
fn read_rejects_lone_low_surrogate() {
    assert_eq!(
        decode(&[0x00, 0x03, 0xED, 0xB8, 0x80], 64).unwrap_err(), // DE00
        JavaCodecError::LoneSurrogate {
            offset: 0,
            surrogate: 0xDE00
        }
    );
}

#[test]
fn read_combines_surrogate_pairs() {
    assert_eq!(
        decode(&[0x00, 0x06, 0xED, 0xA0, 0xBD, 0xED, 0xB8, 0x80], 64).unwrap(),
        "\u{1F600}"
    );
}

#[test]
fn read_cursor_advances_past_string() {
    let mut bytes = encoded("abc");
    bytes.extend_from_slice(&[0xDE, 0xAD]);
    let mut cursor = bytes.as_slice();
    assert_eq!(read_java_string(&mut cursor, 64).unwrap(), "abc");
    assert_eq!(cursor, &[0xDE, 0xAD]);
}

#[test]
fn limit_roundtrip_uses_all_primitives() {
    let mut buf = Vec::new();
    write_java_bool(&mut buf, true);
    write_java_bool(&mut buf, false);
    write_java_u8(&mut buf, 0xAB);
    write_java_u16(&mut buf, 300); // 0x012C, checks big-endian byte order
    write_java_u16(&mut buf, u16::MAX);
    write_java_int(&mut buf, i32::MIN);
    write_java_int(&mut buf, -2);
    write_java_int(&mut buf, i32::MAX);
    write_java_i64(&mut buf, i64::MIN);
    write_java_i64(&mut buf, i64::MAX);
    write_java_string(&mut buf, "hello").unwrap();

    let expected_head = [
        0x01, 0x00, 0xAB, 0x01, 0x2C, 0xFF, 0xFF, 0x80, 0x00, 0x00, 0x00, 0xFF, 0xFF, 0xFF, 0xFE,
        0x7F, 0xFF, 0xFF, 0xFF,
    ];
    assert_eq!(&buf[..expected_head.len()], &expected_head);

    let mut cursor = buf.as_slice();
    assert!(read_java_bool(&mut cursor).unwrap());
    assert!(!read_java_bool(&mut cursor).unwrap());
    assert_eq!(read_java_u8(&mut cursor).unwrap(), 0xAB);
    assert_eq!(read_java_u16(&mut cursor).unwrap(), 300);
    assert_eq!(read_java_u16(&mut cursor).unwrap(), u16::MAX);
    assert_eq!(read_java_int(&mut cursor).unwrap(), i32::MIN);
    assert_eq!(read_java_int(&mut cursor).unwrap(), -2);
    assert_eq!(read_java_int(&mut cursor).unwrap(), i32::MAX);
    assert_eq!(read_java_i64(&mut cursor).unwrap(), i64::MIN);
    assert_eq!(read_java_i64(&mut cursor).unwrap(), i64::MAX);
    assert_eq!(read_java_string(&mut cursor, 64).unwrap(), "hello");
    assert!(cursor.is_empty());
}

#[test]
fn write_int_push_matches_big_endian() {
    let mut buf = Vec::new();
    write_java_int(&mut buf, -2);
    assert_eq!(buf, [0xFF, 0xFF, 0xFF, 0xFE]);
    let mut buf = Vec::new();
    write_java_i64(&mut buf, -1);
    assert_eq!(buf, [0xFF; 8]);
    let mut buf = Vec::new();
    write_java_u16(&mut buf, 300);
    assert_eq!(buf, [0x01, 0x2C]);
}

#[test]
fn primitives_reject_truncated_input() {
    assert_eq!(
        read_java_u8(&mut &[][..]).unwrap_err(),
        JavaCodecError::Truncated {
            needed: 1,
            remaining: 0
        }
    );
    assert_eq!(
        read_java_u16(&mut &[0x01][..]).unwrap_err(),
        JavaCodecError::Truncated {
            needed: 2,
            remaining: 1
        }
    );
    assert_eq!(
        read_java_int(&mut &[0x01, 0x02, 0x03][..]).unwrap_err(),
        JavaCodecError::Truncated {
            needed: 4,
            remaining: 3
        }
    );
    assert_eq!(
        read_java_i64(&mut &[0x01; 7][..]).unwrap_err(),
        JavaCodecError::Truncated {
            needed: 8,
            remaining: 7
        }
    );
    assert_eq!(
        read_java_bool(&mut &[][..]).unwrap_err(),
        JavaCodecError::Truncated {
            needed: 1,
            remaining: 0
        }
    );
}

#[test]
fn read_bool_is_strict() {
    assert!(!read_java_bool(&mut &[0x00][..]).unwrap());
    assert!(read_java_bool(&mut &[0x01][..]).unwrap());
    assert_eq!(
        read_java_bool(&mut &[0x02][..]).unwrap_err(),
        JavaCodecError::InvalidBoolean { byte: 0x02 }
    );
    // 0xFF is truthy to the JVM's readBoolean; we reject it
    assert_eq!(
        read_java_bool(&mut &[0xFF][..]).unwrap_err(),
        JavaCodecError::InvalidBoolean { byte: 0xFF }
    );
}
