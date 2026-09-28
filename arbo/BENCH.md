# arbo benchmarks — Rust vs Go

Same workloads on both sides: 64-level SHA-256 tree, 8-byte keys, 32-byte
values. Go side is `testdata/gen -bench` (vocdoni/arbo on goleveldb-style
memdb and pebble with non-synced commits); Rust side is
`ARBO_BENCH_1M=1 cargo bench -p arbo` (criterion, sample size 10, redb with
`Durability::None` to match pebble's non-synced writes). Machine: 32-core
linux host, both runs under `systemd-run --scope -p MemoryMax=16G`,
2026-09-26.

Go numbers are the range over 4 runs; Rust numbers are the criterion mean.

| workload | Go memdb | Rust memory | Go pebble | Rust redb |
|---|---:|---:|---:|---:|
| add loop, 10k leaves | 93–117 ms | **41.8 ms** | 304–318 ms | 551 ms |
| add_batch 10k | 12.7–16.1 ms | **6.8 ms** | 15.4–16.8 ms | **15.5 ms** |
| add_batch 100k | 214 ms | **140 ms** | 211 ms | **191 ms** |
| add_batch 1M | 2.59 s | **2.29 s** | 4.19 s | **3.08 s** |
| gen_proof, 10k over 100k tree | 42.7 ms | **12.1 ms** | 213 ms | **109 ms** |
| add_batch 1k into 1M-leaf tree | n/a¹ | **19.2 ms** | 645 ms | **91.3 ms** |

¹ Go arbo's `addBatchInDisk` (taken above 65 536 leafs) only supports
pebble; on memdb it panics with `unsupported WriteTx type`.

The "1k into 1M" row uses the incremental `add_batch` path:
on a populated tree the batch is applied as sequential adds through an
in-memory overlay and committed once, touching only the ~log(n) nodes per
key instead of reloading and rehashing the whole tree. The Rust memory
mean is noisy (9.8–32.7 ms across samples) because the tree keeps growing
across criterion iterations; the redb row is stable.

Rust beats Go on 9 of 10 rows. What makes the redb rows fast:

- reads reuse one cached `ReadTransaction` snapshot, invalidated on write
  (gen_proof −23%);
- the snapshot is dropped *before* the write transaction so the commit
  doesn't CoW pages pinned by an open reader;
- batch puts are inserted in sorted key order (rayon parallel sort ≥ 4096
  pairs), which cuts b-tree page churn (add_batch 100k/1M ≈ −20%).

## The one red row: redb add loop

10k individual `add` calls = 10k commits. A micro-profile isolates the
storage layer: 10k empty redb commits cost 25 ms (2.5 µs each), but 10k
commits of ~15 random-key inserts cost 442 ms (44 µs each) — pure redb, no
tree logic. redb is a copy-on-write b-tree: every commit rewrites the page
path for each touched key even at `Durability::None`, while a pebble
non-synced commit is a memtable insert plus a buffered WAL append. That
gap is structural to the backend pairing, not to this port: Rust tree
logic on the same workload is 42 ms (memory row) vs Go's 93–117 ms, and
raising the redb cache to 2 GB made the commit loop slower, not faster.

Per-leaf commit loops are not the bulk path; `add_batch` is, and it wins
at every size. If per-add commit latency ever matters, the fix is a
different storage backend (LSM), not tree-side tuning.
