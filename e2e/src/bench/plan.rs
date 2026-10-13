//! Campaign seed, derivations and the plan of a benchmark campaign.

use std::fmt;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use alloy::primitives::Address;
use anyhow::{Context, Result, anyhow, ensure};
use davinci_client::api::{Fr, ProcessId, ProcessStatus};
use davinci_client::organizer::OnchainProcess;
use davinci_client::voter::random_k;
use davinci_zkvm_sdk::ballot::BallotMode;
use davinci_zkvm_sdk::crypto::babyjubjub::Point;
use davinci_zkvm_sdk::crypto::elgamal;
use davinci_zkvm_sdk::crypto::field::{fr_from_be_mod_order, fr_from_dec, fr_to_dec};
use k256::ecdsa::SigningKey;
use rand::rngs::{OsRng, StdRng};
use rand::{RngCore, SeedableRng};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::fixture::ballot_mode_nf;

/// The two sequencer nodes of a campaign.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NodeId {
    A,
    B,
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
        let units = 1 + usize::from(h[0]) % 6;
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

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Wave {
    pub n: u32,
    pub created: u64,
    pub elections: Vec<Election>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
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
        serde_json::from_slice(&b).with_context(|| format!("parse {}", p.display()))
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
                let root = fr_from_be_mod_order(&seed.hash("root", &spec.tag, n, 0));
                Election {
                    spec: spec.clone(),
                    census_uri: String::new(),
                    census_contract: None,
                    snap: ProcSnap {
                        pid: ProcessId(pid),
                        encryption_key: hex::encode(pk.compress()),
                        ballot_mode: ballot_mode_nf(spec.nf),
                        census_origin: spec.origin,
                        census_root: fr_to_dec(&root),
                    },
                }
            })
            .collect();
        Ok(Plan {
            campaign: "stand-in".into(),
            chain_id: 0,
            registry: Address::ZERO,
            start_block: 0,
            waves: vec![Wave {
                n,
                created: 0,
                elections,
            }],
        })
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::os::unix::fs::PermissionsExt;

    use davinci_zkvm_sdk::census::eth_address;

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
                    assert!(mode.max_value >= 5 && 10 >= sum);
                }
            }
        }
        let differ = (0..200)
            .filter(|&v| s.choices("T", v, 1, 2) != s.choices("T", v, 2, 2))
            .count();
        assert!(differ >= 160, "only {differ} of 200 differ");
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
}
