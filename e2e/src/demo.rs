//! Demo elections for a live deployment (`tests/demo.rs`): the election
//! table, the voter secrets, the public census and metadata files, the
//! ballots each kind of election takes, the vote plan and the resumable run
//! state. Nothing here talks to a chain or a node.
//!
//! Secrets (voter keys, the CSP key, the seed behind every ballot secret and
//! choice, DKG organizer secrets) live only in the private directory, in
//! mode-0600 files, and their `Debug` is redacted.

use std::collections::BTreeMap;
use std::fs::{self, DirBuilder, OpenOptions, Permissions};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use alloy::primitives::keccak256;
use anyhow::{Context, Result, bail, ensure};
use davinci_client::api::{Fr, ProcessId};
use davinci_client::organizer::{census_file, merkle_census};
use davinci_client::voter::random_k;
use davinci_zkvm_sdk::ballot::BallotMode;
use davinci_zkvm_sdk::census::{LeanImt, eth_address, slot_key_address};
use k256::ecdsa::SigningKey;
use rand::rngs::StdRng;
use rand::seq::SliceRandom;
use rand::{CryptoRng, Rng, RngCore, SeedableRng};
use serde::{Deserialize, Serialize};

/// Voter keys, the CSP key and the seed, in the private directory.
pub const SECRETS_FILE: &str = "voters.json";
/// Run progress, in the private directory.
pub const STATE_FILE: &str = "state.json";
/// The public explorer of the Gnosis deployment.
pub const EXPLORER: &str = "https://davinci-explorer-yb2p9.ondigitalocean.app";
/// Lifetime of an election the organizer ends once its votes settle.
pub const TALLY_SECS: u64 = 6 * 3600;
const DAY: u64 = 24 * 3600;

/// What a voter puts on the ballot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BallotKind {
    /// Exactly one field is 1.
    Single,
    /// One to `max` fields are 1.
    Approval { max: u64 },
    /// Every field in `0..=max`; the squares add up to at most the weight.
    Quadratic { max: u64 },
    /// Every field in `0..=max`.
    Rating { max: u64 },
    /// A permutation of `1..=numFields`.
    Ranking,
    /// The voter's whole census weight on one field.
    Weighted,
}

/// Where the census lives.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CensusKind {
    /// Origin 1, a JSON file.
    Static,
    /// Origin 2: `added` members join through `setProcessCensus`.
    Updatable { added: usize },
    /// Origin 3, an `OwnedCensus`: `added` members join while voting runs.
    Contract { added: usize },
    /// Origin 4.
    Csp,
}

/// Where the election key comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeySource {
    /// That node's key (index into the node list); it publishes the results.
    Node(usize),
    DkgAutomatic,
    /// Results stay locked until the organizer reveals its secret.
    DkgLocked,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lifecycle {
    /// The organizer ends it once its votes settle; the run waits for the
    /// results.
    Tally,
    /// Left open for `secs`.
    Open { secs: u64 },
    /// Starts `start_in` s after creation and lasts `secs`; no votes.
    Upcoming { start_in: u64, secs: u64 },
    /// Created, then canceled by the organizer.
    Canceled,
}

/// One demo election.
#[derive(Clone, Copy, Debug)]
pub struct Spec {
    /// 1-based number, the key of every per-election record.
    pub n: usize,
    /// Directory under `e2e/demo`.
    pub dir: &'static str,
    pub title: &'static str,
    pub description: &'static str,
    pub question: &'static str,
    pub question_description: &'static str,
    /// One per ballot field, in field order.
    pub choices: &'static [&'static str],
    /// How the voters lean, one value per choice (see [`choose`]).
    pub lean: &'static [f64],
    pub ballot: BallotKind,
    pub census: CensusKind,
    /// Members in the first census.
    pub members: usize,
    /// Census weights, drawn uniformly in this inclusive range.
    pub weights: (u64, u64),
    pub key: KeySource,
    pub lifecycle: Lifecycle,
    /// Members `0..round1` vote in the first round.
    pub round1: usize,
    /// Members `round2.0..round2.1` vote for the first time in the second.
    pub round2: (usize, usize),
    /// First-round voters who change their ballot in the second round.
    pub revotes: usize,
    pub max_voters: u64,
}

