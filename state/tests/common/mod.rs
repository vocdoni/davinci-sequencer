//! Shared harness: deterministic voters, a lean-IMT census, fake ballot
//! proofs for state tests and real circom proofs (cached) for validation
//! and ziskemu dry runs.
#![allow(dead_code)]

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::OnceLock;

use davinci_client::prover::BallotProver;
use davinci_client::voter::CircomInputs;
use davinci_state::{CensusOrigin, ProcessConfig, ProcessState, VerifiedVote, VotePackage};
use davinci_zkvm_sdk::ballot::{BallotMode, address_to_fr, encrypt_ballot, inputs_hash, vote_id};
use davinci_zkvm_sdk::census::{
    CensusWitness, LeanImt, census_leaf, eth_address, slot_key_address, vote_id_sign,
};
use davinci_zkvm_sdk::crypto::babyjubjub::Point;
use davinci_zkvm_sdk::crypto::elgamal::keygen;
use davinci_zkvm_sdk::crypto::field::{Fr, U256, fr_from_be, fr_to_be};
use davinci_zkvm_sdk::groth16::BallotVerifier;
use davinci_zkvm_sdk::limits::NUM_FIELDS;
use davinci_zkvm_sdk::release;
use davinci_zkvm_sdk::types::SnarkJsProof;
use k256::ecdsa::SigningKey;
use rand::SeedableRng;
use rand::rngs::StdRng;

/// vk_hash of the embedded protocol ballot VK (the 0x07 leaf).
pub fn embedded_vk_hash() -> [u8; 32] {
    static H: OnceLock<[u8; 32]> = OnceLock::new();
    *H.get_or_init(|| {
        BallotVerifier::from_snarkjs_json(release::ballot_vk_json())
            .unwrap()
            .vk_hash()
    })
}

pub fn ballot_mode(nf: u8) -> BallotMode {
    BallotMode {
        num_fields: nf,
        group_size: 1,
        unique_values: false,
        cost_exponent: 1,
        max_value: 5,
        min_value: 0,
        max_value_sum: 5 * nf as u64,
        min_value_sum: 0,
    }
}

/// A fixed 31-byte on-chain process id as Fr (big-endian integer).
pub fn test_pid() -> Fr {
    let mut b = [0u8; 32];
    b[1..21].copy_from_slice(&[0xda; 20]);
    b[24..].copy_from_slice(&1234u64.to_be_bytes());
    fr_from_be(&b).unwrap()
}

/// Deterministic secp256k1 voter key `i`.
pub fn voter_key(i: usize) -> SigningKey {
    let mut b = [0u8; 32];
    b[..8].copy_from_slice(&(0x5eed_0000_0000_0000u64 + i as u64).to_be_bytes());
    b[31] = 1;
    SigningKey::from_bytes(&b.into()).unwrap()
}

pub fn voter_address(i: usize) -> [u8; 20] {
    eth_address(voter_key(i).verifying_key())
}

/// One election: config, secret key, voter census (weight 1 each).
pub struct Env {
    pub cfg: ProcessConfig,
    pub sk: U256,
    pub imt: LeanImt,
    pub n_voters: usize,
}

pub fn env(nf: u8, n_voters: usize) -> Env {
    let (sk, pk) = keygen(&mut StdRng::seed_from_u64(7));
    let leaves: Vec<Fr> = (0..n_voters)
        .map(|i| census_leaf(&voter_address(i), 1).unwrap())
        .collect();
    let imt = LeanImt::from_leaves(leaves);
    let cfg = ProcessConfig {
        process_id: test_pid(),
        ballot_mode: ballot_mode(nf),
        enc_key: pk,
        census_origin: CensusOrigin::MerkleStatic,
        census_root: imt.root(),
        ballot_vk_hash: embedded_vk_hash(),
    };
    Env {
        cfg,
        sk,
        imt,
        n_voters,
    }
}

pub fn new_state(env: &Env) -> ProcessState<arbo::MemoryStorage> {
    ProcessState::create(env.cfg.clone(), arbo::MemoryStorage::new()).unwrap()
}

/// Empty exposed-refresh set, for batches that never left the node.
pub fn none() -> BTreeSet<u64> {
    BTreeSet::new()
}

