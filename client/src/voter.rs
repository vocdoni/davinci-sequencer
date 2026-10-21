//! Voter side: encrypt a ballot, derive the vote id, build the circom inputs,
//! prove them and sign the vote envelope. The inputs follow davinci-circom
//! `ballot_proof.circom` and go-sdk `GenerateBallotProofInputs`.

use ark_ff::PrimeField;
use davinci_zkvm_sdk::ballot::{Ballot, BallotMode, address_to_fr, encrypt_ballot, vote_id};
use davinci_zkvm_sdk::census::{
    CensusWitness, census_leaf, csp_recover, eth_address, slot_key_csp, verify_census_proof,
    vote_sign,
};
use davinci_zkvm_sdk::crypto::babyjubjub::Point;
use davinci_zkvm_sdk::crypto::field::fr_to_dec;
use davinci_zkvm_sdk::groth16::N_PUBLIC;
use davinci_zkvm_sdk::limits::{
    BALLOT_COORDS, CENSUS_ORIGIN_CSP, CENSUS_ORIGIN_MERKLE, NUM_FIELDS,
};
use davinci_zkvm_sdk::types::SnarkJsProof;
use k256::ecdsa::SigningKey;
use num_bigint::{BigInt, BigUint};
use rand::{CryptoRng, RngCore};

use crate::api::{CensusProofWire, CspWire, Fr, MerkleProof, ProcessId, VoteRequest};
use crate::organizer::OnchainProcess;
use crate::{Error, Result};

/// Inputs of `BallotProof(16)`, named and shaped as in the circuit. `Debug`
/// leaves out `k`.
#[derive(Clone, PartialEq, Eq)]
pub struct CircomInputs {
    pub fields: [Fr; NUM_FIELDS],
    pub packed_ballot_mode: Fr,
    pub address: Fr,
    pub weight: Fr,
    pub process_id: Fr,
    pub vote_id: Fr,
    pub encryption_pubkey: [Fr; 2],
    pub k: Fr,
    /// `cipherfields[16][2][2]` flattened: `c1.x, c1.y, c2.x, c2.y` per field.
    pub cipherfields: [Fr; BALLOT_COORDS],
}

impl CircomInputs {
    /// `(signal, flattened values)` for every input signal.
    pub fn signals(&self) -> Vec<(&'static str, Vec<Fr>)> {
        vec![
            ("fields", self.fields.to_vec()),
            ("packed_ballot_mode", vec![self.packed_ballot_mode]),
            ("address", vec![self.address]),
            ("weight", vec![self.weight]),
            ("process_id", vec![self.process_id]),
            ("vote_id", vec![self.vote_id]),
            ("encryption_pubkey", self.encryption_pubkey.to_vec()),
            ("k", vec![self.k]),
            ("cipherfields", self.cipherfields.to_vec()),
        ]
    }

    /// The public signals, in the circuit's declaration order: address,
    /// vote id, process id, packed ballot mode, key x and y, weight, then the
    /// 64 cipherfield coordinates.
    pub fn public_signals(&self) -> [Fr; N_PUBLIC] {
        let mut out = [Fr::from(0u64); N_PUBLIC];
        out[..7].copy_from_slice(&[
            self.address,
            self.vote_id,
            self.process_id,
            self.packed_ballot_mode,
            self.encryption_pubkey[0],
            self.encryption_pubkey[1],
            self.weight,
        ]);
        out[7..].copy_from_slice(&self.cipherfields);
        out
    }

    /// snarkjs `input.json` (decimal strings). Contains the secret `k`.
    pub fn to_json(&self) -> serde_json::Value {
        let m = self
            .signals()
            .into_iter()
            .map(|(name, vals)| {
                let vals = vals.iter().map(|v| fr_to_dec(v).into()).collect();
                (name.to_string(), serde_json::Value::Array(vals))
            })
            .collect();
        serde_json::Value::Object(m)
    }
}

/// A vote ready to be proved: everything but the proof and the signature.
/// `Debug` leaves out `k` and the circom inputs.
#[derive(Clone)]
pub struct PreparedVote {
    pub process_id: ProcessId,
    pub address: [u8; 20],
    pub vote_id: u64,
    pub ballot: Ballot,
    pub weight: u128,
    /// The voter's secret: it opens the ballot and links the vote id.
    pub k: Fr,
    pub census: CensusProofWire,
    pub inputs: CircomInputs,
}

