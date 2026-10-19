//! Ballot shards: JSONL files of generated ballots with a sidecar that
//! records their hash, and the assignment of shards to generating hosts.

use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use davinci_client::api::VoteRequest;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::plan::{Seed, Wave};

/// One generated ballot. Holds no voter key and no `k`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BallotRecord {
    pub tag: String,
    pub voter: u32,
    pub round: u8,
    pub choices: Vec<u64>,
    pub req: VoteRequest,
}

/// The sidecar `<shard>.meta.json`, written after the shard.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ShardMeta {
    pub wave: u32,
    pub tag: String,
    pub round: u8,
    pub from: u32,
    pub to: u32,
    pub count: u32,
    pub sha256: String,
    pub host: String,
    pub bin_sha256: String,
    pub wasm_sha256: String,
    pub zkey_sha256: String,
    pub secs: f64,
}

/// Voters `[from, to)` of one election round.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShardId {
    pub tag: String,
    pub round: u8,
    pub from: u32,
    pub to: u32,
}

/// Voters per shard.
pub const SHARD: u32 = 500;

/// Every non-empty shard of the wave, ordered by `(tag, round, from)`. A
/// block of `size` voters is a shard of a round if it holds a ballot of it.
pub fn shards(wave: &Wave, seed: &Seed, size: u32) -> Vec<ShardId> {
    let size = size.max(1);
    let mut out = Vec::new();
    for e in &wave.elections {
        let mut ids = Vec::new();
        for (round, voter) in e.ballots(seed) {
            let from = voter / size * size;
            let id = ShardId {
                tag: e.spec.tag.clone(),
                round,
                from,
                to: from.saturating_add(size).min(e.spec.voters),
            };
            if ids.last() != Some(&id) {
                ids.push(id);
            }
        }
        // `ballots` is ordered by round then voter, so equal ids are adjacent.
        out.extend(ids);
    }
    out.sort_by(|a, b| (&a.tag, a.round, a.from).cmp(&(&b.tag, b.round, b.from)));
    out.dedup();
    out
}

/// Host `host` of `hosts` takes the shards whose index is `host` mod `hosts`.
pub fn assigned(all: &[ShardId], host: usize, hosts: usize) -> Result<Vec<ShardId>> {
    ensure!(host < hosts, "host {host} out of range for {hosts} hosts");
    Ok(all
        .iter()
        .enumerate()
        .filter(|(i, _)| i % hosts == host)
        .map(|(_, s)| s.clone())
        .collect())
}

pub fn shard_path(campaign: &Path, wave: u32, s: &ShardId) -> PathBuf {
    campaign
        .join("shards")
        .join(format!("w{wave}"))
        .join(&s.tag)
        .join(format!("r{}-{}-{}.jsonl", s.round, s.from, s.to))
}

fn meta_path(path: &Path) -> PathBuf {
    path.with_extension("jsonl.meta.json")
}

pub fn sha256_file(path: &Path) -> Result<String> {
    let mut f = std::fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = f
            .read(&mut buf)
            .with_context(|| format!("read {}", path.display()))?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(hex::encode(h.finalize()))
}

/// Writes the shard through `.tmp` and a rename, then the meta (with the
/// real count and hash), so a meta never exists for a partial shard.
pub fn write_shard(path: &Path, recs: &[BallotRecord], mut meta: ShardMeta) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    }
    let mut body = Vec::new();
    for r in recs {
        serde_json::to_writer(&mut body, r)?;
        body.push(b'\n');
    }
    let tmp = path.with_extension("jsonl.tmp");
    std::fs::write(&tmp, &body).with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("rename to {}", path.display()))?;
    meta.count = u32::try_from(recs.len()).context("too many records")?;
    meta.sha256 = hex::encode(Sha256::digest(&body));
    let mp = meta_path(path);
    let mtmp = mp.with_extension("json.tmp");
    std::fs::write(&mtmp, serde_json::to_vec_pretty(&meta)?)
        .with_context(|| format!("write {}", mtmp.display()))?;
    std::fs::rename(&mtmp, &mp).with_context(|| format!("rename to {}", mp.display()))
}

fn read_meta(path: &Path) -> Result<ShardMeta> {
    let mp = meta_path(path);
    let b = std::fs::read(&mp).with_context(|| format!("read {}", mp.display()))?;
    serde_json::from_slice(&b).with_context(|| format!("parse {}", mp.display()))
}

/// Hashes the file against the sidecar before parsing any line, so a
/// damaged shard fails on its sha256 and not on its JSON.
pub fn read_shard(path: &Path) -> Result<(ShardMeta, Vec<BallotRecord>)> {
    let meta = read_meta(path)?;
    let body = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
    let got = hex::encode(Sha256::digest(&body));
    if got != meta.sha256 {
        bail!(
            "{}: sha256 {got} does not match the meta's {}",
            path.display(),
            meta.sha256
        );
    }
    let recs = body
        .split(|b| *b == b'\n')
        .filter(|l| !l.is_empty())
        .enumerate()
        .map(|(i, l)| {
            serde_json::from_slice::<BallotRecord>(l)
                .with_context(|| format!("{}: line {}", path.display(), i + 1))
        })
        .collect::<Result<Vec<_>>>()?;
    ensure!(
        recs.len() as u64 == u64::from(meta.count),
        "{}: count {} does not match the meta's {}",
        path.display(),
        recs.len(),
        meta.count
    );
    Ok((meta, recs))
}

