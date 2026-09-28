//! Circom-facing proofs: the verifier proof matching Go arbo's
//! `GenerateCircomVerifierProof` (quirks included) and the SMTProcessor
//! transition proofs matching davinci-zkvm's `go-sdk/chain/smt.go`.

use crate::error::Error;
use crate::hash::{EMPTY_HASH, Hash, HashFunction};
use crate::storage::Storage;
use crate::tree::Tree;

/// Go arbo `CircomVerifierProof` (SMTVerifier input). Quirks kept from Go:
/// `is_old0` is never set, and `old_key`/`old_value` are `[0]` on inclusion.
#[derive(Debug, Clone)]
pub struct CircomVerifierProof {
    pub root: Hash,
    /// Padded with empty siblings to `max_levels`.
    pub siblings: Vec<Hash>,
    pub old_key: Vec<u8>,
    pub old_value: Vec<u8>,
    pub is_old0: bool,
    pub key: Vec<u8>,
    pub value: Vec<u8>,
    /// 0 = inclusion, 1 = non-inclusion.
    pub fnc: u8,
}

/// circomlib SMTProcessor transition proof (insert fnc=1,0 / update fnc=0,1),
/// the shape the zkVM guest's `verify_transition` consumes.
#[derive(Debug, Clone)]
pub struct CircomProcessorProof {
    pub old_root: Hash,
    pub new_root: Hash,
    /// Padded with empty siblings to `max_levels`.
    pub siblings: Vec<Hash>,
    pub old_key: Vec<u8>,
    pub old_value: Vec<u8>,
    pub is_old0: bool,
    pub new_key: Vec<u8>,
    pub new_value: Vec<u8>,
    pub fnc0: bool,
    pub fnc1: bool,
}

impl<S: Storage, H: HashFunction> Tree<S, H> {
    /// Go `GenerateCircomVerifierProof`.
    pub fn circom_verifier_proof(&self, k: &[u8]) -> Result<CircomVerifierProof, Error> {
        let p = self.gen_proof(k)?;
        let mut siblings = p.siblings;
        siblings.resize(self.max_levels, EMPTY_HASH);
        let (old_key, old_value) = if p.exists {
            // Go quirk: emptyValue ([0x00]) for both on inclusion.
            (vec![0u8], vec![0u8])
        } else {
            (p.key, p.value.clone())
        };
        Ok(CircomVerifierProof {
            root: self.root(),
            siblings,
            old_key,
            old_value,
            is_old0: false, // Go quirk: never set.
            key: k.to_vec(),
            value: p.value,
            fnc: if p.exists { 0 } else { 1 },
        })
    }

    /// Insert `k` and return the SMTProcessor insert proof (fnc = 1,0),
    /// exactly as `buildArboInsertEntry` in go-sdk/chain/smt.go.
    pub fn insert_with_proof(&mut self, k: &[u8], v: &[u8]) -> Result<CircomProcessorProof, Error> {
        // Proof BEFORE insertion detects the displaced leaf.
        let before = self.gen_proof(k)?;
        if before.exists {
            return Err(Error::KeyAlreadyExists);
        }
        let is_old0 = before.key.is_empty();
        let (old_key, old_value) = if is_old0 {
            (vec![0u8; 32], vec![0u8; 32])
        } else {
            (before.key, before.value)
        };

        let old_root = self.root();
        self.add(k, v)?;
        let after = self.gen_proof(k)?;

        let mut siblings = after.siblings;
        // Drop the last sibling when displacing an existing leaf
        // (pure-insert mode of the processor circuit).
        if !is_old0 && !siblings.is_empty() {
            siblings.pop();
        }
        siblings.resize(self.max_levels, EMPTY_HASH);

        Ok(CircomProcessorProof {
            old_root,
            new_root: self.root(),
            siblings,
            old_key,
            old_value,
            is_old0,
            new_key: k.to_vec(),
            new_value: v.to_vec(),
            fnc0: true,
            fnc1: false,
        })
    }

    /// Update `k` and return the SMTProcessor update proof (fnc = 0,1);
    /// siblings come from the proof BEFORE the update.
    pub fn update_with_proof(&mut self, k: &[u8], v: &[u8]) -> Result<CircomProcessorProof, Error> {
        let before = self.gen_proof(k)?;
        if !before.exists {
            return Err(Error::KeyNotFound);
        }
        let old_root = self.root();
        self.update(k, v)?;

        let mut siblings = before.siblings;
        siblings.resize(self.max_levels, EMPTY_HASH);

        Ok(CircomProcessorProof {
            old_root,
            new_root: self.root(),
            siblings,
            old_key: k.to_vec(),
            old_value: before.value,
            is_old0: false,
            new_key: k.to_vec(),
            new_value: v.to_vec(),
            fnc0: false,
            fnc1: true,
        })
    }
}