impl PreparedVote {
    /// The circuit's 71 public signals for this vote.
    pub fn public_signals(&self) -> [Fr; N_PUBLIC] {
        self.inputs.public_signals()
    }
}

const REDACTED: &str = "<redacted>";

impl std::fmt::Debug for CircomInputs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CircomInputs")
            .field("process_id", &fr_to_dec(&self.process_id))
            .field("address", &fr_to_dec(&self.address))
            .field("vote_id", &fr_to_dec(&self.vote_id))
            .field("k", &REDACTED)
            .finish_non_exhaustive()
    }
}

impl std::fmt::Debug for PreparedVote {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PreparedVote")
            .field("process_id", &self.process_id)
            .field("address", &hex::encode(self.address))
            .field("vote_id", &crate::api::vote_id_hex(self.vote_id))
            .field("weight", &self.weight)
            .field("k", &REDACTED)
            .finish_non_exhaustive()
    }
}

/// A voter, identified by its Ethereum key.
pub struct Voter {
    pub key: SigningKey,
}

impl std::fmt::Debug for Voter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Voter(0x{})", hex::encode(self.address()))
    }
}

/// Uniform field element (64 bytes reduced mod p), like Go's `RandomK`.
pub fn random_k(rng: &mut (impl RngCore + CryptoRng)) -> Fr {
    let mut b = [0u8; 64];
    rng.fill_bytes(&mut b);
    Fr::from_le_bytes_mod_order(&b)
}

/// Nodes in the order a voter tries them for process `pid`: sorted by
/// `SHA256(voter ‖ pid ‖ url)` (20 + 31 bytes, then the URL's UTF-8). Send
/// to the first reachable and fail over down the list. Every cast of one
/// voter then goes through one node, whose slot queue keeps revotes in
/// order; after a failover only the last-settled ballot counts.
pub fn pick_node<'a, S: AsRef<str>>(
    voter: &[u8; 20],
    pid: &[u8; 31],
    nodes: &'a [S],
) -> Vec<&'a S> {
    use sha2::{Digest, Sha256};
    let mut keyed: Vec<([u8; 32], &S)> = nodes
        .iter()
        .map(|n| {
            let h = Sha256::new()
                .chain_update(voter)
                .chain_update(pid)
                .chain_update(n.as_ref().as_bytes());
            (h.finalize().into(), n)
        })
        .collect();
    keyed.sort_by_key(|k| k.0);
    keyed.into_iter().map(|(_, n)| n).collect()
}

fn check_key(pk: &Point) -> Result<()> {
    // The circuit requires a prime-order, non-identity key.
    if !pk.is_on_curve() || *pk == Point::IDENTITY || !pk.in_subgroup() {
        return Err(Error::Invalid(
            "encryption key is not a valid subgroup point".into(),
        ));
    }
    Ok(())
}

fn bad_ballot(why: impl std::fmt::Display) -> Error {
    Error::Invalid(format!("ballot does not fit the ballot mode: {why}"))
}

// The circuit's `LessEqThan(n)`: `a <= b`, or `None` where it has no witness
// (`a + 2^n - b - 1` outside `n + 1` bits).
fn circuit_less_eq(n: u32, a: &BigUint, b: &BigUint) -> Option<bool> {
    let t: BigInt =
        BigInt::from(a.clone()) + (BigInt::from(1u8) << n) - BigInt::from(b.clone()) - 1u8;
    let bits = t.to_biguint()?.bits();
    (bits <= u64::from(n) + 1).then_some(bits <= u64::from(n))
}

