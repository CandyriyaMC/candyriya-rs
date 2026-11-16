//! Tests for the frame codec. Golden vectors come from canonical zlib
//! (CPython's `zlib.compress(packet, 6)`), so the decoder is exercised
//! against a second, independent implementation.

use super::*;

const DEFAULT_THRESHOLD: i32 = 256;

/// Deterministic, highly compressible byte pattern with period 251.
fn patterned_bytes(len: usize) -> Vec<u8> {
    (0..len).map(|i| ((i * 31 + 7) % 251) as u8).collect()
}

fn golden_packet() -> Vec<u8> {
    let mut packet: Vec<u8> = (0..=255u8).collect();
    packet = packet.repeat(4);
    packet.extend_from_slice(b"candyriya golden packet");
    packet
}

/// Builds a raw frame from its parts without going through the encoder,
/// for frames that cannot be produced through the public API (bogus
/// headers, declared sizes that do not match the payload, ...).
fn build_frame(data_length: i32, payload: &[u8]) -> Vec<u8> {
    let mut body = Vec::new();
    write_varint(data_length, &mut body);
    body.extend_from_slice(payload);
    let mut frame = Vec::new();
    write_varint(body.len() as i32, &mut frame);
    frame.extend_from_slice(&body);
    frame
}

/// Splits a frame into (packet_length, data_length, payload).
fn split_frame(frame: &[u8]) -> (i32, i32, &[u8]) {
    let (packet_len, packet_len_bytes) = scan_varint(frame).unwrap();
    let rest = &frame[packet_len_bytes..];
    let (data_length, data_length_bytes) = scan_varint(rest).unwrap();
    (packet_len, data_length, &rest[data_length_bytes..])
}

#[test]
fn roundtrip_sizes_and_levels() {
    for size in [0usize, 1, 255, 256, 257, 4096, 1_048_576] {
        let packet = patterned_bytes(size);
        for level in [-1, 0, 9] {
            let compression = Compression::new(DEFAULT_THRESHOLD, level).unwrap();
            let frame = compression.encode_frame(&packet).unwrap();
            let (consumed, decoded) = compression.decode_frame(&frame).unwrap();
            assert_eq!(
                consumed,
                frame.len(),
                "consumed at size {size} level {level}"
            );
            assert_eq!(decoded, packet, "payload at size {size} level {level}");
        }
    }
}

#[test]
fn roundtrip_around_threshold() {
    for threshold in [-1, 0, 1, DEFAULT_THRESHOLD, i32::MAX] {
        for size in [0usize, 1, 2, 255, 256, 257, 10_000] {
            let packet = patterned_bytes(size);
            let compression = Compression::new(threshold, -1).unwrap();
            let frame = compression.encode_frame(&packet).unwrap();
            let (consumed, decoded) = compression.decode_frame(&frame).unwrap();
            assert_eq!(
                consumed,
                frame.len(),
                "consumed at threshold {threshold} size {size}"
            );
            assert_eq!(
                decoded, packet,
                "payload at threshold {threshold} size {size}"
            );
        }
    }
}

#[test]
fn roundtrip_at_max_unpacked() {
    let packet = patterned_bytes(MAX_UNPACKED);
    let compression = Compression::new(DEFAULT_THRESHOLD, -1).unwrap();
    let frame = compression.encode_frame(&packet).unwrap();
    let (consumed, decoded) = compression.decode_frame(&frame).unwrap();
    assert_eq!(consumed, frame.len());
    assert_eq!(decoded, packet);
}

#[test]
fn one_mib_compressible_frame_shrinks() {
    let packet = patterned_bytes(1_048_576);
    let compression = Compression::new(DEFAULT_THRESHOLD, -1).unwrap();
    let frame = compression.encode_frame(&packet).unwrap();
    assert!(
        frame.len() < packet.len() / 2,
        "expected the compressible payload to shrink substantially, got {} from {}",
        frame.len(),
        packet.len()
    );
    let (consumed, decoded) = compression.decode_frame(&frame).unwrap();
    assert_eq!(consumed, frame.len());
    assert_eq!(decoded, packet);
}

