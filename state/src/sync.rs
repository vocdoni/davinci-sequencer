//! Following other sequencers: apply a proven transition from its DA blob
//! content (`decode_blobs`) and check it reaches the published root.

use arbo::Storage;
use davinci_zkvm_sdk::ballot::{ballot_leaf_hash, leaf_value_bytes};
use davinci_zkvm_sdk::blob::TransitionData;
use davinci_zkvm_sdk::limits::{BALLOT_MAX, BALLOT_MIN, KEY_RESULTS, VOTE_ID_MIN};

use crate::config::key_bytes;
use crate::error::Error;
use crate::process::{Committed, ProcessState};

impl<S: Storage> ProcessState<S> {
    /// Applies a transition another sequencer proved: vote-id leaves, slot
    /// updates and the results leaf from `t`, then requires the tree to land
    /// on `new_root` (the root the accepted proof published). The blob does
    /// not say which updates were overwrites, so `w = votes - new slots`.
    pub fn apply_synced(&mut self, t: &TransitionData, new_root: [u8; 32]) -> Result<(), Error> {
        let old_root = self.tree.root();
        if old_root != self.committed.root {
            return Err(Error::Invalid(
                "tree is not at the committed root; commit or rollback first".into(),
            ));
        }
        let r = self.apply(t, new_root);
        if r.is_err() {
            let _ = self.tree.set_root(&old_root);
        }
        r
    }

    fn apply(&mut self, t: &TransitionData, new_root: [u8; 32]) -> Result<(), Error> {
        if t.num_fields != self.cfg.ballot_mode.num_fields {
            return Err(Error::Invalid(format!(
                "transition carries num_fields {}",
                t.num_fields
            )));
        }
        for &vid in &t.vote_ids {
            if vid < VOTE_ID_MIN {
                return Err(Error::Invalid(format!("vote id {vid:#x} below 2^63")));
            }
            self.tree
                .insert_with_proof(&key_bytes(vid), &[0u8; 32])
                .map_err(|e| Error::Invalid(format!("vote id insert: {e}")))?;
        }
        let mut new_slots: Vec<u64> = Vec::new();
        for (slot, ballot) in &t.updates {
            if !(BALLOT_MIN..=BALLOT_MAX).contains(slot) {
                return Err(Error::Invalid(format!(
                    "slot {slot:#x} outside the ballot namespace"
                )));
            }
            let leaf = leaf_value_bytes(&ballot_leaf_hash(ballot));
            let k = key_bytes(*slot);
            if self.occupied.binary_search(slot).is_ok() {
                self.tree.update_with_proof(&k, &leaf)?;
            } else {
                self.tree.insert_with_proof(&k, &leaf)?;
                new_slots.push(*slot);
            }
        }
        self.tree.update_with_proof(
            &key_bytes(KEY_RESULTS),
            &leaf_value_bytes(&ballot_leaf_hash(&t.accumulator)),
        )?;

        let got = self.tree.root();
        if got != new_root {
            return Err(Error::RootMismatch {
                got: hex::encode(got),
                want: hex::encode(new_root),
            });
        }
        // Persist first, assign after: a failed write must leave the
        // in-memory state at the committed root the caller rewinds to.
        let w = t.vote_ids.len() - new_slots.len().min(t.vote_ids.len());
        let committed = Committed {
            root: new_root,
            voters: self.committed.voters + t.vote_ids.len() as u64,
            overwrites: self.committed.overwrites + w as u64,
            accumulator: t.accumulator,
        };
        let mut occ = self.occupied.clone();
        occ.extend_from_slice(&new_slots);
        occ.sort_unstable();
        self.persist(&t.updates, &occ, &committed)?;
        self.occupied = occ;
        self.committed = committed;
        Ok(())
    }
}
