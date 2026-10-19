//! Shared by the attack suites: the service's `/prove` conversion (the one
//! `dryrun.rs` mirrors from `service/src/api/prove.rs`), one ziskemu run with
//! the service's refusals reported instead of unwrapped, and a ballot prover
//! for any process config, weight and census witness (proofs cached by
//! their public signals in `tests/cache`, like `common::real_vote`).
#![allow(dead_code)]

use std::io::Write;
use std::path::PathBuf;
use std::process::Command;
use std::sync::OnceLock;

use davinci_client::prover::BallotProver;
use davinci_client::voter::CircomInputs;
use davinci_state::{ProcessConfig, VerifiedVote, VotePackage};
use davinci_zkvm_input_gen as ig;
use davinci_zkvm_sdk::ballot::{address_to_fr, encrypt_ballot, vote_id};
use davinci_zkvm_sdk::census::{CensusWitness, eth_address, slot_key_address, vote_id_sign};
use davinci_zkvm_sdk::crypto::field::Fr;
use davinci_zkvm_sdk::limits::NUM_FIELDS;
use davinci_zkvm_sdk::publics::BatchPublics;
use davinci_zkvm_sdk::types::{ProveRequest, SnarkJsProof};
use k256::ecdsa::SigningKey;

pub fn gated() -> bool {
    if std::env::var("ZISKEMU").as_deref() == Ok("1") {
        return true;
    }
    eprintln!("skipped: set ZISKEMU=1 to run the emulator attack matrix");
    false
}

/// `MATRIX_CASES` (default `dflt`): how many seeded cases a matrix runs.
pub fn cases(dflt: usize) -> usize {
    std::env::var("MATRIX_CASES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(dflt)
}

/// `MATRIX_SEED` (default `dflt`).
pub fn seed(dflt: u64) -> u64 {
    std::env::var("MATRIX_SEED")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(dflt)
}

fn ziskemu_bin() -> PathBuf {
    if let Ok(p) = std::env::var("ZISKEMU_BIN") {
        return p.into();
    }
    let home = std::env::var("HOME").unwrap();
    PathBuf::from(home).join(".zisk/bin/ziskemu")
}

fn elf_path() -> PathBuf {
    if let Ok(p) = std::env::var("CIRCUIT_ELF_PATH") {
        return p.into();
    }
    concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../davinci-zkvm/circuit/elf/circuit.elf"
    )
    .into()
}

fn reser<T: serde::Serialize, U: serde::de::DeserializeOwned>(t: &T) -> Result<U, String> {
    serde_json::from_value(serde_json::to_value(t).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())
}

fn smt_entry(e: &davinci_zkvm_sdk::types::SmtEntryJson) -> Result<ig::SmtEntry, String> {
    let h = |s: &str| ig::hex32_to_smt_fr(s).map_err(|e| e.to_string());
    Ok(ig::SmtEntry {
        old_root: h(&e.old_root)?,
        new_root: h(&e.new_root)?,
        old_key: h(&e.old_key)?,
        old_value: h(&e.old_value)?,
        is_old0: e.is_old0 != 0,
        new_key: h(&e.new_key)?,
        new_value: h(&e.new_value)?,
        fnc0: e.fnc0 != 0,
        fnc1: e.fnc1 != 0,
        siblings: e.siblings.iter().map(|s| h(s)).collect::<Result<_, _>>()?,
    })
}

fn es<E: std::fmt::Display>(e: E) -> String {
    e.to_string()
}

fn be(s: &str) -> Result<[u64; 4], String> {
    ig::be_hex32_to_fr_le(s).map_err(|e| e.to_string())
}

