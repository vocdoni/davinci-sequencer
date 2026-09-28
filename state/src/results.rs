//! Final results: decrypt the committed accumulator and build the
//! circuit-results request against the committed root.

use arbo::Storage;
use davinci_zkvm_sdk::ballot::{ballot_leaf_hash, leaf_value_bytes};
use davinci_zkvm_sdk::crypto::field::{U256, fr_to_u256};
use davinci_zkvm_sdk::limits::{BALLOT_COORDS, NUM_FIELDS, SMT_LEVELS};
use davinci_zkvm_sdk::limits::{KEY_ENC_KEY, KEY_RESULTS};
use davinci_zkvm_sdk::results::build_results_request;
use davinci_zkvm_sdk::types::ResultsRequest;
use rand::{CryptoRng, RngCore};

use crate::config::key_bytes;
use crate::error::Error;
use crate::process::ProcessState;

impl<S: Storage> ProcessState<S> {
    /// Builds the `/results` request for the committed state. `sk` is the
    /// election secret key; returns the request and the plaintext tally.
    pub fn results_request(
        &self,
        sk: &U256,
        rng: &mut (impl RngCore + CryptoRng),
    ) -> Result<(ResultsRequest, [u64; NUM_FIELDS]), Error> {
        let root = self.committed.root;
        let key_sibs = self.tree.gen_proof_at(&root, &key_bytes(KEY_ENC_KEY))?;
        let acc_sibs = self.tree.gen_proof_at(&root, &key_bytes(KEY_RESULTS))?;
        // Per circom BallotModeChecker each field is <= max_value (the
        // weight fallback only caps the sum) and the tally adds ballots
        // unscaled, so a field total is at most voters * max_value.
        let max = self
            .committed
            .voters
            .saturating_mul(self.cfg.ballot_mode.max_value)
            .max(1);
        Ok(build_results_request(
            &root,
            &self.cfg.enc_key,
            &key_sibs.siblings,
            &self.committed.accumulator,
            &acc_sibs.siblings,
            sk,
            max,
            rng,
        )?)
    }

    /// Arguments of the registry's `requestResultsDecryption` for the committed
    /// state: the accumulator's 64 coordinates (circomlib TE form, leaf-hash
    /// order) and the key-0x04 inclusion siblings, root to leaf, zero-padded
    /// to 64 so the last one is zero (`Sha256SmtLib.verifyInclusion`).
    pub fn dkg_results_inputs(&self) -> Result<([U256; BALLOT_COORDS], Vec<[u8; 32]>), Error> {
        let root = self.committed.root;
        let acc = &self.committed.accumulator;
        let proof = self.tree.gen_proof_at(&root, &key_bytes(KEY_RESULTS))?;
        if !proof.exists || proof.value != leaf_value_bytes(&ballot_leaf_hash(acc)) {
            return Err(Error::Invalid(
                "results leaf does not hold the committed accumulator".into(),
            ));
        }
        let mut siblings = proof.siblings;
        if siblings.len() >= SMT_LEVELS {
            return Err(Error::Invalid(format!(
                "results leaf at depth {}: no room for the zero terminator",
                siblings.len()
            )));
        }
        siblings.resize(SMT_LEVELS, [0u8; 32]);
        Ok((acc.coords().map(|c| fr_to_u256(&c)), siblings))
    }
}
