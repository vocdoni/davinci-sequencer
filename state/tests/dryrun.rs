//! ziskemu dry runs: the built `/prove` request, converted to `input.bin`
//! exactly as the service does, must make the guest publish the expected
//! registers. This is the byte-exactness oracle against the Go harness.
//!
//! Gated: `ZISKEMU=1 cargo test -p davinci-state --test dryrun`. Needs
//! `ziskemu` (`ZISKEMU_BIN` or `~/.zisk/bin/ziskemu`) and the circuit ELF
//! (`CIRCUIT_ELF_PATH` or `../davinci-zkvm/circuit/elf/circuit.elf`).

mod common;

use std::io::Write;
use std::path::PathBuf;
use std::process::Command;

use common::*;
use davinci_state::{CensusOrigin, ProcessConfig, ProcessState, VerifiedVote};
use davinci_zkvm_input_gen as ig;
use std::collections::BTreeSet;

use davinci_zkvm_sdk::census::{CensusWitness, csp_sign, slot_key_address, slot_key_csp};
use davinci_zkvm_sdk::crypto::field::fr_from_be;
use davinci_zkvm_sdk::limits::required_refresh;
use davinci_zkvm_sdk::publics::BatchPublics;
use davinci_zkvm_sdk::types::ProveRequest;
use rand::SeedableRng;
use rand::rngs::StdRng;