/// The service's conversion of a `/prove` body into `input.bin`. `Err` is
/// what the service answers 400 to (it never reaches the guest).
pub fn input_bytes(req: &ProveRequest) -> Result<Vec<u8>, String> {
    // Service-side request checks (prove.rs) before input generation.
    if let Some(st) = &req.state {
        if st.refresh_smt.len() > ig::MAX_REFRESH {
            return Err("refresh_smt above MAX_REFRESH".into());
        }
        if let Some(bp) = &st.ballot_proofs
            && bp.refreshed_ballots.len() != st.refresh_smt.len()
        {
            return Err("refreshed_ballots count".into());
        }
    }
    let vk: ig::SnarkJsVk = reser(&req.vk)?;
    let proofs: Vec<ig::SnarkJsProof> = req.proofs.iter().map(reser).collect::<Result<_, _>>()?;
    let sigs: Vec<ig::EcdsaSig> = req.sigs.iter().map(reser).collect::<Result<_, _>>()?;
    let mut bytes = ig::generate_input(&vk, &proofs, &sigs).map_err(es)?;

    if let Some(st) = &req.state {
        let bp = match &st.ballot_proofs {
            Some(bp) => {
                let frs = |v: &[String]| -> Result<Vec<[u64; 4]>, String> {
                    v.iter().map(|s| be(s)).collect()
                };
                Some(ig::BallotProofData {
                    old_results: frs(&bp.old_results)?,
                    voter_ballots: bp
                        .voter_ballots
                        .iter()
                        .map(|b| frs(b))
                        .collect::<Result<_, _>>()?,
                    overwritten_ballots: bp
                        .overwritten_ballots
                        .iter()
                        .map(|b| frs(b))
                        .collect::<Result<_, _>>()?,
                    refreshed_ballots: bp
                        .refreshed_ballots
                        .iter()
                        .map(|b| frs(b))
                        .collect::<Result<_, _>>()?,
                })
            }
            None => None,
        };
        let h = |s: &str| ig::hex32_to_smt_fr(s).map_err(|e| e.to_string());
        let sd = ig::StateData {
            n_voters: st.voters_count,
            n_overwritten: st.overwritten_count,
            occupied_before: st.occupied_before,
            process_id: h(&st.process_id)?,
            old_state_root: h(&st.old_state_root)?,
            new_state_root: h(&st.new_state_root)?,
            vote_id_chain: st
                .vote_id_smt
                .iter()
                .map(smt_entry)
                .collect::<Result<_, _>>()?,
            ballot_chain: st
                .ballot_smt
                .iter()
                .map(smt_entry)
                .collect::<Result<_, _>>()?,
            refresh_chain: st
                .refresh_smt
                .iter()
                .map(smt_entry)
                .collect::<Result<_, _>>()?,
            results: st.results_smt.as_ref().map(smt_entry).transpose()?,
            process_proofs: st
                .process_smt
                .iter()
                .map(smt_entry)
                .collect::<Result<_, _>>()?,
            ballot_proof_data: bp,
        };
        bytes.extend(ig::write_state_block(&sd).map_err(es)?);
    }
    if let Some(cps) = &req.census_proofs
        && !cps.is_empty()
    {
        let proofs: Vec<_> = cps
            .iter()
            .map(|cp| ig::census_proof_from_hex(&cp.root, &cp.leaf, cp.index, &cp.siblings))
            .collect::<Result<Vec<_>, _>>()
            .map_err(es)?;
        bytes.extend(ig::write_census_block(&proofs).map_err(es)?);
    }
    if let Some(csp) = &req.csp_data {
        let entries = csp
            .proofs
            .iter()
            .map(|p| {
                Ok(ig::CspEntryData {
                    r: be(&p.r)?,
                    s: be(&p.s)?,
                    recid: p.recid,
                    voter_address: ig::address_hex_to_fr_le(&p.voter_address)
                        .map_err(|e| e.to_string())?,
                    weight: be(&p.weight)?,
                    index: p.index,
                })
            })
            .collect::<Result<_, String>>()?;
        bytes.extend(ig::write_csp_block(&ig::CspBlockData { entries }).map_err(es)?);
    }
    if let Some(r) = &req.reencryption {
        let ct = |c: &davinci_zkvm_sdk::types::BjjCiphertextJson| -> Result<_, String> {
            Ok(ig::BjjCiphertextData {
                c1x: be(&c.c1.x)?,
                c1y: be(&c.c1.y)?,
                c2x: be(&c.c2.x)?,
                c2y: be(&c.c2.y)?,
            })
        };
        let mut entries = Vec::with_capacity(r.entries.len());
        for en in &r.entries {
            let mut o = Vec::with_capacity(NUM_FIELDS);
            let mut n = Vec::with_capacity(NUM_FIELDS);
            for i in 0..NUM_FIELDS {
                o.push(ct(&en.original[i])?);
                n.push(ct(&en.reencrypted[i])?);
            }
            entries.push(ig::ReencEntryData {
                original: o.try_into().map_err(|_| "original".to_string())?,
                reencrypted: n.try_into().map_err(|_| "reencrypted".to_string())?,
            });
        }
        bytes.extend(
            ig::write_reenc_block(
                be(&r.encryption_key_x)?,
                be(&r.encryption_key_y)?,
                be(&r.seed)?,
                &entries,
            )
            .map_err(es)?,
        );
    }
    if let Some(k) = &req.kzg {
        let n = k.commitments.len();
        if n == 0 || n > ig::MAX_BLOBS {
            return Err(format!("commitments count {n}"));
        }
        let mut commitments = Vec::with_capacity(n);
        for s in &k.commitments {
            let b = hex::decode(s.trim_start_matches("0x")).map_err(|e| e.to_string())?;
            let a: [u8; 48] = b.try_into().map_err(|_| "commitment length".to_string())?;
            commitments.push(a);
        }
        bytes.extend(
            ig::write_kzg_block(&ig::KzgData {
                process_id: be(&k.process_id)?,
                root_hash_before: be(&k.root_hash_before)?,
                commitments,
            })
            .map_err(es)?,
        );
    }
    Ok(bytes)
}