/// The demo elections, one of every kind of process, ballot, census, key
/// mode and lifecycle.
pub fn elections() -> Vec<Spec> {
    vec![
        Spec {
            n: 1,
            dir: "1-community-project",
            title: "Next community project",
            description: "The residents' association has funding for one new project in 2027. \
                Choose the one we should start first; the option with the most votes is funded.",
            question: "Which project should we fund first?",
            question_description: "Pick one option.",
            choices: &[
                "Community garden on the empty lot behind the market",
                "Lighting and repairs along the riverside path",
                "A monthly repair café in the civic centre",
                "Covered bike parking at the train station",
            ],
            lean: &[0.34, 0.29, 0.22, 0.15],
            ballot: BallotKind::Single,
            census: CensusKind::Static,
            members: 40,
            weights: (1, 1),
            key: KeySource::Node(0),
            lifecycle: Lifecycle::Tally,
            round1: 22,
            round2: (22, 36),
            revotes: 5,
            max_voters: 40,
        },
        Spec {
            n: 2,
            dir: "2-council-election",
            title: "Council election: approve up to 3",
            description: "Three seats on the coordination council are up for renewal and five \
                members are standing. Approve up to three candidates; the three with the most \
                approvals take the seats for the next two years.",
            question: "Which candidates do you approve?",
            question_description: "Approve at least one and at most three candidates.",
            choices: &[
                "Marta Vidal",
                "Joan Ferrer",
                "Aisha Rahman",
                "Tomás Oliveira",
                "Lena Krüger",
            ],
            lean: &[0.30, 0.18, 0.26, 0.14, 0.12],
            ballot: BallotKind::Approval { max: 3 },
            census: CensusKind::Static,
            members: 30,
            weights: (1, 1),
            key: KeySource::DkgAutomatic,
            lifecycle: Lifecycle::Tally,
            round1: 18,
            round2: (18, 28),
            revotes: 3,
            max_voters: 30,
        },
        Spec {
            n: 3,
            dir: "3-budget-2027",
            title: "2027 budget allocation (quadratic)",
            description: "Members decide how next year's discretionary budget is shared across \
                four lines. Every member has as many voice credits as their membership weight, \
                and giving a line n votes costs n² credits, up to 10 votes per line, so a strong \
                preference costs more than broad support. Members who join before the vote \
                closes can take part.",
            question: "How many votes do you give each budget line?",
            question_description: "From 0 to 10 votes per line; the squares must add up to at \
                most your voice credits.",
            choices: &[
                "Upkeep of shared spaces",
                "Youth and sports programmes",
                "Cultural events and festivals",
                "Emergency support fund",
            ],
            lean: &[0.35, 0.28, 0.15, 0.22],
            ballot: BallotKind::Quadratic { max: 10 },
            census: CensusKind::Updatable { added: 6 },
            members: 20,
            weights: (25, 100),
            key: KeySource::Node(1),
            lifecycle: Lifecycle::Open { secs: 3 * DAY },
            round1: 14,
            round2: (16, 24),
            revotes: 1,
            max_voters: 50,
        },
        Spec {
            n: 4,
            dir: "4-rate-proposals",
            title: "Rate the proposals",
            description: "Three proposals came out of the spring assembly. Rate each from 0 \
                (reject) to 10 (strongly support). The ratings stay sealed until the organizers \
                publish their key after the vote closes.",
            question: "How do you rate each proposal?",
            question_description: "0 means reject, 10 means strong support.",
            choices: &[
                "Open the library on Sundays",
                "Car-free main street on the first Sunday of each month",
                "Solar panels on the primary school roof",
            ],
            lean: &[0.62, 0.41, 0.78],
            ballot: BallotKind::Rating { max: 10 },
            census: CensusKind::Static,
            members: 20,
            weights: (1, 1),
            key: KeySource::DkgLocked,
            lifecycle: Lifecycle::Tally,
            round1: 12,
            round2: (12, 18),
            revotes: 0,
            max_voters: 20,
        },
        Spec {
            n: 5,
            dir: "5-rank-candidates",
            title: "Rank the candidates",
            description: "Four members are running to chair the cooperative's board. Rank all \
                four, from 1 for your first choice to 4 for your last; the lowest total wins. \
                Membership is recorded on-chain, and members who join while the vote is open \
                can take part.",
            question: "How do you rank the candidates for chair?",
            question_description: "Give each candidate a different rank, from 1 (first choice) \
                to 4.",
            choices: &["Clara Soler", "David Mensah", "Irene Costa", "Oriol Puig"],
            lean: &[0.34, 0.27, 0.24, 0.15],
            ballot: BallotKind::Ranking,
            census: CensusKind::Contract { added: 4 },
            members: 12,
            weights: (1, 1),
            key: KeySource::Node(0),
            lifecycle: Lifecycle::Open { secs: 7 * DAY },
            round1: 10,
            round2: (10, 15),
            revotes: 1,
            max_voters: 50,
        },
        Spec {
            n: 6,
            dir: "6-shareholder-resolution",
            title: "Shareholder resolution: yes / no / abstain",
            description: "Resolution 2026-04: approve the merger with the regional credit \
                cooperative on the terms presented at the extraordinary general meeting. Each \
                shareholder casts the full weight of their shares for one option; the weights \
                come from the share register.",
            question: "Do you approve the merger?",
            question_description: "Put all your shares on one option.",
            choices: &["Yes", "No", "Abstain"],
            lean: &[0.56, 0.31, 0.13],
            ballot: BallotKind::Weighted,
            census: CensusKind::Csp,
            members: 12,
            weights: (10, 1000),
            key: KeySource::DkgAutomatic,
            lifecycle: Lifecycle::Tally,
            round1: 7,
            round2: (7, 11),
            revotes: 0,
            max_voters: 12,
        },
        Spec {
            n: 7,
            dir: "7-annual-assembly",
            title: "Annual assembly: 2026 accounts",
            description: "The annual assembly votes on the 2026 accounts presented by the \
                treasurer. Voting opens on the day of the assembly and stays open for 24 hours.",
            question: "Do you approve the 2026 accounts?",
            question_description: "Pick one option.",
            choices: &["Approve", "Reject"],
            lean: &[0.5, 0.5],
            ballot: BallotKind::Single,
            census: CensusKind::Static,
            members: 10,
            weights: (1, 1),
            key: KeySource::Node(1),
            lifecycle: Lifecycle::Upcoming {
                start_in: 2 * DAY,
                secs: DAY,
            },
            round1: 0,
            round2: (0, 0),
            revotes: 0,
            max_voters: 10,
        },
        Spec {
            n: 8,
            dir: "8-clean-up-day",
            title: "Autumn clean-up day: pick a date",
            description: "Which Saturday should we hold the neighbourhood clean-up day? Pick the \
                one that suits you best.",
            question: "Which Saturday works for you?",
            question_description: "Pick one option.",
            choices: &[
                "Saturday 17 October",
                "Saturday 24 October",
                "Saturday 31 October",
            ],
            lean: &[0.4, 0.35, 0.25],
            ballot: BallotKind::Single,
            census: CensusKind::Static,
            members: 5,
            weights: (1, 1),
            key: KeySource::Node(0),
            lifecycle: Lifecycle::Canceled,
            round1: 0,
            round2: (0, 0),
            revotes: 0,
            max_voters: 5,
        },
    ]
}

impl Spec {
    pub fn num_fields(&self) -> usize {
        self.choices.len()
    }

    /// Members who join after creation.
    pub fn added(&self) -> usize {
        match self.census {
            CensusKind::Updatable { added } | CensusKind::Contract { added } => added,
            _ => 0,
        }
    }

    /// Every member the election ever has.
    pub fn total_members(&self) -> usize {
        self.members + self.added()
    }

    pub fn origin(&self) -> u8 {
        match self.census {
            CensusKind::Static => 1,
            CensusKind::Updatable { .. } => 2,
            CensusKind::Contract { .. } => 3,
            CensusKind::Csp => 4,
        }
    }

    pub fn dkg(&self) -> bool {
        matches!(self.key, KeySource::DkgAutomatic | KeySource::DkgLocked)
    }

    pub fn has_votes(&self) -> bool {
        self.round1 > 0 || self.round2.0 < self.round2.1
    }

    /// The ballot mode. Every one keeps groupSize 1 (one question), passes
    /// the registry's checks ([`registry_check`]) and admits every ballot
    /// [`choose`] draws ([`fits`]).
    pub fn ballot_mode(&self) -> BallotMode {
        let nf = self.num_fields() as u64;
        let m = BallotMode {
            num_fields: nf as u8,
            group_size: 1,
            unique_values: false,
            cost_exponent: 1,
            max_value: 1,
            min_value: 0,
            max_value_sum: 0,
            min_value_sum: 0,
        };
        match self.ballot {
            BallotKind::Single => BallotMode {
                max_value_sum: 1,
                min_value_sum: 1,
                ..m
            },
            BallotKind::Approval { max } => BallotMode {
                max_value_sum: max,
                min_value_sum: 1,
                ..m
            },
            // maxValueSum 0: the budget is the census weight.
            BallotKind::Quadratic { max } => BallotMode {
                cost_exponent: 2,
                max_value: max,
                ..m
            },
            BallotKind::Rating { max } => BallotMode {
                max_value: max,
                max_value_sum: nf * max,
                ..m
            },
            // Distinct values in 1..=nf: a permutation, whose sum is fixed.
            BallotKind::Ranking => BallotMode {
                unique_values: true,
                min_value: 1,
                max_value: nf,
                max_value_sum: nf * (nf + 1) / 2,
                ..m
            },
            // Up to the heaviest member on one field, the sum bounded by the
            // voter's own weight.
            BallotKind::Weighted => BallotMode {
                max_value: self.weights.1,
                ..m
            },
        }
    }

    /// `(startTime, duration)` for `newProcess` at unix time `now` (a zero
    /// start means the creation block).
    pub fn timing(&self, now: u64) -> (u64, u64) {
        match self.lifecycle {
            Lifecycle::Tally | Lifecycle::Canceled => (0, TALLY_SECS),
            Lifecycle::Open { secs } => (0, secs),
            Lifecycle::Upcoming { start_in, secs } => (now + start_in, secs),
        }
    }

    pub fn metadata_path(&self) -> String {
        format!("{}/metadata.json", self.dir)
    }

    /// The census document (`updated`: after `setProcessCensus`), or the CSP
    /// note; `None` for a census contract.
    pub fn census_path(&self, updated: bool) -> Option<String> {
        match self.census {
            CensusKind::Static => Some(format!("{}/census.json", self.dir)),
            CensusKind::Updatable { .. } if updated => Some(format!("{}/census-2.json", self.dir)),
            CensusKind::Updatable { .. } => Some(format!("{}/census.json", self.dir)),
            CensusKind::Csp => Some(format!("{}/csp.json", self.dir)),
            CensusKind::Contract { .. } => None,
        }
    }

