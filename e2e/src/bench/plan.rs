//! Campaign seed, derivations and the plan of a benchmark campaign.

use std::fmt;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::time::Duration;

use alloy::primitives::Address;
use alloy::sol_types::SolEvent;
use anyhow::{Context, Result, anyhow, bail, ensure};
use davinci_client::api::{Fr, ProcessId, ProcessStatus};
use davinci_client::organizer::{
    KeyMode, NewProcess, OnchainProcess, Organizer, ProcessRegistry, RegistryReader, census_file,
    merkle_census,
};
use davinci_client::voter::random_k;
use davinci_zkvm_sdk::ballot::BallotMode;
use davinci_zkvm_sdk::census::eth_address;
use davinci_zkvm_sdk::crypto::babyjubjub::Point;
use davinci_zkvm_sdk::crypto::elgamal;
use davinci_zkvm_sdk::crypto::field::{fr_from_dec, fr_to_dec};
use k256::ecdsa::SigningKey;
use rand::rngs::{OsRng, StdRng};
use rand::{RngCore, SeedableRng};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::bench::r#gen::{WEIGHT, census_root};
use crate::bench::rig::{Rig, Role};
use crate::census::{Census, census_dir};
use crate::fixture::{self as fx, ballot_mode_nf};
use crate::net::{self, Net};

/// The two sequencer nodes of a campaign.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NodeId {
    A,
    B,
}

impl fmt::Display for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            NodeId::A => "a",
            NodeId::B => "b",
        })
    }
}

fn role_of(n: NodeId) -> Role {
    match n {
        NodeId::A => Role::A,
        NodeId::B => Role::B,
    }
}

/// The campaign secret: anyone holding it can vote in the benchmark's
/// elections, so it is never printed.
#[derive(Clone, PartialEq, Eq)]
pub struct Seed([u8; 32]);

impl fmt::Debug for Seed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Seed(..)")
    }
}

impl Seed {
    #[cfg(test)]
    pub fn from_bytes(b: [u8; 32]) -> Seed {
        Seed(b)
    }

    /// A fresh seed written as hex to a new file with mode 0600.
    pub fn create(path: &Path) -> Result<Seed> {
        let mut b = [0u8; 32];
        OsRng.fill_bytes(&mut b);
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
            .with_context(|| format!("create seed file {}", path.display()))?;
        f.write_all(hex::encode(b).as_bytes())
            .with_context(|| format!("write seed file {}", path.display()))?;
        Ok(Seed(b))
    }

    /// Errors name the path, never the content.
    pub fn load(path: &Path) -> Result<Seed> {
        let s = std::fs::read_to_string(path)
            .with_context(|| format!("read seed file {}", path.display()))?;
        let mut b = [0u8; 32];
        hex::decode_to_slice(s.trim(), &mut b)
            .map_err(|_| anyhow!("seed file {} is not 64 hex characters", path.display()))?;
        Ok(Seed(b))
    }

    /// `sha256("davinci-bench/v1/" || label || 0 || seed || tag || 0 || be(v) || round)`.
    fn hash(&self, label: &str, tag: &str, v: u32, round: u8) -> [u8; 32] {
        Sha256::new()
            .chain_update(b"davinci-bench/v1/")
            .chain_update(label)
            .chain_update([0])
            .chain_update(self.0)
            .chain_update(tag)
            .chain_update([0])
            .chain_update(v.to_be_bytes())
            .chain_update([round])
            .finalize()
            .into()
    }

    fn signing_key(&self, label: &str, tag: &str, v: u32) -> SigningKey {
        let mut h = self.hash(label, tag, v, 0);
        // A hash outside [1, n) is a 2^-128 event; rehash rather than panic.
        loop {
            if let Ok(k) = SigningKey::from_slice(&h) {
                return k;
            }
            h = Sha256::digest(h).into();
        }
    }

    pub fn voter_key(&self, tag: &str, v: u32) -> SigningKey {
        self.signing_key("voter", tag, v)
    }

    pub fn vote_k(&self, tag: &str, v: u32, round: u8) -> Fr {
        random_k(&mut StdRng::from_seed(self.hash("k", tag, v, round)))
    }

    pub fn revoter(&self, tag: &str, v: u32, permille: u16) -> bool {
        let h = self.hash("revote", tag, v, 0);
        let x = u32::from_be_bytes([h[0], h[1], h[2], h[3]]) % 1000;
        x < u32::from(permille)
    }

    pub fn csp_key(&self, wave: u32) -> SigningKey {
        self.signing_key("csp", "", wave)
    }

