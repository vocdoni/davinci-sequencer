//! State-transition builder: turns a batch of verified votes into the exact
//! `/prove` request the zkVM guest accepts (davinci-zkvm go-sdk
//! `tests/integration/election.go` is the reference implementation).

use std::collections::BTreeSet;

use arbo::{CircomProcessorProof, Storage};
use davinci_zkvm_sdk::ballot::{Ballot, address_to_fr, ballot_leaf_hash, leaf_value_bytes};
use davinci_zkvm_sdk::blob::{self, TransitionBlobs, TransitionData};
use davinci_zkvm_sdk::census::CensusWitness;
use davinci_zkvm_sdk::crypto::field::{Fr, fr_to_dec, fr_to_le};
use davinci_zkvm_sdk::limits::{
    self, KEY_RESULTS, MAX_BATCH_SIZE, MAX_REFRESH, PROCESS_KEYS, SMT_LEVELS,
};
use davinci_zkvm_sdk::publics::BatchPublics;
use davinci_zkvm_sdk::reenc::{ReencChain, random_seed, reencrypt_ballot};
use davinci_zkvm_sdk::types::{
    BallotProofsJson, CensusProofJson, CspDataJson, CspProofJson, EcdsaSigJson, KzgJson,
    ProveRequest, ReencryptionEntryJson, ReencryptionJson, SmtEntryJson, StateTransitionJson, enc,
};
use rand::{CryptoRng, Rng, RngCore};

use crate::config::key_bytes;
use crate::error::Error;
use crate::process::ProcessState;
use crate::validate::VerifiedVote;

/// A built transition, ready to prove. `request` carries the private
/// witness (the re-encryption seed and the refresh selection), so `Debug`
/// redacts it and it must never be logged or persisted.
pub struct PreparedBatch {
    pub request: ProveRequest,
    pub blobs: TransitionBlobs,
    pub expected: BatchPublics,
    pub old_root: [u8; 32],
    pub new_root: [u8; 32],
    pub vote_ids: Vec<u64>,
    pub overwrites: usize,
    /// Refreshed slots, ascending.
    pub(crate) refresh_slots: Vec<u64>,
    /// New ballot values per touched slot (batch writes and refreshes).
    pub(crate) slot_writes: Vec<(u64, Ballot)>,
    /// Slots this batch occupies for the first time.
    pub(crate) new_slots: Vec<u64>,
    /// Accumulator after the batch.
    pub(crate) accumulator: Ballot,
}

impl std::fmt::Debug for PreparedBatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PreparedBatch")
            .field("old_root", &hex::encode(self.old_root))
            .field("new_root", &hex::encode(self.new_root))
            .field("votes", &self.vote_ids.len())
            .field("overwrites", &self.overwrites)
            .field("n_blobs", &self.blobs.blobs.len())
            .field("request", &"<redacted>")
            .finish()
    }
}

impl PreparedBatch {
    /// Every ballot slot the transition writes (batch slots and refreshes),
    /// ascending: the key list its blobs publish. Once those blobs may be
    /// public, the caller keeps this as the exposed set and passes it back
    /// as `must_include` on every re-seal.
    pub fn updated_slots(&self) -> Vec<u64> {
        let mut s: Vec<u64> = self.slot_writes.iter().map(|(k, _)| *k).collect();
        s.sort_unstable();
        s
    }

    /// Test hook: the refresh subset of [`Self::updated_slots`]. Never
    /// persist or log it: next to the public key list it separates the
    /// batch's writes from the refreshes.
    #[doc(hidden)]
    pub fn refresh_slots(&self) -> &[u64] {
        &self.refresh_slots
    }

    /// Compares the guest's published registers against what this batch
    /// must produce; any mismatch means the proof is not for this batch.
    pub fn check_publics(&self, got: &BatchPublics) -> Result<(), Error> {
        let e = &self.expected;
        let field = |name: &str| Err(Error::PublicsMismatch(name.into()));
        if !got.ok || got.fail_mask != 0 {
            return Err(Error::PublicsMismatch(format!(
                "guest failed: mask {:#x}",
                got.fail_mask
            )));
        }
        if got.root_before != e.root_before {
            return field("root_before");
        }
        if got.root_after != e.root_after {
            return field("root_after");
        }
        if got.voters != e.voters {
            return field("voters");
        }
        if got.overwrites != e.overwrites {
            return field("overwrites");
        }
        if got.census_root != e.census_root {
            return field("census_root");
        }
        if got.blobs_digest != e.blobs_digest {
            return field("blobs_digest");
        }
        if got.n_blobs != e.n_blobs {
            return field("n_blobs");
        }
        if got.occupied_before != e.occupied_before {
            return field("occupied_before");
        }
        if got.nproofs != e.nproofs {
            return field("nproofs");
        }
        if got.n_public != e.n_public {
            return field("n_public");
        }
        if got.log_n != e.log_n {
            return field("log_n");
        }
        Ok(())
    }
}