    /// Ballot, census and key, in words.
    pub fn kind(&self) -> String {
        let ballot = match self.ballot {
            BallotKind::Single => format!("single choice of {}", self.num_fields()),
            BallotKind::Approval { max } => format!("approval, up to {max}"),
            BallotKind::Quadratic { .. } => "quadratic".into(),
            BallotKind::Rating { max } => format!("rating 0-{max}"),
            BallotKind::Ranking => "ranking".into(),
            BallotKind::Weighted => "weighted".into(),
        };
        let census = match self.census {
            CensusKind::Static => "Merkle static",
            CensusKind::Updatable { .. } => "Merkle updatable",
            CensusKind::Contract { .. } => "census contract",
            CensusKind::Csp => "CSP",
        };
        let key = match self.key {
            KeySource::Node(i) => format!("key of node {}", i + 1),
            KeySource::DkgAutomatic => "DKG automatic".into(),
            KeySource::DkgLocked => "DKG locked".into(),
        };
        format!("{ballot}, {census}, {key}")
    }
}

/// The registry's `newProcess` checks on a ballot mode (`_validateNewProcess`
/// and `_validateMaxPossibleResultCap`); the error name it would revert with.
pub fn registry_check(m: &BallotMode, max_voters: u64) -> Result<(), &'static str> {
    const MAX_POSSIBLE_RESULT_CAP: u64 = 1_000_000_000_000;
    if m.num_fields == 0 || m.num_fields > 16 {
        return Err("InvalidMaxCount");
    }
    if m.group_size > m.num_fields {
        return Err("InvalidGroupSize");
    }
    if m.min_value > m.max_value {
        return Err("InvalidMaxMinValueBounds");
    }
    if m.min_value_sum > m.max_value_sum {
        return Err("InvalidValueSumBounds");
    }
    if max_voters == 0 {
        return Err("InvalidMaxVoters");
    }
    if m.max_value > MAX_POSSIBLE_RESULT_CAP / max_voters {
        return Err("MaxPossibleResultCapExceeded");
    }
    Ok(())
}

/// davinci-circom `CheckBallotMode` for a voter of census weight `weight`:
/// every field in `[minValue, maxValue]`, distinct when `uniqueValues`, and
/// the sum of `value^costExponent` in `[minValueSum, maxValueSum]`, with the
/// weight as the upper bound when `maxValueSum` is 0.
pub fn fits(m: &BallotMode, weight: u128, fields: &[u64]) -> bool {
    if fields.len() != usize::from(m.num_fields) || m.group_size > m.num_fields {
        return false;
    }
    if fields
        .iter()
        .any(|v| *v < m.min_value || *v > m.max_value || *v >> 48 != 0)
    {
        return false;
    }
    if m.unique_values {
        let mut seen = fields.to_vec();
        seen.sort_unstable();
        seen.dedup();
        if seen.len() != fields.len() {
            return false;
        }
    }
    let mut sum: u128 = 0;
    for v in fields {
        match u128::from(*v).checked_pow(u32::from(m.cost_exponent)) {
            Some(p) => sum = sum.saturating_add(p),
            None => return false,
        }
    }
    let max = if m.max_value_sum == 0 {
        weight
    } else {
        u128::from(m.max_value_sum)
    };
    sum >= u128::from(m.min_value_sum) && sum <= max
}

/// Index drawn with probability proportional to `w` (non-positive entries
/// are never drawn).
fn pick(rng: &mut impl Rng, w: &[f64]) -> usize {
    let total: f64 = w.iter().filter(|x| **x > 0.0).sum();
    let mut x = rng.r#gen::<f64>() * total;
    let mut last = 0;
    for (i, wi) in w.iter().enumerate() {
        if *wi <= 0.0 {
            continue;
        }
        if x < *wi {
            return i;
        }
        x -= wi;
        last = i;
    }
    last
}