    /// 1 to 6 units spread over `nf` fields, each field at most 5, so the
    /// vector fits `ballot_mode_nf(nf)` with weight 10.
    pub fn choices(&self, tag: &str, v: u32, round: u8, nf: u8) -> Vec<u64> {
        let nf = usize::from(nf.max(1));
        let h = self.hash("choices", tag, v, round);
        // Capped so an nf of 1 (5 units fit) cannot loop forever below.
        let units = (1 + usize::from(h[0]) % 6).min(5 * nf);
        let mut c = vec![0u64; nf];
        for &b in &h[1..=units] {
            let mut j = usize::from(b) % nf;
            // At low nf six units can overflow one field; move on to the next.
            while c[j] >= 5 {
                j = (j + 1) % nf;
            }
            c[j] += 1;
        }
        c
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ElectionSpec {
    pub tag: String,
    pub scenario: String,
    pub nf: u8,
    pub origin: u8,
    pub voters: u32,
    pub rounds: u8,
    pub revote_permille: u16,
    pub key_node: NodeId,
    pub grace: u32,
}

impl ElectionSpec {
    /// The registry's `maxVoters`. Revoters are a subset of the voters, so
    /// the planned distinct voters equal this exactly.
    pub fn max_voters(&self) -> u32 {
        self.voters
    }
}

/// What ballot generation needs of a process, so `gen` needs no RPC.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcSnap {
    pub pid: ProcessId,
    /// Compressed point, hex.
    pub encryption_key: String,
    pub ballot_mode: BallotMode,
    pub census_origin: u8,
    /// Decimal.
    pub census_root: String,
}

impl ProcSnap {
    pub fn of(p: &OnchainProcess) -> ProcSnap {
        ProcSnap {
            pid: ProcessId(p.id),
            encryption_key: hex::encode(p.encryption_key.compress()),
            ballot_mode: p.ballot_mode,
            census_origin: p.census_origin,
            census_root: fr_to_dec(&p.census_root),
        }
    }