/// Refuses a ballot the circuit's `CheckBallotMode` would reject, so no proof
/// is spent on it and the error names the rule. `fields` are the first
/// values, the rest of the `num_fields` are zero; the circuit checks those
/// too.
pub fn check_ballot(mode: &BallotMode, fields: &[u64], weight: u128) -> Result<()> {
    let nf = mode.num_fields as usize;
    if fields.len() > nf {
        return Err(bad_ballot(format!(
            "{} values for {nf} fields",
            fields.len()
        )));
    }
    mode.pack()?;
    let at = |i: usize| fields.get(i).copied().unwrap_or(0);
    let mut sum = BigUint::from(0u8);
    for i in 0..nf {
        let v = at(i);
        if v > mode.max_value || v < mode.min_value {
            return Err(bad_ballot(format!(
                "field {i} is {v}, outside [{}, {}]",
                mode.min_value, mode.max_value
            )));
        }
        if mode.unique_values && (0..i).any(|j| at(j) == v) {
            return Err(bad_ballot(format!("field {i} repeats the value {v}")));
        }
        // 0^0 is 1 in the circuit, so exponent 0 counts every field once.
        sum += BigUint::from(v).pow(u32::from(mode.cost_exponent));
    }
    let cap = if mode.max_value_sum == 0 {
        BigUint::from(weight)
    } else {
        BigUint::from(mode.max_value_sum)
    };
    match circuit_less_eq(63, &sum, &cap) {
        Some(true) => {}
        Some(false) => {
            return Err(bad_ballot(format!(
                "cost {} above the limit {cap}",
                if sum.bits() > 128 {
                    "(huge)".into()
                } else {
                    sum.to_string()
                }
            )));
        }
        None => return Err(bad_ballot("cost and weight are out of the circuit's range")),
    }
    if circuit_less_eq(63, &BigUint::from(mode.min_value_sum), &sum) != Some(true) {
        return Err(bad_ballot(format!(
            "cost is below the minimum {}",
            mode.min_value_sum
        )));
    }
    Ok(())
}

impl Voter {
    pub fn new(key: SigningKey) -> Self {
        Voter { key }
    }

    pub fn random(rng: &mut (impl RngCore + CryptoRng)) -> Self {
        Voter {
            key: SigningKey::random(rng),
        }
    }

    pub fn address(&self) -> [u8; 20] {
        eth_address(self.key.verifying_key())
    }

    // The census witness must be this voter's, for this process's census.
    fn census_wire(
        &self,
        p: &OnchainProcess,
        census: CensusWitness,
        weight: u128,
    ) -> Result<CensusProofWire> {
        let addr = self.address();
        let origin = p.census_origin as u64;
        match census {
            // Origins 1..=3 are lean-IMT censuses. An origin-3 root lives in the
            // census contract and moves with it, so only 1 and 2 pin it here.
            CensusWitness::Merkle(proof) if (CENSUS_ORIGIN_MERKLE..=3).contains(&origin) => {
                if proof.leaf != census_leaf(&addr, weight)?
                    || (origin != 3 && proof.root != p.census_root)
                    || !verify_census_proof(&proof)
                {
                    return Err(Error::Invalid(
                        "census proof is not for this voter and census".into(),
                    ));
                }
                Ok(CensusProofWire::Merkle(MerkleProof::from(&proof)))
            }
            CensusWitness::Csp(att) if origin == CENSUS_ORIGIN_CSP => {
                let root = &p.census_root;
                let signer = csp_recover(&ProcessId(p.id).to_fr(), &att)?;
                if att.address != addr
                    || att.weight != weight
                    || root[..12] != [0u8; 12]
                    || root[12..] != signer
                {
                    return Err(Error::Invalid(
                        "CSP attestation is not for this voter and census".into(),
                    ));
                }
                slot_key_csp(att.index)?;
                census_leaf(&addr, weight)?;
                Ok(CensusProofWire::Csp(CspWire {
                    r: att.r,
                    s: att.s,
                    recid: att.recid,
                    index: att.index,
                }))
            }
            _ => Err(Error::Invalid(
                "census witness does not match the census origin".into(),
            )),
        }
    }

    /// Encrypts `fields` under the process key with the secret `k` and
    /// derives what the proof binds. `fields` may be shorter than
    /// `num_fields` (missing values are zero). `p` must come from the registry
    /// (`RegistryReader::process`), never from a sequencer's `ProcessView`: a
    /// sequencer could otherwise hand out its own key or census.
    pub fn prepare_vote(
        &self,
        p: &OnchainProcess,
        fields: &[u64],
        census: CensusWitness,
        weight: u128,
        k: Fr,
    ) -> Result<PreparedVote> {
        let mode = &p.ballot_mode;
        let nf = mode.num_fields;
        if nf == 0 || nf as usize > NUM_FIELDS {
            return Err(Error::Invalid(format!("num_fields {nf} not in 1..=16")));
        }
        if fields.len() > nf as usize {
            return Err(Error::Invalid(format!(
                "{} values for {nf} fields",
                fields.len()
            )));
        }
        let pk = p.encryption_key;
        check_key(&pk)?;
        let census = self.census_wire(p, census, weight)?;
        check_ballot(mode, fields, weight)?;

        let address = self.address();
        let addr = address_to_fr(&address);
        let pid = ProcessId(p.id).to_fr();
        let vid = vote_id(&pid, &addr, &k);
        let ballot = encrypt_ballot(&pk, fields, &k, nf);
        let w = Fr::from(weight);

        let mut padded = [Fr::from(0u64); NUM_FIELDS];
        for (o, v) in padded.iter_mut().zip(fields) {
            *o = Fr::from(*v);
        }
        let inputs = CircomInputs {
            fields: padded,
            packed_ballot_mode: mode.pack()?,
            address: addr,
            weight: w,
            process_id: pid,
            vote_id: Fr::from(vid),
            encryption_pubkey: [pk.x, pk.y],
            k,
            cipherfields: ballot.coords(),
        };
        Ok(PreparedVote {
            process_id: ProcessId(p.id),
            address,
            vote_id: vid,
            ballot,
            weight,
            k,
            census,
            inputs,
        })
    }