fn pad32(b: &[u8]) -> [u8; 32] {
    let mut out = [0u8; 32];
    out[..b.len().min(32)].copy_from_slice(&b[..b.len().min(32)]);
    out
}

fn smt_entry(p: &CircomProcessorProof) -> SmtEntryJson {
    SmtEntryJson {
        old_root: enc::le_hex(&p.old_root),
        new_root: enc::le_hex(&p.new_root),
        old_key: enc::le_hex(&pad32(&p.old_key)),
        old_value: enc::le_hex(&pad32(&p.old_value)),
        is_old0: p.is_old0 as u8,
        new_key: enc::le_hex(&pad32(&p.new_key)),
        new_value: enc::le_hex(&pad32(&p.new_value)),
        fnc0: p.fnc0 as u8,
        fnc1: p.fnc1 as u8,
        siblings: p.siblings.iter().map(enc::le_hex).collect(),
    }
}

/// READ entry (fnc = 0,0) of an existing key under `root`.
fn read_entry(root: &[u8; 32], key: u64, p: &arbo::Proof) -> Result<SmtEntryJson, Error> {
    if !p.exists {
        return Err(Error::Invalid(format!("config key {key:#x} not in tree")));
    }
    let mut siblings = p.siblings.clone();
    siblings.resize(SMT_LEVELS, arbo::EMPTY_HASH);
    let k = enc::key_hex(key);
    let v = enc::le_hex(&pad32(&p.value));
    Ok(SmtEntryJson {
        old_root: enc::le_hex(root),
        new_root: enc::le_hex(root),
        old_key: k.clone(),
        old_value: v.clone(),
        is_old0: 0,
        new_key: k,
        new_value: v,
        fnc0: 0,
        fnc1: 0,
        siblings: siblings.iter().map(enc::le_hex).collect(),
    })
}

fn reenc_entry(original: &Ballot, reencrypted: &Ballot) -> ReencryptionEntryJson {
    ReencryptionEntryJson {
        original: std::array::from_fn(|i| enc::ciphertext_json(&original.0[i])),
        reencrypted: std::array::from_fn(|i| enc::ciphertext_json(&reencrypted.0[i])),
    }
}

const ZERO_HEX: &str = "0x0000000000000000000000000000000000000000000000000000000000000000";

impl<S: Storage> ProcessState<S> {
    /// `(candidates, w)`: occupied slots the batch does not write, and how
    /// many batch slots are overwrites.
    fn refresh_candidates(&self, votes: &[VerifiedVote]) -> (Vec<u64>, usize) {
        let slots: std::collections::HashSet<u64> = votes.iter().map(|v| v.slot).collect();
        let cands: Vec<u64> = self
            .occupied
            .iter()
            .copied()
            .filter(|s| !slots.contains(s))
            .collect();
        let w = self.occupied.len() - cands.len();
        (cands, w)
    }

    /// Refresh selection for `votes`: every `must_include` slot that
    /// is still a candidate, plus a uniform sample of the other candidates
    /// up to the guest's minimum. Ascending. `RefreshOverflow` when the
    /// kept slots push the transition past `MAX_REFRESH` or the blob cap.
    #[doc(hidden)]
    pub fn refresh_selection(
        &self,
        votes: &[VerifiedVote],
        rng: &mut (impl RngCore + CryptoRng),
        must_include: &BTreeSet<u64>,
    ) -> Result<Vec<u64>, Error> {
        let (cands, w) = self.refresh_candidates(votes);
        let n = votes.len();
        let required = limits::refresh_target(n, w).min(cands.len());
        let (mut sel, mut rest): (Vec<u64>, Vec<u64>) =
            cands.into_iter().partition(|s| must_include.contains(s));
        let kept = sel.len();
        if kept > required
            && (kept > MAX_REFRESH
                || blob::blob_count(n, n + kept, self.cfg.ballot_mode.num_fields) > self.blob_cap)
        {
            return Err(Error::RefreshOverflow {
                exposed: kept,
                votes: n,
            });
        }
        // Partial Fisher-Yates: uniform top-up from the other candidates.
        let top_up = required.saturating_sub(kept);
        for i in 0..top_up {
            let j = rng.gen_range(i..rest.len());
            rest.swap(i, j);
        }
        sel.extend_from_slice(&rest[..top_up]);
        sel.sort_unstable();
        Ok(sel)
    }

