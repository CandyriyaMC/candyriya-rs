//! Throughput benchmarks for the wire-format codec: `VarInt` decoding in the
//! size classes the proxy actually sees, plus a full frame round trip for a
//! sample Login Start packet.

use std::hint::black_box;

use bytes::BytesMut;
use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use tokio_util::codec::{Decoder, Encoder};

use candyriya_protocol::{Packet, PacketCodec, decode_varint, write_string, write_varint};

/// VarInts decoded per iteration; big enough to amortize the loop overhead,
/// small enough to keep the inputs in cache.
const BATCH: usize = 1024;

/// Tiny deterministic PRNG so the "random i32" case is reproducible without a
/// dev-dependency on `rand`.
struct XorShift32(u32);

impl XorShift32 {
    fn next_i32(&mut self) -> i32 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.0 = x;
        x as i32
    }
}

fn encode_values(values: &[i32]) -> Vec<u8> {
    // 5 bytes is the widest VarInt, so this never reallocates mid-fill
    let mut buf = Vec::with_capacity(values.len() * 5);
    for &value in values {
        write_varint(value, &mut buf);
    }
    buf
}

fn decode_batch(encoded: &[u8]) -> i64 {
    let mut src = encoded;
    let mut acc = 0i64;
    while !src.is_empty() {
        let value = decode_varint(&mut src).expect("benchmark input is well-formed");
        acc = acc.wrapping_add(i64::from(value));
    }
    acc
}

fn varint_decode(c: &mut Criterion) {
    let small: Vec<i32> = (0..BATCH).map(|i| (i % 128) as i32).collect();
    // 128..=16_383: exactly the two-byte class
    let medium: Vec<i32> = (0..BATCH)
        .map(|i| 128 + ((i * 149) % 16_256) as i32)
        .collect();
    // i32::MAX and friends: the five-byte class
    let large: Vec<i32> = (0..BATCH).map(|i| i32::MAX - (i % 1024) as i32).collect();
    let mut rng = XorShift32(0x9E37_79B9);
    let random: Vec<i32> = (0..BATCH).map(|_| rng.next_i32()).collect();

    let mut group = c.benchmark_group("varint_decode");
    for (name, values) in [
        ("small_0_127", small),
        ("medium_128_16383", medium),
        ("large_i32_max", large),
        ("random_i32", random),
    ] {
        let encoded = encode_values(&values);
        group.throughput(Throughput::Elements(BATCH as u64));
        group.bench_function(name, |b| {
            b.iter(|| black_box(decode_batch(black_box(&encoded))))
        });
    }
    group.finish();
}

/// Serverbound Login Start: packet id 0x00 followed by the username string,
/// framed by `PacketCodec` on the wire.
fn login_start_packet() -> Packet {
    let mut payload = BytesMut::new();
    write_string(&mut payload, "Notch", 16).expect("usernames fit the String(16) field");
    Packet {
        id: 0x00,
        payload: payload.freeze(),
    }
}

fn packet_roundtrip(c: &mut Criterion) {
    let packet = login_start_packet();
    c.bench_function("packet_login_start_roundtrip", |b| {
        b.iter(|| {
            let mut codec = PacketCodec::new();
            let mut buf = BytesMut::new();
            codec
                .encode(black_box(packet.clone()), &mut buf)
                .expect("encoding a login start never fails");
            let decoded = codec
                .decode(&mut buf)
                .expect("decoding a well-formed frame never fails")
                .expect("the buffer holds a complete frame");
            black_box(decoded)
        })
    });
}

criterion_group!(benches, varint_decode, packet_roundtrip);
criterion_main!(benches);