#[test]
fn uncompressed_fast_path_emits_data_length_zero() {
    let packet = patterned_bytes(100);
    let compression = Compression::new(DEFAULT_THRESHOLD, -1).unwrap();
    let frame = compression.encode_frame(&packet).unwrap();

    // packet_length = 1 (Data Length varint) + 100 = 101 = 0x65
    let mut expected = vec![0x65, 0x00];
    expected.extend_from_slice(&packet);
    assert_eq!(frame, expected);

    let (packet_len, data_length, payload) = split_frame(&frame);
    assert_eq!(packet_len, 101);
    assert_eq!(data_length, 0);
    assert_eq!(payload, packet.as_slice());
}

#[test]
fn threshold_zero_compresses_nonempty_packet() {
    let packet = patterned_bytes(10);
    let compression = Compression::new(0, -1).unwrap();
    let frame = compression.encode_frame(&packet).unwrap();

    let (packet_len, data_length, _) = split_frame(&frame);
    assert_eq!(
        data_length, 10,
        "a packet at threshold 0 must be compressed"
    );
    assert!(
        i64::from(packet_len) > 11,
        "compressed stream must outlive the empty layout"
    );

    let (consumed, decoded) = compression.decode_frame(&frame).unwrap();
    assert_eq!(consumed, frame.len());
    assert_eq!(decoded, packet);
}

#[test]
fn empty_packet_is_always_emitted_uncompressed() {
    // With threshold 0 even an empty packet would be "at" the threshold,
    // but Data Length = 0 is the uncompressed marker, so a compressed
    // empty packet could not be told apart from one on the wire.
    let compression = Compression::new(0, -1).unwrap();
    let frame = compression.encode_frame(&[]).unwrap();
    assert_eq!(frame, vec![0x01, 0x00]);

    let (consumed, decoded) = compression.decode_frame(&frame).unwrap();
    assert_eq!(consumed, 2);
    assert!(decoded.is_empty());
}

#[test]
fn threshold_minus_one_never_compresses() {
    let packet = patterned_bytes(1_048_576);
    let compression = Compression::new(-1, -1).unwrap();
    let frame = compression.encode_frame(&packet).unwrap();

    let (_, data_length, payload) = split_frame(&frame);
    assert_eq!(data_length, 0);
    assert_eq!(payload, packet.as_slice());
}

#[test]
fn max_unpacked_is_two_mib() {
    assert_eq!(MAX_UNPACKED, 2 * 1024 * 1024);
    assert_eq!(max_unpacked(), MAX_UNPACKED);
}

#[test]
fn zip_bomb_declared_over_cap_rejected_before_decompress() {
    // The payload looks like a valid zlib stream header; the declared
    // Data Length is what triggers the rejection, before any of it is
    // fed into the decoder.
    for declared in [MAX_UNPACKED as i32 + 1, i32::MAX] {
        let frame = build_frame(declared, &[0x78, 0x9c]);
        let err = Compression::new(DEFAULT_THRESHOLD, -1)
            .unwrap()
            .decode_frame(&frame)
            .unwrap_err();
        match err {
            CompressionError::UnpackedTooLarge { declared: got, max } => {
                assert_eq!(got, i64::from(declared));
                assert_eq!(max, MAX_UNPACKED);
            }
            other => panic!("expected UnpackedTooLarge, got {other:?}"),
        }
    }
}

#[test]
fn exactly_max_unpacked_is_accepted_by_cap_check() {
    // The cap admits exactly MAX_UNPACKED; failures below the zlib layer
    // are fine, an UnpackedTooLarge is not.
    let frame = build_frame(MAX_UNPACKED as i32, &[]);
    let err = Compression::new(DEFAULT_THRESHOLD, -1)
        .unwrap()
        .decode_frame(&frame)
        .unwrap_err();
    assert!(
        !matches!(err, CompressionError::UnpackedTooLarge { .. }),
        "the cap must be inclusive, got {err:?}"
    );
}

#[test]
fn truncated_every_prefix_is_rejected() {
    for size in [0usize, 10, 300] {
        let packet = patterned_bytes(size);
        let compression = Compression::new(DEFAULT_THRESHOLD, -1).unwrap();
        let frame = compression.encode_frame(&packet).unwrap();
        for prefix in 0..frame.len() {
            let err = compression.decode_frame(&frame[..prefix]).unwrap_err();
            assert!(
                matches!(err, CompressionError::Truncated),
                "prefix of {prefix} bytes of a {size}-byte packet: expected Truncated, got {err:?}"
            );
        }
    }
}