    /// An `OnchainProcess` carrying only the snapshot's fields; the rest is
    /// neutral and not used by ballot generation.
    pub fn onchain(&self) -> Result<OnchainProcess> {
        let mut key = [0u8; 32];
        hex::decode_to_slice(&self.encryption_key, &mut key)
            .map_err(|_| anyhow!("encryption key is not 32 bytes of hex"))?;
        Ok(OnchainProcess {
            id: self.pid.0,
            status: ProcessStatus::Ready,
            organization_id: [0; 20],
            encryption_key: Point::decompress(&key).map_err(|e| anyhow!("encryption key: {e}"))?,
            state_root: [0; 32],
            result: vec![],
            start_time: 0,
            duration: 0,
            max_voters: 0,
            voters_count: 0,
            overwritten_votes_count: 0,
            ballot_mode: self.ballot_mode,
            census_origin: self.census_origin,
            census_root: fr_from_dec(&self.census_root).map_err(|e| anyhow!("census root: {e}"))?,
            census_contract: [0; 20],
            census_uri: String::new(),
            metadata_uri: String::new(),
            metadata_hash: [0; 32],
            dkg: None,
            grace: 0,
            last_vote_at: 0,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Election {
    pub spec: ElectionSpec,
    pub census_uri: String,
    pub census_contract: Option<Address>,
    pub snap: ProcSnap,
}

impl Election {
    /// `(round, voter)` pairs: round 1 is every voter, rounds 2 and up the
    /// revoters, ordered by round then voter.
    pub fn ballots(&self, seed: &Seed) -> Vec<(u8, u32)> {
        let s = &self.spec;
        let mut out: Vec<(u8, u32)> = (0..s.voters).map(|v| (1, v)).collect();
        let revoters: Vec<u32> = (0..s.voters)
            .filter(|&v| seed.revoter(&s.tag, v, s.revote_permille))
            .collect();
        for r in 2..=s.rounds {
            out.extend(revoters.iter().map(|&v| (r, v)));
        }
        out
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Wave {
    pub n: u32,
    pub created: u64,
    /// The scale the wave was planned at (1 live): `run` sizes the ceiling
    /// bursts from it. Plans from before the field read as 1.
    #[serde(default = "one")]
    pub scale: f64,
    pub elections: Vec<Election>,
}

fn one() -> f64 {
    1.0
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Plan {
    pub campaign: String,
    pub chain_id: u64,
    pub registry: Address,
    pub start_block: u64,
    pub waves: Vec<Wave>,
}

impl Plan {
    pub fn load(dir: &Path) -> Result<Plan> {
        let p = dir.join("plan.json");
        let b = std::fs::read(&p).with_context(|| format!("read {}", p.display()))?;
        let plan: Plan =
            serde_json::from_slice(&b).with_context(|| format!("parse {}", p.display()))?;
        plan.check_tags()?;
        Ok(plan)
    }

    /// Tags become path components and shard keys: reject empty, `/`, `..`
    /// and repeats in any wave (the seed derivations ignore the wave, so a
    /// repeated tag repeats the voter keys, `k` and the census).
    pub fn check_tags(&self) -> Result<()> {
        let mut seen = std::collections::HashSet::new();
        for w in &self.waves {
            for e in &w.elections {
                let t = e.spec.tag.as_str();
                check_tag(t)?;
                ensure!(seen.insert(t), "duplicate tag {t:?} (wave {})", w.n);
            }
        }
        Ok(())
    }

    /// Whether `specs` may go into wave `n`: valid tags, none twice, none in
    /// another wave. A tag already in wave `n` is a rerun.
    pub fn check_new_tags(&self, n: u32, specs: &[ElectionSpec]) -> Result<()> {
        let mut seen = std::collections::HashSet::new();
        for s in specs {
            let t = s.tag.as_str();
            check_tag(t)?;
            ensure!(seen.insert(t), "tag {t:?} twice in wave {n}");
            let other = self
                .waves
                .iter()
                .find(|w| w.n != n && w.elections.iter().any(|e| e.spec.tag == t));
            if let Some(w) = other {
                bail!("tag {t:?} is already in wave {}", w.n);
            }
        }
        Ok(())
    }

    /// Written to `plan.json.tmp` first so a crash never leaves a torn plan.
    pub fn save(&self, dir: &Path) -> Result<()> {
        let p = dir.join("plan.json");
        let tmp = dir.join("plan.json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)
            .with_context(|| format!("write {}", tmp.display()))?;
        std::fs::rename(&tmp, &p).with_context(|| format!("rename to {}", p.display()))
    }

    pub fn wave(&self, n: u32) -> Result<&Wave> {
        self.waves
            .iter()
            .find(|w| w.n == n)
            .ok_or_else(|| anyhow!("plan has no wave {n}"))
    }

    fn wave_mut(&mut self, n: u32) -> Result<&mut Wave> {
        self.waves
            .iter_mut()
            .find(|w| w.n == n)
            .ok_or_else(|| anyhow!("plan has no wave {n}"))
    }

    /// The scale wave `n` was planned at.
    pub fn scale(&self, n: u32) -> Result<f64> {
        Ok(self.wave(n)?.scale)
    }

    /// Wave `n`, added at `scale` (created at unix time `created`) when
    /// absent. An existing wave must have been planned at `scale`.
    pub fn open_wave(&mut self, n: u32, scale: f64, created: u64) -> Result<&mut Wave> {
        if let Some(w) = self.waves.iter().find(|w| w.n == n) {
            ensure!(
                w.scale == scale,
                "wave {n} was planned at scale {}, not {scale}",
                w.scale
            );
        } else {
            self.waves.push(Wave {
                n,
                created,
                scale,
                elections: Vec::new(),
            });
            self.waves.sort_by_key(|w| w.n);
        }
        self.wave_mut(n)
    }

    /// A plan with locally generated election keys and no chain, for tests
    /// and dry runs of generation.
    pub fn stand_in(seed: &Seed, n: u32, specs: &[ElectionSpec]) -> Result<Plan> {
        ensure!(!specs.is_empty(), "no election specs");
        let elections = specs
            .iter()
            .map(|spec| {
                let mut pid = [0u8; 31];
                pid.copy_from_slice(&seed.hash("pid", &spec.tag, n, 0)[..31]);
                let mut rng = StdRng::from_seed(seed.hash("ekey", &spec.tag, n, 0));
                let (_, pk) = elgamal::keygen(&mut rng);
                let mut e = Election {
                    spec: spec.clone(),
                    census_uri: String::new(),
                    census_contract: None,
                    snap: ProcSnap {
                        pid: ProcessId(pid),
                        encryption_key: hex::encode(pk.compress()),
                        ballot_mode: ballot_mode_nf(spec.nf),
                        census_origin: spec.origin,
                        census_root: String::new(),
                    },
                };
                // The root the ballots will prove against, so they verify.
                let root = crate::bench::r#gen::census_root(&e, seed, n)?;
                e.snap.census_root = fr_to_dec(&root);
                Ok(e)
            })
            .collect::<Result<Vec<_>>>()?;
        let plan = Plan {
            campaign: "stand-in".into(),
            chain_id: 0,
            registry: Address::ZERO,
            start_block: 0,
            waves: vec![Wave {
                n,
                created: 0,
                scale: 1.0,
                elections,
            }],
        };
        plan.check_tags()?;
        Ok(plan)
    }
}

/// Tags become path components and shard keys: no empty tag, `/`, `\`,
/// NUL or `..`.
fn check_tag(t: &str) -> Result<()> {
    ensure!(
        !t.is_empty() && !t.contains(['/', '\\', '\0']) && !t.contains(".."),
        "bad election tag {t:?}"
    );
    Ok(())
}

/// Writes `pins.json`, the release pins the TS script checks the registry
/// against (`0x` hex).
pub fn write_pins(campaign: &Path) -> Result<()> {
    use davinci_zkvm_sdk::release as r;
    let h = |b: &[u8]| format!("0x{}", hex::encode(b));
    let pins = serde_json::json!({
        "batchProgramVK": h(&r::BATCH_PROGRAM_VK),
        "resultsProgramVK": h(&r::RESULTS_PROGRAM_VK),
        "rootCVadcopFinal": h(&r::ROOT_C_VADCOP_FINAL),
        "ziskVerifierCodeHash": h(&r::ZISK_VERIFIER_CODEHASH),
        "ballotVKHash": h(&crate::chain::ballot_vk_hash()?),
    });
    let p = campaign.join("pins.json");
    std::fs::write(&p, serde_json::to_vec_pretty(&pins)?)
        .with_context(|| format!("write {}", p.display()))
}

/// Registry duration of every campaign election: its ballots are proved
/// before the run, so it must outlive gen, merge and the run.
pub const DURATION: u64 = 72 * 3600;

/// Census contract members per `addMembers` transaction.
const MEMBER_CHUNK: usize = 64;

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Creates the elections of wave `n` the plan does not hold yet and
/// records each in `plan.json` as soon as it exists. A process an earlier,
/// crashed or interrupted call created but never recorded is adopted rather
/// than created again. Origin-3 specs are skipped unless `onchain_census`.
/// Every election gets `spec.max_voters()`, its grace, the key of its key
/// node, and waits until A and B serve it.
#[allow(clippy::too_many_arguments)]
pub async fn create_wave(
    net: &Net,
    rig: &Rig,
    campaign: &Path,
    seed: &Seed,
    n: u32,
    scale: f64,
    specs: &[ElectionSpec],
    onchain_census: bool,
) -> Result<()> {
    let mut plan = Plan::load(campaign)?;
    ensure!(
        (plan.chain_id, plan.registry) == (net.chain_id, net.registry),
        "plan.json is for chain {} registry {}, not chain {} registry {}",
        plan.chain_id,
        plan.registry,
        net.chain_id,
        net.registry
    );
    let org = net.organizer()?;
    let grace = org.grace_params().await.context("registry grace")?;
    // Fail before creating anything.
    plan.check_new_tags(n, specs)?;
    for s in specs {
        ensure!(s.voters > 0, "{}: no voters", s.tag);
        ensure!(
            s.grace == grace.default_grace
                || (grace.grace_floor..=grace.grace_ceil).contains(&s.grace),
            "{}: grace {} s is outside the registry's {}..={}",
            s.tag,
            s.grace,
            grace.grace_floor,
            grace.grace_ceil
        );
    }
    plan.open_wave(n, scale, unix_now())?;
    plan.save(campaign)?;
    let census = campaign.join("census");
    std::fs::create_dir_all(&census)?;
    let reader = RegistryReader::connect(&net.rpc, net.registry)?;
    let c = Creator {
        net,
        rig,
        org: &org,
        reader: &reader,
    };
    let mut orphans = orphans(&plan, &c).await?;
    for s in specs {
        let planned = plan.wave(n)?.elections.iter().find(|e| e.spec.tag == s.tag);
        if let Some(e) = planned.cloned() {
            ensure!(e.spec == *s, "{}: plan.json holds another spec", s.tag);
            ensure_grace(&c, &e).await?;
            println!("election {} already planned", s.tag);
            continue;
        }
        if s.origin == 3 && !onchain_census {
            println!(
                "election {} skipped: no on-chain census (plan --onchain-census)",
                s.tag
            );
            continue;
        }
        let d = draft(&census, seed, n, s).with_context(|| format!("election {}", s.tag))?;
        let adopted = adopt(&c, &d, &mut orphans)
            .await
            .with_context(|| format!("election {}", s.tag))?;
        let (e, how) = match adopted {
            Some(e) => (e, " (adopted)"),
            None => (
                create(&c, d)
                    .await
                    .with_context(|| format!("election {}", s.tag))?,
                "",
            ),
        };
        println!(
            "election {} {} nf {} origin {} key {}{how}",
            s.tag, e.snap.pid, s.nf, s.origin, s.key_node
        );
        // Recorded first: the process exists. A rerun repairs the grace, and
        // `run` waits for the nodes again.
        plan.wave_mut(n)?.elections.push(e.clone());
        plan.check_tags()?;
        plan.save(campaign)?;
        ensure_grace(&c, &e).await?;
        wait_served(rig, &e, seed).await?;
    }
    Ok(())
}

/// What creating one election needs.
struct Creator<'a> {
    net: &'a Net,
    rig: &'a Rig,
    org: &'a Organizer,
    reader: &'a RegistryReader,
}

/// What an election's process must carry, worked out before it exists.
struct Draft {
    /// The snapshot lacks the pid and the key; the census URI of origin 3
    /// comes with its contract.
    e: Election,
    root: Fr,
    /// `(address, weight)` of every voter, in voter order.
    parts: Vec<([u8; 20], u128)>,
    metadata: String,
    metadata_hash: [u8; 32],
}

/// The metadata title of an election: its hash finds the election's process
/// again if `plan.json` never recorded it.
fn metadata_title(n: u32, tag: &str) -> String {
    format!("davinci-bench wave {n} {tag}")
}

/// The census root `gen` proves against (for a CSP, the wave's CSP key),
/// the census file of origin 1 and the metadata, written to `census` (the
/// nodes' `--census-dir`).
fn draft(census: &Path, seed: &Seed, n: u32, s: &ElectionSpec) -> Result<Draft> {
    let mut e = Election {
        spec: s.clone(),
        census_uri: String::new(),
        census_contract: None,
        snap: ProcSnap {
            pid: ProcessId([0; 31]),
            encryption_key: String::new(),
            ballot_mode: ballot_mode_nf(s.nf),
            census_origin: s.origin,
            census_root: String::new(),
        },
    };
    let root = census_root(&e, seed, n)?;
    let parts: Vec<([u8; 20], u128)> = (0..s.voters)
        .map(|v| {
            (
                eth_address(seed.voter_key(&s.tag, v).verifying_key()),
                WEIGHT,
            )
        })
        .collect();
    match s.origin {
        1 => {
            let file = census_file(&parts);
            ensure!(merkle_census(&file)?.root() == root, "census file root");
            e.census_uri = fx::write_census(census, &format!("w{n}-{}.json", s.tag), &file)?;
        }
        3 => {}
        4 => e.census_uri = "csp://davinci-bench".into(),
        o => bail!("census origin {o} is not supported"),
    }
    let (metadata, metadata_hash) = fx::write_metadata(
        census,
        &format!("w{n}-{}-metadata.json", s.tag),
        &metadata_title(n, &s.tag),
    )?;
    Ok(Draft {
        e,
        root,
        parts,
        metadata,
        metadata_hash,
    })
}

impl Draft {
    /// Whether `p` is this election with key `pk`: the snapshot, max
    /// voters, metadata and census URI asked for. The registry's snapshot.
    fn check(&self, p: &OnchainProcess, pk: &Point) -> Result<ProcSnap> {
        let want = ProcSnap {
            pid: ProcessId(p.id),
            encryption_key: hex::encode(pk.compress()),
            census_root: fr_to_dec(&self.root),
            ..self.e.snap.clone()
        };
        let got = ProcSnap::of(p);
        ensure!(got == want, "snapshot (key, mode, origin or root) differs");
        let max = u64::from(self.e.spec.max_voters());
        ensure!(p.max_voters == max, "maxVoters {}, not {max}", p.max_voters);
        ensure!(
            p.metadata_hash == self.metadata_hash,
            "metadata hash differs"
        );
        ensure!(
            self.e.spec.origin == 3 || p.census_uri == self.e.census_uri,
            "census URI differs"
        );
        Ok(got)
    }
}

/// The organizer's processes since the plan's start block that `plan.json`
/// does not hold, as the registry has them.
async fn orphans(plan: &Plan, c: &Creator<'_>) -> Result<Vec<OnchainProcess>> {
    let planned: std::collections::HashSet<ProcessId> = plan
        .waves
        .iter()
        .flat_map(|w| w.elections.iter().map(|e| e.snap.pid))
        .collect();
    let mut out = Vec::new();
    let all = cancel_candidates(plan, &c.net.rpc, c.org.address(), CancelScope::All).await?;
    for pid in all.into_iter().filter(|p| !planned.contains(p)) {
        out.push(c.reader.process(&pid.0).await?);
    }
    Ok(out)
}

/// Orphans that may be an election's unrecorded process: READY, with the
/// election's metadata hash (its wave and tag). Never a canceled one.
pub fn orphans_matching<'a>(
    cands: &'a [OnchainProcess],
    metadata_hash: &[u8; 32],
) -> Vec<&'a OnchainProcess> {
    cands
        .iter()
        .filter(|p| p.status == ProcessStatus::Ready && p.metadata_hash == *metadata_hash)
        .collect()
}

/// The first orphan that passes `d`'s full check, with the key node's key
/// for its id (a key is a function of the id), taken out of `orphans`.
async fn adopt(
    c: &Creator<'_>,
    d: &Draft,
    orphans: &mut Vec<OnchainProcess>,
) -> Result<Option<Election>> {
    let s = &d.e.spec;
    let ids: Vec<[u8; 31]> = orphans_matching(orphans, &d.metadata_hash)
        .iter()
        .map(|p| p.id)
        .collect();
    for id in ids {
        let Some(i) = orphans.iter().position(|p| p.id == id) else {
            continue;
        };
        let pk = election_key(c.rig, s.key_node, &id).await?;
        match d.check(&orphans[i], &pk) {
            Ok(snap) => {
                let p = orphans.remove(i);
                fx::check_metadata(&p)?;
                let mut e = d.e.clone();
                e.snap = snap;
                e.census_uri = p.census_uri;
                e.census_contract =
                    (p.census_origin == 3).then(|| Address::from(p.census_contract));
                return Ok(Some(e));
            }
            Err(why) => println!(
                "process {} has the metadata of {} but is not it ({why:#}); left alone",
                ProcessId(id),
                s.tag
            ),
        }
    }
    Ok(None)
}

/// Creates `d`'s process: the census contract of origin 3, the key node's
/// key for the next id, `newProcess`, then the check of what the registry
/// holds.
async fn create(c: &Creator<'_>, mut d: Draft) -> Result<Election> {
    let s = d.e.spec.clone();
    if s.origin == 3 {
        let census = Census::deploy(c.net.census_provider(c.org)?, &census_dir()).await?;
        for chunk in d.parts.chunks(MEMBER_CHUNK) {
            census.add_members(chunk).await?;
        }
        ensure!(
            census.root().await? == d.root && census.size().await? == u64::from(s.voters),
            "census contract {} does not hold the planned voters",
            census.address
        );
        d.e.census_uri = format!("onchain://{}", census.address);
        d.e.census_contract = Some(census.address);
    }
    let next = c.org.next_process_id().await?;
    let pk = election_key(c.rig, s.key_node, &next).await?;
    let pid = c
        .org
        .create_process(&NewProcess {
            process_id: next,
            start_time: 0,
            duration: DURATION,
            max_voters: u64::from(s.max_voters()),
            ballot_mode: d.e.snap.ballot_mode,
            census_origin: s.origin,
            census_root: d.root,
            census_contract: d.e.census_contract.map_or([0; 20], |a| a.into_array()),
            census_uri: d.e.census_uri.clone(),
            metadata: d.metadata.clone(),
            metadata_hash: d.metadata_hash,
            key_mode: KeyMode::Sequencer(pk),
        })
        .await?
        .pid;
    let p = c.reader.process(&pid).await?;
    fx::check_metadata(&p)?;
    ensure!(
        pid == next,
        "created {}, not {}",
        ProcessId(pid),
        ProcessId(next)
    );
    let snap = d.check(&p, &pk).with_context(|| {
        format!(
            "the registry's process {} differs from the one asked for",
            ProcessId(pid)
        )
    })?;
    d.e.snap = snap;
    Ok(d.e)
}

/// Sets the spec's grace on `e`'s process when the registry holds another
/// and the process is still READY: a crash may have come between
/// `newProcess` and `setProcessGrace`.
async fn ensure_grace(c: &Creator<'_>, e: &Election) -> Result<()> {
    let (pid, want) = (&e.snap.pid.0, u64::from(e.spec.grace));
    let p = c.reader.process(pid).await?;
    if p.grace == want {
        return Ok(());
    }
    if p.status != ProcessStatus::Ready {
        println!(
            "election {}: grace {} s, not {want}, and the process is {:?}",
            e.spec.tag, p.grace, p.status
        );
        return Ok(());
    }
    c.org.set_process_grace(pid, e.spec.grace).await?;
    let got = c.reader.process(pid).await?.grace;
    ensure!(
        got == want,
        "{}: grace {got} s after setProcessGrace",
        e.spec.tag
    );
    Ok(())
}

/// The key node's election key for `pid`. A node gives at most
/// `--keys-per-minute` (10) keys a minute; a 42901 waits a minute.
async fn election_key(rig: &Rig, n: NodeId, pid: &[u8; 31]) -> Result<Point> {
    let node = rig.node(role_of(n))?;
    for _ in 0..5 {
        match node.api.new_key(pid).await {
            Err(davinci_client::Error::Api {
                code: Some(42901), ..
            }) => {
                println!("node {n}: key rate limit, waiting 60 s");
                tokio::time::sleep(Duration::from_secs(60)).await;
            }
            r => return r.with_context(|| format!("election key from node {n}")),
        }
    }
    bail!("node {n} kept refusing keys (42901)")
}

/// Waits until A and B list `e`, accept votes and serve its census: the
/// last voter's Merkle proof (a CSP census has none to serve).
pub async fn wait_served(rig: &Rig, e: &Election, seed: &Seed) -> Result<()> {
    let (pid, tag) = (e.snap.pid, e.spec.tag.as_str());
    let last = seed.voter_key(tag, e.spec.voters.saturating_sub(1));
    let last = eth_address(last.verifying_key());
    for id in [NodeId::A, NodeId::B] {
        let node = rig.node(role_of(id))?;
        crate::wait::until(
            &format!("node {id} to serve {tag}"),
            net::scaled(Duration::from_secs(180)),
            Duration::from_secs(1),
            || async {
                node.check_alive()?;
                if !node.api.processes().await?.contains(&pid) {
                    return Ok(None);
                }
                let v = node.api.process(&pid.0).await?;
                ensure!(!v.ignored, "node {id} refuses to serve {tag}");
                if !v.is_accepting_votes {
                    return Ok(None);
                }
                if e.spec.origin == 4 {
                    return Ok(Some(()));
                }
                Ok(node.api.participant(&pid.0, &last).await.ok().map(|_| ()))
            },
        )
        .await?;
    }
    Ok(())
}

/// The processes `cancel` looks at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CancelScope {
    /// A wave's elections, from `plan.json`.
    Wave(u32),
    /// Every process the organizer created since the plan's start block,
    /// from the registry logs: the TS runs' processes too.
    All,
}

pub async fn cancel_candidates(
    plan: &Plan,
    rpc: &str,
    organizer: Address,
    scope: CancelScope,
) -> Result<Vec<ProcessId>> {
    use ProcessRegistry::ProcessCreated as Ev;
    match scope {
        CancelScope::Wave(n) => Ok(plan.wave(n)?.elections.iter().map(|e| e.snap.pid).collect()),
        CancelScope::All => {
            let logs = crate::chain::registry_logs(
                rpc,
                plan.registry,
                Ev::SIGNATURE_HASH,
                plan.start_block,
            )
            .await?;
            let mut out = Vec::new();
            for l in &logs {
                let d = l.log_decode::<Ev>()?.inner.data;
                if d.creator == organizer {
                    out.push(ProcessId(d.processId.0));
                }
            }
            Ok(out)
        }
    }
}

/// Cancels each READY or PAUSED process of `pids`, printing one line per
/// process. Tries them all, then fails with the errors; returns how many it
/// canceled.
pub async fn cancel_open(org: &Organizer, pids: &[ProcessId]) -> Result<usize> {
    let (mut done, mut errs) = (0, Vec::new());
    for pid in pids {
        let r = async {
            let st = org.process(&pid.0).await?.status;
            if matches!(st, ProcessStatus::Ready | ProcessStatus::Paused) {
                org.cancel_process(&pid.0).await?;
                println!("{pid}: {st:?}, canceled");
                Ok::<_, anyhow::Error>(true)
            } else {
                println!("{pid}: {st:?}, left alone");
                Ok(false)
            }
        }
        .await;
        match r {
            Ok(c) => done += usize::from(c),
            Err(e) => {
                let e = crate::redact_urls(&format!("{e:#}"));
                println!("{pid}: {e}");
                errs.push(format!("{pid}: {e}"));
            }
        }
    }
    ensure!(errs.is_empty(), "cancel failed: {}", errs.join("; "));
    Ok(done)
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::os::unix::fs::PermissionsExt;

    use davinci_client::organizer::metadata_hash;

    use super::*;

    fn seed(b: u8) -> Seed {
        Seed::from_bytes([b; 32])
    }

    fn addr(s: &Seed, tag: &str, v: u32) -> [u8; 20] {
        eth_address(s.voter_key(tag, v).verifying_key())
    }

    #[test]
    fn derivations_are_deterministic_and_separated() {
        let s = seed(1);
        assert_eq!(addr(&s, "R1", 5), addr(&s, "R1", 5));
        assert_ne!(addr(&s, "R1", 5), addr(&s, "R2", 5));
        assert_ne!(addr(&s, "R1", 5), addr(&s, "R1", 6));
        assert_ne!(addr(&s, "R1", 5), addr(&seed(2), "R1", 5));
        assert_eq!(s.vote_k("R1", 5, 1), s.vote_k("R1", 5, 1));
        assert_ne!(s.vote_k("R1", 5, 1), s.vote_k("R1", 5, 2));
        assert_ne!(s.vote_k("R1", 5, 1), s.vote_k("R1", 6, 1));
        assert_ne!(s.vote_k("R1", 5, 1), s.vote_k("R2", 5, 1));
        let all: HashSet<_> = (0..1000).map(|v| addr(&s, "R1", v)).collect();
        assert_eq!(all.len(), 1000);
    }

    #[test]
    fn choices_fit_every_ballot_mode() {
        let s = seed(3);
        for nf in 2..=16u8 {
            let mode = ballot_mode_nf(nf);
            for v in 0..200 {
                for r in 1..=3 {
                    let c = s.choices("T", v, r, nf);
                    assert_eq!(c.len(), usize::from(nf));
                    assert!(c.iter().all(|&x| x <= 5));
                    let sum: u64 = c.iter().sum();
                    assert!((1..=6).contains(&sum), "sum {sum}");
                    assert_eq!(mode.num_fields, nf);
                    assert!(c.iter().all(|&x| x <= mode.max_value));
                    assert!(sum <= 10);
                }
            }
        }
        let differ = (0..200)
            .filter(|&v| s.choices("T", v, 1, 2) != s.choices("T", v, 2, 2))
            .count();
        assert!(differ >= 160, "only {differ} of 200 differ");
    }

    #[test]
    fn choices_terminate_at_nf_one() {
        let s = seed(6);
        for v in 0..100 {
            let c = s.choices("T", v, 1, 1);
            assert_eq!(c.len(), 1);
            assert!((1..=5).contains(&c[0]));
        }
    }

    #[test]
    fn bad_tags_are_rejected() {
        let s = seed(7);
        for t in ["", "a/b", "..", "a..b"] {
            assert!(
                Plan::stand_in(&s, 1, &[spec(t, 2, 1, 3, 1, 0)]).is_err(),
                "{t:?}"
            );
        }
        let dup = [spec("A", 2, 1, 3, 1, 0), spec("A", 2, 1, 3, 1, 0)];
        assert!(Plan::stand_in(&s, 1, &dup).is_err());
    }

    #[test]
    fn revoter_rate() {
        let s = seed(4);
        let n = (0..10_000).filter(|&v| s.revoter("T", v, 150)).count();
        assert!((1400..=1600).contains(&n), "{n}");
    }

    #[test]
    fn seed_file_is_private() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("seed");
        let s = Seed::create(&p).unwrap();
        assert_eq!(
            std::fs::metadata(&p).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(Seed::create(&p).is_err());
        assert_eq!(Seed::load(&p).unwrap(), s);
        let hexed = hex::encode(s.0);
        assert!(!format!("{s:?}").contains(&hexed[..8]));
        assert_eq!(format!("{s:?}"), "Seed(..)");
        // A bad file: right alphabet, wrong length.
        let bad = dir.path().join("bad");
        std::fs::write(&bad, &hexed[..60]).unwrap();
        let e = format!("{:#}", Seed::load(&bad).unwrap_err());
        assert!(e.contains("bad") && !e.contains(&hexed[..8]), "{e}");
    }

    #[test]
    fn snapshot_round_trip() {
        let mut rng = StdRng::seed_from_u64(9);
        let (_, pk) = elgamal::keygen(&mut rng);
        let mut p = ProcSnap {
            pid: ProcessId([7; 31]),
            encryption_key: hex::encode(pk.compress()),
            ballot_mode: ballot_mode_nf(8),
            census_origin: 2,
            census_root: "12345".into(),
        }
        .onchain()
        .unwrap();
        p.census_root = random_k(&mut rng);
        let snap = ProcSnap::of(&p);
        let back = serde_json::from_str::<ProcSnap>(&serde_json::to_string(&snap).unwrap())
            .unwrap()
            .onchain()
            .unwrap();
        assert_eq!(back.id, p.id);
        assert_eq!(back.encryption_key, p.encryption_key);
        assert_eq!(back.ballot_mode, p.ballot_mode);
        assert_eq!(back.census_origin, p.census_origin);
        assert_eq!(back.census_root, p.census_root);
    }

    pub(crate) fn spec(
        tag: &str,
        nf: u8,
        origin: u8,
        voters: u32,
        rounds: u8,
        pm: u16,
    ) -> ElectionSpec {
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
    fn plan_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let s = seed(5);
        let plan = Plan::stand_in(
            &s,
            1,
            &[spec("A", 2, 1, 10, 2, 100), spec("B", 8, 2, 5, 1, 0)],
        )
        .unwrap();
        plan.save(dir.path()).unwrap();
        assert!(!dir.path().join("plan.json.tmp").exists());
        assert_eq!(Plan::load(dir.path()).unwrap(), plan);
        assert!(plan.wave(2).is_err());
    }

    #[test]
    fn pins_json_matches_release() {
        use davinci_zkvm_sdk::release;
        let dir = tempfile::tempdir().unwrap();
        write_pins(dir.path()).unwrap();
        let v: serde_json::Value =
            serde_json::from_slice(&std::fs::read(dir.path().join("pins.json")).unwrap()).unwrap();
        let hex = |b: &[u8]| serde_json::Value::from(format!("0x{}", hex::encode(b)));
        assert_eq!(v["batchProgramVK"], hex(&release::BATCH_PROGRAM_VK));
        assert_eq!(v["resultsProgramVK"], hex(&release::RESULTS_PROGRAM_VK));
        assert_eq!(v["rootCVadcopFinal"], hex(&release::ROOT_C_VADCOP_FINAL));
        assert_eq!(
            v["ziskVerifierCodeHash"],
            hex(&release::ZISK_VERIFIER_CODEHASH)
        );
        assert_eq!(
            v["ballotVKHash"],
            hex(&crate::chain::ballot_vk_hash().unwrap())
        );
        assert_eq!(v.as_object().unwrap().len(), 5);
    }

    #[test]
    fn plan_save_roundtrips_scale() {
        let dir = tempfile::tempdir().unwrap();
        let mut plan = Plan::stand_in(&seed(8), 1, &[spec("A", 2, 1, 10, 1, 0)]).unwrap();
        plan.waves[0].scale = 0.05;
        plan.save(dir.path()).unwrap();
        let back = Plan::load(dir.path()).unwrap();
        assert_eq!(back, plan);
        assert_eq!(back.scale(1).unwrap(), 0.05);
        assert!(back.scale(2).is_err());
        // A plan.json from before the field reads as scale 1.
        let mut v = serde_json::to_value(&plan).unwrap();
        v["waves"][0].as_object_mut().unwrap().remove("scale");
        std::fs::write(dir.path().join("plan.json"), v.to_string()).unwrap();
        assert_eq!(Plan::load(dir.path()).unwrap().scale(1).unwrap(), 1.0);
    }

    #[test]
    fn a_wave_keeps_its_scale() {
        let mut plan = Plan::stand_in(&seed(8), 1, &[spec("A", 2, 1, 10, 1, 0)]).unwrap();
        assert!(plan.open_wave(1, 1.0, 5).is_ok());
        let e = format!("{:#}", plan.open_wave(1, 0.05, 5).unwrap_err());
        assert!(e.contains("scale"), "{e}");
        let w = plan.open_wave(2, 0.05, 7).unwrap();
        assert_eq!((w.n, w.scale, w.created), (2, 0.05, 7));
        assert_eq!(plan.waves.iter().map(|w| w.n).collect::<Vec<_>>(), [1, 2]);
    }

    #[test]
    fn tags_are_unique_across_waves() {
        let s = seed(9);
        let mut plan = Plan::stand_in(&s, 1, &[spec("A", 2, 1, 3, 1, 0)]).unwrap();
        let other = Plan::stand_in(&s, 2, &[spec("A", 2, 1, 3, 1, 0)]).unwrap();
        // A rerun of wave 1 may name its own tags again; wave 2 may not.
        plan.check_new_tags(1, &[spec("A", 2, 1, 3, 1, 0)]).unwrap();
        let e = format!(
            "{:#}",
            plan.check_new_tags(2, &[spec("A", 2, 1, 3, 1, 0)])
                .unwrap_err()
        );
        assert!(e.contains("\"A\"") && e.contains("wave 1"), "{e}");
        let twice = [spec("B", 2, 1, 3, 1, 0), spec("B", 2, 1, 3, 1, 0)];
        assert!(plan.check_new_tags(2, &twice).is_err());
        plan.waves.extend(other.waves);
        let e = format!("{:#}", plan.check_tags().unwrap_err());
        assert!(e.contains("duplicate tag \"A\" (wave 2)"), "{e}");
    }

    #[test]
    fn orphan_matcher_takes_the_metadata_hash() {
        let mut rng = StdRng::seed_from_u64(10);
        let (_, pk) = elgamal::keygen(&mut rng);
        let snap = |b: u8| ProcSnap {
            pid: ProcessId([b; 31]),
            encryption_key: hex::encode(pk.compress()),
            ballot_mode: ballot_mode_nf(2),
            census_origin: 1,
            census_root: "1".into(),
        };
        let want = metadata_hash(b"wave 1 R1");
        let mut a = snap(1).onchain().unwrap();
        a.metadata_hash = metadata_hash(b"wave 1 R2");
        let mut b = snap(2).onchain().unwrap();
        b.metadata_hash = want;
        // Canceled by `cancel --all`: never adopted.
        let mut c = snap(3).onchain().unwrap();
        c.metadata_hash = want;
        c.status = ProcessStatus::Canceled;
        let cands = [a, b, c];
        let got: Vec<_> = orphans_matching(&cands, &want)
            .iter()
            .map(|p| p.id)
            .collect();
        assert_eq!(got, [[2; 31]]);
        assert!(orphans_matching(&cands, &[0; 32]).is_empty());
    }
}
