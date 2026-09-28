//! Real ballot proofs from the davinci-circom artifacts. Gated by
//! `CIRCOM_ARTIFACTS`, the absolute path of a davinci-circom `artifacts/` dir.
#![cfg(feature = "prover")]

mod common;

use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::Instant;

use common::{Election, csp_root, election, merkle_census, voters};
use davinci_client::Error;
use davinci_client::api::{CensusProofWire, ProcessId, VoteRequest};
use davinci_client::prover::BallotProver;
use davinci_client::voter::Voter;
use davinci_state::{CensusOrigin, ProcessConfig, VoteError, VotePackage, validate_vote};
use davinci_zkvm_sdk::ballot::{address_to_fr, encrypt_ballot, inputs_hash, vote_id};
use davinci_zkvm_sdk::census::{
    CensusWitness, csp_sign, slot_key_address, slot_key_csp, vote_id_recover,
};
use davinci_zkvm_sdk::crypto::elgamal::decrypt;
use davinci_zkvm_sdk::crypto::field::Fr;
use davinci_zkvm_sdk::groth16::BallotVerifier;
use davinci_zkvm_sdk::release;
use k256::ecdsa::SigningKey;
use rand::SeedableRng;
use rand::rngs::StdRng;

fn artifacts() -> Option<PathBuf> {
    std::env::var_os("CIRCOM_ARTIFACTS").map(PathBuf::from)
}

// Loading the zkey takes seconds; share one prover across tests.
fn prover() -> Option<&'static BallotProver> {
    static P: OnceLock<BallotProver> = OnceLock::new();
    let dir = artifacts()?;
    Some(P.get_or_init(|| {
        let t = Instant::now();
        let p = BallotProver::load(
            &dir.join("ballot_proof.wasm"),
            &dir.join("ballot_proof_pkey.zkey"),
        )
        .expect("load circom artifacts");
        eprintln!("prover loaded in {:?}", t.elapsed());
        p
    }))
}

fn verifier() -> BallotVerifier {
    BallotVerifier::from_snarkjs_json(release::ballot_vk_json()).unwrap()
}

// The sequencer's config for this process, from the registry data.
fn config(e: &Election) -> ProcessConfig {
    let p = &e.chain;
    ProcessConfig {
        process_id: ProcessId(p.id).to_fr(),
        ballot_mode: p.ballot_mode,
        enc_key: p.encryption_key,
        census_origin: match p.census_origin {
            1 => CensusOrigin::MerkleStatic,
            4 => CensusOrigin::Csp,
            o => panic!("census origin {o}"),
        },
        census_root: p.census_root,
        ballot_vk_hash: verifier().vk_hash(),
    }
}

// What the sequencer's API hands to `validate_vote`. `census` is the
// sequencer's own witness: re-derived from its tree for Merkle, the
// request's attestation for CSP.
fn package(req: &VoteRequest, census: CensusWitness) -> VotePackage {
    VotePackage {
        process_id: req.process_id.to_fr(),
        vote_id: req.vote_id,
        address: req.address,
        ballot: req.ballot,
        proof: req.ballot_proof.clone(),
        inputs_hash: req.ballot_inputs_hash,
        signature: req.signature(),
        census,
        weight: req.weight,
    }
}

// davinci-state accepts the vote; one changed ballot coordinate is caught
// by the inputs hash.
fn check_state_accepts(e: &Election, req: &VoteRequest, census: CensusWitness, slot: u64) {
    let cfg = config(e);
    let vf = verifier();
    let ok = validate_vote(&cfg, &vf, package(req, census.clone())).unwrap();
    assert_eq!(ok.slot, slot);
    assert_eq!(ok.pkg.vote_id, req.vote_id);

    // Negating x keeps the point on the curve: only the hash can notice.
    let mut bad = package(req, census);
    bad.ballot.0[0].c1.x = -bad.ballot.0[0].c1.x;
    assert!(bad.ballot.0[0].c1.is_on_curve());
    match validate_vote(&cfg, &vf, bad) {
        Err(VoteError::InputsHashMismatch) => {}
        other => panic!("want InputsHashMismatch, got {other:?}"),
    }
}

