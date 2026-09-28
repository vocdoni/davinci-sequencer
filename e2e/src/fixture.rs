//! The seeded election fixture: voters, choices, censuses, ballots and the
//! expected tally. Everything but the election keys (drawn by the nodes) and
//! the Groth16 randomness is a function of `SEED`.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{Context, Result, ensure};
use davinci_client::api::{CensusFile, Fr, VoteRequest};
use davinci_client::organizer::{OnchainProcess, census_file, merkle_census};
use davinci_client::prover::BallotProver;
use davinci_client::voter::{Voter, random_k};
use davinci_zkvm_sdk::ballot::BallotMode;
use davinci_zkvm_sdk::census::{
    CensusWitness, LeanImt, census_leaf, csp_sign, eth_address, vote_id_sign,
};
use davinci_zkvm_sdk::crypto::field::{fr_from_be, fr_to_dec};
use k256::ecdsa::SigningKey;
use rand::SeedableRng;
use rand::rngs::StdRng;

pub const SEED: u64 = 0xda71_c10e_2e00_0011;
/// Voters in the Merkle census of process 1.
pub const N_VOTERS: usize = 24;
/// Voters in the CSP census of process 2.
pub const N_CSP: usize = 3;
pub const NUM_FIELDS: u8 = 4;
pub const WEIGHT: u128 = 10;
/// Round-1 packages also sent, unchanged, to a second node.
pub const DUPLICATES: [usize; 4] = [1, 6, 11, 16];
/// Voters who cast a second ballot through another node.
pub const REVOTERS: [usize; 8] = [0, 3, 5, 8, 11, 14, 19, 22];
/// Voter whose round-1 `k` is reused for a second, different ballot.
pub const REUSER: usize = 20;
/// Voters whose fresh ballots are sent with a broken proof / signature.
pub const BAD_PROOF: usize = 23;
pub const BAD_SIG: usize = 21;
pub const N_NODES: usize = 3;

/// Four fields, values in 0..=5; the sum is bounded by the weight
/// (`max_value_sum = 0`).
pub fn ballot_mode() -> BallotMode {
    BallotMode {
        num_fields: NUM_FIELDS,
        group_size: 1,
        unique_values: false,
        cost_exponent: 1,
        max_value: 5,
        min_value: 0,
        max_value_sum: 0,
        min_value_sum: 0,
    }
}

fn rng(tag: u64, i: u64) -> StdRng {
    StdRng::seed_from_u64(SEED ^ (tag << 56) ^ i.wrapping_mul(0x9e37_79b9_7f4a_7c15))
}

fn keys(tag: u64, n: usize) -> Vec<SigningKey> {
    let mut r = rng(tag, 0);
    (0..n).map(|_| SigningKey::random(&mut r)).collect()
}

pub fn merkle_voters() -> Vec<Voter> {
    keys(1, N_VOTERS).into_iter().map(Voter::new).collect()
}

pub fn csp_voters() -> Vec<Voter> {
    keys(2, N_CSP).into_iter().map(Voter::new).collect()
}

/// Members of the origin-3 census contract, in insertion order.
pub const N_ONCHAIN: usize = 18;
/// Members of the origin-2 census: `N_DYN_OLD` in the first dump, the rest
/// added by the organizer's update.
pub const N_DYN: usize = 12;
pub const N_DYN_OLD: usize = 6;

pub fn onchain_voters() -> Vec<Voter> {
    keys(5, N_ONCHAIN).into_iter().map(Voter::new).collect()
}

pub fn dynamic_voters() -> Vec<Voter> {
    keys(6, N_DYN).into_iter().map(Voter::new).collect()
}

/// Benchmark voters; a shorter set is a prefix of a longer one.
pub fn bench_voters(n: usize) -> Vec<Voter> {
    keys(7, n).into_iter().map(Voter::new).collect()
}

/// [`ballot_mode`] with `nf` fields.
pub fn ballot_mode_nf(nf: u8) -> BallotMode {
    BallotMode {
        num_fields: nf,
        ..ballot_mode()
    }
}

/// Six units spread over `nf >= 2` fields from voter `v`'s offset: each
/// field at most 3, sum 6, within the weight-10 budget.
pub fn choices_nf(v: usize, nf: usize) -> Vec<u64> {
    let mut c = vec![0u64; nf];
    for j in 0..6 {
        c[(5 * v + j) % nf] += 1;
    }
    c
}