/// Placeholder Groth16 proof for tests that never run the guest or verify.
pub fn dummy_proof() -> SnarkJsProof {
    let z = || "0".to_string();
    let o = || "1".to_string();
    SnarkJsProof {
        pi_a: [z(), o(), o()],
        pi_b: [[z(), z()], [o(), z()], [o(), z()]],
        pi_c: [z(), o(), o()],
        protocol: "groth16".into(),
        curve: "bn128".into(),
    }
}

/// A verified vote by census member `idx`, secret `kseed`, dummy proof.
pub fn fake_vote(env: &Env, idx: usize, fields: &[u64], kseed: u64) -> VerifiedVote {
    let cfg = &env.cfg;
    let key = voter_key(idx);
    let address = eth_address(key.verifying_key());
    let addr = address_to_fr(&address);
    let k = Fr::from(kseed);
    let vid = vote_id(&cfg.process_id, &addr, &k);
    let ballot = encrypt_ballot(&cfg.enc_key, fields, &k, cfg.ballot_mode.num_fields);
    let ih = inputs_hash(
        &cfg.process_id,
        &cfg.ballot_mode,
        &cfg.enc_key,
        &addr,
        vid,
        &ballot,
        &Fr::from(1u64),
    )
    .unwrap();
    let census = env.imt.proof(idx).unwrap();
    let slot = slot_key_address(&address);
    VerifiedVote {
        pkg: VotePackage {
            process_id: cfg.process_id,
            vote_id: vid,
            address,
            ballot,
            proof: dummy_proof(),
            inputs_hash: ih,
            signature: vote_id_sign(&key, vid),
            census: CensusWitness::Merkle(census),
            weight: 1,
        },
        slot,
    }
}

/// The election key point of `env` (for assertions).
pub fn env_pk(env: &Env) -> Point {
    env.cfg.enc_key
}

// `CIRCOM_ARTIFACTS`, else the davinci-circom checkout beside the workspace.
fn artifacts_dir() -> PathBuf {
    std::env::var_os("CIRCOM_ARTIFACTS")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../davinci-circom/artifacts")
        })
}

fn prover() -> &'static BallotProver {
    static P: OnceLock<BallotProver> = OnceLock::new();
    P.get_or_init(|| {
        let a = artifacts_dir();
        BallotProver::load(
            &a.join("ballot_proof.wasm"),
            &a.join("ballot_proof_pkey.zkey"),
        )
        .unwrap()
    })
}

// The inputs hash binds everything the proof binds, so it keys the cache.
fn cached_proof(inputs: &CircomInputs, ih: &Fr) -> SnarkJsProof {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/cache");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("{}.json", hex::encode(fr_to_be(ih))));
    if let Ok(s) = std::fs::read_to_string(&path)
        && let Ok(p) = serde_json::from_str(&s)
    {
        return p;
    }
    let (proof, _) = prover().prove(inputs).unwrap();
    std::fs::write(&path, serde_json::to_string(&proof).unwrap()).unwrap();
    proof
}

/// Like [`fake_vote`], but with a real (cached) circom proof under the
/// embedded protocol VK.
pub fn real_vote(env: &Env, idx: usize, fields: &[u64], kseed: u64) -> VerifiedVote {
    let mut v = fake_vote(env, idx, fields, kseed);
    let cfg = &env.cfg;
    let mut padded = [Fr::from(0u64); NUM_FIELDS];
    for (o, f) in padded.iter_mut().zip(fields) {
        *o = Fr::from(*f);
    }
    let inputs = CircomInputs {
        fields: padded,
        packed_ballot_mode: cfg.ballot_mode.pack().unwrap(),
        address: address_to_fr(&v.pkg.address),
        weight: Fr::from(1u64),
        process_id: cfg.process_id,
        vote_id: Fr::from(v.pkg.vote_id),
        encryption_pubkey: [cfg.enc_key.x, cfg.enc_key.y],
        k: Fr::from(kseed),
        cipherfields: v.pkg.ballot.coords(),
        inputs_hash: v.pkg.inputs_hash,
    };
    v.pkg.proof = cached_proof(&inputs, &v.pkg.inputs_hash);
    v
}
