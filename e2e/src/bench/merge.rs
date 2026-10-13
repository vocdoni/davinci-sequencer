//! Joins the shards of both generating hosts into one file per election,
//! after checking every ballot against the plan and the seed.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail, ensure};
use davinci_client::api::Fr;
use davinci_client::organizer::RegistryReader;
use davinci_zkvm_sdk::ballot::address_to_fr;
use davinci_zkvm_sdk::census::{eth_address, vote_id_recover};
use davinci_zkvm_sdk::groth16::BallotVerifier;
use davinci_zkvm_sdk::release;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::ballots::{self, BallotRecord, SHARD, read_shard, shard_path};
use super::r#gen::WEIGHT;
use super::plan::{Election, Plan, ProcSnap, Seed};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verify {
    Full,
    Skip,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ElectionIndex {
    pub ballots: usize,
    pub rounds: BTreeMap<u8, usize>,
    pub sha256: String,
}

fn proof_ok(r: &BallotRecord, v: &BallotVerifier) -> Result<()> {
    let q = &r.req;
    let pubs: [Fr; 3] = [
        address_to_fr(&q.address),
        Fr::from(q.vote_id),
        q.ballot_inputs_hash,
    ];
    ensure!(
        v.verify(&q.ballot_proof, &pubs),
        "{} round {} voter {}: ballot proof does not verify",
        r.tag,
        r.round,
        r.voter
    );
    Ok(())
}

/// One record against the plan and the seed: its pid, voter address,
/// choices, weight and vote-id signature, and with a verifier its proof.
pub fn check_record(
    e: &Election,
    seed: &Seed,
    r: &BallotRecord,
    v: Option<&BallotVerifier>,
) -> Result<()> {
    let who = format!("{} round {} voter {}", r.tag, r.round, r.voter);
    let q = &r.req;
    ensure!(r.tag == e.spec.tag, "{who}: record of another election");
    ensure!(
        q.process_id == e.snap.pid,
        "{who}: pid {} is not the plan's {}",
        q.process_id,
        e.snap.pid
    );
    let want = eth_address(seed.voter_key(&r.tag, r.voter).verifying_key());
    ensure!(q.address == want, "{who}: address is not the seed's voter");
    ensure!(
        r.choices == seed.choices(&r.tag, r.voter, r.round, e.spec.nf),
        "{who}: choices differ from the seed's"
    );
    ensure!(
        q.weight == WEIGHT,
        "{who}: weight {} is not {WEIGHT}",
        q.weight
    );
    let got = vote_id_recover(q.vote_id, &q.signature())
        .map_err(|e| anyhow!("{who}: vote id signature: {e}"))?;
    ensure!(got == want, "{who}: vote id signed by another key");
    if let Some(v) = v {
        proof_ok(r, v)?;
    }
    Ok(())
}

fn merged_dir(campaign: &Path, wave: u32) -> PathBuf {
    campaign.join("ballots").join(format!("w{wave}"))
}

fn write_atomic(path: &Path, body: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, body).with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("rename to {}", path.display()))
}