#[test]
fn packet_length_prefix_must_fit_three_bytes() {
    // 4-byte VarInt for the value 0
    let frame = &[0x80, 0x80, 0x80, 0x00];
    let err = Compression::new(DEFAULT_THRESHOLD, -1)
        .unwrap()
        .decode_frame(frame)
        .unwrap_err();
    assert!(matches!(err, CompressionError::PacketLengthTooLarge));

    // 4-byte VarInt for 2097152 = 0x10 << 21
    let frame = &[0x80, 0x80, 0x80, 0x10];
    let err = Compression::new(DEFAULT_THRESHOLD, -1)
        .unwrap()
        .decode_frame(frame)
        .unwrap_err();
    assert!(matches!(err, CompressionError::PacketLengthTooLarge));
}

#[test]
fn packet_length_at_wire_limit_is_accepted() {
    // 0xFF 0xFF 0x7F = 2097151, exactly the 3-byte VarInt maximum; the
    // frame body is missing, so the error must be Truncated, not
    // PacketLengthTooLarge.
    let frame = &[0xFF, 0xFF, 0x7F];
    let err = Compression::new(DEFAULT_THRESHOLD, -1)
        .unwrap()
        .decode_frame(frame)
        .unwrap_err();
    assert!(matches!(err, CompressionError::Truncated));
}

#[test]
fn data_length_mismatch_is_rejected() {
    let packet = patterned_bytes(100);
    let mut encoder = ZlibEncoder::new(Vec::new(), Level::new(6));
    encoder.write_all(&packet).unwrap();
    let compressed = encoder.finish().unwrap();

    // declared smaller than the actual output: the decoder is stopped one
    // byte past the declaration, so `actual` is `declared + 1`
    let frame = build_frame(50, &compressed);
    let err = Compression::new(DEFAULT_THRESHOLD, -1)
        .unwrap()
        .decode_frame(&frame)
        .unwrap_err();
    assert!(matches!(
        err,
        CompressionError::DataLengthMismatch {
            declared: 50,
            actual: 51
        }
    ));

    // declared larger than the actual output
    let frame = build_frame(150, &compressed);
    let err = Compression::new(DEFAULT_THRESHOLD, -1)
        .unwrap()
        .decode_frame(&frame)
        .unwrap_err();
    assert!(matches!(
        err,
        CompressionError::DataLengthMismatch {
            declared: 150,
            actual: 100
        }
    ));
}

#[test]
fn trailing_bytes_are_left_unconsumed() {
    let packet = patterned_bytes(300);
    let compression = Compression::new(DEFAULT_THRESHOLD, -1).unwrap();
    let frame = compression.encode_frame(&packet).unwrap();

    let mut with_garbage = frame.clone();
    with_garbage.extend_from_slice(&[0xDE, 0xAD]);
    let (consumed, decoded) = compression.decode_frame(&with_garbage).unwrap();
    assert_eq!(consumed, frame.len());
    assert_eq!(decoded, packet);
}

#[test]
fn varint_too_big_in_data_length_position() {
    // packet_length = 5, then a 5-byte VarInt whose fifth byte keeps the
    // continuation bit: not a legal Minecraft VarInt.
    let frame = &[0x05, 0xFF, 0xFF, 0xFF, 0xFF, 0x8F];
    let err = Compression::new(DEFAULT_THRESHOLD, -1)
        .unwrap()
        .decode_frame(frame)
        .unwrap_err();
    assert!(matches!(err, CompressionError::VarIntTooBig));
}

#[test]
fn level_boundaries_and_out_of_range() {
    for level in [-1, 0, 1, 6, 9] {
        assert!(
            Compression::new(DEFAULT_THRESHOLD, level).is_ok(),
            "level {level}"
        );
    }
    for level in [i32::MIN, -2, 10, i32::MAX] {
        let err = Compression::new(DEFAULT_THRESHOLD, level).unwrap_err();
        assert!(matches!(err, CompressionError::InvalidLevel { level: got } if got == level));
    }
}

#[test]
fn threshold_boundaries_and_out_of_range() {
    for threshold in [-1, 0, 1, DEFAULT_THRESHOLD, i32::MAX] {
        assert!(
            Compression::new(threshold, -1).is_ok(),
            "threshold {threshold}"
        );
    }
    for threshold in [i32::MIN, -2] {
        let err = Compression::new(threshold, -1).unwrap_err();
        assert!(
            matches!(err, CompressionError::InvalidThreshold { threshold: got } if got == threshold)
        );
    }
}