/// A voter in no census.
pub fn outsider() -> Voter {
    Voter::new(keys(3, 1).remove(0))
}

pub fn csp_key() -> SigningKey {
    keys(4, 1).remove(0)
}

/// CSP census root: the CSP address as an integer.
pub fn csp_root(csp: &SigningKey) -> Result<Fr> {
    let mut be = [0u8; 32];
    be[12..].copy_from_slice(&eth_address(csp.verifying_key()));
    Ok(fr_from_be(&be)?)
}

/// Round-robin node of a voter's first ballot.
pub fn first_node(v: usize) -> usize {
    v % N_NODES
}

/// Node that gets the duplicate of a round-1 package.
pub fn dup_node(v: usize) -> usize {
    (v + 1) % N_NODES
}

/// Node of a revote: one that did not see the voter's first ballot.
pub fn revote_node(v: usize) -> usize {
    if DUPLICATES.contains(&v) {
        (v + 2) % N_NODES
    } else {
        (v + 1) % N_NODES
    }
}

/// The ballot of voter `v` in `round`: each value at most 5, sum at most 10.
pub fn choices(v: usize, round: usize) -> Vec<u64> {
    let (v, r) = (v as u64, round as u64);
    vec![
        (v + r) % 4,
        (2 * v + 3 * r) % 3,
        (v / 2 + r) % 2,
        (3 * v + r) % 5,
    ]
}

/// Sum of the last ballot of every voter.
pub fn expected_tally(last: &[Vec<u64>]) -> Vec<u64> {
    let mut t = vec![0u64; NUM_FIELDS as usize];
    for b in last {
        for (o, x) in t.iter_mut().zip(b) {
            *o += x;
        }
    }
    t
}

/// Last ballot per Merkle voter: round 2 for revoters, else round 1.
pub fn last_choices() -> Vec<Vec<u64>> {
    (0..N_VOTERS)
        .map(|v| choices(v, if REVOTERS.contains(&v) { 2 } else { 1 }))
        .collect()
}

/// The census file of process 1 and the lean-IMT it commits to.
pub fn merkle_census_of(voters: &[Voter]) -> Result<(CensusFile, LeanImt)> {
    let parts: Vec<_> = voters.iter().map(|v| (v.address(), WEIGHT)).collect();
    let file = census_file(&parts);
    let tree = merkle_census(&file)?;
    Ok((file, tree))
}

/// Writes the census JSON into `dir`; returns its `file://` URI.
pub fn write_census(dir: &Path, name: &str, file: &CensusFile) -> Result<String> {
    write_file(dir, name, &serde_json::to_vec(file)?)
}

/// The lean-IMT of `parts` (address, weight), leaves in order.
pub fn tree_of(parts: &[([u8; 20], u128)]) -> Result<LeanImt> {
    Ok(merkle_census(&census_file(parts))?)
}

/// Writes lean-imt-go's `CensusDump` (numeric root and weights,
/// `addressIndex`) into `dir`; returns its `file://` URI.
pub fn write_census_dump(dir: &Path, name: &str, parts: &[([u8; 20], u128)]) -> Result<String> {
    let root = fr_to_dec(&tree_of(parts)?.root());
    let total: u128 = parts.iter().map(|(_, w)| w).sum();
    let entries: Vec<String> = parts
        .iter()
        .enumerate()
        .map(|(i, (a, w))| {
            format!(
                r#"{{"addressIndex":{i},"address":"0x{}","weight":{w}}}"#,
                hex::encode(a)
            )
        })
        .collect();
    let body = format!(
        r#"{{"root":{root},"timestamp":"2026-09-27T00:00:00Z","totalEntries":{},"totalWeight":{total},"participants":[{}]}}"#,
        parts.len(),
        entries.join(",")
    );
    write_file(dir, name, body.as_bytes())
}