/// `n` distinct indexes, each drawn in proportion to its lean with some
/// personal noise; in draw order.
fn draw(rng: &mut impl Rng, lean: &[f64], n: usize) -> Vec<usize> {
    let mut w: Vec<f64> = lean
        .iter()
        .map(|l| l * (0.5 + rng.r#gen::<f64>()))
        .collect();
    let mut out = Vec::with_capacity(n);
    for _ in 0..n.min(w.len()) {
        let i = pick(rng, &w);
        out.push(i);
        w[i] = 0.0;
    }
    out
}

/// A ballot of `kind` for a voter of census weight `weight`. `lean` holds
/// one value per field: the relative popularity of each option (single
/// choice, approval, ranking, quadratic, weighted), or the average support
/// in `0..=1` (rating).
pub fn choose(kind: BallotKind, lean: &[f64], weight: u64, rng: &mut impl Rng) -> Vec<u64> {
    let nf = lean.len();
    let mut v = vec![0u64; nf];
    match kind {
        BallotKind::Single => v[pick(rng, lean)] = 1,
        BallotKind::Approval { max } => {
            // Two approvals are the most common, then one, then three.
            let counts: Vec<f64> = (0..max as usize)
                .map(|c| [1.0, 1.5, 0.8].get(c).copied().unwrap_or(0.5))
                .collect();
            let n = pick(rng, &counts) + 1;
            for i in draw(rng, lean, n) {
                v[i] = 1;
            }
        }
        BallotKind::Quadratic { max } => {
            let pref: Vec<f64> = lean
                .iter()
                .map(|l| l * (0.2 + rng.r#gen::<f64>()))
                .collect();
            // Spend 60% to all of the credits, one vote at a time.
            let target = weight as f64 * rng.gen_range(0.6..=1.0);
            let mut cost = 0;
            for _ in 0..64 {
                let i = pick(rng, &pref);
                let step = 2 * v[i] + 1;
                if v[i] < max && cost + step <= weight {
                    v[i] += 1;
                    cost += step;
                }
                if cost as f64 >= target {
                    break;
                }
            }
        }
        BallotKind::Rating { max } => {
            for (o, l) in v.iter_mut().zip(lean) {
                let noise = (rng.r#gen::<f64>() + rng.r#gen::<f64>() - 1.0) * 4.0;
                *o = (l * max as f64 + noise).round().clamp(0.0, max as f64) as u64;
            }
        }
        BallotKind::Ranking => {
            for (rank, i) in draw(rng, lean, nf).into_iter().enumerate() {
                v[i] = rank as u64 + 1;
            }
        }
        BallotKind::Weighted => v[pick(rng, lean)] = weight,
    }
    v
}

/// A second ballot that differs from `prev`; `prev` itself when none turns
/// up (a one-field ballot), which the plan test rules out for the table.
pub fn revote(
    kind: BallotKind,
    lean: &[f64],
    weight: u64,
    prev: &[u64],
    rng: &mut impl Rng,
) -> Vec<u64> {
    for _ in 0..1000 {
        let v = choose(kind, lean, weight, rng);
        if v != prev {
            return v;
        }
    }
    prev.to_vec()
}

/// `keccak256(seed ‖ tag ‖ parts as BE u64)`.
fn derive(seed: &[u8; 32], tag: &[u8], parts: &[u64]) -> [u8; 32] {
    let mut b = Vec::with_capacity(32 + tag.len() + 8 * parts.len());
    b.extend_from_slice(seed);
    b.extend_from_slice(tag);
    for p in parts {
        b.extend_from_slice(&p.to_be_bytes());
    }
    keccak256(&b).0
}

/// The ballot secret of `voter`'s ballot in `round` of election `n`. A
/// function of the seed, so a resent ballot has the same vote id.
pub fn vote_k(seed: &[u8; 32], n: usize, voter: usize, round: u8) -> Fr {
    let s = derive(
        seed,
        b"davinci-demo-k",
        &[n as u64, voter as u64, u64::from(round)],
    );
    random_k(&mut StdRng::from_seed(s))
}

/// One vote of the plan.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Planned {
    pub round: u8,
    pub voter: usize,
    /// Index into the node list.
    pub node: usize,
    pub fields: Vec<u64>,
}

/// Every vote of election `spec` over `nodes` nodes, a function of the
/// seed: round 1 (members `0..round1`), then round 2 (members `round2`,
/// then the revotes, each through the other node).
pub fn plan(spec: &Spec, weights: &[u64], seed: &[u8; 32], nodes: usize) -> Vec<Planned> {
    let mut rng = StdRng::from_seed(derive(seed, b"davinci-demo-plan", &[spec.n as u64]));
    let nodes = nodes.max(1);
    let node = |v: usize| (v + spec.n) % nodes;
    let ballot = |rng: &mut StdRng, v: usize| choose(spec.ballot, spec.lean, weights[v], rng);
    let mut out: Vec<Planned> = (0..spec.round1)
        .map(|v| Planned {
            round: 1,
            voter: v,
            node: node(v),
            fields: ballot(&mut rng, v),
        })
        .collect();
    let mut revoters: Vec<usize> = (0..spec.round1).collect();
    revoters.shuffle(&mut rng);
    revoters.truncate(spec.revotes);
    revoters.sort_unstable();
    for v in spec.round2.0..spec.round2.1 {
        out.push(Planned {
            round: 2,
            voter: v,
            node: node(v),
            fields: ballot(&mut rng, v),
        });
    }
    for v in revoters {
        let fields = revote(spec.ballot, spec.lean, weights[v], &out[v].fields, &mut rng);
        out.push(Planned {
            round: 2,
            voter: v,
            node: (node(v) + 1) % nodes,
            fields,
        });
    }
    out
}

/// A secret kept as hex; `Debug` is redacted.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SecretHex(pub String);

impl std::fmt::Debug for SecretHex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("<redacted>")
    }
}

impl SecretHex {
    pub fn from_bytes(b: &[u8]) -> Self {
        SecretHex(hex::encode(b))
    }

    pub fn bytes32(&self) -> Result<[u8; 32]> {
        let mut out = [0u8; 32];
        hex::decode_to_slice(self.0.trim_start_matches("0x"), &mut out)
            .map_err(|_| anyhow::anyhow!("secret is not 32 bytes of hex"))?;
        Ok(out)
    }

    fn signing_key(&self) -> Result<SigningKey> {
        SigningKey::from_slice(&self.bytes32()?).map_err(|_| anyhow::anyhow!("not a secp256k1 key"))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Member {
    pub key: SecretHex,
    pub weight: u64,
}

/// Every private value `prepare` draws. `Debug` shows nothing secret.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Secrets {
    pub version: u32,
    /// Derives every ballot secret and the vote plan.
    pub seed: SecretHex,
    pub csp_key: SecretHex,
    /// Members per election, in census order.
    pub elections: BTreeMap<usize, Vec<Member>>,
}

impl Secrets {
    /// Fresh keys and weights for every member of every election.
    pub fn generate(specs: &[Spec], rng: &mut (impl RngCore + CryptoRng)) -> Result<Secrets> {
        let mut seed = [0u8; 32];
        rng.fill_bytes(&mut seed);
        let mut elections = BTreeMap::new();
        for s in specs {
            let members = (0..s.total_members())
                .map(|_| Member {
                    key: SecretHex::from_bytes(&SigningKey::random(rng).to_bytes()),
                    weight: rng.gen_range(s.weights.0..=s.weights.1),
                })
                .collect();
            elections.insert(s.n, members);
        }
        let out = Secrets {
            version: 1,
            seed: SecretHex::from_bytes(&seed),
            csp_key: SecretHex::from_bytes(&SigningKey::random(rng).to_bytes()),
            elections,
        };
        out.check(specs)?;
        Ok(out)
    }

    /// Fails unless every election has its members, every key parses, no
    /// address repeats and no two members of one census share a ballot slot.
    pub fn check(&self, specs: &[Spec]) -> Result<()> {
        ensure!(self.version == 1, "voter file version {}", self.version);
        self.seed.bytes32().context("seed")?;
        self.csp().context("CSP key")?;
        let mut all = std::collections::BTreeSet::new();
        for s in specs {
            let m = self
                .elections
                .get(&s.n)
                .with_context(|| format!("no members for election {}", s.n))?;
            ensure!(
                m.len() == s.total_members(),
                "election {}: {} members on file, the table wants {}",
                s.n,
                m.len(),
                s.total_members()
            );
            let mut slots = std::collections::BTreeSet::new();
            for (i, (addr, w)) in self.parts(s.n, m.len())?.into_iter().enumerate() {
                ensure!(
                    (s.weights.0..=s.weights.1).contains(&(w as u64)),
                    "election {} member {i}: weight {w} out of range",
                    s.n
                );
                ensure!(
                    all.insert(addr),
                    "election {} member {i}: address repeated",
                    s.n
                );
                ensure!(
                    slots.insert(slot_key_address(&addr)),
                    "election {} member {i}: ballot slot taken",
                    s.n
                );
            }
        }
        Ok(())
    }

    pub fn seed(&self) -> Result<[u8; 32]> {
        self.seed.bytes32()
    }

    pub fn csp(&self) -> Result<SigningKey> {
        self.csp_key.signing_key()
    }

    /// The signing keys of election `n`'s members.
    pub fn keys(&self, n: usize) -> Result<Vec<SigningKey>> {
        self.elections
            .get(&n)
            .with_context(|| format!("no members for election {n}"))?
            .iter()
            .map(|m| m.key.signing_key())
            .collect()
    }

    pub fn weights(&self, n: usize) -> Result<Vec<u64>> {
        Ok(self
            .elections
            .get(&n)
            .with_context(|| format!("no members for election {n}"))?
            .iter()
            .map(|m| m.weight)
            .collect())
    }

    /// (address, weight) of election `n`'s first `count` members.
    pub fn parts(&self, n: usize, count: usize) -> Result<Vec<([u8; 20], u128)>> {
        let m = self
            .elections
            .get(&n)
            .with_context(|| format!("no members for election {n}"))?;
        ensure!(count <= m.len(), "election {n}: {count} members asked");
        m[..count]
            .iter()
            .map(|m| {
                Ok((
                    eth_address(m.key.signing_key()?.verifying_key()),
                    u128::from(m.weight),
                ))
            })
            .collect()
    }

    pub fn load(path: &Path) -> Result<Secrets> {
        let body = fs::read(path).with_context(|| path.display().to_string())?;
        serde_json::from_slice(&body).with_context(|| format!("parse {}", path.display()))
    }

    /// The secrets in `dir`, or fresh ones written there (never over an
    /// existing file); `true` when fresh.
    pub fn load_or_generate(dir: &Path, specs: &[Spec]) -> Result<(Secrets, bool)> {
        let path = dir.join(SECRETS_FILE);
        if path.exists() {
            let s = Secrets::load(&path)?;
            s.check(specs)
                .with_context(|| format!("{} does not fit the election table", path.display()))?;
            return Ok((s, false));
        }
        let s = Secrets::generate(specs, &mut rand::rngs::OsRng)?;
        write_private(&path, &serde_json::to_vec_pretty(&s)?)?;
        Ok((s, true))
    }
}

/// The private directory: `DAVINCI_DEMO_DIR`, default
/// `~/.davinci-gnosis/demo`, created mode 0700.
pub fn private_dir() -> Result<PathBuf> {
    let dir = match std::env::var_os("DAVINCI_DEMO_DIR") {
        Some(d) if !d.is_empty() => PathBuf::from(d),
        _ => PathBuf::from(std::env::var_os("HOME").context("HOME")?).join(".davinci-gnosis/demo"),
    };
    open_private_dir(&dir)?;
    Ok(dir)
}

/// Creates `dir` (0700, and 0700 again if it existed); refuses one inside
/// this repository before touching anything.
pub fn open_private_dir(dir: &Path) -> Result<()> {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .canonicalize()?;
    ensure!(
        !resolved(dir)?.starts_with(&repo),
        "the private directory {} is inside the repository",
        dir.display()
    );
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .with_context(|| dir.display().to_string())?;
    fs::set_permissions(dir, Permissions::from_mode(0o700))?;
    Ok(())
}

/// `p` absolute, through the canonical form of its deepest existing ancestor.
fn resolved(p: &Path) -> Result<PathBuf> {
    let mut base = std::path::absolute(p)?;
    let mut rest = Vec::new();
    while !base.exists() {
        match base.file_name() {
            Some(name) => rest.push(name.to_os_string()),
            None => break,
        }
        base.pop();
    }
    let mut out = base.canonicalize()?;
    out.extend(rest.iter().rev());
    Ok(out)
}

/// Writes `body` to `path` mode 0600, through a temporary file and a rename.
pub fn write_private(path: &Path, body: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    let mut f = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)
        .with_context(|| tmp.display().to_string())?;
    // `mode` only applies to a new file.
    f.set_permissions(Permissions::from_mode(0o600))?;
    f.write_all(body)?;
    f.sync_all()?;
    fs::rename(&tmp, path).with_context(|| path.display().to_string())?;
    Ok(())
}

/// `e2e/demo` in this checkout.
pub fn public_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("demo")
}

/// A public file under `e2e/demo`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PublicFile {
    /// Relative to `e2e/demo`, `/`-separated (also its URL path).
    pub path: String,
    pub body: Vec<u8>,
}

