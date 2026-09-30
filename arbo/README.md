# arbo

A binary sparse Merkle tree, ported from Go
[vocdoni/arbo](https://github.com/vocdoni/arbo). It keeps arbo's node encodings,
packed-sibling format, dump format and root for the same contents, so trees and
proofs move between the two implementations, and it produces circomlib `SMTVerifier`
and `SMTProcessor` proofs. The sequencer's state tree lives on it.

## Overview

- Keys are read LSB-first up to `max_levels` bits; a lone leaf in a subtree sits at
  its top. Leaf hash `H(k ‖ v ‖ 0x01)`, node hash `H(l ‖ r)`, empty = zero.
- Nodes are content-addressed and never deleted, so any past root can be reopened
  with `set_root` or read with `gen_proof_at`. That is how the sequencer rolls back
  a failed batch.
- `add_batch` builds arbo's virtual tree in parallel (rayon) and hashes each node
  once. On a populated tree it applies the batch through an in-memory overlay and
  commits once.
- Storage is a trait: `MemoryStorage`, or `RedbStorage` (feature `redb`, on by
  default) inside an existing redb database.
- `Sha256` is the only hash shipped; the Poseidon test vectors run behind the
  `sdk-poseidon` feature.
- `#![forbid(unsafe_code)]`; malformed storage and proof input give typed
  errors, not panics.

## Usage

```rust
use arbo::{MemoryStorage, Sha256, Tree, check_proof};

let mut t = Tree::new(MemoryStorage::new(), 64, Sha256)?;
t.add(&1u64.to_le_bytes(), &[7u8; 32])?;
let p = t.gen_proof(&1u64.to_le_bytes())?;
assert!(p.exists);
assert!(check_proof(&Sha256, &p.key, &p.value, &t.root(), &p.packed)?);

// circomlib processor proof for an update, as the zkVM guest consumes it.
let w = t.update_with_proof(&1u64.to_le_bytes(), &[8u8; 32])?;
```

## Development

The tests replay differential vectors written by the Go generator in
`testdata/gen` (SHA-256 at 64, 160 and 256 levels), check processor and verifier
proofs with a port of the zkVM guest's SMT verifier, and run proptests. Fuzz
targets for proof parsing, sibling unpacking, dumps and node decoding are in
`fuzz/` (`cargo +nightly fuzz run <target>`).

```bash
cargo test -p arbo
cargo test -p arbo --features sdk-poseidon   # arbo's circom Poseidon vectors
ARBO_BENCH_1M=1 cargo bench -p arbo          # adds the 1M-leaf rows
```

[BENCH.md](BENCH.md) compares it with Go arbo.

## License

GNU Affero General Public License v3.0 or later. See [LICENSE](../LICENSE).
