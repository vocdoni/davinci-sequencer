//! Joins the shards of both generating hosts into one file per election,
//! after checking every ballot against the plan and the seed.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail, ensure};
use davinci_client::api::CensusProofWire;
use davinci_client::api::Fr;
use davinci_client::organizer::OnchainProcess;
use davinci_client::organizer::RegistryReader;
use davinci_zkvm_sdk::ballot::{Ballot, address_to_fr, encrypt_ballot, public_signals, vote_id};
use davinci_zkvm_sdk::census::{
    CspProof, csp_recover, eth_address, node_from_hex, vote_id_recover,
};
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

// Against the 71 public signals the guest derives from the record and the
// planned process.
fn proof_ok(e: &Election, r: &BallotRecord, v: &BallotVerifier) -> Result<()> {
    let q = &r.req;
    let p = e.snap.onchain()?;
    let pubs = public_signals(
        &q.process_id.to_fr(),
        &p.ballot_mode,
        &p.encryption_key,
        &address_to_fr(&q.address),
        q.vote_id,
        &q.ballot,
        &Fr::from(q.weight),
    )?;
    ensure!(
        v.verify(&q.ballot_proof, &pubs),
        "{} round {} voter {}: ballot proof does not verify",
        r.tag,
        r.round,
        r.voter
    );
    Ok(())
}