#[derive(Serialize, Deserialize)]
struct Text {
    default: String,
}

impl Text {
    fn of(s: &str) -> Text {
        Text {
            default: s.to_string(),
        }
    }
}

#[derive(Serialize, Deserialize)]
struct Choice {
    title: Text,
    value: usize,
}

#[derive(Serialize, Deserialize)]
struct Question {
    title: Text,
    description: Text,
    choices: Vec<Choice>,
}

/// Vocdoni election metadata, as the explorer reads it.
#[derive(Serialize, Deserialize)]
struct Metadata {
    version: String,
    title: Text,
    description: Text,
    questions: Vec<Question>,
}

/// What the CSP census URI points to: the signing address.
#[derive(Serialize, Deserialize)]
struct CspNote {
    signer: String,
    members: usize,
}

fn pretty<T: Serialize>(v: &T) -> Result<Vec<u8>> {
    let mut b = serde_json::to_vec_pretty(v)?;
    b.push(b'\n');
    Ok(b)
}

/// The metadata document of `spec`: one question whose choice `value` is
/// the field index.
pub fn metadata(spec: &Spec) -> Result<Vec<u8>> {
    pretty(&Metadata {
        version: "1.1".into(),
        title: Text::of(spec.title),
        description: Text::of(spec.description),
        questions: vec![Question {
            title: Text::of(spec.question),
            description: Text::of(spec.question_description),
            choices: spec
                .choices
                .iter()
                .enumerate()
                .map(|(i, c)| Choice {
                    title: Text::of(c),
                    value: i,
                })
                .collect(),
        }],
    })
}

/// The census document the nodes download: `{"participants": [{"key",
/// "weight"}]}`, leaves in order.
pub fn census_json(parts: &[([u8; 20], u128)]) -> Result<Vec<u8>> {
    pretty(&census_file(parts))
}

/// The lean-IMT of `parts`, leaves in order.
pub fn census_tree(parts: &[([u8; 20], u128)]) -> Result<LeanImt> {
    Ok(merkle_census(&census_file(parts))?)
}

pub fn census_root(parts: &[([u8; 20], u128)]) -> Result<Fr> {
    Ok(census_tree(parts)?.root())
}

/// Every public file of the elections: metadata, census documents and the
/// CSP note. Nothing secret goes in.
pub fn public_files(specs: &[Spec], s: &Secrets) -> Result<Vec<PublicFile>> {
    let mut out = Vec::new();
    for spec in specs {
        out.push(PublicFile {
            path: spec.metadata_path(),
            body: metadata(spec)?,
        });
        let census = |updated: bool, count: usize| -> Result<PublicFile> {
            Ok(PublicFile {
                path: spec.census_path(updated).context("census path")?,
                body: census_json(&s.parts(spec.n, count)?)?,
            })
        };
        match spec.census {
            CensusKind::Static => out.push(census(false, spec.members)?),
            CensusKind::Updatable { .. } => {
                out.push(census(false, spec.members)?);
                out.push(census(true, spec.total_members())?);
            }
            CensusKind::Csp => out.push(PublicFile {
                path: spec.census_path(false).context("CSP path")?,
                body: pretty(&CspNote {
                    signer: format!("0x{}", hex::encode(eth_address(s.csp()?.verifying_key()))),
                    members: spec.members,
                })?,
            }),
            CensusKind::Contract { .. } => {}
        }
    }
    Ok(out)
}

/// Writes `files` under `dir`.
pub fn write_public(dir: &Path, files: &[PublicFile]) -> Result<()> {
    for f in files {
        let p = dir.join(&f.path);
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&p, &f.body).with_context(|| p.display().to_string())?;
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VoteState {
    /// Accepted by its node.
    Sent,
    Settled,
    /// The node put it in `error`; it is not waited on again.
    Error,
}

/// A vote a node accepted.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VoteRecord {
    pub round: u8,
    pub voter: usize,
    pub node: usize,
    #[serde(with = "davinci_client::api::enc::vote_id")]
    pub vote_id: u64,
    pub fields: Vec<u64>,
    pub state: VoteState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Progress of one election.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ElectionState {
    /// The id `newProcess` was sent for, until the process id is known: a
    /// resumed run adopts it if it exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending: Option<ProcessId>,
    #[serde(default)]
    pub pid: Option<ProcessId>,
    /// DKG_LOCKED: the organizer secret, 32 bytes BE.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub organizer_secret: Option<SecretHex>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub census_contract: Option<String>,
    #[serde(default)]
    pub census_updated: bool,
    #[serde(default)]
    pub revealed: bool,
    #[serde(default)]
    pub votes: Vec<VoteRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub results: Option<Vec<u64>>,
}

impl ElectionState {
    pub fn vote(&self, round: u8, voter: usize) -> Option<&VoteRecord> {
        self.votes
            .iter()
            .find(|v| v.round == round && v.voter == voter)
    }

    /// Records `v`, sent as `vote_id`, unless it is already there.
    pub fn record(&mut self, v: &Planned, vote_id: u64) {
        if self.vote(v.round, v.voter).is_none() {
            self.votes.push(VoteRecord {
                round: v.round,
                voter: v.voter,
                node: v.node,
                vote_id,
                fields: v.fields.clone(),
                state: VoteState::Sent,
                error: None,
            });
        }
    }

    /// Each voter's last settled ballot.
    pub fn last_ballots(&self) -> BTreeMap<usize, &[u64]> {
        let mut settled: Vec<&VoteRecord> = self
            .votes
            .iter()
            .filter(|v| v.state == VoteState::Settled)
            .collect();
        settled.sort_by_key(|v| v.round);
        settled
            .into_iter()
            .map(|v| (v.voter, v.fields.as_slice()))
            .collect()
    }

    /// The tally the settled votes add up to, `nf` fields.
    pub fn expected_tally(&self, nf: usize) -> Vec<u64> {
        let mut t = vec![0u64; nf];
        for b in self.last_ballots().values() {
            for (o, x) in t.iter_mut().zip(b.iter()) {
                *o += x;
            }
        }
        t
    }
}

/// Everything a resumed run needs, per election number.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct State {
    pub version: u32,
    pub chain_id: u64,
    pub registry: String,
    /// Where the first run pointed the census and metadata URIs.
    pub base_url: String,
    pub elections: BTreeMap<usize, ElectionState>,
}

