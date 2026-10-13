//! Ahead-of-run ballot generation: proves the shards a host owns from the
//! campaign seed and the plan's process snapshots, with no RPC.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use davinci_client::api::Fr;
use davinci_client::prover::BallotProver;
use davinci_client::voter::Voter;
use davinci_zkvm_sdk::census::{CensusWitness, LeanImt, csp_sign, eth_address};
use k256::ecdsa::SigningKey;

use super::ballots::{
    self, BallotRecord, SHARD, ShardId, ShardMeta, assigned, done, sha256_file, shard_path,
};
use super::plan::{Election, Plan, Seed};
use crate::fixture as fx;

/// Voting weight of every benchmark voter.
pub const WEIGHT: u128 = 10;

/// Where an election's census witnesses come from.
pub enum CensusSource {
    Merkle(LeanImt),
    Csp(SigningKey),
}

/// Origins 1 to 3 commit to `(addr, 10)` in voter order; 4 is a CSP.
pub fn census_source(e: &Election, seed: &Seed, wave: u32) -> Result<CensusSource> {
    match e.spec.origin {
        1..=3 => {
            let parts: Vec<_> = (0..e.spec.voters)
                .map(|v| {
                    let k = seed.voter_key(&e.spec.tag, v);
                    (eth_address(k.verifying_key()), WEIGHT)
                })
                .collect();
            Ok(CensusSource::Merkle(fx::tree_of(&parts)?))
        }
        4 => Ok(CensusSource::Csp(seed.csp_key(wave))),
        o => bail!("{}: census origin {o} is not supported", e.spec.tag),
    }
}

/// The census root a process of `e` commits to.
pub fn census_root(e: &Election, seed: &Seed, wave: u32) -> Result<Fr> {
    Ok(match census_source(e, seed, wave)? {
        CensusSource::Merkle(t) => t.root(),
        CensusSource::Csp(k) => fx::csp_root(&k)?,
    })
}

pub struct GenOpts {
    pub host: usize,
    pub hosts: usize,
    pub threads: usize,
    pub host_name: String,
    pub artifacts: PathBuf,
}

#[derive(Debug, Clone, PartialEq)]
pub struct GenStats {
    /// Shards assigned to this host.
    pub shards: usize,
    /// Of those, already done and left alone.
    pub skipped: usize,
    /// Ballots proved by this call.
    pub ballots: usize,
    pub secs: f64,
}

/// The `(round, voter)` pairs of one shard.
fn shard_ballots(e: &Election, seed: &Seed, s: &ShardId) -> Vec<u32> {
    e.ballots(seed)
        .into_iter()
        .filter(|(r, v)| *r == s.round && (s.from..s.to).contains(v))
        .map(|(_, v)| v)
        .collect()
}

fn jobs_of(
    e: &Election,
    seed: &Seed,
    src: &CensusSource,
    s: &ShardId,
    voters: &[u32],
) -> Result<Vec<fx::Job>> {
    let process = e.snap.onchain()?;
    let pid = e.snap.pid.to_fr();
    voters
        .iter()
        .map(|&v| {
            let key = seed.voter_key(&s.tag, v);
            let census = match src {
                CensusSource::Merkle(t) => fx::merkle_witness(t, v as usize)?,
                CensusSource::Csp(csp) => CensusWitness::Csp(csp_sign(
                    csp,
                    &pid,
                    &Voter::new(key.clone()).address(),
                    WEIGHT,
                    u64::from(v),
                )),
            };
            Ok(fx::Job {
                label: format!("{} r{} v{v}", s.tag, s.round),
                key,
                process: process.clone(),
                fields: seed.choices(&s.tag, v, s.round, e.spec.nf),
                census,
                weight: WEIGHT,
                k: seed.vote_k(&s.tag, v, s.round),
            })
        })
        .collect()
}

