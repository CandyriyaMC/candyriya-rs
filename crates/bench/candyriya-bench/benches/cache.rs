//! Buffer pool latency: take-release cycle through the pooled arena versus
//! allocating a fresh `Vec` every time. Pooled buffers are recycled on drop,
//! so the hot path should show the pool actually earning its keep.

use std::hint::black_box;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};

use candyriya_cache::CacheManager;

/// A couple cache lines' worth of payload — enough to touch real memory,
/// small enough to keep the loop tight.
const PAYLOAD: &[u8] = &[0xAB; 64];

fn buffer_pool(c: &mut Criterion) {
    // the manager needs a root; give it a throwaway dir that disappears with
    // the bench process
    let dir = tempfile::tempdir().expect("create temp dir for the cache root");
    let cache = CacheManager::new(dir.path()).expect("cache manager builds");

    let mut group = c.benchmark_group("buffer_pool");
    for size in [1024_usize, 4096, 16384] {
        group.throughput(Throughput::Elements(1));
        group.bench_with_input(
            BenchmarkId::new("acquire_release", size),
            &size,
            |b, &size| {
                b.iter(|| {
                    let mut buf = cache.buffer(black_box(size));
                    buf.extend_from_slice(black_box(PAYLOAD));
                    black_box(buf.len())
                })
            },
        );
        group.bench_with_input(
            BenchmarkId::new("vec_with_capacity", size),
            &size,
            |b, &size| {
                b.iter(|| {
                    let mut buf = Vec::with_capacity(black_box(size));
                    buf.extend_from_slice(black_box(PAYLOAD));
                    black_box(buf.len())
                })
            },
        );
    }
    group.finish();

    cache.shutdown().expect("cache manager shuts down");
}

criterion_group!(benches, buffer_pool);
criterion_main!(benches);