/// Writes one `{"addressIndex","address","weight"}` object per line into
/// `dir`; returns its `file://` URI.
pub fn write_census_jsonl(dir: &Path, name: &str, parts: &[([u8; 20], u128)]) -> Result<String> {
    let mut body = String::new();
    for (i, (a, w)) in parts.iter().enumerate() {
        body.push_str(&format!(
            "{{\"addressIndex\":{i},\"address\":\"0x{}\",\"weight\":{w}}}\n",
            hex::encode(a)
        ));
    }
    write_file(dir, name, body.as_bytes())
}

fn write_file(dir: &Path, name: &str, body: &[u8]) -> Result<String> {
    let p: PathBuf = dir.join(name);
    std::fs::write(&p, body)?;
    let p = p.canonicalize()?;
    Ok(format!("file://{}", p.display()))
}

/// Deterministic ballot secret of voter `v` in `round` of process `tag`.
pub fn vote_k(tag: u64, v: usize, round: usize) -> Fr {
    random_k(&mut rng(0x10 + tag, (v as u64) << 8 | round as u64))
}

/// One ballot to prove.
pub struct Job {
    pub label: String,
    pub key: SigningKey,
    pub process: OnchainProcess,
    pub fields: Vec<u64>,
    pub census: CensusWitness,
    pub weight: u128,
    pub k: Fr,
}

pub fn merkle_witness(tree: &LeanImt, v: usize) -> Result<CensusWitness> {
    Ok(CensusWitness::Merkle(tree.proof(v)?))
}

pub fn csp_witness(
    csp: &SigningKey,
    p: &OnchainProcess,
    voter: &Voter,
    index: u64,
) -> CensusWitness {
    let pid = davinci_client::api::ProcessId(p.id).to_fr();
    CensusWitness::Csp(csp_sign(csp, &pid, &voter.address(), WEIGHT, index))
}

fn build(prover: &BallotProver, j: &Job) -> Result<VoteRequest> {
    let voter = Voter::new(j.key.clone());
    let prep = voter
        .prepare_vote(&j.process, &j.fields, j.census.clone(), j.weight, j.k)
        .with_context(|| format!("prepare {}", j.label))?;
    let (proof, pubs) = prover
        .prove(&prep.inputs)
        .with_context(|| format!("prove {}", j.label))?;
    ensure!(
        pubs == prep.public_signals(),
        "{}: public signals differ",
        j.label
    );
    Ok(voter.finish_vote(prep, proof)?)
}

/// Proves every job, several at a time; results keep the job order.
pub fn prove_all(prover: &BallotProver, jobs: &[Job]) -> Result<Vec<VoteRequest>> {
    let threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .clamp(1, 8);
    prove_all_on(prover, jobs, threads, &AtomicUsize::new(0))
}

/// Proves every job on `threads` threads, each taking the next job as it
/// frees up; `done` counts finished proofs. Results keep the job order; the
/// first error stops the rest.
pub fn prove_all_on(
    prover: &BallotProver,
    jobs: &[Job],
    threads: usize,
    done: &AtomicUsize,
) -> Result<Vec<VoteRequest>> {
    let next = AtomicUsize::new(0);
    let worker = || -> Result<Vec<(usize, VoteRequest)>> {
        let mut out = Vec::new();
        loop {
            let i = next.fetch_add(1, Ordering::Relaxed);
            let Some(j) = jobs.get(i) else {
                return Ok(out);
            };
            match build(prover, j) {
                Ok(r) => out.push((i, r)),
                Err(e) => {
                    next.store(jobs.len(), Ordering::Relaxed);
                    return Err(e);
                }
            }
            done.fetch_add(1, Ordering::Relaxed);
        }
    };
    let parts: Vec<Result<Vec<(usize, VoteRequest)>>> = std::thread::scope(|s| {
        let hs: Vec<_> = (0..threads.clamp(1, jobs.len().max(1)))
            .map(|_| s.spawn(worker))
            .collect();
        hs.into_iter()
            .map(|h| {
                h.join()
                    .unwrap_or_else(|_| Err(anyhow::anyhow!("prover thread panicked")))
            })
            .collect()
    });
    let mut out = Vec::with_capacity(jobs.len());
    for p in parts {
        out.extend(p?);
    }
    out.sort_unstable_by_key(|(i, _)| *i);
    Ok(out.into_iter().map(|(_, r)| r).collect())
}

