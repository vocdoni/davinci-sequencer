//! arbo: sparse Merkle tree compatible with Go vocdoni/arbo — same node
//! encodings, packed-sibling wire format, dumps and circomlib SMT proofs.
//! The DAVINCI state tree (SHA-256, 64 levels, 8-byte keys) lives on it and
//! its proofs are consumed byte-for-byte by the zkVM guest.
//!
//! Storage layout: nodes are content-addressed under their 32-byte hash
//! (leaf `[0x01, key_len u8, key, value]`, intermediate `[0x02, 32, l, r]`,
//! immutable, never deleted). Three mutable metadata keys sit beside them:
//! `b"root"` (32 bytes), `b"nleafs"` (u64 LE) and `b"params"`
//! (`[hash_id u8 | max_levels u32 LE]`, written once when a tree is created
//! on empty storage; reopening with different parameters, or opening a tree
//! that has a root but no params, fails with `ParamsMismatch`).
#![forbid(unsafe_code)]

mod circom;
mod dump;
mod error;
mod hash;
mod proof;
mod storage;
mod tree;
mod vt;

pub use circom::{CircomProcessorProof, CircomVerifierProof};
pub use error::Error;
pub use hash::{EMPTY_HASH, Hash, HashFunction, Sha256};
pub use proof::{Proof, check_proof, pack_siblings, unpack_siblings};
#[cfg(feature = "redb")]
pub use storage::RedbStorage;
pub use storage::{MemoryStorage, Storage, WriteBatch};
pub use tree::{Invalid, Tree};