/// The ballot and vote id a voter's record must carry, from the same
/// functions `Voter::prepare_vote` uses.
pub fn binding(e: &Election, seed: &Seed, round: u8, voter: u32) -> Result<(Ballot, u64)> {
    let p = e.snap.onchain()?;
    let tag = &e.spec.tag;
    let key = seed.voter_key(tag, voter);
    let addr = address_to_fr(&eth_address(key.verifying_key()));
    let pid = e.snap.pid.to_fr();
    let k = seed.vote_k(tag, voter, round);
    let fields = seed.choices(tag, voter, round, e.spec.nf);
    let ballot = encrypt_ballot(&p.encryption_key, &fields, &k, p.ballot_mode.num_fields);
    let vid = vote_id(&pid, &addr, &k);
    Ok((ballot, vid))
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
    let (ballot, vid) = binding(e, seed, r.round, r.voter)?;
    ensure!(
        q.ballot == ballot,
        "{who}: ballot is not the seed's under the plan's key and mode"
    );
    ensure!(q.vote_id == vid, "{who}: vote id is not the seed's");
    if e.snap.census_origin == 4 {
        let Some(CensusProofWire::Csp(c)) = &q.census_proof else {
            bail!("{who}: CSP election without a CSP attestation");
        };
        ensure!(
            c.index == u64::from(r.voter),
            "{who}: CSP index {} is not the voter's",
            c.index
        );
        let att = CspProof {
            r: c.r,
            s: c.s,
            recid: c.recid,
            address: q.address,
            weight: q.weight,
            index: c.index,
        };
        let signer = csp_recover(&e.snap.pid.to_fr(), &att)
            .map_err(|x| anyhow!("{who}: CSP attestation: {x}"))?;
        let root = node_from_hex(&e.snap.census_root).map_err(|x| anyhow!("{x}"))?;
        ensure!(
            root[..12] == [0u8; 12] && root[12..] == signer,
            "{who}: CSP attestation is not signed by the census root's CSP"
        );
    }
    if let Some(v) = v {
        proof_ok(e, r, v)?;
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
    // After the vote-id scan, so a repeated id is named as such.
    for (e, (_, recs)) in w.elections.iter().zip(&merged) {
        for r in recs {
            check_record(e, seed, r, None)?;
        }
    }

    if let Some(vf) = &verifier {
        let flat: Vec<(&Election, &BallotRecord)> = w
            .elections
            .iter()
            .zip(&merged)
            .flat_map(|(e, (_, recs))| recs.iter().map(move |r| (e, r)))
            .collect();
        let threads = std::thread::available_parallelism().map_or(4, |n| n.get());
        let next = std::sync::atomic::AtomicUsize::new(0);
        std::thread::scope(|sc| {
            let hs: Vec<_> = (0..threads)
                .map(|_| {
                    sc.spawn(|| -> Result<()> {
                        loop {
                            let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            let Some(&(e, r)) = flat.get(i) else {
                                return Ok(());
                            };
                            if let Err(e) = proof_ok(e, r, vf) {
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

/// Whether the registry's process is the one the plan snapshots: pid, key,
/// mode and origin, and the root except for origin 3, where it moves.
pub fn snapshot_matches(e: &Election, p: &OnchainProcess) -> Result<()> {
    let mut live = ProcSnap::of(p);
    if e.snap.census_origin == 3 {
        live.census_root = e.snap.census_root.clone();
    }
    ensure!(
        live == e.snap,
        "{}: the registry's process differs from the plan's snapshot",
        e.spec.tag
    );
    Ok(())
}

/// Live: every planned process is on the registry as snapshotted.
pub async fn check_snapshots(plan: &Plan, wave: u32, rpc: &str) -> Result<()> {
    let reader = RegistryReader::connect(rpc, plan.registry)?;
    for e in &plan.wave(wave)?.elections {
        let p = reader
            .process(&e.snap.pid.0)
            .await
            .with_context(|| format!("read process of {}", e.spec.tag))?;
        snapshot_matches(e, &p)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bench::ballots::{ShardMeta, fake_req, write_shard};
    use crate::bench::plan::{ElectionSpec, NodeId};
    use davinci_zkvm_sdk::ballot::Ballot;
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
        let (ballot, vid) = binding(e, seed, round, voter).unwrap();
        (req.ballot, req.vote_id) = (ballot, vid);
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
        tweak: impl Fn(&mut BallotRecord),
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
                    tweak(&mut x);
                    x
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

    fn same(_: &mut BallotRecord) {}

    #[test]
    fn merge_reports_missing_and_duplicates() {
        let seed = Seed::from_bytes([9; 32]);
        let p = plan(&seed);
        // Round 2 of A is shard index 1 (A r1, A r2, B r1).
        let d = tempfile::tempdir().unwrap();
        write_all(d.path(), &p, &seed, Some(1), same);
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
            if r.tag == "B" {
                r.req.vote_id = 1 << 63 | 1;
            }
        });
        // B's three ballots now share one vote id.
        let err = format!(
            "{:#}",
            merge_wave(d.path(), &p, 1, &seed, Verify::Skip).unwrap_err()
        );
        assert!(err.contains("duplicate vote id"), "{err}");

        // A foreign pid in one record.
        let d = tempfile::tempdir().unwrap();
        write_all(d.path(), &p, &seed, None, same);
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
    fn check_record_rejects_another_voters_signature_and_binding() {
        let seed = Seed::from_bytes([9; 32]);
        let p = plan(&seed);
        let e = &p.wave(1).unwrap().elections[0];
        let good = rec(e, &seed, 1, 0, 0);
        check_record(e, &seed, &good, None).unwrap();
        let mut bad = good.clone();
        bad.req.signature = rec(e, &seed, 1, 1, 0).req.signature;
        assert!(check_record(e, &seed, &bad, None).is_err());
        let mut bad = good.clone();
        bad.req.ballot = Ballot::identity();
        let err = check_record(e, &seed, &bad, None).unwrap_err().to_string();
        assert!(err.contains("ballot"), "{err}");
        let mut bad = good;
        bad.req.vote_id ^= 1;
        assert!(check_record(e, &seed, &bad, None).is_err());
    }

    #[test]
    fn snapshot_matches_compares_key_mode_and_root() {
        let seed = Seed::from_bytes([9; 32]);
        let p = plan(&seed);
        let e = &p.wave(1).unwrap().elections[0];
        let mut live = e.snap.onchain().unwrap();
        snapshot_matches(e, &live).unwrap();
        live.census_root[31] ^= 1;
        assert!(snapshot_matches(e, &live).is_err());
        let mut e3 = e.clone();
        e3.snap.census_origin = 3;
        live.census_origin = 3;
        snapshot_matches(&e3, &live).unwrap();
        live.ballot_mode.max_value += 1;
        assert!(snapshot_matches(&e3, &live).is_err());
    }

    #[test]
    fn merge_writes_round_voter_order() {
        let seed = Seed::from_bytes([9; 32]);
        let p = plan(&seed);
        let d = tempfile::tempdir().unwrap();
        write_all(d.path(), &p, &seed, None, same);
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