/// Done once the meta exists and the shard's hash matches it.
pub fn done(path: &Path) -> bool {
    let Ok(m) = read_meta(path) else {
        return false;
    };
    // The meta must describe the shard its path names: file name always,
    // tag when the path has the `w<N>/<tag>/` layout of `shard_path`.
    let name = format!("r{}-{}-{}.jsonl", m.round, m.from, m.to);
    let name_ok = path.file_name().is_some_and(|n| *n == *name);
    let dir = path.parent();
    let in_layout = dir
        .and_then(Path::parent)
        .and_then(Path::file_name)
        .and_then(|w| w.to_str())
        .is_some_and(|w| w.starts_with('w') && w[1..].parse::<u32>().is_ok());
    let tag_ok = !in_layout || dir.and_then(Path::file_name).is_some_and(|t| *t == *m.tag);
    name_ok && tag_ok && sha256_file(path).is_ok_and(|h| h == m.sha256)
}

/// An identity ballot with a zero proof and signature.
#[cfg(test)]
pub fn fake_req(vid: u64, address: [u8; 20]) -> VoteRequest {
    use davinci_client::api::ProcessId;
    use davinci_zkvm_sdk::ballot::Ballot;
    use davinci_zkvm_sdk::types::SnarkJsProof;

    let z = || "0".to_string();
    VoteRequest {
        process_id: ProcessId([1; 31]),
        address,
        vote_id: vid,
        ballot: Ballot::identity(),
        ballot_proof: SnarkJsProof {
            pi_a: [z(), z(), z()],
            pi_b: [[z(), z()], [z(), z()], [z(), z()]],
            pi_c: [z(), z(), z()],
            protocol: "groth16".into(),
            curve: "bn128".into(),
        },
        signature: [0; 65],
        weight: 10,
        census_proof: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bench::plan::{ElectionSpec, NodeId, Plan};

    fn meta(tag: &str, round: u8, from: u32, to: u32) -> ShardMeta {
        ShardMeta {
            wave: 1,
            tag: tag.into(),
            round,
            from,
            to,
            count: 0,
            sha256: String::new(),
            host: "z6".into(),
            bin_sha256: String::new(),
            wasm_sha256: String::new(),
            zkey_sha256: String::new(),
            secs: 0.0,
        }
    }

    fn spec(tag: &str, nf: u8, origin: u8, voters: u32, rounds: u8, pm: u16) -> ElectionSpec {
        ElectionSpec {
            tag: tag.into(),
            scenario: "t".into(),
            nf,
            origin,
            voters,
            rounds,
            revote_permille: pm,
            key_node: NodeId::A,
            grace: 60,
        }
    }

    #[test]
    fn shard_round_trip_and_tamper() {
        let dir = tempfile::tempdir().unwrap();
        let recs: Vec<BallotRecord> = (0..3u32)
            .map(|v| BallotRecord {
                tag: "R1".into(),
                voter: v,
                round: 1,
                choices: vec![1, 2],
                req: fake_req(1 << 63 | v as u64, [v as u8; 20]),
            })
            .collect();
        let p = dir.path().join("r1-0-3.jsonl");
        write_shard(&p, &recs, meta("R1", 1, 0, 3)).unwrap();
        let (m, back) = read_shard(&p).unwrap();
        assert_eq!((m.count, &back), (3, &recs));
        assert!(done(&p));
        let good = std::fs::read(&p).unwrap();
        let mut bad = good.clone();
        bad[20] ^= 1;
        std::fs::write(&p, &bad).unwrap();
        assert!(read_shard(&p).unwrap_err().to_string().contains("sha256"));
        assert!(!done(&p));
        // A dropped line with a matching sha256 still fails the count.
        let short: Vec<u8> = good
            .split(|b| *b == b'\n')
            .take(2)
            .flat_map(|l| [l, b"\n".as_slice()].concat())
            .collect();
        std::fs::write(&p, &short).unwrap();
        let mut m2 = m.clone();
        m2.sha256 = sha256_file(&p).unwrap();
        std::fs::write(
            p.with_extension("jsonl.meta.json"),
            serde_json::to_vec(&m2).unwrap(),
        )
        .unwrap();
        assert!(read_shard(&p).unwrap_err().to_string().contains("count"));
    }

    #[test]
    fn hosts_partition_the_shards() {
        let seed = Seed::from_bytes([7; 32]);
        let plan = Plan::stand_in(
            &seed,
            1,
            &[spec("A", 2, 1, 1200, 2, 150), spec("B", 16, 4, 300, 1, 0)],
        )
        .unwrap();
        let all = shards(plan.wave(1).unwrap(), &seed, SHARD);
        for hosts in 1..=3 {
            let mut got: Vec<_> = (0..hosts)
                .flat_map(|h| assigned(&all, h, hosts).unwrap())
                .collect();
            got.sort_by(|a, b| (&a.tag, a.round, a.from).cmp(&(&b.tag, b.round, b.from)));
            assert_eq!(got, all);
        }
        assert!(assigned(&all, 2, 2).is_err() && assigned(&all, 0, 0).is_err());
        // Every planned ballot falls in exactly one shard.
        for e in &plan.wave(1).unwrap().elections {
            for (r, v) in e.ballots(&seed) {
                let n = all
                    .iter()
                    .filter(|s| s.tag == e.spec.tag && s.round == r && (s.from..s.to).contains(&v))
                    .count();
                assert_eq!(n, 1, "{} r{r} v{v}", e.spec.tag);
            }
        }
    }
}
