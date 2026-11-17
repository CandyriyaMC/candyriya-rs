//! Lookup throughput for the protocol registry hot path.
//!
//! The proxy classifies every status/login packet a connection sends; a hit
//! has to stay comfortably inside the sub-microsecond budget, a miss has to
//! be just as cheap because the play state lands in the same tables' `else`
//! arm.

use std::collections::HashSet;
use std::hint::black_box;

use criterion::{Criterion, Throughput, criterion_group, criterion_main};

use candyriya_protocol_registry::{ConnectionState, Direction, PacketSlot, Registry};

fn registry() -> Registry {
    Registry::embedded().expect("the embedded snapshot is valid")
}

fn packet_id_lookup(c: &mut Criterion) {
    let registry = registry();
    let protocol = registry.latest().protocol;
    // a protocol with no table at all: every slot lookup must return None
    let unknown_protocol = i32::MAX;

    let mut group = c.benchmark_group("packet_id_lookup");
    group.throughput(Throughput::Elements(1));
    group.bench_function("hit", |b| {
        b.iter(|| {
            black_box(registry.packet_id(
                black_box(protocol),
                black_box(PacketSlot::LoginServerboundStart),
            ))
        })
    });
    group.bench_function("miss", |b| {
        b.iter(|| {
            black_box(registry.packet_id(
                black_box(unknown_protocol),
                black_box(PacketSlot::LoginServerboundStart),
            ))
        })
    });
    group.finish();
}

fn classify_lookup(c: &mut Criterion) {
    let registry = registry();
    let version = registry.latest();
    let protocol = version.protocol;
    let start_id = registry
        .packet_id(protocol, PacketSlot::LoginServerboundStart)
        .expect("login start has no gap on the latest release");

    // pick an id no slot of this protocol uses, so the miss case exercises
    // the full scan without accidentally landing on a real slot
    let known: HashSet<i32> = PacketSlot::ALL
        .iter()
        .filter_map(|&slot| registry.packet_id(protocol, slot))
        .collect();
    let miss_id = (0..)
        .find(|id| !known.contains(id))
        .expect("some small id is unused");

    let mut group = c.benchmark_group("classify_login_serverbound");
    group.throughput(Throughput::Elements(1));
    group.bench_function("hit", |b| {
        b.iter(|| {
            black_box(registry.classify(
                black_box(ConnectionState::Login),
                black_box(Direction::Serverbound),
                black_box(version),
                black_box(start_id),
            ))
        })
    });
    group.bench_function("miss", |b| {
        b.iter(|| {
            black_box(registry.classify(
                black_box(ConnectionState::Login),
                black_box(Direction::Serverbound),
                black_box(version),
                black_box(miss_id),
            ))
        })
    });
    group.finish();
}

criterion_group!(benches, packet_id_lookup, classify_lookup);
criterion_main!(benches);