impl State {
    /// The state at `path`; empty when there is none yet.
    pub fn load(path: &Path) -> Result<State> {
        match fs::read(path) {
            Ok(b) => {
                serde_json::from_slice(&b).with_context(|| format!("parse {}", path.display()))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(State::default()),
            Err(e) => Err(e).with_context(|| path.display().to_string()),
        }
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        write_private(path, &serde_json::to_vec_pretty(self)?)
    }

    /// Binds a fresh state to a deployment; refuses a state of another one.
    pub fn bind(&mut self, chain_id: u64, registry: &str, base_url: &str) -> Result<()> {
        if self.version == 0 {
            *self = State {
                version: 1,
                chain_id,
                registry: registry.to_string(),
                base_url: base_url.to_string(),
                elections: BTreeMap::new(),
            };
        }
        if self.version != 1
            || self.chain_id != chain_id
            || !self.registry.eq_ignore_ascii_case(registry)
        {
            bail!(
                "the state file is for chain {} registry {}, not chain {chain_id} registry {registry}",
                self.chain_id,
                self.registry
            );
        }
        Ok(())
    }

    pub fn election(&mut self, n: usize) -> &mut ElectionState {
        self.elections.entry(n).or_default()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use davinci_client::api::CensusFile;

    use super::*;

    fn secrets() -> Secrets {
        Secrets::generate(&elections(), &mut StdRng::seed_from_u64(7)).unwrap()
    }

    #[test]
    fn election_table_is_consistent() {
        let specs = elections();
        assert_eq!(specs.len(), 8);
        let dirs: BTreeSet<_> = specs.iter().map(|s| s.dir).collect();
        assert_eq!(dirs.len(), specs.len());
        for (i, s) in specs.iter().enumerate() {
            assert_eq!(s.n, i + 1);
            assert!(s.dir.starts_with(&format!("{}-", s.n)));
            let m = s.ballot_mode();
            assert_eq!(usize::from(m.num_fields), s.num_fields());
            assert_eq!(s.lean.len(), s.num_fields());
            assert_eq!(registry_check(&m, s.max_voters), Ok(()), "election {}", s.n);
            m.pack().unwrap();
            assert!(s.weights.0 >= 1 && s.weights.0 <= s.weights.1);
            // Rounds stay inside the census; the second-round voters are new.
            assert!(s.round1 <= s.members);
            assert!(s.round2.0 >= s.round1 && s.round2.0 <= s.round2.1);
            assert!(s.round2.1 <= s.total_members());
            assert!(s.revotes <= s.round1);
            let voters = s.round1 + s.round2.1 - s.round2.0;
            assert!(voters as u64 <= s.max_voters);
            // Only elections that stay open or get tallied take votes.
            match s.lifecycle {
                Lifecycle::Tally | Lifecycle::Open { .. } => assert!(s.has_votes()),
                _ => assert!(!s.has_votes()),
            }
            // A growing census lets some of its new members vote.
            if s.added() > 0 {
                assert!(s.round2.1 > s.members, "election {}", s.n);
            }
            // The DKG results come from ending; a sequencer key from a node.
            if s.dkg() {
                assert_eq!(s.lifecycle, Lifecycle::Tally);
            }
            if let KeySource::Node(i) = s.key {
                assert!(i < 2);
            }
        }
        // One of every census origin and key mode.
        let origins: BTreeSet<_> = specs.iter().map(Spec::origin).collect();
        assert_eq!(origins, BTreeSet::from([1, 2, 3, 4]));
        assert!(specs.iter().any(|s| s.key == KeySource::DkgAutomatic));
        assert!(specs.iter().any(|s| s.key == KeySource::DkgLocked));
        assert!(specs.iter().any(|s| matches!(s.key, KeySource::Node(_))));
    }

    #[test]
    fn ballot_modes() {
        let specs = elections();
        let single = specs[0].ballot_mode();
        assert_eq!(
            (single.max_value, single.min_value_sum, single.max_value_sum),
            (1, 1, 1)
        );
        let approval = specs[1].ballot_mode();
        assert_eq!((approval.max_value_sum, approval.min_value_sum), (3, 1));
        let quadratic = specs[2].ballot_mode();
        assert_eq!(
            (
                quadratic.cost_exponent,
                quadratic.max_value,
                quadratic.max_value_sum
            ),
            (2, 10, 0)
        );
        let rating = specs[3].ballot_mode();
        assert_eq!((rating.max_value, rating.max_value_sum), (10, 30));
        let ranking = specs[4].ballot_mode();
        assert!(ranking.unique_values);
        assert_eq!(
            (ranking.min_value, ranking.max_value, ranking.max_value_sum),
            (1, 4, 10)
        );
        let weighted = specs[5].ballot_mode();
        assert_eq!((weighted.max_value, weighted.max_value_sum), (1000, 0));
        assert!(specs.iter().all(|s| s.ballot_mode().group_size == 1));

        let now = 1_000_000;
        assert_eq!(specs[0].timing(now), (0, TALLY_SECS));
        assert_eq!(specs[2].timing(now), (0, 3 * DAY));
        assert_eq!(specs[4].timing(now), (0, 7 * DAY));
        assert_eq!(specs[6].timing(now), (now + 2 * DAY, DAY));
    }

    #[test]
    fn fits_follows_the_circuit() {
        let s = elections();
        let (single, approval, quad, rating, rank, weighted) = (
            s[0].ballot_mode(),
            s[1].ballot_mode(),
            s[2].ballot_mode(),
            s[3].ballot_mode(),
            s[4].ballot_mode(),
            s[5].ballot_mode(),
        );
        assert!(fits(&single, 1, &[0, 1, 0, 0]));
        assert!(!fits(&single, 1, &[0, 0, 0, 0]));
        assert!(!fits(&single, 1, &[1, 1, 0, 0]));
        assert!(!fits(&single, 1, &[0, 2, 0, 0]));
        assert!(!fits(&single, 1, &[0, 1, 0]));
        assert!(fits(&approval, 1, &[1, 0, 1, 1, 0]));
        assert!(!fits(&approval, 1, &[1, 1, 1, 1, 0]));
        assert!(!fits(&approval, 1, &[0; 5]));
        assert!(fits(&quad, 100, &[10, 0, 0, 0]));
        assert!(!fits(&quad, 99, &[10, 0, 0, 0]));
        assert!(fits(&quad, 30, &[3, 4, 2, 1]));
        assert!(!fits(&quad, 29, &[3, 4, 2, 1]));
        assert!(!fits(&quad, 1000, &[11, 0, 0, 0]));
        assert!(fits(&rating, 1, &[10, 10, 10]));
        assert!(!fits(&rating, 1, &[11, 0, 0]));
        assert!(fits(&rank, 1, &[2, 4, 1, 3]));
        assert!(!fits(&rank, 1, &[2, 2, 1, 3]));
        assert!(!fits(&rank, 1, &[0, 1, 2, 3]));
        assert!(!fits(&rank, 1, &[1, 2, 3, 5]));
        assert!(fits(&weighted, 640, &[0, 640, 0]));
        assert!(fits(&weighted, 640, &[100, 0, 0]));
        assert!(!fits(&weighted, 640, &[0, 641, 0]));
        assert!(!fits(&weighted, 1100, &[0, 1100, 0]));
        // Cost exponent 0: every field costs 1.
        let flat = BallotMode {
            cost_exponent: 0,
            max_value: 5,
            max_value_sum: 4,
            min_value_sum: 0,
            ..single
        };
        assert!(fits(&flat, 1, &[5, 5, 5, 5]));
        // 0^0 counts 1 too, as the circuit's Pow does.
        assert!(!fits(
            &BallotMode {
                max_value_sum: 3,
                ..flat
            },
            1,
            &[0; 4]
        ));
        // Values past the circuit's 48 bits, whatever the bounds say.
        let wide = BallotMode {
            max_value: u64::MAX >> 8,
            max_value_sum: u64::MAX >> 8,
            ..rating
        };
        assert!(fits(&wide, 1, &[1 << 47, 0, 0]));
        assert!(!fits(&wide, 1, &[1 << 48, 0, 0]));
        // With maxValueSum 0 the weight bounds the squares.
        assert!(fits(&quad, 25, &[3, 4, 0, 0]));
        assert!(!fits(&quad, 24, &[3, 4, 0, 0]));
    }

    #[test]
    fn registry_refusals() {
        let m = elections()[0].ballot_mode();
        let refused = |m: BallotMode, voters| registry_check(&m, voters).unwrap_err();
        assert_eq!(
            refused(
                BallotMode {
                    num_fields: 0,
                    group_size: 0,
                    ..m
                },
                1
            ),
            "InvalidMaxCount"
        );
        assert_eq!(
            refused(
                BallotMode {
                    num_fields: 17,
                    ..m
                },
                1
            ),
            "InvalidMaxCount"
        );
        assert_eq!(
            refused(BallotMode { group_size: 5, ..m }, 1),
            "InvalidGroupSize"
        );
        assert_eq!(
            refused(BallotMode { min_value: 2, ..m }, 1),
            "InvalidMaxMinValueBounds"
        );
        assert_eq!(
            refused(
                BallotMode {
                    min_value_sum: 2,
                    ..m
                },
                1
            ),
            "InvalidValueSumBounds"
        );
        assert_eq!(refused(m, 0), "InvalidMaxVoters");
        let big = BallotMode {
            max_value: 1_000_000,
            ..m
        };
        assert_eq!(registry_check(&big, 1_000_000), Ok(()));
        assert_eq!(refused(big, 1_000_001), "MaxPossibleResultCapExceeded");
    }

    #[test]
    fn choices_fit_their_ballot_mode() {
        let mut rng = StdRng::seed_from_u64(1);
        for s in elections() {
            let m = s.ballot_mode();
            for _ in 0..500 {
                let w = rng.gen_range(s.weights.0..=s.weights.1);
                let c = choose(s.ballot, s.lean, w, &mut rng);
                assert!(fits(&m, u128::from(w), &c), "election {}: {c:?} w={w}", s.n);
                let r = revote(s.ballot, s.lean, w, &c, &mut rng);
                assert_ne!(r, c, "election {}", s.n);
                assert!(fits(&m, u128::from(w), &r), "election {}: {r:?} w={w}", s.n);
            }
        }
    }

    #[test]
    fn choices_are_varied() {
        let mut rng = StdRng::seed_from_u64(2);
        for s in elections() {
            let w = s.weights.1;
            let ballots: Vec<Vec<u64>> = (0..200)
                .map(|_| choose(s.ballot, s.lean, w, &mut rng))
                .collect();
            let distinct: BTreeSet<_> = ballots.iter().collect();
            assert!(distinct.len() >= s.num_fields(), "election {}", s.n);
            // Totals follow the lean, not a flat split.
            let mut t = vec![0u64; s.num_fields()];
            for b in &ballots {
                for (o, x) in t.iter_mut().zip(b) {
                    *o += x;
                }
            }
            if s.lean.iter().any(|l| *l != s.lean[0]) {
                assert!(t.iter().any(|x| *x != t[0]), "election {}: {t:?}", s.n);
            }
        }
        // Quadratic voters spend most of their credits.
        let s = &elections()[2];
        for w in [25, 60, 100] {
            let c = choose(s.ballot, s.lean, w, &mut rng);
            let cost: u64 = c.iter().map(|v| v * v).sum();
            assert!(cost > 0 && cost <= w);
        }
    }

    #[test]
    fn plan_follows_the_table() {
        let sec = secrets();
        let seed = sec.seed().unwrap();
        for s in elections() {
            let w = sec.weights(s.n).unwrap();
            let p = plan(&s, &w, &seed, 2);
            assert_eq!(p, plan(&s, &w, &seed, 2), "deterministic");
            let r1: Vec<_> = p.iter().filter(|v| v.round == 1).collect();
            let r2: Vec<_> = p.iter().filter(|v| v.round == 2).collect();
            assert_eq!(r1.len(), s.round1);
            assert_eq!(r2.len(), s.round2.1 - s.round2.0 + s.revotes);
            let firsts: BTreeSet<_> = r1.iter().map(|v| v.voter).collect();
            let mut revotes = 0;
            for v in &r2 {
                if let Some(f) = r1.iter().find(|f| f.voter == v.voter) {
                    revotes += 1;
                    assert_ne!(v.fields, f.fields, "a revote changes the ballot");
                    assert_ne!(v.node, f.node, "a revote goes through the other node");
                } else {
                    assert!((s.round2.0..s.round2.1).contains(&v.voter));
                    assert!(!firsts.contains(&v.voter));
                }
            }
            assert_eq!(revotes, s.revotes);
            for v in &p {
                assert!(fits(&s.ballot_mode(), u128::from(w[v.voter]), &v.fields));
            }
            if p.len() > 4 {
                let nodes: BTreeSet<_> = p.iter().map(|v| v.node).collect();
                assert_eq!(nodes.len(), 2, "votes spread over both nodes");
            }
        }
        // Another seed, another plan.
        let other = [9u8; 32];
        let s = &elections()[0];
        let w = sec.weights(1).unwrap();
        assert_ne!(plan(s, &w, &seed, 2), plan(s, &w, &other, 2));
        // One node takes every vote, revotes included.
        assert!(plan(s, &w, &seed, 1).iter().all(|v| v.node == 0));
    }

    #[test]
    fn bad_secrets_are_refused() {
        let specs = elections();
        let good = secrets();
        let mut repeated = good.clone();
        let first = repeated.elections[&1][0].clone();
        repeated.elections.get_mut(&2).unwrap()[3] = first;
        assert!(repeated.check(&specs).is_err());
        let mut heavy = good.clone();
        heavy.elections.get_mut(&3).unwrap()[0].weight = 101;
        assert!(heavy.check(&specs).is_err());
        let mut short = good.clone();
        short.elections.get_mut(&4).unwrap().pop();
        assert!(short.check(&specs).is_err());
        let mut bad_key = good;
        bad_key.csp_key = SecretHex("00".repeat(32));
        assert!(bad_key.check(&specs).is_err());
    }

    #[test]
    fn ballot_secrets_are_deterministic() {
        let seed = [3u8; 32];
        assert_eq!(vote_k(&seed, 1, 4, 1), vote_k(&seed, 1, 4, 1));
        assert_ne!(vote_k(&seed, 1, 4, 1), vote_k(&seed, 1, 4, 2));
        assert_ne!(vote_k(&seed, 1, 4, 1), vote_k(&seed, 2, 4, 1));
        assert_ne!(vote_k(&seed, 1, 4, 1), vote_k(&[4u8; 32], 1, 4, 1));
    }

    #[test]
    fn public_files_carry_no_secret() {
        let specs = elections();
        let sec = secrets();
        let files = public_files(&specs, &sec).unwrap();
        let mut secret_hex = vec![sec.seed.0.clone(), sec.csp_key.0.clone()];
        for m in sec.elections.values().flatten() {
            secret_hex.push(m.key.0.clone());
        }
        for f in &files {
            let body = String::from_utf8(f.body.clone()).unwrap().to_lowercase();
            for s in &secret_hex {
                assert!(!body.contains(&s[..16]), "{} leaks a secret", f.path);
            }
            assert!(f.body.ends_with(b"}\n"));
        }
        let paths: BTreeSet<_> = files.iter().map(|f| f.path.as_str()).collect();
        assert!(paths.contains("3-budget-2027/census-2.json"));
        assert!(paths.contains("6-shareholder-resolution/csp.json"));
        assert!(
            !paths
                .iter()
                .any(|p| p.starts_with("5-") && p.contains("census"))
        );
        assert_eq!(paths.len(), files.len());
    }

    #[test]
    fn census_files_match_their_roots() {
        let specs = elections();
        let sec = secrets();
        let files = public_files(&specs, &sec).unwrap();
        let body = |p: &str| {
            files
                .iter()
                .find(|f| f.path == p)
                .map(|f| f.body.clone())
                .unwrap()
        };
        for s in &specs {
            let counts: Vec<(bool, usize)> = match s.census {
                CensusKind::Static => vec![(false, s.members)],
                CensusKind::Updatable { .. } => {
                    vec![(false, s.members), (true, s.total_members())]
                }
                _ => continue,
            };
            for (updated, count) in counts {
                let raw = body(&s.census_path(updated).unwrap());
                let file: CensusFile = serde_json::from_slice(&raw).unwrap();
                let parts = sec.parts(s.n, count).unwrap();
                assert_eq!(file.participants.len(), count);
                assert_eq!(
                    merkle_census(&file).unwrap().root(),
                    census_root(&parts).unwrap()
                );
                // Weights are decimal strings, keys 0x addresses.
                let j: serde_json::Value = serde_json::from_slice(&raw).unwrap();
                let p0 = &j["participants"][0];
                assert_eq!(p0["weight"], parts[0].1.to_string());
                assert_eq!(p0["key"], format!("0x{}", hex::encode(parts[0].0)));
            }
        }
        // The updated census keeps the first members as they were.
        let s = &specs[2];
        let before = census_tree(&sec.parts(3, s.members).unwrap()).unwrap();
        let after = census_tree(&sec.parts(3, s.total_members()).unwrap()).unwrap();
        assert_ne!(before.root(), after.root());
        for i in 0..s.members {
            assert_eq!(before.proof(i).unwrap().leaf, after.proof(i).unwrap().leaf);
        }
        let csp: serde_json::Value =
            serde_json::from_slice(&body("6-shareholder-resolution/csp.json")).unwrap();
        assert_eq!(
            csp["signer"],
            format!(
                "0x{}",
                hex::encode(eth_address(sec.csp().unwrap().verifying_key()))
            )
        );
    }

    #[test]
    fn metadata_lines_up_with_the_fields() {
        for s in elections() {
            let j: serde_json::Value = serde_json::from_slice(&metadata(&s).unwrap()).unwrap();
            assert_eq!(j["version"], "1.1");
            assert_eq!(j["title"]["default"], s.title);
            assert!(!j["description"]["default"].as_str().unwrap().is_empty());
            let q = j["questions"].as_array().unwrap();
            assert_eq!(q.len(), 1);
            assert!(!q[0]["title"]["default"].as_str().unwrap().is_empty());
            assert!(!q[0]["description"]["default"].as_str().unwrap().is_empty());
            let c = q[0]["choices"].as_array().unwrap();
            assert_eq!(c.len(), s.num_fields());
            for (i, c) in c.iter().enumerate() {
                assert_eq!(c["value"], i);
                assert_eq!(c["title"]["default"], s.choices[i]);
            }
            let text = String::from_utf8(metadata(&s).unwrap())
                .unwrap()
                .to_lowercase();
            for word in ["test", "lorem", "ipsum", "demo"] {
                assert!(!text.contains(word), "election {} says {word}", s.n);
            }
        }
    }

    #[test]
    fn secrets_round_trip_privately() {
        let base = tempfile::tempdir().unwrap();
        let dir = base.path().join("a/demo");
        open_private_dir(&dir).unwrap();
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&dir), 0o700);
        // An existing directory is tightened.
        fs::set_permissions(&dir, Permissions::from_mode(0o755)).unwrap();
        open_private_dir(&dir).unwrap();
        assert_eq!(mode(&dir), 0o700);

        let specs = elections();
        let (a, fresh) = Secrets::load_or_generate(&dir, &specs).unwrap();
        assert!(fresh);
        assert_eq!(mode(&dir.join(SECRETS_FILE)), 0o600);
        let (b, fresh) = Secrets::load_or_generate(&dir, &specs).unwrap();
        assert!(!fresh);
        assert_eq!(a, b);
        assert_eq!(
            public_files(&specs, &a).unwrap(),
            public_files(&specs, &b).unwrap()
        );
        let dbg = format!("{a:?}");
        assert!(!dbg.contains(&a.seed.0) && !dbg.contains(&a.csp_key.0));
        assert!(!dbg.contains(&a.elections[&1][0].key.0));
        // A table the file does not fit is refused, not regenerated.
        let mut more = specs.clone();
        more[0].members += 1;
        assert!(Secrets::load_or_generate(&dir, &more).is_err());
        // Never inside the repository, and nothing is created there.
        let inside = Path::new(env!("CARGO_MANIFEST_DIR")).join("no-such-dir/private");
        assert!(open_private_dir(&inside).is_err());
        assert!(!inside.parent().unwrap().exists());
    }