#[test]
fn encode_validates_even_directly_constructed_instances() {
    let compression = Compression {
        threshold: DEFAULT_THRESHOLD,
        level: 42,
    };
    assert!(matches!(
        compression.encode_frame(b"x"),
        Err(CompressionError::InvalidLevel { level: 42 })
    ));

    let compression = Compression {
        threshold: -100,
        level: 0,
    };
    assert!(matches!(
        compression.encode_frame(b"x"),
        Err(CompressionError::InvalidThreshold { threshold: -100 })
    ));
}

#[test]
fn decode_ignores_threshold_and_level() {
    // Decoding does not consult the configuration: frames are parsed
    // against the format, not against what the sender meant.
    let compression = Compression {
        threshold: i32::MIN,
        level: i32::MAX,
    };
    let packet = patterned_bytes(100);
    let encoder = Compression::new(DEFAULT_THRESHOLD, -1).unwrap();
    let frame = encoder.encode_frame(&packet).unwrap();
    let (consumed, decoded) = compression.decode_frame(&frame).unwrap();
    assert_eq!(consumed, frame.len());
    assert_eq!(decoded, packet);
}

#[test]
fn encode_refuses_packet_over_max_unpacked() {
    let packet = patterned_bytes(MAX_UNPACKED + 1);
    let err = Compression::new(DEFAULT_THRESHOLD, -1)
        .unwrap()
        .encode_frame(&packet)
        .unwrap_err();
    match err {
        CompressionError::UnpackedTooLarge { declared, max } => {
            assert_eq!(declared, MAX_UNPACKED as i64 + 1);
            assert_eq!(max, MAX_UNPACKED);
        }
        other => panic!("expected UnpackedTooLarge, got {other:?}"),
    }
}

#[test]
fn encode_refuses_frame_over_wire_limit() {
    // 2 MiB of xorshift64 output is incompressible; with level 0 the zlib
    // stream is strictly larger than the input, so the frame body exceeds
    // the 2097151-byte wire limit and must be refused instead of emitting
    // a frame that vanilla peers reject.
    let mut state = 0x9E37_79B9_7F4A_7C15_u64;
    let raw: Vec<u8> = (0..MAX_UNPACKED)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state >> 32) as u8
        })
        .collect();
    let err = Compression::new(0, 0)
        .unwrap()
        .encode_frame(&raw)
        .unwrap_err();
    assert!(matches!(err, CompressionError::PacketLengthTooLarge));
}