    /// Builds the transition for `votes`. The re-encryption seed and the
    /// refresh selection are drawn from `rng`; the selection keeps every
    /// `must_include` slot that is still a candidate (see
    /// [`PreparedBatch::updated_slots`]).
    pub fn prepare(
        &mut self,
        votes: &[VerifiedVote],
        rng: &mut (impl RngCore + CryptoRng),
        must_include: &BTreeSet<u64>,
    ) -> Result<PreparedBatch, Error> {
        let seed = random_seed(rng);
        let selection = self.refresh_selection(votes, rng, must_include)?;
        self.prepare_inner(votes, seed, selection, must_include)
    }

    /// Test hook: injected seed and refresh selection (sorted slot keys).
    #[doc(hidden)]
    pub fn prepare_with(
        &mut self,
        votes: &[VerifiedVote],
        seed: [u8; 32],
        refresh_selection: &[u64],
        must_include: &BTreeSet<u64>,
    ) -> Result<PreparedBatch, Error> {
        self.prepare_inner(votes, seed, refresh_selection.to_vec(), must_include)
    }

    fn prepare_inner(
        &mut self,
        votes: &[VerifiedVote],
        seed: [u8; 32],
        selection: Vec<u64>,
        must_include: &BTreeSet<u64>,
    ) -> Result<PreparedBatch, Error> {
        let old_root = self.tree.root();
        let r = self.build(votes, seed, selection, must_include, &old_root);
        if r.is_err() {
            // Best effort: drop the partial transition.
            let _ = self.tree.set_root(&old_root);
        }
        r
    }