pub fn merge_wave(
    campaign: &Path,
    plan: &Plan,
    wave: u32,
    seed: &Seed,
    v: Verify,
) -> Result<BTreeMap<String, ElectionIndex>> {
    let w = plan.wave(wave)?;
    let all = ballots::shards(w, seed, SHARD);
    let verifier = match v {
        Verify::Full => Some(BallotVerifier::from_snarkjs_json(release::ballot_vk_json())?),
        Verify::Skip => None,
    };
    let dir = merged_dir(campaign, wave);
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;

    let mut vote_ids: HashMap<u64, (String, u8, u32)> = HashMap::new();
    let mut merged: Vec<(String, Vec<BallotRecord>)> = Vec::new();
    for e in &w.elections {
        let tag = &e.spec.tag;
        let mut recs: Vec<BallotRecord> = Vec::new();
        for s in all.iter().filter(|s| &s.tag == tag) {
            let range = format!("{} round {} voters {}..{}", s.tag, s.round, s.from, s.to);
            let path = shard_path(campaign, wave, s);
            ensure!(path.exists(), "missing shard {range} ({})", path.display());
            ensure!(
                ballots::done(&path),
                "shard {range} is damaged or not finished"
            );
            let (meta, got) = read_shard(&path)?;
            ensure!(
                (meta.wave, meta.round, meta.from, meta.to) == (wave, s.round, s.from, s.to)
                    && meta.tag == s.tag,
                "shard {range}: its meta describes another shard"
            );
            let mut want: HashSet<(u8, u32)> = e
                .ballots(seed)
                .into_iter()
                .filter(|(r, v)| *r == s.round && (s.from..s.to).contains(v))
                .collect();
            for r in &got {
                ensure!(
                    want.remove(&(r.round, r.voter)),
                    "shard {range}: unplanned or repeated ballot round {} voter {}",
                    r.round,
                    r.voter
                );
                check_record(e, seed, r, None)?;
                if let Some(prev) =
                    vote_ids.insert(r.req.vote_id, (r.tag.clone(), r.round, r.voter))
                {
                    bail!(
                        "duplicate vote id {:#x}: {} round {} voter {} and {} round {} voter {}",
                        r.req.vote_id,
                        prev.0,
                        prev.1,
                        prev.2,
                        r.tag,
                        r.round,
                        r.voter
                    );
                }
            }
            if let Some(&(round, voter)) = want.iter().min() {
                bail!(
                    "shard {range}: missing {} ballots, first round {round} voter {voter}",
                    want.len()
                );
            }
            recs.extend(got);
        }
        recs.sort_by_key(|r| (r.round, r.voter));
        merged.push((tag.clone(), recs));
    }

    if let Some(vf) = &verifier {
        let flat: Vec<&BallotRecord> = merged.iter().flat_map(|(_, r)| r).collect();
        let threads = std::thread::available_parallelism().map_or(4, |n| n.get());
        let next = std::sync::atomic::AtomicUsize::new(0);
        std::thread::scope(|sc| {
            let hs: Vec<_> = (0..threads)
                .map(|_| {
                    sc.spawn(|| -> Result<()> {
                        loop {
                            let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            let Some(r) = flat.get(i) else {
                                return Ok(());
                            };
                            if let Err(e) = proof_ok(r, vf) {
                                next.store(flat.len(), std::sync::atomic::Ordering::Relaxed);
                                return Err(e);
                            }
                        }
                    })
                })
                .collect();
            hs.into_iter()
                .map(|h| {
                    h.join()
                        .unwrap_or_else(|_| Err(anyhow!("verify thread panicked")))
                })
                .collect::<Result<Vec<_>>>()
        })?;
    }

    let mut index = BTreeMap::new();
    for (tag, recs) in merged {
        let mut body = Vec::new();
        let mut rounds = BTreeMap::new();
        for r in &recs {
            serde_json::to_writer(&mut body, r)?;
            body.push(b'\n');
            *rounds.entry(r.round).or_insert(0usize) += 1;
        }
        write_atomic(&dir.join(format!("{tag}.jsonl")), &body)?;
        index.insert(
            tag,
            ElectionIndex {
                ballots: recs.len(),
                rounds,
                sha256: hex::encode(Sha256::digest(&body)),
            },
        );
    }
    write_atomic(&dir.join("index.json"), &serde_json::to_vec_pretty(&index)?)?;
    Ok(index)
}