pub fn gen_wave(
    campaign: &Path,
    plan: &Plan,
    wave: u32,
    seed: &Seed,
    prover: &BallotProver,
    o: &GenOpts,
) -> Result<GenStats> {
    let t0 = Instant::now();
    let w = plan.wave(wave)?;
    let mine = assigned(&ballots::shards(w, seed, SHARD), o.host, o.hosts)?;
    let (pending, skipped): (Vec<_>, Vec<_>) = mine
        .iter()
        .partition(|s| !done(&shard_path(campaign, wave, s)));
    let election = |tag: &str| {
        w.elections
            .iter()
            .find(|e| e.spec.tag == tag)
            .with_context(|| format!("shard of unknown election {tag}"))
    };
    let todo: Vec<(&ShardId, Vec<u32>)> = pending
        .iter()
        .map(|s| Ok((*s, shard_ballots(election(&s.tag)?, seed, s))))
        .collect::<Result<_>>()?;
    let total: usize = todo.iter().map(|(_, v)| v.len()).sum();
    let hashes = (
        sha256_file(&std::env::current_exe()?).unwrap_or_default(),
        sha256_file(&o.artifacts.join("ballot_proof.wasm"))?,
        sha256_file(&o.artifacts.join("ballot_proof_pkey.zkey"))?,
    );

    let proved = AtomicUsize::new(0);
    let shards_done = AtomicUsize::new(0);
    let stop = AtomicBool::new(false);
    let nshards = todo.len();
    let res = std::thread::scope(|sc| {
        sc.spawn(|| {
            let mut last = Instant::now();
            while !stop.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(200));
                if last.elapsed() < Duration::from_secs(30) {
                    continue;
                }
                last = Instant::now();
                let n = proved.load(Ordering::Relaxed);
                let rate = n as f64 / t0.elapsed().as_secs_f64().max(1e-9);
                let eta = if rate > 0.0 {
                    (total - n) as f64 / rate / 60.0
                } else {
                    f64::NAN
                };
                println!(
                    "gen host {}/{}: {}/{} shards, {n} ballots, {rate:.2}/s, eta {eta:.0} min",
                    o.host,
                    o.hosts,
                    shards_done.load(Ordering::Relaxed),
                    nshards,
                );
            }
        });
        let r = (|| -> Result<()> {
            let mut sources: HashMap<&str, CensusSource> = HashMap::new();
            for (s, voters) in &todo {
                let e = election(&s.tag)?;
                if !sources.contains_key(s.tag.as_str()) {
                    sources.insert(&s.tag, census_source(e, seed, wave)?);
                }
                let jobs = jobs_of(e, seed, &sources[s.tag.as_str()], s, voters)?;
                let ts = Instant::now();
                let reqs = fx::prove_all_on(prover, &jobs, o.threads, &proved)?;
                let recs: Vec<BallotRecord> = voters
                    .iter()
                    .zip(reqs)
                    .map(|(&voter, req)| BallotRecord {
                        tag: s.tag.clone(),
                        voter,
                        round: s.round,
                        choices: seed.choices(&s.tag, voter, s.round, e.spec.nf),
                        req,
                    })
                    .collect();
                let meta = ShardMeta {
                    wave,
                    tag: s.tag.clone(),
                    round: s.round,
                    from: s.from,
                    to: s.to,
                    count: 0,
                    sha256: String::new(),
                    host: o.host_name.clone(),
                    bin_sha256: hashes.0.clone(),
                    wasm_sha256: hashes.1.clone(),
                    zkey_sha256: hashes.2.clone(),
                    secs: ts.elapsed().as_secs_f64(),
                };
                ballots::write_shard(&shard_path(campaign, wave, s), &recs, meta)?;
                shards_done.fetch_add(1, Ordering::Relaxed);
            }
            Ok(())
        })();
        stop.store(true, Ordering::Relaxed);
        r
    });
    res?;
    Ok(GenStats {
        shards: mine.len(),
        skipped: skipped.len(),
        ballots: proved.load(Ordering::Relaxed),
        secs: t0.elapsed().as_secs_f64(),
    })
}