    fn build(
        &mut self,
        votes: &[VerifiedVote],
        seed: [u8; 32],
        selection: Vec<u64>,
        must_include: &BTreeSet<u64>,
        old_root: &[u8; 32],
    ) -> Result<PreparedBatch, Error> {
        let cfg = self.cfg.clone();
        let nf = cfg.ballot_mode.num_fields;
        let n = votes.len();
        if n == 0 || n > MAX_BATCH_SIZE {
            return Err(Error::Invalid(format!("batch of {n} votes")));
        }
        if *old_root != self.committed.root {
            return Err(Error::Invalid(
                "tree is not at the committed root; commit or rollback first".into(),
            ));
        }
        let occupied_before = self.occupied.len();
        for (i, v) in votes.iter().enumerate() {
            if v.pkg.process_id != cfg.process_id {
                return Err(Error::Invalid(format!("vote {i} is for another process")));
            }
            if votes[..i].iter().any(|o| o.slot == v.slot) {
                return Err(Error::Invalid(format!("vote {i} reuses a batch slot")));
            }
            if votes[..i].iter().any(|o| o.pkg.vote_id == v.pkg.vote_id) {
                return Err(Error::Invalid(format!(
                    "vote id {:#x} used twice",
                    v.pkg.vote_id
                )));
            }
        }

        // Config read proofs against the old root, before any mutation.
        let mut process_smt = Vec::with_capacity(PROCESS_KEYS.len());
        for k in PROCESS_KEYS {
            let p = self.tree.gen_proof(&key_bytes(k))?;
            process_smt.push(read_entry(old_root, k, &p)?);
        }

        // One seed per batch; scalars run over the batch entries in ballot
        // order, then the refreshes in key order.
        let mut chain = ReencChain::new(&seed, old_root);
        let reenc: Vec<Ballot> = votes
            .iter()
            .map(|v| reencrypt_ballot(&v.pkg.ballot, &cfg.enc_key, nf, &mut chain))
            .collect();

        // Vote-id INSERT chain (value 0), one per vote in ballot order.
        let mut vote_id_smt = Vec::with_capacity(n);
        for v in votes {
            let p = self
                .tree
                .insert_with_proof(&key_bytes(v.pkg.vote_id), &[0u8; 32])
                .map_err(|e| Error::Invalid(format!("vote id insert: {e}")))?;
            vote_id_smt.push(smt_entry(&p));
        }

        // Ballot chain: INSERT on a free slot, UPDATE on an overwrite.
        let mut ballot_smt = Vec::with_capacity(n);
        let mut overwritten: Vec<Ballot> = Vec::new();
        let mut new_slots: Vec<u64> = Vec::new();
        let mut slot_writes: Vec<(u64, Ballot)> = Vec::with_capacity(n);
        for (i, (v, rb)) in votes.iter().zip(&reenc).enumerate() {
            let leaf = leaf_value_bytes(&ballot_leaf_hash(rb));
            let k = key_bytes(v.slot);
            if self.occupied.binary_search(&v.slot).is_ok() {
                let old = self
                    .slot_ballot(v.slot)?
                    .ok_or_else(|| Error::Invalid(format!("vote {i}: slot has no ballot")))?;
                ballot_smt.push(smt_entry(&self.tree.update_with_proof(&k, &leaf)?));
                overwritten.push(old);
            } else {
                ballot_smt.push(smt_entry(&self.tree.insert_with_proof(&k, &leaf)?));
                new_slots.push(v.slot);
            }
            slot_writes.push((v.slot, *rb));
        }
        let w = overwritten.len();

        // net = old + sum(reencrypted) - sum(overwritten); refresh deltas
        // fold in below.
        let old_results = self.committed.accumulator;
        let mut net = old_results;
        for rb in &reenc {
            net = net.add(rb);
        }
        for ob in &overwritten {
            net = net.sub(ob);
        }

        // Silent refreshes: strictly increasing keys, disjoint from the
        // batch, continuing the same scalar chain.
        let required = limits::required_refresh(n, w, occupied_before);
        if selection.len() < required {
            return Err(Error::Invalid(format!(
                "{} refreshes selected, guest requires {required}",
                selection.len()
            )));
        }
        let batch_slots: std::collections::HashSet<u64> = votes.iter().map(|v| v.slot).collect();
        let mut refresh_smt = Vec::with_capacity(selection.len());
        let mut refreshed: Vec<Ballot> = Vec::with_capacity(selection.len());
        let mut refresh_writes: Vec<(u64, Ballot)> = Vec::with_capacity(selection.len());
        let mut prev: Option<u64> = None;
        // Errors name refresh positions, never slot keys.
        for (i, &slot) in selection.iter().enumerate() {
            if prev.is_some_and(|p| p >= slot) {
                return Err(Error::Invalid("refresh selection not ascending".into()));
            }
            prev = Some(slot);
            if batch_slots.contains(&slot) {
                return Err(Error::Invalid(format!(
                    "refresh {i} is written by the batch"
                )));
            }
            let old = self
                .slot_ballot(slot)?
                .ok_or_else(|| Error::Invalid(format!("refresh {i} is not occupied")))?;
            let new = reencrypt_ballot(&old, &cfg.enc_key, nf, &mut chain);
            let leaf = leaf_value_bytes(&ballot_leaf_hash(&new));
            refresh_smt.push(smt_entry(
                &self.tree.update_with_proof(&key_bytes(slot), &leaf)?,
            ));
            net = net.add(&new).sub(&old);
            refreshed.push(old);
            refresh_writes.push((slot, new));
        }
        // Every exposed slot that is still a candidate is refreshed.
        let missing = must_include
            .iter()
            .filter(|s| {
                self.occupied.binary_search(s).is_ok()
                    && !batch_slots.contains(s)
                    && selection.binary_search(s).is_err()
            })
            .count();
        if missing > 0 {
            return Err(Error::Invalid(format!(
                "{missing} exposed slots missing from the refresh selection"
            )));
        }

        // Results UPDATE of key 0x04 with the net accumulator leaf.
        let results_smt = smt_entry(&self.tree.update_with_proof(
            &key_bytes(KEY_RESULTS),
            &leaf_value_bytes(&ballot_leaf_hash(&net)),
        )?);
        let new_root = self.tree.root();

        // DA blobs: the guest rebuilds the same cells from verified state.
        let vote_ids: Vec<u64> = votes.iter().map(|v| v.pkg.vote_id).collect();
        let mut updates: Vec<(u64, Ballot)> = slot_writes.clone();
        updates.extend(refresh_writes.iter().copied());
        let tdata = TransitionData {
            vote_ids: vote_ids.clone(),
            updates,
            accumulator: net,
            num_fields: nf,
        };
        let blobs = blob::build_blobs(&tdata, &cfg.process_id, old_root)?;

        // The wire request.
        let mut proofs = Vec::with_capacity(n);
        let mut public_inputs = Vec::with_capacity(n);
        let mut sigs = Vec::with_capacity(n);
        let mut census_proofs = Vec::new();
        let mut csp_proofs = Vec::new();
        for v in votes {
            let addr = address_to_fr(&v.pkg.address);
            proofs.push(v.pkg.proof.clone());
            public_inputs.push([
                fr_to_dec(&addr),
                v.pkg.vote_id.to_string(),
                fr_to_dec(&v.pkg.inputs_hash),
            ]);
            let sig = &v.pkg.signature;
            sigs.push(EcdsaSigJson {
                // The guest recovers the key from (r, s, v); x/y are unused.
                public_key_x: ZERO_HEX.into(),
                public_key_y: ZERO_HEX.into(),
                signature_r: format!("0x{}", hex::encode(sig.r)),
                signature_s: format!("0x{}", hex::encode(sig.s)),
                signature_v: if sig.v >= 27 { sig.v - 27 } else { sig.v },
                vote_id: v.pkg.vote_id,
                address: fr_to_dec(&addr),
            });
            match &v.pkg.census {
                CensusWitness::Merkle(p) => census_proofs.push(CensusProofJson {
                    root: enc::fr_be_hex(&p.root),
                    leaf: enc::fr_be_hex(&p.leaf),
                    index: p.path_bits,
                    siblings: p.siblings.iter().map(enc::fr_be_hex).collect(),
                }),
                CensusWitness::Csp(p) => csp_proofs.push(CspProofJson {
                    r: format!("0x{}", hex::encode(p.r)),
                    s: format!("0x{}", hex::encode(p.s)),
                    recid: if p.recid >= 27 { p.recid - 27 } else { p.recid },
                    voter_address: format!("0x{}", hex::encode(p.address)),
                    weight: enc::fr_be_hex(&Fr::from(p.weight)),
                    index: p.index,
                }),
            }
        }

        let state = StateTransitionJson {
            voters_count: n as u64,
            overwritten_count: w as u64,
            occupied_before: occupied_before as u64,
            process_id: enc::fr_le_hex(&cfg.process_id),
            old_state_root: enc::le_hex(old_root),
            new_state_root: enc::le_hex(&new_root),
            vote_id_smt,
            ballot_smt,
            refresh_smt,
            results_smt: Some(results_smt),
            process_smt,
            ballot_proofs: Some(BallotProofsJson {
                old_results: enc::ballot_be_hex(&old_results),
                voter_ballots: reenc.iter().map(enc::ballot_be_hex).collect(),
                overwritten_ballots: overwritten.iter().map(enc::ballot_be_hex).collect(),
                refreshed_ballots: refreshed.iter().map(enc::ballot_be_hex).collect(),
            }),
        };
        let reencryption = ReencryptionJson {
            encryption_key_x: enc::fr_be_hex(&cfg.enc_key.x),
            encryption_key_y: enc::fr_be_hex(&cfg.enc_key.y),
            seed: enc::be_hex(&seed),
            entries: votes
                .iter()
                .zip(&reenc)
                .map(|(v, rb)| reenc_entry(&v.pkg.ballot, rb))
                .collect(),
        };
        let kzg = KzgJson {
            process_id: enc::fr_be_hex(&cfg.process_id),
            root_hash_before: enc::root_be_hex(old_root),
            commitments: blobs
                .commitments
                .iter()
                .map(|c| format!("0x{}", hex::encode(c)))
                .collect(),
        };
        let request = ProveRequest {
            vk: self.vk.clone(),
            proofs,
            public_inputs,
            sigs,
            state: Some(state),
            census_proofs: (!census_proofs.is_empty()).then_some(census_proofs),
            csp_data: (!csp_proofs.is_empty()).then_some(CspDataJson { proofs: csp_proofs }),
            reencryption: Some(reencryption),
            kzg: Some(kzg),
            output: None,
        };

        let expected = BatchPublics {
            ok: true,
            fail_mask: 0,
            root_before: *old_root,
            root_after: new_root,
            voters: n as u32,
            overwrites: w as u32,
            census_root: fr_to_le(&cfg.census_root),
            blobs_digest: blobs.digest,
            n_blobs: blobs.blobs.len() as u32,
            occupied_before: occupied_before as u32,
            nproofs: n as u32,
            // The ballot circuit has 3 public inputs; input-gen publishes
            // log_n = floor(log2(nproofs)) (f64 log2 truncated).
            n_public: 3,
            log_n: (n as u32).ilog2(),
        };

        slot_writes.extend(refresh_writes);
        Ok(PreparedBatch {
            request,
            blobs,
            expected,
            old_root: *old_root,
            new_root,
            vote_ids,
            overwrites: w,
            refresh_slots: selection,
            slot_writes,
            new_slots,
            accumulator: net,
        })
    }
}