fn gated() -> bool {
    if std::env::var("ZISKEMU").as_deref() == Ok("1") {
        return true;
    }
    eprintln!("skipped: set ZISKEMU=1 to run the emulator dry runs");
    false
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

// serde round trip: the sdk wire structs into input-gen's (same JSON shape).
fn reser<T: serde::Serialize, U: serde::de::DeserializeOwned>(t: &T) -> U {
    serde_json::from_value(serde_json::to_value(t).unwrap()).unwrap()
}

fn smt_entry(e: &davinci_zkvm_sdk::types::SmtEntryJson) -> ig::SmtEntry {
    let h = |s: &str| ig::hex32_to_smt_fr(s).unwrap();
    ig::SmtEntry {
        old_root: h(&e.old_root),
        new_root: h(&e.new_root),
        old_key: h(&e.old_key),
        old_value: h(&e.old_value),
        is_old0: e.is_old0 != 0,
        new_key: h(&e.new_key),
        new_value: h(&e.new_value),
        fnc0: e.fnc0 != 0,
        fnc1: e.fnc1 != 0,
        siblings: e.siblings.iter().map(|s| h(s)).collect(),
    }
}

/// The exact conversion `service/src/api/prove.rs` applies to the request.
fn input_bytes(req: &ProveRequest) -> Vec<u8> {
    let vk: ig::SnarkJsVk = reser(&req.vk);
    let proofs: Vec<ig::SnarkJsProof> = req.proofs.iter().map(reser).collect();
    let public_inputs: Vec<Vec<String>> = req.public_inputs.iter().map(|p| p.to_vec()).collect();
    let sigs: Vec<ig::EcdsaSig> = req.sigs.iter().map(reser).collect();
    let mut bytes = ig::generate_input(&vk, &proofs, &public_inputs, &sigs).unwrap();

    if let Some(st) = &req.state {
        let bp = st.ballot_proofs.as_ref().map(|bp| {
            let frs = |v: &[String]| -> Vec<[u64; 4]> {
                v.iter()
                    .map(|s| ig::be_hex32_to_fr_le(s).unwrap())
                    .collect()
            };
            ig::BallotProofData {
                old_results: frs(&bp.old_results),
                voter_ballots: bp.voter_ballots.iter().map(|b| frs(b)).collect(),
                overwritten_ballots: bp.overwritten_ballots.iter().map(|b| frs(b)).collect(),
                refreshed_ballots: bp.refreshed_ballots.iter().map(|b| frs(b)).collect(),
            }
        });
        let sd = ig::StateData {
            n_voters: st.voters_count,
            n_overwritten: st.overwritten_count,
            occupied_before: st.occupied_before,
            process_id: ig::hex32_to_smt_fr(&st.process_id).unwrap(),
            old_state_root: ig::hex32_to_smt_fr(&st.old_state_root).unwrap(),
            new_state_root: ig::hex32_to_smt_fr(&st.new_state_root).unwrap(),
            vote_id_chain: st.vote_id_smt.iter().map(smt_entry).collect(),
            ballot_chain: st.ballot_smt.iter().map(smt_entry).collect(),
            refresh_chain: st.refresh_smt.iter().map(smt_entry).collect(),
            results: st.results_smt.as_ref().map(smt_entry),
            process_proofs: st.process_smt.iter().map(smt_entry).collect(),
            ballot_proof_data: bp,
        };
        bytes.extend(ig::write_state_block(&sd).unwrap());
    }
    if let Some(cps) = &req.census_proofs {
        let proofs: Vec<_> = cps
            .iter()
            .map(|cp| {
                ig::census_proof_from_hex(&cp.root, &cp.leaf, cp.index, &cp.siblings).unwrap()
            })
            .collect();
        bytes.extend(ig::write_census_block(&proofs).unwrap());
    }
    if let Some(csp) = &req.csp_data {
        let entries = csp
            .proofs
            .iter()
            .map(|p| ig::CspEntryData {
                r: ig::be_hex32_to_fr_le(&p.r).unwrap(),
                s: ig::be_hex32_to_fr_le(&p.s).unwrap(),
                recid: p.recid,
                voter_address: ig::address_hex_to_fr_le(&p.voter_address).unwrap(),
                weight: ig::be_hex32_to_fr_le(&p.weight).unwrap(),
                index: p.index,
            })
            .collect();
        bytes.extend(ig::write_csp_block(&ig::CspBlockData { entries }).unwrap());
    }
    if let Some(r) = &req.reencryption {
        let ct = |c: &davinci_zkvm_sdk::types::BjjCiphertextJson| ig::BjjCiphertextData {
            c1x: ig::be_hex32_to_fr_le(&c.c1.x).unwrap(),
            c1y: ig::be_hex32_to_fr_le(&c.c1.y).unwrap(),
            c2x: ig::be_hex32_to_fr_le(&c.c2.x).unwrap(),
            c2y: ig::be_hex32_to_fr_le(&c.c2.y).unwrap(),
        };
        let entries: Vec<ig::ReencEntryData> = r
            .entries
            .iter()
            .map(|e| ig::ReencEntryData {
                original: std::array::from_fn(|i| ct(&e.original[i])),
                reencrypted: std::array::from_fn(|i| ct(&e.reencrypted[i])),
            })
            .collect();
        bytes.extend(
            ig::write_reenc_block(
                ig::be_hex32_to_fr_le(&r.encryption_key_x).unwrap(),
                ig::be_hex32_to_fr_le(&r.encryption_key_y).unwrap(),
                ig::be_hex32_to_fr_le(&r.seed).unwrap(),
                &entries,
            )
            .unwrap(),
        );
    }
    if let Some(k) = &req.kzg {
        let commitments = k
            .commitments
            .iter()
            .map(|s| {
                let b = hex::decode(s.trim_start_matches("0x")).unwrap();
                let mut a = [0u8; 48];
                a.copy_from_slice(&b);
                a
            })
            .collect();
        bytes.extend(
            ig::write_kzg_block(&ig::KzgData {
                process_id: ig::be_hex32_to_fr_le(&k.process_id).unwrap(),
                root_hash_before: ig::be_hex32_to_fr_le(&k.root_hash_before).unwrap(),
                commitments,
            })
            .unwrap(),
        );
    }
    bytes
}

/// Runs the guest on `req` and parses its output registers.
fn run_guest(req: &ProveRequest) -> BatchPublics {
    let bytes = input_bytes(req);
    let dir = tempfile::tempdir().unwrap();
    let in_path = dir.path().join("input.bin");
    let out_path = dir.path().join("output.bin");
    // read_slice framing: u64 LE length, payload, zero-pad to 8 bytes.
    let mut f = std::fs::File::create(&in_path).unwrap();
    f.write_all(&(bytes.len() as u64).to_le_bytes()).unwrap();
    f.write_all(&bytes).unwrap();
    let pad = (8 - bytes.len() % 8) % 8;
    f.write_all(&vec![0u8; pad]).unwrap();
    drop(f);
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
    BatchPublics::parse(&std::fs::read(&out_path).unwrap()).unwrap()
}

fn assert_accepted(b: &davinci_state::PreparedBatch) {
    let got = run_guest(&b.request);
    assert!(
        got.ok && got.fail_mask == 0,
        "guest rejected: mask {:#x}",
        got.fail_mask
    );
    assert_eq!(got, b.expected);
    b.check_publics(&got).unwrap();
}

#[test]
fn first_batch_accepted_by_the_guest() {
    if !gated() {
        return;
    }
    let env = env(2, 4);
    let mut st = new_state(&env);
    let votes: Vec<_> = (0..3)
        .map(|i| real_vote(&env, i, &[1, 2], 20 + i as u64))
        .collect();
    let b = st
        .prepare(&votes, &mut StdRng::seed_from_u64(21), &none())
        .unwrap();
    assert_accepted(&b);
    st.commit(&b).unwrap();
}

#[test]
fn overwrites_and_refreshes_accepted() {
    if !gated() {
        return;
    }
    let env = env(2, 6);
    let mut st = new_state(&env);
    let mut rng = StdRng::seed_from_u64(22);
    let votes: Vec<_> = (0..4)
        .map(|i| real_vote(&env, i, &[1, 0], 30 + i as u64))
        .collect();
    let b = st.prepare(&votes, &mut rng, &none()).unwrap();
    st.commit(&b).unwrap();
    // Voter 0 revotes, voter 4 is new: 3 untouched occupied slots refresh.
    let votes2 = vec![
        real_vote(&env, 0, &[2, 1], 35),
        real_vote(&env, 4, &[0, 1], 36),
    ];
    let b2 = st.prepare(&votes2, &mut rng, &none()).unwrap();
    assert_eq!(b2.expected.overwrites, 1);
    assert!(!b2.request.state.as_ref().unwrap().refresh_smt.is_empty());
    assert_accepted(&b2);
}

#[test]
fn exposed_refresh_set_accepted() {
    if !gated() {
        return;
    }
    // 20 committed voters (fake proofs: that batch never reaches the guest).
    let env = env(2, 24);
    let mut st = new_state(&env);
    let mut rng = StdRng::seed_from_u64(26);
    let first: Vec<_> = (0..20)
        .map(|i| fake_vote(&env, i, &[1, 0], 80 + i as u64))
        .collect();
    let b = st.prepare(&first, &mut rng, &none()).unwrap();
    st.commit(&b).unwrap();

    // Voter 0 revotes, voter 21 is new: 19 candidates, 16 required.
    let votes = vec![
        real_vote(&env, 0, &[2, 1], 90),
        real_vote(&env, 21, &[0, 1], 91),
    ];
    assert_eq!(required_refresh(2, 1, 20), 16);
    let slot = |i: usize| slot_key_address(&voter_address(i));
    let dropped = [slot(0), slot(22)];

    // 18 live exposed slots: all refreshed, above the guest's minimum.
    let live: BTreeSet<u64> = (1..19).map(slot).collect();
    let mut must = live.clone();
    must.extend(dropped);
    let b = st.prepare(&votes, &mut rng, &must).unwrap();
    assert_eq!(b.refresh_slots(), live.iter().copied().collect::<Vec<_>>());
    assert_accepted(&b);
    st.rollback(&b).unwrap();

    // 5 live exposed slots, topped up to 16.
    let live: BTreeSet<u64> = (1..6).map(slot).collect();
    let mut must = live.clone();
    must.extend(dropped);
    let b = st.prepare(&votes, &mut rng, &must).unwrap();
    assert_eq!(b.refresh_slots().len(), 16);
    assert!(live.iter().all(|s| b.refresh_slots().contains(s)));
    assert!(dropped.iter().all(|s| !b.refresh_slots().contains(s)));
    assert_accepted(&b);
}

#[test]
fn csp_batch_accepted() {
    if !gated() {
        return;
    }
    let env = env(2, 4);
    let csp = voter_key(99);
    let mut be = [0u8; 32];
    be[12..].copy_from_slice(&voter_address(99));
    let cfg = ProcessConfig {
        census_origin: CensusOrigin::Csp,
        census_root: fr_from_be(&be).unwrap(),
        ..env.cfg.clone()
    };
    let mut st = ProcessState::create(cfg.clone(), arbo::MemoryStorage::new()).unwrap();
    let votes: Vec<VerifiedVote> = (0..2)
        .map(|i| {
            let mut v = real_vote(&env, i, &[1, 2], 40 + i as u64);
            let att = csp_sign(&csp, &cfg.process_id, &v.pkg.address, 1, i as u64);
            v.slot = slot_key_csp(att.index).unwrap();
            v.pkg.census = CensusWitness::Csp(att);
            v
        })
        .collect();
    let b = st
        .prepare(&votes, &mut StdRng::seed_from_u64(23), &none())
        .unwrap();
    assert_eq!(
        b.expected.census_root,
        davinci_zkvm_sdk::crypto::field::fr_to_le(&cfg.census_root)
    );
    assert_accepted(&b);
}

#[test]
fn multi_blob_batch_accepted() {
    if !gated() {
        return;
    }
    // 16 fields, 125 votes: 4286 cells, two blobs.
    let env = env(16, 125);
    let mut st = new_state(&env);
    // Values within the mode's max_value (5) and sum cap.
    let fields: Vec<u64> = (0..16).map(|i| i % 6).collect();
    let votes: Vec<_> = (0..125)
        .map(|i| real_vote(&env, i, &fields, 60 + i as u64))
        .collect();
    let b = st
        .prepare(&votes, &mut StdRng::seed_from_u64(24), &none())
        .unwrap();
    assert!(b.expected.n_blobs >= 2, "n_blobs {}", b.expected.n_blobs);
    assert_accepted(&b);
}

#[test]
fn two_blob_cap_batch_accepted() {
    if !gated() {
        return;
    }
    // Gnosis takes two blobs per block. 200 committed voters (fake proofs)
    // make each new vote bring a refresh, so the 125 votes of
    // `multi_blob_batch_accepted` (cached proofs) no longer all fit.
    let env = env(16, 325);
    let mut st = new_state(&env);
    let mut rng = StdRng::seed_from_u64(27);
    let fields: Vec<u64> = (0..16).map(|i| i % 6).collect();
    let first: Vec<_> = (125..325)
        .map(|i| fake_vote(&env, i, &fields, 400 + i as u64))
        .collect();
    let b = st.prepare(&first, &mut rng, &none()).unwrap();
    st.commit(&b).unwrap();

    st.set_blob_cap(2).unwrap();
    let pending: Vec<_> = (0..125)
        .map(|i| real_vote(&env, i, &fields, 60 + i as u64))
        .collect();
    let sel: Vec<VerifiedVote> = st
        .select_batch(&pending, u64::MAX, &none(), usize::MAX)
        .unwrap()
        .0
        .into_iter()
        .cloned()
        .collect();
    assert!(
        !sel.is_empty() && sel.len() < pending.len(),
        "no shrink: {}",
        sel.len()
    );
    let b = st.prepare(&sel, &mut rng, &none()).unwrap();
    assert_eq!(b.blobs.blobs.len(), 2);
    assert_eq!(b.expected.n_blobs, 2);
    assert_accepted(&b);
}

#[test]
fn guest_rejects_tampered_requests() {
    if !gated() {
        return;
    }
    let env = env(2, 4);
    let mut st = new_state(&env);
    let votes: Vec<_> = (0..2)
        .map(|i| real_vote(&env, i, &[1, 2], 70 + i as u64))
        .collect();
    let b = st
        .prepare(&votes, &mut StdRng::seed_from_u64(25), &none())
        .unwrap();

    // Byte-order confusion: old_state_root shipped BE instead of arbo-LE.
    let mut bad = b.request.clone();
    {
        let s = bad.state.as_mut().unwrap();
        let mut r = hex::decode(s.old_state_root.trim_start_matches("0x")).unwrap();
        r.reverse();
        s.old_state_root = format!("0x{}", hex::encode(r));
    }
    let got = run_guest(&bad);
    assert!(!got.ok || got.fail_mask != 0, "BE root accepted");

    // Swapped ballot chain entries: the SMT chain no longer links.
    let mut bad = b.request.clone();
    bad.state.as_mut().unwrap().ballot_smt.swap(0, 1);
    let got = run_guest(&bad);
    assert!(!got.ok || got.fail_mask != 0, "swapped slots accepted");

    // The untampered request still passes.
    assert_accepted(&b);
}
