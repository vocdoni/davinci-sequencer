use sha2::{Digest, Sha256 as Sha2};

/// A node hash / node storage key. All arbo hash functions output 32 bytes.
pub type Hash = [u8; 32];

/// The empty node: 32 zero bytes, never stored.
pub const EMPTY_HASH: Hash = [0u8; 32];

/// Tree hash function. `parts` are hashed as arbo does: SHA-256 digests the
/// concatenation; Poseidon-style functions map each part to a field element.
pub trait HashFunction: Send + Sync + Clone {
    /// Function identifier (informational).
    fn id(&self) -> u8;
    /// Hash the given parts.
    fn hash(&self, parts: &[&[u8]]) -> Hash;
}

/// Arbo's `HashFunctionSha256`: SHA-256 over the concatenated parts.
#[derive(Clone, Copy, Default, Debug)]
pub struct Sha256;

impl HashFunction for Sha256 {
    fn id(&self) -> u8 {
        1
    }

    fn hash(&self, parts: &[&[u8]]) -> Hash {
        let mut h = Sha2::new();
        for p in parts {
            h.update(p);
        }
        h.finalize().into()
    }
}