/// `CIRCOM_ARTIFACTS` (default `../davinci-circom/artifacts` next to the
/// workspace).
pub fn load_prover() -> Result<BallotProver> {
    let dir = std::env::var_os("CIRCOM_ARTIFACTS")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../davinci-circom/artifacts")
        });
    Ok(BallotProver::load(
        &dir.join("ballot_proof.wasm"),
        &dir.join("ballot_proof_pkey.zkey"),
    )?)
}

/// Every package of the scenario. The four negative ones are already
/// tampered.
pub struct Ballots {
    pub round1: Vec<VoteRequest>,
    /// In `REVOTERS` order.
    pub round2: Vec<VoteRequest>,
    pub csp: Vec<VoteRequest>,
    /// `REUSER`'s round-1 `k` (so its vote id) with other choices.
    pub reused: VoteRequest,
    /// `pi_a` replaced by `pi_c`.
    pub bad_proof: VoteRequest,
    /// Vote id signed by the previous voter's key.
    pub bad_sig: VoteRequest,
    /// A voter proved against a census of its own.
    pub outsider: VoteRequest,
}

/// Builds and proves every ballot for process 1 (`p1`, census `tree`) and
/// process 2 (`p2`, CSP `csp`).
pub fn build_ballots(
    prover: &BallotProver,
    p1: &OnchainProcess,
    tree: &LeanImt,
    p2: &OnchainProcess,
    csp: &SigningKey,
) -> Result<Ballots> {
    let voters = merkle_voters();
    let job = |label: String, v: &Voter, p: &OnchainProcess, fields, census, k| Job {
        label,
        key: v.key.clone(),
        process: p.clone(),
        fields,
        census,
        weight: WEIGHT,
        k,
    };
    let mut jobs = Vec::new();
    for (v, voter) in voters.iter().enumerate() {
        let w = merkle_witness(tree, v)?;
        let label = format!("p1 voter {v} round 1");
        jobs.push(job(label, voter, p1, choices(v, 1), w, vote_k(1, v, 1)));
    }
    for v in REVOTERS {
        let w = merkle_witness(tree, v)?;
        let label = format!("p1 voter {v} round 2");
        jobs.push(job(
            label,
            &voters[v],
            p1,
            choices(v, 2),
            w,
            vote_k(1, v, 2),
        ));
    }
    for (i, voter) in csp_voters().iter().enumerate() {
        let w = csp_witness(csp, p2, voter, i as u64);
        let label = format!("p2 voter {i}");
        jobs.push(job(label, voter, p2, choices(i, 1), w, vote_k(2, i, 1)));
    }
    let (r, fields) = (REUSER, choices(REUSER, 2));
    let w = merkle_witness(tree, r)?;
    jobs.push(job(
        "reused vote id".into(),
        &voters[r],
        p1,
        fields,
        w,
        vote_k(1, r, 1),
    ));
    for (v, label) in [(BAD_PROOF, "bad proof"), (BAD_SIG, "bad signature")] {
        let w = merkle_witness(tree, v)?;
        jobs.push(job(
            label.into(),
            &voters[v],
            p1,
            choices(v, 3),
            w,
            vote_k(1, v, 3),
        ));
    }
    // The inputs hash does not bind the census root, so the proof is valid
    // for process 1; only census membership is wrong.
    let out = outsider();
    let mut fake = LeanImt::new();
    fake.insert(census_leaf(&out.address(), WEIGHT)?);
    let mut p1_fake = p1.clone();
    p1_fake.census_root = fake.root();
    let w = CensusWitness::Merkle(fake.proof(0)?);
    jobs.push(job(
        "not in census".into(),
        &out,
        &p1_fake,
        choices(0, 1),
        w,
        vote_k(3, 0, 1),
    ));

    let mut all = prove_all(prover, &jobs)?.into_iter();
    let mut take = |n: usize| all.by_ref().take(n).collect::<Vec<_>>();
    let round1 = take(N_VOTERS);
    let round2 = take(REVOTERS.len());
    let csp_votes = take(N_CSP);
    let [reused, mut bad_proof, mut bad_sig, outsider]: [VoteRequest; 4] = take(4)
        .try_into()
        .map_err(|_| anyhow::anyhow!("missing ballots"))?;
    let pr = &mut bad_proof.ballot_proof;
    pr.pi_a = pr.pi_c.clone();
    let sig = vote_id_sign(&voters[BAD_SIG - 1].key, bad_sig.vote_id);
    bad_sig.signature[..32].copy_from_slice(&sig.r);
    bad_sig.signature[32..64].copy_from_slice(&sig.s);
    bad_sig.signature[64] = sig.v;
    Ok(Ballots {
        round1,
        round2,
        csp: csp_votes,
        reused,
        bad_proof,
        bad_sig,
        outsider,
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn bench_choices_are_valid() {
        for nf in 2..=16 {
            for v in 0..40 {
                let c = choices_nf(v, nf);
                assert_eq!(c.len(), nf);
                assert_eq!(c.iter().sum::<u64>(), 6);
                assert!(c.iter().all(|x| *x <= 5), "{nf} {v} {c:?}");
            }
        }
    }

    use super::*;

    #[test]
    fn choices_fit_the_ballot_mode() {
        let m = ballot_mode();
        for v in 0..N_VOTERS {
            for r in 1..=8 {
                let c = choices(v, r);
                assert_eq!(c.len(), NUM_FIELDS as usize);
                assert!(c.iter().all(|x| *x <= m.max_value));
                assert!(c.iter().sum::<u64>() as u128 <= WEIGHT);
            }
        }
        // Revotes and the reused-k ballot change something.
        for v in REVOTERS.iter().chain([&REUSER]) {
            assert_ne!(choices(*v, 1), choices(*v, 2));
        }
    }

    #[test]
    fn census_documents_parse() {
        let parts: Vec<_> = dynamic_voters()
            .iter()
            .map(|v| (v.address(), WEIGHT))
            .collect();
        let dir = tempfile::tempdir().unwrap();
        let uri = write_census_dump(dir.path(), "d.json", &parts).unwrap();
        let body = std::fs::read(uri.trim_start_matches("file://")).unwrap();
        let j: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let root = fr_to_dec(&tree_of(&parts).unwrap().root());
        // A bare JSON number, as big.Int dumps it.
        let raw = std::str::from_utf8(&body).unwrap();
        assert!(raw.starts_with(&format!("{{\"root\":{root},")));
        assert_eq!(j["participants"].as_array().unwrap().len(), N_DYN);
        assert_eq!(j["participants"][3]["addressIndex"], 3);
        let uri = write_census_jsonl(dir.path(), "d.jsonl", &parts).unwrap();
        let body = std::fs::read_to_string(uri.trim_start_matches("file://")).unwrap();
        let lines: Vec<serde_json::Value> = body
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(lines.len(), N_DYN);
        assert_eq!(
            lines[1]["address"].as_str().unwrap(),
            format!("0x{}", hex::encode(parts[1].0))
        );
        assert_eq!(lines[1]["weight"], 10);
    }

    #[test]
    fn routing_is_consistent() {
        for v in 0..N_VOTERS {
            assert_ne!(first_node(v), revote_node(v));
            if DUPLICATES.contains(&v) {
                assert_ne!(first_node(v), dup_node(v));
                assert_ne!(dup_node(v), revote_node(v));
            }
        }
        assert!(!REVOTERS.contains(&REUSER));
    }

    #[test]
    fn fixture_is_deterministic() {
        let a: Vec<_> = merkle_voters().iter().map(|v| v.address()).collect();
        let b: Vec<_> = merkle_voters().iter().map(|v| v.address()).collect();
        assert_eq!(a, b);
        let mut all = a.clone();
        all.extend(csp_voters().iter().map(|v| v.address()));
        all.push(outsider().address());
        all.extend(onchain_voters().iter().map(|v| v.address()));
        all.extend(dynamic_voters().iter().map(|v| v.address()));
        all.sort();
        all.dedup();
        assert_eq!(all.len(), N_VOTERS + N_CSP + 1 + N_ONCHAIN + N_DYN);
        assert_eq!(vote_k(1, 3, 1), vote_k(1, 3, 1));
        assert_ne!(vote_k(1, 3, 1), vote_k(1, 3, 2));
        assert_eq!(
            expected_tally(&[vec![1, 2, 0, 4], vec![3, 0, 1, 1]]),
            [4, 2, 1, 5]
        );
    }
}