    /// Signs the vote envelope (process id, vote id, ciphertexts) and wraps a
    /// proof of `prep.inputs` into the request.
    pub fn finish_vote(&self, prep: PreparedVote, proof: SnarkJsProof) -> Result<VoteRequest> {
        if prep.address != self.address() {
            return Err(Error::Invalid(
                "prepared vote belongs to another voter".into(),
            ));
        }
        let sig = vote_sign(
            &self.key,
            &prep.process_id.to_fr(),
            prep.vote_id,
            &prep.ballot,
        );
        let mut signature = [0u8; 65];
        signature[..32].copy_from_slice(&sig.r);
        signature[32..64].copy_from_slice(&sig.s);
        signature[64] = sig.v;
        Ok(VoteRequest {
            process_id: prep.process_id,
            address: prep.address,
            vote_id: prep.vote_id,
            ballot: prep.ballot,
            ballot_proof: proof,
            signature,
            weight: prep.weight,
            census_proof: Some(prep.census),
        })
    }

    /// Full vote with a fresh random `k`; returns the request and `k`. `p`
    /// comes from the registry, as for [`Voter::prepare_vote`].
    #[cfg(feature = "prover")]
    pub fn build_vote(
        &self,
        prover: &crate::prover::BallotProver,
        p: &OnchainProcess,
        fields: &[u64],
        census: CensusWitness,
        weight: u128,
        rng: &mut (impl RngCore + CryptoRng),
    ) -> Result<(VoteRequest, Fr)> {
        let prep = self.prepare_vote(p, fields, census, weight, random_k(rng))?;
        let t = std::time::Instant::now();
        let (proof, pubs) = prover.prove(&prep.inputs)?;
        tracing::debug!(elapsed = ?t.elapsed(), "ballot proof");
        if pubs != prep.public_signals() {
            return Err(Error::Prover(
                "public signals differ from the inputs".into(),
            ));
        }
        let k = prep.k;
        Ok((self.finish_vote(prep, proof)?, k))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pick_node_orders_by_voter_process_and_url() {
        let nodes = ["http://a:8080", "http://b:8080", "http://c:8080"];
        let pid = [7u8; 31];
        let order = pick_node(&[1u8; 20], &pid, &nodes);
        // A permutation, stable across calls and input order.
        let mut sorted: Vec<_> = order.iter().map(|n| **n).collect();
        sorted.sort();
        assert_eq!(sorted, nodes);
        let reversed = ["http://c:8080", "http://b:8080", "http://a:8080"];
        assert_eq!(
            pick_node(&[1u8; 20], &pid, &reversed)
                .into_iter()
                .copied()
                .collect::<Vec<_>>(),
            order.iter().map(|n| **n).collect::<Vec<_>>()
        );
        // The key is exactly SHA256(voter ‖ pid ‖ url).
        let key = |url: &str| {
            let mut b = vec![1u8; 20];
            b.extend_from_slice(&pid);
            b.extend_from_slice(url.as_bytes());
            <[u8; 32]>::from(<sha2::Sha256 as sha2::Digest>::digest(&b))
        };
        for w in order.windows(2) {
            assert!(key(w[0]) < key(w[1]));
        }
        // Voters spread over the nodes.
        let firsts: std::collections::BTreeSet<&str> = (0..32u8)
            .map(|v| *pick_node(&[v; 20], &pid, &nodes)[0])
            .collect();
        assert_eq!(firsts.len(), nodes.len());
    }
}