/// Writes `bytes` as ziskemu reads an input file: u64 LE length, the
/// payload, zero padding to 8 bytes.
pub fn write_framed(path: &std::path::Path, bytes: &[u8]) {
    let mut f = std::fs::File::create(path).unwrap();
    f.write_all(&(bytes.len() as u64).to_le_bytes()).unwrap();
    f.write_all(bytes).unwrap();
    let pad = (8 - bytes.len() % 8) % 8;
    f.write_all(&vec![0u8; pad]).unwrap();
}

/// Runs the guest on raw `input.bin` bytes: its publics and every output
/// register as u32 (zero-extended to 64).
pub fn run_bytes(bytes: &[u8]) -> (BatchPublics, Vec<u32>) {
    let dir = tempfile::tempdir().unwrap();
    let in_path = dir.path().join("input.bin");
    let out_path = dir.path().join("output.bin");
    write_framed(&in_path, bytes);
    let out = Command::new(ziskemu_bin())
        .arg("-e")
        .arg(elf_path())
        .arg("-i")
        .arg(&in_path)
        .arg("-o")
        .arg(&out_path)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "ziskemu failed:\n{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let raw = std::fs::read(&out_path).unwrap();
    let mut regs: Vec<u32> = raw
        .chunks_exact(4)
        .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect();
    assert!(regs.len() <= 64, "{} output registers", regs.len());
    regs.resize(64, 0);
    (BatchPublics::parse(&raw).unwrap(), regs)
}

/// The 64 registers a passing batch publishes, from the node's expectation:
/// `batch_ok` and `ecdsa_ok` set, reserved registers and 54..63 zero.
pub fn expected_regs(e: &BatchPublics) -> Vec<u32> {
    let mut r = vec![0u32; 64];
    let put = |r: &mut Vec<u32>, base: usize, b: &[u8; 32]| {
        for i in 0..8 {
            r[base + i] = u32::from_le_bytes(b[i * 4..i * 4 + 4].try_into().unwrap());
        }
    };
    r[0] = e.ok as u32;
    r[1] = e.fail_mask;
    put(&mut r, 2, &e.root_before);
    put(&mut r, 10, &e.root_after);
    r[18] = e.voters;
    r[19] = e.overwrites;
    put(&mut r, 20, &e.census_root);
    put(&mut r, 28, &e.blobs_digest);
    r[36] = e.n_blobs;
    r[40] = 1;
    r[41] = 1;
    r[42] = e.occupied_before;
    r[43] = e.nproofs;
    r[44] = e.n_public;
    r[45] = e.log_n;
    put(&mut r, 46, &e.cells_digest);
    r
}

/// What the prover stack does with a request.
#[derive(Debug)]
pub enum Verdict {
    /// `ok = 1`, mask 0: the publics and all 64 registers.
    Accepted(BatchPublics, Vec<u32>),
    /// The guest ran and failed: the mask.
    Rejected(u32),
    /// The service refused the input (input generation failed).
    Refused(String),
}

impl Verdict {
    pub fn mask(&self) -> Option<u32> {
        match self {
            Verdict::Rejected(m) => Some(*m),
            _ => None,
        }
    }
    pub fn accepted(&self) -> bool {
        matches!(self, Verdict::Accepted(..))
    }
}

pub fn run(req: &ProveRequest) -> Verdict {
    match input_bytes(req) {
        Err(e) => Verdict::Refused(e),
        Ok(b) => {
            let (got, regs) = run_bytes(&b);
            if got.ok && got.fail_mask == 0 {
                Verdict::Accepted(got, regs)
            } else {
                Verdict::Rejected(got.fail_mask)
            }
        }
    }
}