/// Live: every planned process is on the registry with the snapshot's
/// pid, key, mode and origin (and root, except origin 3 where it moves).
pub async fn check_snapshots(plan: &Plan, wave: u32, rpc: &str) -> Result<()> {
    let reader = RegistryReader::connect(rpc, plan.registry)?;
    for e in &plan.wave(wave)?.elections {
        let p = reader
            .process(&e.snap.pid.0)
            .await
            .with_context(|| format!("read process of {}", e.spec.tag))?;
        let mut live = ProcSnap::of(&p);
        if e.snap.census_origin == 3 {
            live.census_root = e.snap.census_root.clone();
        }
        ensure!(
            live == e.snap,
            "{}: the registry's process differs from the plan's snapshot",
            e.spec.tag
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bench::ballots::{ShardMeta, fake_req, write_shard};
    use crate::bench::plan::{ElectionSpec, NodeId};
    use davinci_zkvm_sdk::census::vote_id_sign;

    fn plan(seed: &Seed) -> Plan {
        let spec = |tag: &str, voters, rounds, pm| ElectionSpec {
            tag: tag.into(),
            scenario: "t".into(),
            nf: 2,
            origin: 1,
            voters,
            rounds,
            revote_permille: pm,
            key_node: NodeId::A,
            grace: 60,
        };
        Plan::stand_in(seed, 1, &[spec("A", 4, 2, 1000), spec("B", 3, 1, 0)]).unwrap()
    }

    fn rec(e: &Election, seed: &Seed, round: u8, voter: u32, vid: u64) -> BallotRecord {
        let key = seed.voter_key(&e.spec.tag, voter);
        let mut req = fake_req(vid, eth_address(key.verifying_key()));
        req.process_id = e.snap.pid;
        let sig = vote_id_sign(&key, vid);
        req.signature[..32].copy_from_slice(&sig.r);
        req.signature[32..64].copy_from_slice(&sig.s);
        req.signature[64] = sig.v;
        BallotRecord {
            tag: e.spec.tag.clone(),
            voter,
            round,
            choices: seed.choices(&e.spec.tag, voter, round, e.spec.nf),
            req,
        }
    }

    /// Writes every shard of the wave; `vid` picks a vote id per record.
    fn write_all(
        dir: &Path,
        p: &Plan,
        seed: &Seed,
        skip: Option<usize>,
        vid: impl Fn(&BallotRecord) -> u64,
    ) {
        let w = p.wave(1).unwrap();
        for (i, s) in ballots::shards(w, seed, SHARD).iter().enumerate() {
            if skip == Some(i) {
                continue;
            }
            let e = w.elections.iter().find(|e| e.spec.tag == s.tag).unwrap();
            let recs: Vec<_> = e
                .ballots(seed)
                .into_iter()
                .filter(|(r, v)| *r == s.round && (s.from..s.to).contains(v))
                .map(|(r, v)| {
                    let mut x = rec(e, seed, r, v, 0);
                    x.req.vote_id = vid(&x);
                    rec(e, seed, r, v, x.req.vote_id)
                })
                .collect();
            let meta = ShardMeta {
                wave: 1,
                tag: s.tag.clone(),
                round: s.round,
                from: s.from,
                to: s.to,
                count: 0,
                sha256: String::new(),
                host: "z6".into(),
                bin_sha256: String::new(),
                wasm_sha256: String::new(),
                zkey_sha256: String::new(),
                secs: 0.0,
            };
            write_shard(&shard_path(dir, 1, s), &recs, meta).unwrap();
        }
    }

    fn vid(r: &BallotRecord) -> u64 {
        1 << 63 | u64::from(r.voter) << 8 | u64::from(r.round) << 4 | u64::from(r.tag == "B")
    }

    #[test]
    fn merge_reports_missing_and_duplicates() {
        let seed = Seed::from_bytes([9; 32]);
        let p = plan(&seed);
        // Round 2 of A is shard index 1 (A r1, A r2, B r1).
        let d = tempfile::tempdir().unwrap();
        write_all(d.path(), &p, &seed, Some(1), vid);
        let err = format!(
            "{:#}",
            merge_wave(d.path(), &p, 1, &seed, Verify::Skip).unwrap_err()
        );
        assert!(
            err.contains("A") && err.contains("round 2") && err.contains("0..4"),
            "{err}"
        );

        let d = tempfile::tempdir().unwrap();
        write_all(d.path(), &p, &seed, None, |r| {
            if r.tag == "B" { 1 << 63 | 1 } else { vid(r) }
        });
        // B's three ballots now share one vote id.
        let err = format!(
            "{:#}",
            merge_wave(d.path(), &p, 1, &seed, Verify::Skip).unwrap_err()
        );
        assert!(err.contains("duplicate vote id"), "{err}");

        // A foreign pid in one record.
        let d = tempfile::tempdir().unwrap();
        write_all(d.path(), &p, &seed, None, vid);
        let s = ballots::shards(p.wave(1).unwrap(), &seed, SHARD).remove(0);
        let path = shard_path(d.path(), 1, &s);
        let (m, mut recs) = read_shard(&path).unwrap();
        recs[0].req.process_id = davinci_client::api::ProcessId([0xee; 31]);
        write_shard(&path, &recs, m).unwrap();
        let err = format!(
            "{:#}",
            merge_wave(d.path(), &p, 1, &seed, Verify::Skip).unwrap_err()
        );
        assert!(err.contains("pid"), "{err}");
    }

    #[test]
    fn merge_writes_round_voter_order() {
        let seed = Seed::from_bytes([9; 32]);
        let p = plan(&seed);
        let d = tempfile::tempdir().unwrap();
        write_all(d.path(), &p, &seed, None, vid);
        let idx = merge_wave(d.path(), &p, 1, &seed, Verify::Skip).unwrap();
        assert_eq!(idx["A"].rounds, BTreeMap::from([(1, 4), (2, 4)]));
        assert_eq!((idx["A"].ballots, idx["B"].ballots), (8, 3));
        let body = std::fs::read_to_string(d.path().join("ballots/w1/A.jsonl")).unwrap();
        let order: Vec<(u8, u32)> = body
            .lines()
            .map(|l| serde_json::from_str::<BallotRecord>(l).unwrap())
            .map(|r| (r.round, r.voter))
            .collect();
        let mut sorted = order.clone();
        sorted.sort();
        assert_eq!(order, sorted);
        assert_eq!(order.len(), 8);
        assert_eq!(
            idx["A"].sha256,
            ballots::sha256_file(&d.path().join("ballots/w1/A.jsonl")).unwrap()
        );
        let on_disk: BTreeMap<String, ElectionIndex> =
            serde_json::from_slice(&std::fs::read(d.path().join("ballots/w1/index.json")).unwrap())
                .unwrap();
        assert_eq!(on_disk, idx);
    }
}
