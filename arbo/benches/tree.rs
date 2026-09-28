//! Criterion benches mirroring the Go workloads in `testdata/gen -bench`:
//! 64-level tree, 8-byte keys, 32-byte values, on MemoryStorage and
//! RedbStorage (durability None, matching pebbledb's non-synced commits).

use std::sync::Arc;
use std::time::Duration;

use arbo::{MemoryStorage, RedbStorage, Sha256, Storage, Tree};
use criterion::{BatchSize, Criterion, criterion_group, criterion_main};
use sha2::Digest;

const LEVELS: usize = 64;

fn kvs(n: usize) -> Vec<(Vec<u8>, Vec<u8>)> {
    (0..n)
        .map(|i| {
            let h: [u8; 32] = sha2::Sha256::digest(format!("bench-{i}")).into();
            let v: [u8; 32] = sha2::Sha256::digest(h).into();
            (h[..8].to_vec(), v.to_vec())
        })
        .collect()
}

struct RedbEnv {
    _dir: tempfile::TempDir,
    db: Arc<redb::Database>,
    n: std::cell::Cell<usize>,
}

impl RedbEnv {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(redb::Database::create(dir.path().join("bench.redb")).unwrap());
        Self {
            _dir: dir,
            db,
            n: std::cell::Cell::new(0),
        }
    }

    fn storage(&self) -> RedbStorage {
        let n = self.n.get() + 1;
        self.n.set(n);
        let mut s = RedbStorage::new(self.db.clone(), &format!("t{n}")).unwrap();
        s.set_durability(redb::Durability::None);
        s
    }
}

fn bench_backend<S, F>(c: &mut Criterion, name: &str, mk: F)
where
    S: Storage + 'static,
    F: Fn() -> S,
{
    let mut g = c.benchmark_group(name);
    g.sample_size(10).measurement_time(Duration::from_secs(20));

    // add loop, 10k
    let data = kvs(10_000);
    g.bench_function("add_loop_10k", |b| {
        b.iter_batched(
            || (Tree::new(mk(), LEVELS, Sha256).unwrap(), data.clone()),
            |(mut t, data)| {
                for (k, v) in &data {
                    t.add(k, v).unwrap();
                }
                t.root()
            },
            BatchSize::PerIteration,
        )
    });

    // add_batch 10k / 100k / 1M
    let sizes: &[usize] = if std::env::var("ARBO_BENCH_1M").is_ok() {
        &[10_000, 100_000, 1_000_000]
    } else {
        &[10_000, 100_000]
    };
    for &n in sizes {
        let data = kvs(n);
        g.bench_function(format!("add_batch_{n}"), |b| {
            b.iter_batched(
                || (Tree::new(mk(), LEVELS, Sha256).unwrap(), data.clone()),
                |(mut t, data)| {
                    let inv = t.add_batch(&data).unwrap();
                    assert!(inv.is_empty());
                    t.root()
                },
                BatchSize::PerIteration,
            )
        });
    }

    // add_batch: 1k new keys into a populated 1M-leaf tree (incremental
    // path). Short measurement window so tree growth across iterations
    // stays negligible; keys are fresh each round.
    if std::env::var("ARBO_BENCH_1M").is_ok() {
        let mut t = Tree::new(mk(), LEVELS, Sha256).unwrap();
        t.add_batch(&kvs(1_000_000)).unwrap();
        let mut round = 0usize;
        g.measurement_time(Duration::from_secs(2))
            .warm_up_time(Duration::from_millis(500));
        g.bench_function("add_batch_1k_into_1M", |b| {
            b.iter(|| {
                let extra: Vec<(Vec<u8>, Vec<u8>)> = (0..1000)
                    .map(|i| {
                        let h: [u8; 32] = sha2::Sha256::digest(format!("extra-{round}-{i}")).into();
                        let v: [u8; 32] = sha2::Sha256::digest(h).into();
                        (h[..8].to_vec(), v.to_vec())
                    })
                    .collect();
                round += 1;
                let inv = t.add_batch(&extra).unwrap();
                assert!(inv.is_empty());
            })
        });
        g.measurement_time(Duration::from_secs(20))
            .warm_up_time(Duration::from_secs(3));
    }

    // gen_proof: 10k proofs over a 100k tree
    let data = kvs(100_000);
    let mut t = Tree::new(mk(), LEVELS, Sha256).unwrap();
    t.add_batch(&data).unwrap();
    g.bench_function("gen_proof_10k_over_100k", |b| {
        b.iter(|| {
            for i in 0..10_000usize {
                let (k, _) = &data[(i * 7919) % data.len()];
                std::hint::black_box(t.gen_proof(k).unwrap());
            }
        })
    });

    g.finish();
}

fn benches(c: &mut Criterion) {
    bench_backend(c, "memory", MemoryStorage::new);
    let env = RedbEnv::new();
    bench_backend(c, "redb", || env.storage());
}

criterion_group!(benches_group, benches);
criterion_main!(benches_group);
