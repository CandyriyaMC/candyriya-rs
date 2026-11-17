//! Sealing cost of the modern forwarding payload, parameterized by how many
//! signed properties the Mojang profile carries (1–4 is the typical range).

use std::hint::black_box;

use uuid::Uuid;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};

use candyriya_forwarding::create_forwarding_data;
use candyriya_player::{GameProfile, Property};

/// Raw forwarding secret bytes, exactly as configured.
const SECRET: &[u8] = b"bench-forwarding-secret-0123456789ab";
/// The client's IP as the proxy sees it (no port, as the format requires).
const ADDRESS: &str = "203.0.113.7";

// Mojang property values and signatures are base64 blobs; the sizes below
// match a real `textures` payload closely enough to time the byte copies.
const VALUE_LEN: usize = 336;
const SIGNATURE_LEN: usize = 344;

/// Property names Mojang actually attaches, in rough order of frequency.
const PROPERTY_NAMES: &[&str] = &["textures", "legacy_skin", "legacy_cape", "cape_signed"];

fn signed_profile(property_count: usize) -> GameProfile {
    let value = "v".repeat(VALUE_LEN);
    let signature = "s".repeat(SIGNATURE_LEN);
    let properties = PROPERTY_NAMES
        .iter()
        .take(property_count)
        .map(|&name| Property::signed(name, value.clone(), signature.clone()))
        .collect();
    GameProfile {
        id: Uuid::from_u128(0x123e_4567_e89b_12d3_a456_4266_1417_4000),
        name: "Notch".to_owned(),
        properties,
    }
}

fn forwarding_data(c: &mut Criterion) {
    let mut group = c.benchmark_group("forwarding_data_create");
    for property_count in 1..=4usize {
        let profile = signed_profile(property_count);
        let sealed_len = create_forwarding_data(SECRET, ADDRESS, &profile).len();
        group.throughput(Throughput::Bytes(sealed_len as u64));
        group.bench_with_input(
            BenchmarkId::new("signed_properties", property_count),
            &profile,
            |b, profile| {
                b.iter(|| {
                    black_box(create_forwarding_data(
                        black_box(SECRET),
                        black_box(ADDRESS),
                        black_box(profile),
                    ))
                })
            },
        );
    }
    group.finish();
}

criterion_group!(benches, forwarding_data);
criterion_main!(benches);