// Everything the sequencer's stateless checks rely on, recomputed with the SDK.
fn check_vote(e: &Election, v: &Voter, req: &VoteRequest, k: &Fr, fields: &[u64], weight: u128) {
    let p = &e.chain;
    let nf = p.ballot_mode.num_fields;
    let pid = ProcessId(p.id).to_fr();
    let addr = address_to_fr(&v.address());
    assert_eq!(req.process_id.0, p.id);
    assert_eq!(req.address, v.address());
    assert_eq!(req.weight, weight);
    assert_eq!(req.vote_id, vote_id(&pid, &addr, k));
    assert!(req.vote_id >= 1 << 63);
    assert_eq!(req.ballot, encrypt_ballot(&p.encryption_key, fields, k, nf));
    assert!(
        req.ballot.is_padded_ok(nf),
        "padded fields must be identity"
    );
    for (i, ct) in req.ballot.0.iter().enumerate().take(nf as usize) {
        let want = fields.get(i).copied().unwrap_or(0);
        assert_eq!(decrypt(&e.sk, ct, 64), Some(want), "field {i}");
    }
    let ih = inputs_hash(
        &pid,
        &p.ballot_mode,
        &p.encryption_key,
        &addr,
        req.vote_id,
        &req.ballot,
        &Fr::from(weight),
    )
    .unwrap();
    assert_eq!(req.ballot_inputs_hash, ih);
    let pubs = [addr, Fr::from(req.vote_id), ih];
    let vf = verifier();
    assert!(
        vf.verify(&req.ballot_proof, &pubs),
        "proof under embedded VK"
    );
    assert_eq!(req.ballot_proof.protocol, "groth16");
    assert_eq!(req.ballot_proof.curve, "bn128");
    // The checks bite: any other public signal fails.
    for i in 0..3 {
        let mut bad = pubs;
        bad[i] += Fr::from(1u64);
        assert!(!vf.verify(&req.ballot_proof, &bad), "tampered public {i}");
    }
    assert_eq!(
        vote_id_recover(req.vote_id, &req.signature()).unwrap(),
        v.address()
    );
    // What goes over the wire comes back identical.
    let json = serde_json::to_string(req).unwrap();
    let back: VoteRequest = serde_json::from_str(&json).unwrap();
    assert_eq!(&back, req);
}

fn merkle_vote(nf: u8, fields: &[u64], seed: u64) {
    let Some(prover) = prover() else {
        eprintln!("CIRCOM_ARTIFACTS not set, skipping");
        return;
    };
    let mut rng = StdRng::seed_from_u64(seed);
    let vs = voters(5, &mut rng);
    let weight = 10;
    let census = merkle_census(&vs, weight);
    let e = election(nf, 1, census.root(), &mut rng);
    let v = &vs[3];
    let witness = CensusWitness::Merkle(census.proof(3).unwrap());

    let t = Instant::now();
    let (req, k) = v
        .build_vote(prover, &e.chain, fields, witness, weight, &mut rng)
        .unwrap();
    eprintln!("nf={nf}: vote built and proved in {:?}", t.elapsed());

    check_vote(&e, v, &req, &k, fields, weight);
    // The sequencer finds the voter in its own copy of the census.
    let idx = vs.iter().position(|x| x.address() == req.address).unwrap();
    let rederived = census.proof(idx).unwrap();
    let slot = slot_key_address(&req.address);
    check_state_accepts(&e, &req, CensusWitness::Merkle(rederived), slot);
    match &req.census_proof {
        Some(CensusProofWire::Merkle(m)) => {
            assert_eq!(m.to_census_proof(), census.proof(3).unwrap())
        }
        other => panic!("want a merkle proof, got {other:?}"),
    }
}

#[test]
fn vote_nf2_merkle() {
    merkle_vote(2, &[3, 5], 1);
}