#[test]
fn golden_minecraft_packet_decodes() {
    // Packet ID 0x00 followed by a repeating 0..=255 sequence and the
    // marker string; compressed by canonical zlib at level 6, wrapped in
    // the wire format by hand: Packet Length = 0x02B7 (311),
    // Data Length = 0x0897 (1047).
    const GOLDEN_ZLIB: &[u8] = &[
        0x78, 0x9c, 0x63, 0x60, 0x64, 0x62, 0x66, 0x61, 0x65, 0x63, 0xe7, 0xe0, 0xe4, 0xe2, 0xe6,
        0xe1, 0xe5, 0xe3, 0x17, 0x10, 0x14, 0x12, 0x16, 0x11, 0x15, 0x13, 0x97, 0x90, 0x94, 0x92,
        0x96, 0x91, 0x95, 0x93, 0x57, 0x50, 0x54, 0x52, 0x56, 0x51, 0x55, 0x53, 0xd7, 0xd0, 0xd4,
        0xd2, 0xd6, 0xd1, 0xd5, 0xd3, 0x37, 0x30, 0x34, 0x32, 0x36, 0x31, 0x35, 0x33, 0xb7, 0xb0,
        0xb4, 0xb2, 0xb6, 0xb1, 0xb5, 0xb3, 0x77, 0x70, 0x74, 0x72, 0x76, 0x71, 0x75, 0x73, 0xf7,
        0xf0, 0xf4, 0xf2, 0xf6, 0xf1, 0xf5, 0xf3, 0x0f, 0x08, 0x0c, 0x0a, 0x0e, 0x09, 0x0d, 0x0b,
        0x8f, 0x88, 0x8c, 0x8a, 0x8e, 0x89, 0x8d, 0x8b, 0x4f, 0x48, 0x4c, 0x4a, 0x4e, 0x49, 0x4d,
        0x4b, 0xcf, 0xc8, 0xcc, 0xca, 0xce, 0xc9, 0xcd, 0xcb, 0x2f, 0x28, 0x2c, 0x2a, 0x2e, 0x29,
        0x2d, 0x2b, 0xaf, 0xa8, 0xac, 0xaa, 0xae, 0xa9, 0xad, 0xab, 0x6f, 0x68, 0x6c, 0x6a, 0x6e,
        0x69, 0x6d, 0x6b, 0xef, 0xe8, 0xec, 0xea, 0xee, 0xe9, 0xed, 0xeb, 0x9f, 0x30, 0x71, 0xd2,
        0xe4, 0x29, 0x53, 0xa7, 0x4d, 0x9f, 0x31, 0x73, 0xd6, 0xec, 0x39, 0x73, 0xe7, 0xcd, 0x5f,
        0xb0, 0x70, 0xd1, 0xe2, 0x25, 0x4b, 0x97, 0x2d, 0x5f, 0xb1, 0x72, 0xd5, 0xea, 0x35, 0x6b,
        0xd7, 0xad, 0xdf, 0xb0, 0x71, 0xd3, 0xe6, 0x2d, 0x5b, 0xb7, 0x6d, 0xdf, 0xb1, 0x73, 0xd7,
        0xee, 0x3d, 0x7b, 0xf7, 0xed, 0x3f, 0x70, 0xf0, 0xd0, 0xe1, 0x23, 0x47, 0x8f, 0x1d, 0x3f,
        0x71, 0xf2, 0xd4, 0xe9, 0x33, 0x67, 0xcf, 0x9d, 0xbf, 0x70, 0xf1, 0xd2, 0xe5, 0x2b, 0x57,
        0xaf, 0x5d, 0xbf, 0x71, 0xf3, 0xd6, 0xed, 0x3b, 0x77, 0xef, 0xdd, 0x7f, 0xf0, 0xf0, 0xd1,
        0xe3, 0x27, 0x4f, 0x9f, 0x3d, 0x7f, 0xf1, 0xf2, 0xd5, 0xeb, 0x37, 0x6f, 0xdf, 0xbd, 0xff,
        0xf0, 0xf1, 0xd3, 0xe7, 0x2f, 0x5f, 0xbf, 0x7d, 0xff, 0xf1, 0xf3, 0xd7, 0xef, 0x3f, 0x7f,
        0xff, 0xfd, 0x67, 0x18, 0xf5, 0xff, 0xa8, 0xff, 0x47, 0xb0, 0xff, 0x93, 0x13, 0xf3, 0x52,
        0x2a, 0x8b, 0x32, 0x2b, 0x13, 0x15, 0xd2, 0xf3, 0x73, 0x52, 0x52, 0xf3, 0x14, 0x0a, 0x12,
        0x93, 0xb3, 0x53, 0x4b, 0x00, 0x26, 0x11, 0x07, 0x14,
    ];

    assert_eq!(GOLDEN_ZLIB.len(), 309);

    let mut frame = vec![0xB7, 0x02, 0x97, 0x08];
    frame.extend_from_slice(GOLDEN_ZLIB);

    let compression = Compression::new(DEFAULT_THRESHOLD, -1).unwrap();
    let (consumed, decoded) = compression.decode_frame(&frame).unwrap();
    assert_eq!(consumed, frame.len());
    assert_eq!(decoded, golden_packet());

    // packet_length prefix is 0x02B7 = 311, the body is 311 bytes
    assert_eq!(frame[..2], [0xB7, 0x02]);
    assert_eq!((frame[0] & 0x7F) as i32 | ((frame[1] as i32) << 7), 311);
    assert_eq!(frame[2..4], [0x97, 0x08]);
    assert_eq!(frame.len() - 4, GOLDEN_ZLIB.len());

    // a trailing garbage byte must not affect the result, it just stays unconsumed
    let mut with_garbage = frame.clone();
    with_garbage.push(0xEE);
    let (consumed, decoded) = compression.decode_frame(&with_garbage).unwrap();
    assert_eq!(consumed, frame.len());
    assert_eq!(decoded, golden_packet());

    // sanity: our own encoder output is accepted by our decoder (the
    // golden vector itself is produced by canonical zlib)
    let encoded = Compression::new(DEFAULT_THRESHOLD, 6)
        .unwrap()
        .encode_frame(&golden_packet())
        .unwrap();
    let (consumed, decoded) = Compression::new(DEFAULT_THRESHOLD, -1)
        .unwrap()
        .decode_frame(&encoded)
        .unwrap();
    assert_eq!(consumed, encoded.len());
    assert_eq!(decoded, golden_packet());
}