    #[test]
    fn state_resumes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(STATE_FILE);
        let mut st = State::load(&path).unwrap();
        assert_eq!(st, State::default());
        st.bind(100, "0xAbC", "https://x/demo").unwrap();
        let pid = ProcessId([7u8; 31]);
        let e = st.election(1);
        e.pending = Some(pid);
        e.organizer_secret = Some(SecretHex::from_bytes(&[9u8; 32]));
        let v = |round, voter, fields: Vec<u64>| Planned {
            round,
            voter,
            node: 0,
            fields,
        };
        e.record(&v(1, 0, vec![1, 0]), 1 << 63 | 5);
        e.record(&v(1, 1, vec![0, 1]), 1 << 63 | 6);
        e.record(&v(1, 2, vec![1, 0]), 1 << 63 | 7);
        e.record(&v(2, 0, vec![0, 1]), 1 << 63 | 8);
        e.record(&v(2, 1, vec![1, 0]), 1 << 63 | 10);
        // Recording again changes nothing.
        e.record(&v(1, 0, vec![0, 1]), 1 << 63 | 9);
        assert_eq!(e.votes.len(), 5);
        assert_eq!(e.vote(1, 0).unwrap().fields, [1, 0]);
        for r in e.votes.iter_mut() {
            r.state = VoteState::Settled;
        }
        e.votes[2].state = VoteState::Error;
        e.votes[4].state = VoteState::Error;
        // Voter 0's revote replaces its first ballot; voter 1's errored
        // revote leaves its first; voter 2 errored.
        assert_eq!(e.expected_tally(2), [0, 2]);
        e.votes[4].state = VoteState::Sent;
        assert_eq!(e.expected_tally(2), [0, 2], "only settled votes count");
        st.save(&path).unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let back = State::load(&path).unwrap();
        assert_eq!(back, st);
        let raw = fs::read_to_string(&path).unwrap();
        assert!(raw.contains("\"voteId\": \"0x8000000000000005\""));
        assert!(!format!("{back:?}").contains(&"09".repeat(32)));
        // The same deployment binds again; another one is refused.
        let mut again = back.clone();
        again.bind(100, "0xabc", "https://y/demo").unwrap();
        assert_eq!(again.base_url, "https://x/demo");
        assert!(again.clone().bind(1, "0xabc", "").is_err());
        assert!(again.bind(100, "0xdef", "").is_err());
        // A damaged file stops the run instead of starting over.
        fs::write(&path, b"{\"version\": 1, \"elections\": ").unwrap();
        assert!(State::load(&path).is_err());
    }
}