#[test]
fn vote_nf16_merkle() {
    let fields: Vec<u64> = (0..16).map(|i| (i % 2) as u64).collect();
    merkle_vote(16, &fields, 2);
}

#[test]
fn vote_csp() {
    let Some(prover) = prover() else {
        return;
    };
    let mut rng = StdRng::seed_from_u64(3);
    let csp = SigningKey::random(&mut rng);
    let e = election(4, 4, csp_root(&csp), &mut rng);
    let v = Voter::random(&mut rng);
    let att = csp_sign(&csp, &ProcessId(e.chain.id).to_fr(), &v.address(), 3, 17);

    let t = Instant::now();
    let (req, k) = v
        .build_vote(
            prover,
            &e.chain,
            &[1, 0, 2],
            CensusWitness::Csp(att.clone()),
            3,
            &mut rng,
        )
        .unwrap();
    eprintln!("nf=4 csp: vote built and proved in {:?}", t.elapsed());
    check_vote(&e, &v, &req, &k, &[1, 0, 2], 3);
    assert_eq!(req.census_witness(), Some(CensusWitness::Csp(att.clone())));
    check_state_accepts(&e, &req, CensusWitness::Csp(att), slot_key_csp(17).unwrap());
}

// A ballot outside the mode never gets a proof: the circuit's own asserts fail.
#[test]
fn invalid_ballot_is_not_proved() {
    let Some(prover) = prover() else {
        return;
    };
    let mut rng = StdRng::seed_from_u64(4);
    let vs = voters(2, &mut rng);
    let census = merkle_census(&vs, 10);
    let e = election(2, 1, census.root(), &mut rng);
    let w = || CensusWitness::Merkle(census.proof(0).unwrap());
    let circuit_rejects = |fields: &[u64], rng: &mut StdRng| match vs[0].build_vote(
        prover,
        &e.chain,
        fields,
        w(),
        10,
        rng,
    ) {
        Err(Error::Prover(m)) => assert!(m.contains("rejected by the circuit"), "{m}"),
        other => panic!("want a circuit rejection, got {other:?}"),
    };
    // max_value is 7.
    circuit_rejects(&[8, 0], &mut rng);
    // Sum above the weight (max_value_sum = 0 means the weight bounds it).
    circuit_rejects(&[7, 7], &mut rng);
    // More values than fields: refused before proving.
    assert!(matches!(
        vs[0].build_vote(prover, &e.chain, &[1, 1, 1], w(), 10, &mut rng),
        Err(Error::Invalid(_))
    ));
    // The prover is still usable after a failed witness.
    assert!(
        vs[0]
            .build_vote(prover, &e.chain, &[2, 3], w(), 10, &mut rng)
            .is_ok()
    );
}

// A zkey whose VK is not the protocol's embedded VK is refused at load.
#[test]
fn zkey_must_match_the_embedded_vk() {
    let Some(dir) = artifacts() else {
        return;
    };
    let wasm = dir.join("ballot_proof.wasm");
    let zkey = dir.join("ballot_proof_pkey.zkey");
    // Same shape, one IC point swapped: a different circuit's VK.
    let mut vk: serde_json::Value = serde_json::from_str(release::ballot_vk_json()).unwrap();
    let ic1 = vk["IC"][1].clone();
    vk["IC"][1] = vk["IC"][2].clone();
    vk["IC"][2] = ic1;
    match BallotProver::load_with_vk(&wasm, &zkey, &vk.to_string()) {
        Err(Error::Prover(m)) => assert!(m.contains("does not match"), "{m}"),
        other => panic!("want a VK mismatch, got {:?}", other.map(|_| ())),
    }
    // Another circuit's artifacts are refused too.
    match BallotProver::load(
        &dir.join("ballot_cipher_test.wasm"),
        &dir.join("ballot_cipher_test_pkey.zkey"),
    ) {
        Err(Error::Prover(_)) => {}
        other => panic!("want a refusal, got {:?}", other.map(|_| ())),
    }
}