/// The batch must pass the guest with exactly the expected 64 outputs.
pub fn assert_accepted(b: &davinci_state::PreparedBatch, ctx: &str) -> BatchPublics {
    match run(&b.request) {
        Verdict::Accepted(got, regs) => {
            assert_eq!(got, b.expected, "{ctx}: publics differ from the node's");
            assert_eq!(regs, expected_regs(&b.expected), "{ctx}: 64 registers");
            b.check_publics(&got).unwrap();
            got
        }
        v => panic!("{ctx}: guest verdict {v:?}"),
    }
}

pub const FAIL_ECDSA: u32 = 1 << 3;
pub const FAIL_CONSISTENCY: u32 = 1 << 14;
pub const FAIL_BALLOT_NS: u32 = 1 << 15;
pub const FAIL_CENSUS: u32 = 1 << 16;
pub const FAIL_REENC: u32 = 1 << 17;
pub const FAIL_KZG: u32 = 1 << 18;
pub const FAIL_BINDING: u32 = 1 << 22;
pub const FAIL_CSP: u32 = 1 << 23;
pub const FAIL_REFRESH: u32 = 1 << 24;

// ---------------------------------------------------------------- proving

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

/// Cached circom proof of `inputs`; `None` when the circuit refuses them
/// (an out-of-mode ballot). Refusals are cached too (an empty file).
pub fn prove_cached(inputs: &CircomInputs) -> Option<SnarkJsProof> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/cache");
    std::fs::create_dir_all(&dir).unwrap();
    let key = crate::common::signals_key(&inputs.public_signals());
    let path = dir.join(format!("{key}.json"));
    let refused = dir.join(format!("{key}.refused"));
    if refused.exists() {
        return None;
    }
    if let Ok(s) = std::fs::read_to_string(&path)
        && let Ok(p) = serde_json::from_str(&s)
    {
        return Some(p);
    }
    match prover().prove(inputs) {
        Ok((proof, _)) => {
            std::fs::write(&path, serde_json::to_string(&proof).unwrap()).unwrap();
            Some(proof)
        }
        Err(_) => {
            std::fs::write(&refused, b"").unwrap();
            None
        }
    }
}

/// A voter: secp256k1 key `seed`, its address.
pub fn key(seed: u64) -> SigningKey {
    let mut b = [0u8; 32];
    b[..8].copy_from_slice(&(0xa77a_c000_0000_0000u64 ^ seed).to_be_bytes());
    b[31] = 7;
    SigningKey::from_bytes(&b.into()).unwrap()
}

/// A vote by `sk` under `cfg` with `fields`, secret `k`, `weight` and the
/// given census witness. `proof` is `None` when the circuit refuses the
/// ballot (out of mode); the package then carries no usable proof.
pub fn vote(
    cfg: &ProcessConfig,
    sk: &SigningKey,
    fields: &[u64],
    k: u64,
    weight: u128,
    census: CensusWitness,
) -> (VerifiedVote, bool) {
    let address = eth_address(sk.verifying_key());
    let addr = address_to_fr(&address);
    let kf = Fr::from(k);
    let vid = vote_id(&cfg.process_id, &addr, &kf);
    let ballot = encrypt_ballot(&cfg.enc_key, fields, &kf, cfg.ballot_mode.num_fields);
    let w = Fr::from(weight);
    let mut padded = [Fr::from(0u64); NUM_FIELDS];
    for (o, f) in padded.iter_mut().zip(fields) {
        *o = Fr::from(*f);
    }
    let inputs = CircomInputs {
        fields: padded,
        packed_ballot_mode: cfg.ballot_mode.pack().unwrap(),
        address: addr,
        weight: w,
        process_id: cfg.process_id,
        vote_id: Fr::from(vid),
        encryption_pubkey: [cfg.enc_key.x, cfg.enc_key.y],
        k: kf,
        cipherfields: ballot.coords(),
    };
    let proof = prove_cached(&inputs);
    let ok = proof.is_some();
    let slot = match &census {
        CensusWitness::Csp(p) => davinci_zkvm_sdk::census::slot_key_csp(p.index).unwrap_or(0),
        CensusWitness::Merkle(_) => slot_key_address(&address),
    };
    let pkg = VotePackage {
        process_id: cfg.process_id,
        vote_id: vid,
        address,
        ballot,
        proof: proof.unwrap_or_else(placeholder_proof),
        signature: vote_id_sign(sk, vid),
        census,
        weight,
    };
    (VerifiedVote { pkg, slot }, ok)
}

fn placeholder_proof() -> SnarkJsProof {
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
