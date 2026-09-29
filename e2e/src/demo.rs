//! Demo elections for a live deployment (`tests/demo.rs`): the election
//! tables of both waves, the voter secrets, the public census and metadata
//! files, the ballots each kind of election takes, the vote plan and the
//! resumable run state. Nothing here talks to a chain or a node.
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
use davinci_client::organizer::{census_file, merkle_census, metadata_hash};
use davinci_client::voter::random_k;
use davinci_zkvm_sdk::ballot::BallotMode;
use davinci_zkvm_sdk::census::{LeanImt, eth_address, slot_key_address};
use k256::ecdsa::SigningKey;
use rand::rngs::StdRng;
use rand::seq::SliceRandom;
use rand::{CryptoRng, Rng, RngCore, SeedableRng};
use serde::ser::SerializeMap;
use serde::{Deserialize, Serialize, Serializer};

pub mod wave2;

/// Voter keys, the CSP key and the seed of the first wave, in the private
/// directory.
pub const SECRETS_FILE: &str = "voters.json";
/// Run progress of the first wave, in the private directory.
pub const STATE_FILE: &str = "state.json";
/// Lifetime of an election the organizer ends once its votes settle.
pub const TALLY_SECS: u64 = 6 * 3600;
pub const MINUTE: u64 = 60;
pub const DAY: u64 = 24 * 3600;
/// Round tags of the ballot secrets of refused votes: never a real round.
const REFUSAL_ROUND: u8 = 10;

/// Which set of elections a phase drives: `DAVINCI_DEMO_WAVE`, 1 (the
/// default) or 2. Each wave keeps its own secrets, state and public
/// directory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Wave {
    One,
    Two,
}

impl Wave {
    pub fn parse(v: &str) -> Result<Wave> {
        match v.trim() {
            "" | "1" => Ok(Wave::One),
            "2" => Ok(Wave::Two),
            v => bail!("DAVINCI_DEMO_WAVE={v:.10}: 1 or 2"),
        }
    }

    pub fn from_env() -> Result<Wave> {
        Wave::parse(&std::env::var("DAVINCI_DEMO_WAVE").unwrap_or_default())
    }

    pub fn number(self) -> u8 {
        match self {
            Wave::One => 1,
            Wave::Two => 2,
        }
    }

    pub fn elections(self) -> Vec<Spec> {
        match self {
            Wave::One => elections(),
            Wave::Two => wave2::elections(),
        }
    }

    /// Voter keys, the CSP key and the seed, in the private directory.
    pub fn secrets_file(self) -> &'static str {
        match self {
            Wave::One => SECRETS_FILE,
            Wave::Two => "voters-wave2.json",
        }
    }

    /// Run progress, next to the secrets.
    pub fn state_file(self) -> &'static str {
        match self {
            Wave::One => STATE_FILE,
            Wave::Two => "state-wave2.json",
        }
    }
}

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
    /// davinci-sdk `single_choice`: one field is 1, or none when `abstain`
    /// (a blank ballot).
    SingleChoice { abstain: bool },
    /// davinci-sdk `multiple_choice`: `min..=max` fields are 1.
    MultipleChoice { min: u64, max: u64 },
    /// davinci-sdk `approval`: any number of fields are 1.
    ApproveAny,
    /// davinci-sdk `rating` with a floor: every field in `min..=max`.
    RatingFrom { min: u64, max: u64 },
    /// davinci-sdk `quadratic`: the squares add up to `min_spend..=budget`.
    QuadraticBudget { budget: u64, min_spend: u64 },
    /// davinci-sdk's budget recipe: up to `total` points, at most `cap` on
    /// one field.
    Budget { total: u64, cap: u64 },
    /// Every field in `0..=cap`; `value^exp` adds up to at most the weight.
    CostExponent { exp: u8, cap: u64 },
    /// One field, a number in `0..=max`.
    Numeric { max: u64 },
}

/// Where the census lives.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CensusKind {
    /// Origin 1, a JSON file.
    Static,
    /// Origin 2: `added` members join through `setProcessCensus`, which also
    /// reweights member `reweight` (see [`reweighted`]) while that member's
    /// round-2 vote is pending.
    Updatable {
        added: usize,
        reweight: Option<usize>,
    },
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
    /// Results stay locked until the organizer reveals its secret, after the
    /// end.
    DkgLocked,
    /// Locked, and the organizer reveals its secret while voting is open:
    /// from then on the automatic trust model.
    DkgLockedEarly,
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
    /// Canceled by the organizer once its last round settles (at the end
    /// of the run when it takes no votes).
    Canceled,
    /// Ends by itself `secs` after creation; the key holder publishes the
    /// results.
    Timed { secs: u64 },
    /// Opens `start_in` s after creation and votes once open; the organizer
    /// then ends it.
    Later { start_in: u64 },
    /// Would open `start_in` s after creation; canceled right away.
    CanceledEarly { start_in: u64 },
}

/// The texts of an election in one more language, shaped like the English
/// ones.
#[derive(Clone, Copy, Debug)]
pub struct Lang {
    /// ISO 639-1 code.
    pub code: &'static str,
    pub title: &'static str,
    pub description: &'static str,
    pub question: &'static str,
    pub question_description: &'static str,
    pub choices: &'static [&'static str],
}

/// Another version of an election's description: English, then one per
/// language of the election, in its order.
#[derive(Clone, Copy, Debug)]
pub struct Revision {
    pub description: &'static str,
    pub i18n: &'static [&'static str],
}

/// How the metadata of an election changes.
#[derive(Clone, Copy, Debug)]
pub enum MetaPlan {
    /// One document, registered at creation.
    Fixed,
    /// `metadata-2.json`, with the revised description, replaces it right
    /// after creation, before the start.
    BeforeStart(&'static Revision),
    /// The same, once the first round settled, while voting is open.
    WhileOpen(&'static Revision),
    /// The registry gets the hash of a draft (the document with the revised
    /// description), not of the document served.
    Mismatch(&'static Revision),
}

/// A vote the nodes must refuse, and the answer they must give.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Refusal {
    /// A key outside the census, with a ballot proved against a census of
    /// its own.
    NotInCensus,
    /// The vote id signed by another member.
    BadSignature,
    /// A settled vote's ballot secret, so its vote id, on another ballot.
    ReusedVoteId,
    /// A ballot the rules forbid: the client refuses to prove it, and one
    /// proved under looser rules fails the inputs hash.
    BreaksRules,
    /// A new voter once max voters is reached.
    OverMaxVoters,
    /// A vote once the election ended.
    AfterEnd,
}

impl Refusal {
    /// HTTP status and API error code the node must answer with.
    pub fn expected(self) -> (u16, u32) {
        match self {
            Refusal::NotInCensus => (400, 40001),
            Refusal::BadSignature | Refusal::BreaksRules => (400, 40002),
            Refusal::ReusedVoteId => (409, 40901),
            Refusal::OverMaxVoters => (412, 41202),
            Refusal::AfterEnd => (412, 41201),
        }
    }

    /// Cast by a census member who never votes otherwise.
    pub fn needs_member(self) -> bool {
        !matches!(self, Refusal::NotInCensus | Refusal::ReusedVoteId)
    }

    pub fn describe(self) -> &'static str {
        match self {
            Refusal::NotInCensus => "a voter not in the census",
            Refusal::BadSignature => "a bad signature",
            Refusal::ReusedVoteId => "a reused vote id",
            Refusal::BreaksRules => "a ballot outside the rules",
            Refusal::OverMaxVoters => "a vote beyond max voters",
            Refusal::AfterEnd => "a vote after the end",
        }
    }
}

/// An organizer action or a refused vote, run once the first round settled
/// (a vote after the end, once the election ended).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    /// Paused; its round-2 votes go in while paused, and the run resumes it
    /// a while later.
    Pause,
    /// `setProcessDuration`, `secs` longer.
    Extend(u64),
    /// `setProcessDuration`, `secs` shorter. The registry only moves the end
    /// later, so the revert is what gets recorded.
    Shorten(u64),
    /// `setProcessMaxVoters`.
    MaxVoters(u64),
    Refuse(Refusal),
}

/// Where first ballots go.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Route {
    /// Spread over the nodes by voter.
    Spread,
    /// All to one node (index into the node list).
    One(usize),
}

/// One demo election.
#[derive(Clone, Copy, Debug)]
pub struct Spec {
    /// 1-based number within its wave, the key of every per-election record.
    pub n: usize,
    /// Directory under `e2e/demo`.
    pub dir: &'static str,
    pub title: &'static str,
    pub description: &'static str,
    pub question: &'static str,
    pub question_description: &'static str,
    /// One per ballot field, in field order.
    pub choices: &'static [&'static str],
    /// The same texts in other languages.
    pub i18n: &'static [Lang],
    /// How the voters lean, one value per choice (see [`choose`]).
    pub lean: &'static [f64],
    pub ballot: BallotKind,
    /// Ballot modes as davinci-sdk resolves its presets: groupSize is
    /// numFields, and a ranking pins its sum both ways.
    pub sdk: bool,
    pub census: CensusKind,
    /// Members in the first census.
    pub members: usize,
    /// Census weights, drawn uniformly in this inclusive range.
    pub weights: (u64, u64),
    pub key: KeySource,
    pub lifecycle: Lifecycle,
    pub metadata: MetaPlan,
    /// Run in order once the first round settled.
    pub actions: &'static [Action],
    /// Members `0..round1` vote in the first round.
    pub round1: usize,
    /// Members `round2.0..round2.1` vote for the first time in the second.
    pub round2: (usize, usize),
    /// First-round voters who change their ballot in the second round.
    pub revotes: usize,
    /// Of those revotes, how many go through the node of the first ballot;
    /// the rest go through the other one.
    pub same_node: usize,
    /// Members `round3.0..round3.1` vote for the first time in the third.
    pub round3: (usize, usize),
    /// Second-round revoters who change their ballot once more in the third.
    pub revotes3: usize,
    pub route: Route,
    /// First-round votes go out in groups of this many, a while apart; 0
    /// sends them at once.
    pub chunk: usize,
    pub max_voters: u64,
}

impl Spec {
    /// What an election leaves alone unless it says otherwise.
    pub const BASE: Spec = Spec {
        n: 0,
        dir: "",
        title: "",
        description: "",
        question: "",
        question_description: "",
        choices: &[],
        i18n: &[],
        lean: &[],
        ballot: BallotKind::Single,
        sdk: false,
        census: CensusKind::Static,
        members: 0,
        weights: (1, 1),
        key: KeySource::Node(0),
        lifecycle: Lifecycle::Tally,
        metadata: MetaPlan::Fixed,
        actions: &[],
        round1: 0,
        round2: (0, 0),
        revotes: 0,
        same_node: 0,
        round3: (0, 0),
        revotes3: 0,
        route: Route::Spread,
        chunk: 0,
        max_voters: 0,
    };
}

/// The first wave of demo elections, one of every kind of process, ballot,
/// census, key mode and lifecycle.
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
            ..Spec::BASE
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
            ..Spec::BASE
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
            census: CensusKind::Updatable {
                added: 6,
                reweight: None,
            },
            members: 20,
            weights: (25, 100),
            key: KeySource::Node(1),
            lifecycle: Lifecycle::Open { secs: 3 * DAY },
            round1: 14,
            round2: (16, 24),
            revotes: 1,
            max_voters: 50,
            ..Spec::BASE
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
            ..Spec::BASE
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
            ..Spec::BASE
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
            ..Spec::BASE
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
            ..Spec::BASE
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
            ..Spec::BASE
        },
    ]
}

/// The weight a census update gives a reweighted member: doubled, or halved
/// when doubling passes `top`. Never the old weight while `top` is at least 2.
pub fn reweighted(w: u64, top: u64) -> u64 {
    if w.saturating_mul(2) <= top {
        w * 2
    } else {
        (w / 2).max(1)
    }
}

/// davinci-sdk's `ElectionPreset`, as it writes it into
/// `meta.electionPreset`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum Preset {
    SingleChoice {
        #[serde(skip_serializing_if = "std::ops::Not::not")]
        allow_abstain: bool,
    },
    MultipleChoice {
        max_selections: u64,
        min_selections: u64,
    },
    Approval,
    Rating {
        max_value: u64,
        min_value: u64,
    },
    Ranking,
    Quadratic {
        budget: u64,
        min_value_sum: u64,
    },
}

impl Spec {
    pub fn num_fields(&self) -> usize {
        self.choices.len()
    }

    /// Members who join after creation.
    pub fn added(&self) -> usize {
        match self.census {
            CensusKind::Updatable { added, .. } | CensusKind::Contract { added } => added,
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
        matches!(
            self.key,
            KeySource::DkgAutomatic | KeySource::DkgLocked | KeySource::DkgLockedEarly
        )
    }

    /// The organizer holds part of the key.
    pub fn locked(&self) -> bool {
        matches!(self.key, KeySource::DkgLocked | KeySource::DkgLockedEarly)
    }

    /// The member the census update reweights.
    pub fn reweight(&self) -> Option<usize> {
        match self.census {
            CensusKind::Updatable { reweight, .. } => reweight,
            _ => None,
        }
    }

    /// Whether any ballot is cast in `round` (1 to 3).
    pub fn votes_in(&self, round: u8) -> bool {
        match round {
            1 => self.round1 > 0,
            2 => self.round2.0 < self.round2.1 || self.revotes > 0,
            3 => self.round3.0 < self.round3.1 || self.revotes3 > 0 || self.reweight().is_some(),
            _ => false,
        }
    }

    pub fn has_votes(&self) -> bool {
        (1..=3).any(|r| self.votes_in(r))
    }

    /// The last round with ballots.
    pub fn last_round(&self) -> u8 {
        (1..=3).rev().find(|r| self.votes_in(*r)).unwrap_or(0)
    }

    /// Ends with results the run waits for.
    pub fn tallied(&self) -> bool {
        matches!(
            self.lifecycle,
            Lifecycle::Tally | Lifecycle::Later { .. } | Lifecycle::Timed { .. }
        )
    }

    /// Whether the run pauses it.
    pub fn pauses(&self) -> bool {
        self.actions.contains(&Action::Pause)
    }

    /// Census weight of `voter` in a ballot of `round`: the reweighted
    /// member's changes from round 3, cast after the update.
    pub fn weight_at(&self, weights: &[u64], voter: usize, round: u8) -> u64 {
        let w = weights[voter];
        match self.reweight() {
            Some(x) if x == voter && round >= 3 => reweighted(w, self.weights.1),
            _ => w,
        }
    }

    /// The refused votes of the election, in order.
    pub fn refusals(&self) -> Vec<Refusal> {
        self.actions
            .iter()
            .filter_map(|a| match a {
                Action::Refuse(r) => Some(*r),
                _ => None,
            })
            .collect()
    }

    /// The member who casts refused vote `r`: the last members of the
    /// census, one per refusal that needs one, in order. They vote in no
    /// round.
    pub fn refusal_member(&self, r: Refusal) -> Option<usize> {
        let takers = self.refusals().into_iter().filter(|x| x.needs_member());
        for (i, x) in takers.enumerate() {
            if x == r {
                return self.total_members().checked_sub(1 + i);
            }
        }
        None
    }

    /// The ballot mode. Every one passes the registry's checks
    /// ([`registry_check`]) and admits every ballot [`choose`] draws
    /// ([`fits`]).
    pub fn ballot_mode(&self) -> BallotMode {
        let nf = self.num_fields() as u64;
        let m = BallotMode {
            num_fields: nf as u8,
            group_size: if self.sdk { nf as u8 } else { 1 },
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
                min_value_sum: if self.sdk { nf * (nf + 1) / 2 } else { 0 },
                ..m
            },
            // Up to the heaviest member on one field, the sum bounded by the
            // voter's own weight.
            BallotKind::Weighted => BallotMode {
                max_value: self.weights.1,
                ..m
            },
            BallotKind::SingleChoice { abstain } => BallotMode {
                max_value_sum: 1,
                min_value_sum: u64::from(!abstain),
                ..m
            },
            BallotKind::MultipleChoice { min, max } => BallotMode {
                max_value_sum: max,
                min_value_sum: min,
                ..m
            },
            BallotKind::ApproveAny => BallotMode {
                max_value_sum: nf,
                ..m
            },
            BallotKind::RatingFrom { min, max } => BallotMode {
                min_value: min,
                max_value: max,
                min_value_sum: nf * min,
                max_value_sum: nf * max,
                ..m
            },
            // As the SDK resolves it: maxValue is the budget too.
            BallotKind::QuadraticBudget { budget, min_spend } => BallotMode {
                cost_exponent: 2,
                max_value: budget,
                max_value_sum: budget,
                min_value_sum: min_spend,
                ..m
            },
            BallotKind::Budget { total, cap } => BallotMode {
                max_value: cap,
                max_value_sum: total,
                ..m
            },
            // maxValueSum 0: the credits are the census weight.
            BallotKind::CostExponent { exp, cap } => BallotMode {
                cost_exponent: exp,
                max_value: cap,
                ..m
            },
            BallotKind::Numeric { max } => BallotMode {
                max_value: max,
                max_value_sum: max,
                ..m
            },
        }
    }

    /// The davinci-sdk preset the ballot mode resolves from, when it is one
    /// (only for `sdk` elections).
    pub fn preset(&self) -> Option<Preset> {
        if !self.sdk {
            return None;
        }
        Some(match self.ballot {
            BallotKind::SingleChoice { abstain } => Preset::SingleChoice {
                allow_abstain: abstain,
            },
            BallotKind::MultipleChoice { min, max } => Preset::MultipleChoice {
                max_selections: max,
                min_selections: min,
            },
            BallotKind::ApproveAny => Preset::Approval,
            BallotKind::RatingFrom { min, max } => Preset::Rating {
                max_value: max,
                min_value: min,
            },
            BallotKind::Ranking => Preset::Ranking,
            BallotKind::QuadraticBudget { budget, min_spend } => Preset::Quadratic {
                budget,
                min_value_sum: min_spend,
            },
            _ => return None,
        })
    }

    /// `(startTime, duration)` for `newProcess` at unix time `now` (a zero
    /// start means the creation block).
    pub fn timing(&self, now: u64) -> (u64, u64) {
        match self.lifecycle {
            Lifecycle::Tally | Lifecycle::Canceled => (0, TALLY_SECS),
            Lifecycle::Open { secs } | Lifecycle::Timed { secs } => (0, secs),
            Lifecycle::Upcoming { start_in, secs } => (now + start_in, secs),
            Lifecycle::Later { start_in } => (now + start_in, TALLY_SECS),
            Lifecycle::CanceledEarly { start_in } => (now + start_in, DAY),
        }
    }

    pub fn metadata_path(&self) -> String {
        format!("{}/metadata.json", self.dir)
    }

    /// The revised document of a metadata update.
    pub fn metadata_update_path(&self) -> String {
        format!("{}/metadata-2.json", self.dir)
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

    /// The ballot, in words.
    pub fn ballot_name(&self) -> String {
        let nf = self.num_fields();
        match self.ballot {
            BallotKind::Single => format!("single choice of {nf}"),
            BallotKind::Approval { max } => format!("approval, up to {max}"),
            BallotKind::Quadratic { .. } => "quadratic".into(),
            BallotKind::Rating { max } => format!("rating 0-{max}"),
            BallotKind::Ranking if self.sdk => format!("ranking of {nf}"),
            BallotKind::Ranking => "ranking".into(),
            BallotKind::Weighted if self.sdk => "weighted single choice".into(),
            BallotKind::Weighted => "weighted".into(),
            BallotKind::SingleChoice { abstain: true } => {
                format!("single choice of {nf}, abstain allowed")
            }
            BallotKind::SingleChoice { abstain: false } => format!("single choice of {nf}"),
            BallotKind::MultipleChoice { min, max } if min == max => {
                format!("multiple choice, exactly {max} of {nf}")
            }
            BallotKind::MultipleChoice { min, max } => {
                format!("multiple choice, {min} to {max} of {nf}")
            }
            BallotKind::ApproveAny => format!("approval of {nf}"),
            BallotKind::RatingFrom { min, max } => format!("rating {min}-{max} on {nf}"),
            BallotKind::QuadraticBudget { budget, min_spend } => {
                format!("quadratic, {budget} credits, at least {min_spend} spent")
            }
            BallotKind::Budget { total, cap } => {
                format!("{total} points over {nf}, at most {cap} each")
            }
            BallotKind::CostExponent { exp, .. } => format!("cost exponent {exp}"),
            BallotKind::Numeric { max } => format!("one number, 0-{max}"),
        }
    }

    /// The census, in words.
    pub fn census_name(&self) -> String {
        match self.census {
            CensusKind::Static => "Merkle static".into(),
            CensusKind::Updatable {
                reweight: Some(_), ..
            } => "Merkle updatable, a reweight".into(),
            CensusKind::Updatable { .. } => "Merkle updatable".into(),
            CensusKind::Contract { .. } => "census contract".into(),
            CensusKind::Csp if self.weights.0 < self.weights.1 => "CSP, weighted".into(),
            CensusKind::Csp => "CSP".into(),
        }
    }

    /// The key mode, in words.
    pub fn key_name(&self) -> String {
        match self.key {
            KeySource::Node(i) => format!("key of node {}", i + 1),
            KeySource::DkgAutomatic => "DKG automatic".into(),
            KeySource::DkgLocked => "DKG locked".into(),
            KeySource::DkgLockedEarly => "DKG locked, revealed while open".into(),
        }
    }

    /// Ballot, census and key, in words.
    pub fn kind(&self) -> String {
        format!(
            "{}, {}, {}",
            self.ballot_name(),
            self.census_name(),
            self.key_name()
        )
    }

    /// What happens to the election, in words.
    pub fn lifecycle_name(&self) -> String {
        let mut out = vec![match self.lifecycle {
            Lifecycle::Tally if !self.has_votes() => "no votes, ended by the organizer".into(),
            Lifecycle::Tally => "ended by the organizer".into(),
            Lifecycle::Open { secs } => format!("left open for {} h", secs / 3600),
            Lifecycle::Upcoming { start_in, .. } => {
                format!("opens {} h after creation", start_in / 3600)
            }
            Lifecycle::Canceled if self.has_votes() => "canceled during voting".into(),
            Lifecycle::Canceled => "canceled".into(),
            Lifecycle::Timed { secs } => format!("ends by time, {} min", secs / MINUTE),
            Lifecycle::Later { start_in } => format!(
                "opens {} min after creation, ended by the organizer",
                start_in / MINUTE
            ),
            Lifecycle::CanceledEarly { .. } => "canceled before its start".into(),
        }];
        for a in self.actions {
            match a {
                Action::Pause => out.push("paused and resumed".into()),
                Action::Extend(s) => out.push(format!("extended by {} min", s / MINUTE)),
                Action::Shorten(s) => out.push(format!("shortened by {} min (tried)", s / MINUTE)),
                Action::MaxVoters(m) => out.push(format!("max voters raised to {m}")),
                Action::Refuse(Refusal::OverMaxVoters) => out.push("max voters reached".into()),
                Action::Refuse(_) => {}
            }
        }
        match self.metadata {
            MetaPlan::Fixed => {}
            MetaPlan::BeforeStart(_) => out.push("metadata updated before the start".into()),
            MetaPlan::WhileOpen(_) => out.push("metadata updated while open".into()),
            MetaPlan::Mismatch(_) => out.push("metadata hash mismatch".into()),
        }
        out.join("; ")
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

/// The lean with some personal noise: one voter's preferences.
fn preferences(rng: &mut impl Rng, lean: &[f64]) -> Vec<f64> {
    lean.iter()
        .map(|l| l * (0.2 + rng.r#gen::<f64>()))
        .collect()
}

/// Casts votes one at a time on options drawn by `pref`, each costing
/// `(v+1)^exp - v^exp` credits, until `target` is spent or 200 draws pass;
/// never more than `cap` on one option or `budget` in all. The spend.
fn spend(
    rng: &mut impl Rng,
    pref: &[f64],
    v: &mut [u64],
    exp: u32,
    cap: u64,
    budget: u64,
    target: u64,
) -> u64 {
    let step = |x: u64| (x + 1).pow(exp) - x.pow(exp);
    let mut spent: u64 = v.iter().map(|x| x.pow(exp)).sum();
    for _ in 0..200 {
        if spent >= target {
            break;
        }
        let i = pick(rng, pref);
        if v[i] < cap && spent + step(v[i]) <= budget {
            spent += step(v[i]);
            v[i] += 1;
        }
    }
    // Short of the target: the cheapest vote that still fits, anywhere.
    while spent < target {
        let Some(i) = (0..v.len())
            .filter(|i| v[*i] < cap && spent + step(v[*i]) <= budget)
            .min_by_key(|i| step(v[*i]))
        else {
            break;
        };
        spent += step(v[i]);
        v[i] += 1;
    }
    spent
}

/// A ballot of `kind` for a voter of census weight `weight`. `lean` holds
/// one value per field: the relative popularity of each option (choices,
/// approvals, rankings, credits and points), or the average support in
/// `0..=1` (ratings and the number, as a share of its range).
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
            let pref = preferences(rng, lean);
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
        BallotKind::SingleChoice { abstain } => {
            // One voter in twelve leaves it blank where the rules allow.
            if !(abstain && rng.gen_bool(1.0 / 12.0)) {
                v[pick(rng, lean)] = 1;
            }
        }
        BallotKind::MultipleChoice { min, max } => {
            let n = rng.gen_range(min.max(1)..=max) as usize;
            for i in draw(rng, lean, n) {
                v[i] = 1;
            }
        }
        BallotKind::ApproveAny => {
            // Most approve two to five.
            let counts = [0.6, 1.0, 1.3, 1.1, 0.8, 0.5, 0.3, 0.2];
            let n = pick(rng, &counts) + 1;
            for i in draw(rng, lean, n) {
                v[i] = 1;
            }
        }
        BallotKind::RatingFrom { min, max } => {
            let span = (max - min) as f64;
            for (o, l) in v.iter_mut().zip(lean) {
                let noise = (rng.r#gen::<f64>() + rng.r#gen::<f64>() - 1.0) * span * 0.45;
                *o = (min as f64 + l * span + noise)
                    .round()
                    .clamp(min as f64, max as f64) as u64;
            }
        }
        BallotKind::QuadraticBudget { budget, min_spend } => {
            let pref = preferences(rng, lean);
            let target = rng.gen_range(min_spend.max(budget * 6 / 10)..=budget);
            spend(rng, &pref, &mut v, 2, budget, budget, target);
        }
        BallotKind::Budget { total, cap } => {
            // Points in fives, 70% to all of them, favourites first.
            let pref = preferences(rng, lean);
            let target = 5 * rng.gen_range(total * 7 / 50..=total / 5);
            let mut sum = 0;
            for _ in 0..400 {
                if sum >= target {
                    break;
                }
                let i = pick(rng, &pref);
                if v[i] + 5 <= cap && sum + 5 <= total {
                    v[i] += 5;
                    sum += 5;
                }
            }
        }
        BallotKind::CostExponent { exp, cap } => {
            let pref = preferences(rng, lean);
            let target = ((weight as f64 * rng.gen_range(0.6..=1.0)) as u64).max(1);
            spend(rng, &pref, &mut v, u32::from(exp), cap, weight, target);
        }
        BallotKind::Numeric { max } => {
            let noise = (rng.r#gen::<f64>() + rng.r#gen::<f64>() - 1.0) * 0.35 * max as f64;
            let mut x = (lean[0] * max as f64 + noise).clamp(0.0, max as f64);
            // Most answers are round numbers.
            if rng.gen_bool(0.7) {
                x = (x / 5.0).round() * 5.0;
            }
            v[0] = (x.round() as u64).min(max);
        }
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

/// A ballot the rules of `spec` forbid, and looser rules it fits: one more
/// field set than a 0/1 ballot allows. `None` for other ballots.
pub fn breaking_ballot(spec: &Spec) -> Option<(Vec<u64>, BallotMode)> {
    let m = spec.ballot_mode();
    let nf = spec.num_fields();
    let binary = m.min_value == 0 && m.max_value == 1 && m.cost_exponent == 1;
    let over = usize::try_from(m.max_value_sum).ok()? + 1;
    if !binary || m.unique_values || m.max_value_sum == 0 || over > nf {
        return None;
    }
    let fields = (0..nf).map(|i| u64::from(i < over)).collect();
    Some((
        fields,
        BallotMode {
            max_value_sum: over as u64,
            ..m
        },
    ))
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

/// The ballot secret of refused vote `r` of election `n`, cast by `voter`.
pub fn refusal_k(seed: &[u8; 32], n: usize, voter: usize, r: Refusal) -> Fr {
    vote_k(seed, n, voter, REFUSAL_ROUND + r as u8)
}

/// A key no census of election `n` holds, for the vote from outside it.
pub fn outsider(seed: &[u8; 32], n: usize) -> SigningKey {
    (0u64..)
        .find_map(|i| {
            SigningKey::from_slice(&derive(seed, b"davinci-demo-outsider", &[n as u64, i])).ok()
        })
        .expect("a valid key within a few tries")
}

/// Whether ballot `v` of `spec` goes in under the updated census: an
/// updatable census changes once round 1 settled, except that the
/// reweighted member's round-2 ballot goes in just before the change.
pub fn under_updated_census(spec: &Spec, v: &Planned) -> bool {
    matches!(spec.census, CensusKind::Updatable { .. })
        && v.round >= 2
        && !(v.round == 2 && spec.reweight() == Some(v.voter))
}

/// One vote of the plan.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Planned {
    pub round: u8,
    pub voter: usize,
    /// Index into the node list.
    pub node: usize,
    /// The census weight the ballot is cast with.
    pub weight: u64,
    pub fields: Vec<u64>,
}

/// Every vote of election `spec` over `nodes` nodes, a function of the
/// seed: round 1 (members `0..round1`), round 2 (members `round2`, then the
/// revotes, each through the other node unless it is one of the first
/// `same_node`), round 3 (members `round3`, the second revotes of the first
/// `revotes3` revoters, and the reweighted member's recast).
pub fn plan(spec: &Spec, weights: &[u64], seed: &[u8; 32], nodes: usize) -> Vec<Planned> {
    let mut rng = StdRng::from_seed(derive(seed, b"davinci-demo-plan", &[spec.n as u64]));
    let nodes = nodes.max(1);
    let first = |v: usize| match spec.route {
        Route::Spread => (v + spec.n) % nodes,
        Route::One(i) => i % nodes,
    };
    let vote = |rng: &mut StdRng, round: u8, v: usize, node: usize| {
        let weight = spec.weight_at(weights, v, round);
        Planned {
            round,
            voter: v,
            node,
            weight,
            fields: choose(spec.ballot, spec.lean, weight, rng),
        }
    };
    let mut out: Vec<Planned> = (0..spec.round1)
        .map(|v| vote(&mut rng, 1, v, first(v)))
        .collect();
    let mut revoters: Vec<usize> = (0..spec.round1).collect();
    revoters.shuffle(&mut rng);
    revoters.truncate(spec.revotes);
    revoters.sort_unstable();
    for v in spec.round2.0..spec.round2.1 {
        out.push(vote(&mut rng, 2, v, first(v)));
    }
    for (i, &v) in revoters.iter().enumerate() {
        let weight = spec.weight_at(weights, v, 2);
        let fields = revote(spec.ballot, spec.lean, weight, &out[v].fields, &mut rng);
        let node = if i < spec.same_node {
            first(v)
        } else {
            (first(v) + 1) % nodes
        };
        out.push(Planned {
            round: 2,
            voter: v,
            node,
            weight,
            fields,
        });
    }
    // Every draw above is the first wave's, so its plans do not move.
    for v in spec.round3.0..spec.round3.1 {
        out.push(vote(&mut rng, 3, v, first(v)));
    }
    for &v in revoters.iter().take(spec.revotes3) {
        let Some(prev) = out.iter().rev().find(|p| p.voter == v).cloned() else {
            continue;
        };
        let weight = spec.weight_at(weights, v, 3);
        out.push(Planned {
            round: 3,
            voter: v,
            node: (prev.node + 1) % nodes,
            weight,
            fields: revote(spec.ballot, spec.lean, weight, &prev.fields, &mut rng),
        });
    }
    if let Some(x) = spec.reweight() {
        out.push(vote(&mut rng, 3, x, (first(x) + 1) % nodes));
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
        // The key voting from outside a census belongs to none.
        let seed = self.seed()?;
        for s in specs
            .iter()
            .filter(|s| s.refusals().contains(&Refusal::NotInCensus))
        {
            ensure!(
                !all.contains(&eth_address(outsider(&seed, s.n).verifying_key())),
                "election {}: the outsider is a member",
                s.n
            );
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

    /// The secrets at `path`, or fresh ones written there (never over an
    /// existing file); `true` when fresh.
    pub fn load_or_generate(path: &Path, specs: &[Spec]) -> Result<(Secrets, bool)> {
        if path.exists() {
            let s = Secrets::load(path)?;
            s.check(specs)
                .with_context(|| format!("{} does not fit the election table", path.display()))?;
            return Ok((s, false));
        }
        let s = Secrets::generate(specs, &mut rand::rngs::OsRng)?;
        write_private(path, &serde_json::to_vec_pretty(&s)?)?;
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

/// A string in every language of the election: `default` (English), then
/// `en` and the others, when there are others.
struct Text(Vec<(&'static str, String)>);

impl Text {
    fn of(en: &str, others: &[(&'static str, &str)]) -> Text {
        let mut t = vec![("default", en.to_string())];
        if !others.is_empty() {
            t.push(("en", en.to_string()));
            t.extend(others.iter().map(|(code, s)| (*code, s.to_string())));
        }
        Text(t)
    }
}

impl Serialize for Text {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut m = s.serialize_map(Some(self.0.len()))?;
        for (k, v) in &self.0 {
            m.serialize_entry(k, v)?;
        }
        m.end()
    }
}

#[derive(Serialize)]
struct Choice {
    title: Text,
    value: usize,
}

#[derive(Serialize)]
struct Question {
    title: Text,
    description: Text,
    choices: Vec<Choice>,
}

#[derive(Serialize)]
struct Meta {
    #[serde(rename = "electionPreset")]
    election_preset: Preset,
}

/// Vocdoni election metadata, as the explorer reads it.
#[derive(Serialize)]
struct Metadata {
    version: String,
    title: Text,
    description: Text,
    questions: Vec<Question>,
    #[serde(skip_serializing_if = "Option::is_none")]
    meta: Option<Meta>,
}

/// What the CSP census URI points to: the signing address.
#[derive(Serialize)]
struct CspNote {
    signer: String,
    members: usize,
}

fn pretty<T: Serialize>(v: &T) -> Result<Vec<u8>> {
    let mut b = serde_json::to_vec_pretty(v)?;
    b.push(b'\n');
    Ok(b)
}

/// The document of `spec` with the description `description` (English,
/// then one per language of the election): one question whose choice
/// `value` is the field index.
fn render(spec: &Spec, description: &str, translated: &[&str]) -> Result<Vec<u8>> {
    ensure!(
        translated.len() == spec.i18n.len(),
        "election {}: {} translated descriptions for {} languages",
        spec.n,
        translated.len(),
        spec.i18n.len()
    );
    let t = |en: &str, pick: fn(&Lang) -> &'static str| {
        let others: Vec<(&'static str, &str)> =
            spec.i18n.iter().map(|l| (l.code, pick(l))).collect();
        Text::of(en, &others)
    };
    let others: Vec<(&'static str, &str)> = spec
        .i18n
        .iter()
        .zip(translated)
        .map(|(l, d)| (l.code, *d))
        .collect();
    let mut choices = Vec::with_capacity(spec.num_fields());
    for (i, c) in spec.choices.iter().enumerate() {
        let mut others = Vec::with_capacity(spec.i18n.len());
        for l in spec.i18n {
            let tr = l
                .choices
                .get(i)
                .with_context(|| format!("election {}: no {} choice {i}", spec.n, l.code))?;
            others.push((l.code, *tr));
        }
        choices.push(Choice {
            title: Text::of(c, &others),
            value: i,
        });
    }
    pretty(&Metadata {
        version: "1.1".into(),
        title: t(spec.title, |l| l.title),
        description: Text::of(description, &others),
        questions: vec![Question {
            title: t(spec.question, |l| l.question),
            description: t(spec.question_description, |l| l.question_description),
            choices,
        }],
        meta: spec.preset().map(|p| Meta { election_preset: p }),
    })
}

/// The metadata document of `spec`, served at its metadata path.
pub fn metadata(spec: &Spec) -> Result<Vec<u8>> {
    let translated: Vec<&str> = spec.i18n.iter().map(|l| l.description).collect();
    render(spec, spec.description, &translated)
}

/// The document with a revised description: `metadata-2.json` of an update,
/// or the draft whose hash a mismatched election registers.
pub fn metadata_revised(spec: &Spec, r: &Revision) -> Result<Vec<u8>> {
    render(spec, r.description, r.i18n)
}

/// The hash `newProcess` registers: the served document's, or the draft's
/// for a deliberate mismatch.
pub fn registered_hash(spec: &Spec) -> Result<[u8; 32]> {
    Ok(match spec.metadata {
        MetaPlan::Mismatch(r) => metadata_hash(&metadata_revised(spec, r)?),
        _ => metadata_hash(&metadata(spec)?),
    })
}

/// The path and hash `setProcessMetadata` moves the election to, if it
/// updates its metadata.
pub fn metadata_update(spec: &Spec) -> Result<Option<(String, [u8; 32])>> {
    Ok(match spec.metadata {
        MetaPlan::BeforeStart(r) | MetaPlan::WhileOpen(r) => Some((
            spec.metadata_update_path(),
            metadata_hash(&metadata_revised(spec, r)?),
        )),
        _ => None,
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

/// Members and weights of `spec`'s census, the first one or the one after
/// the organizer's update (`updated`: the added members, and the
/// reweighted one with its new weight).
pub fn census_parts(spec: &Spec, s: &Secrets, updated: bool) -> Result<Vec<([u8; 20], u128)>> {
    let count = if updated {
        spec.total_members()
    } else {
        spec.members
    };
    let mut parts = s.parts(spec.n, count)?;
    if updated && let Some(x) = spec.reweight() {
        let p = parts
            .get_mut(x)
            .with_context(|| format!("election {}: no member {x} to reweight", spec.n))?;
        p.1 = u128::from(reweighted(u64::try_from(p.1)?, spec.weights.1));
    }
    Ok(parts)
}

/// Every public file of the elections: metadata documents, census documents
/// and the CSP note. Nothing secret goes in.
pub fn public_files(specs: &[Spec], s: &Secrets) -> Result<Vec<PublicFile>> {
    let mut out = Vec::new();
    for spec in specs {
        out.push(PublicFile {
            path: spec.metadata_path(),
            body: metadata(spec)?,
        });
        if let MetaPlan::BeforeStart(r) | MetaPlan::WhileOpen(r) = spec.metadata {
            out.push(PublicFile {
                path: spec.metadata_update_path(),
                body: metadata_revised(spec, r)?,
            });
        }
        let census = |updated: bool| -> Result<PublicFile> {
            Ok(PublicFile {
                path: spec.census_path(updated).context("census path")?,
                body: census_json(&census_parts(spec, s, updated)?)?,
            })
        };
        match spec.census {
            CensusKind::Static => out.push(census(false)?),
            CensusKind::Updatable { .. } => {
                out.push(census(false)?);
                out.push(census(true)?);
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
    /// Steps of the run done for this election, by name, so a resumed run
    /// skips them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub done: Vec<String>,
    /// What the run saw on the way, for the report.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
    /// Votes sent to be refused, with the answers they got.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub refused: Vec<RefusedVote>,
}

/// A vote sent to be refused, and what the node answered.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RefusedVote {
    pub case: Refusal,
    pub node: usize,
    #[serde(with = "davinci_client::api::enc::vote_id")]
    pub vote_id: u64,
    /// HTTP status, 200 when the node took the vote.
    pub status: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<u32>,
    #[serde(default)]
    pub error: String,
}

impl RefusedVote {
    /// The status and code the case calls for.
    pub fn as_expected(&self) -> bool {
        let (status, code) = self.case.expected();
        self.status == status && self.code == Some(code)
    }
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

    /// `(voters, overwrites)` the registry must count for the settled
    /// votes: every voter with one, and every settled vote after a voter's
    /// first.
    pub fn counts(&self) -> (u64, u64) {
        let mut per_voter: BTreeMap<usize, u64> = BTreeMap::new();
        for v in self.votes.iter().filter(|v| v.state == VoteState::Settled) {
            *per_voter.entry(v.voter).or_default() += 1;
        }
        let voters = per_voter.len() as u64;
        (voters, per_voter.values().sum::<u64>() - voters)
    }

    pub fn is_done(&self, step: &str) -> bool {
        self.done.iter().any(|d| d == step)
    }

    pub fn mark(&mut self, step: &str) {
        if !self.is_done(step) {
            self.done.push(step.to_string());
        }
    }

    /// Adds `line` to the notes, once.
    pub fn note(&mut self, line: impl Into<String>) {
        let line = line.into();
        if !self.notes.contains(&line) {
            self.notes.push(line);
        }
    }

    /// The answer refused vote `r` got, once sent.
    pub fn refusal(&self, r: Refusal) -> Option<&RefusedVote> {
        self.refused.iter().find(|x| x.case == r)
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

    const WAVES: [Wave; 2] = [Wave::One, Wave::Two];

    fn secrets_of(wave: Wave) -> Secrets {
        Secrets::generate(&wave.elections(), &mut StdRng::seed_from_u64(7)).unwrap()
    }

    fn secrets() -> Secrets {
        secrets_of(Wave::One)
    }

    fn spec(wave: Wave, n: usize) -> Spec {
        wave.elections().into_iter().find(|s| s.n == n).unwrap()
    }

    /// What every election of every wave must satisfy.
    fn check_table(wave: Wave) {
        let specs = wave.elections();
        let dirs: BTreeSet<_> = specs.iter().map(|s| s.dir).collect();
        assert_eq!(dirs.len(), specs.len());
        let prefix = match wave {
            Wave::One => "",
            Wave::Two => "wave2/",
        };
        for (i, s) in specs.iter().enumerate() {
            let at = format!("wave {} election {}", wave.number(), s.n);
            assert_eq!(s.n, i + 1, "{at}");
            assert!(s.dir.starts_with(&format!("{prefix}{}-", s.n)), "{at}");
            assert_eq!(s.sdk, wave == Wave::Two, "{at}");
            let m = s.ballot_mode();
            assert_eq!(usize::from(m.num_fields), s.num_fields(), "{at}");
            assert_eq!(s.lean.len(), s.num_fields(), "{at}");
            assert_eq!(registry_check(&m, s.max_voters), Ok(()), "{at}");
            m.pack().unwrap();
            assert!(s.weights.0 >= 1 && s.weights.0 <= s.weights.1, "{at}");
            for l in s.i18n {
                assert_eq!(l.choices.len(), s.num_fields(), "{at} {}", l.code);
            }
            // Rounds stay inside the census; each round's new voters are
            // new.
            assert!(s.round1 <= s.members, "{at}");
            assert!(s.round2.0 >= s.round1 && s.round2.0 <= s.round2.1, "{at}");
            assert!(s.round3.0 <= s.round3.1, "{at}");
            assert!(
                s.round3.0 >= s.round2.1 || s.round3.1 <= s.round1.min(s.round2.0),
                "{at}"
            );
            assert!(s.round2.1.max(s.round3.1) <= s.total_members(), "{at}");
            assert!(s.revotes <= s.round1 && s.same_node <= s.revotes, "{at}");
            assert!(s.revotes3 <= s.revotes, "{at}");
            let voters = s.round1 + s.round2.1 - s.round2.0 + s.round3.1 - s.round3.0;
            // Max voters binds at the end only when it is raised meanwhile.
            let cap = s
                .actions
                .iter()
                .find_map(|a| match a {
                    Action::MaxVoters(m) => Some(*m),
                    _ => None,
                })
                .unwrap_or(s.max_voters);
            assert!(voters as u64 <= cap, "{at}");
            assert!(s.round1 as u64 <= s.max_voters, "{at}");
            if s.chunk > 0 {
                assert!(s.round1 > 2 * s.chunk, "{at}");
            }
            if let Route::One(i) = s.route {
                assert!(i < 2, "{at}");
            }
            if let KeySource::Node(i) = s.key {
                assert!(i < 2, "{at}");
            }
            // Only elections that get tallied, stay open or are canceled
            // after a round take votes; round 3 comes after the others.
            match s.lifecycle {
                Lifecycle::Open { .. } => assert!(s.has_votes(), "{at}"),
                Lifecycle::Upcoming { .. } | Lifecycle::CanceledEarly { .. } => {
                    assert!(!s.has_votes(), "{at}")
                }
                Lifecycle::Later { start_in } => {
                    assert!(!s.votes_in(1) && !s.votes_in(2) && s.votes_in(3), "{at}");
                    assert!(start_in >= 10 * MINUTE, "{at}");
                }
                Lifecycle::Canceled => assert!(s.last_round() <= 1, "{at}"),
                Lifecycle::Timed { secs } => {
                    assert!((30 * MINUTE..=90 * MINUTE).contains(&secs), "{at}");
                    assert!(s.votes_in(1), "{at}");
                }
                Lifecycle::Tally => {}
            }
            // A growing census lets some of its new members vote.
            if s.added() > 0 {
                assert!(s.round2.1.max(s.round3.1) > s.members, "{at}");
            }
            // The reweighted member is an original one whose first ballot is
            // in round 2, and the weights leave room to change.
            if let Some(x) = s.reweight() {
                assert!(
                    x < s.members && (s.round2.0..s.round2.1).contains(&x),
                    "{at}"
                );
                assert!(s.weights.1 >= 2, "{at}");
            }
            // Pausing holds round-2 votes; the metadata changes while open
            // after a round.
            if s.pauses() {
                assert!(s.votes_in(1) && s.votes_in(2), "{at}");
            }
            if let MetaPlan::WhileOpen(r) | MetaPlan::BeforeStart(r) | MetaPlan::Mismatch(r) =
                s.metadata
            {
                assert_eq!(r.i18n.len(), s.i18n.len(), "{at}");
                assert_ne!(r.description, s.description, "{at}");
            }
            if let MetaPlan::WhileOpen(_) = s.metadata {
                assert!(s.votes_in(1), "{at}");
            }
            if let MetaPlan::BeforeStart(_) = s.metadata {
                assert!(matches!(s.lifecycle, Lifecycle::Later { .. }), "{at}");
            }
            for a in s.actions {
                match a {
                    Action::MaxVoters(m) => {
                        assert!(*m > s.max_voters && s.round1 as u64 == s.max_voters, "{at}")
                    }
                    Action::Extend(_) | Action::Shorten(_) => {
                        assert!(matches!(s.lifecycle, Lifecycle::Timed { .. }), "{at}")
                    }
                    Action::Refuse(Refusal::OverMaxVoters) => {
                        assert_eq!(s.round1 as u64, s.max_voters, "{at}");
                        assert_eq!(s.round2.0, s.round2.1, "{at}");
                    }
                    Action::Refuse(Refusal::AfterEnd) => assert!(s.tallied(), "{at}"),
                    Action::Refuse(Refusal::BreaksRules) => {
                        assert!(breaking_ballot(s).is_some(), "{at}")
                    }
                    Action::Refuse(Refusal::ReusedVoteId) => assert!(s.votes_in(1), "{at}"),
                    Action::Refuse(Refusal::NotInCensus) => {
                        assert_ne!(s.census, CensusKind::Csp, "{at}")
                    }
                    _ => {}
                }
            }
            if s.dkg() && wave == Wave::One {
                assert_eq!(s.lifecycle, Lifecycle::Tally, "{at}");
            }
        }
        let origins: BTreeSet<_> = specs.iter().map(Spec::origin).collect();
        assert_eq!(origins, BTreeSet::from([1, 2, 3, 4]));
        assert!(specs.iter().any(|s| s.key == KeySource::DkgAutomatic));
        assert!(specs.iter().any(|s| s.key == KeySource::DkgLocked));
        assert!(specs.iter().any(|s| s.key == KeySource::Node(0)));
        assert!(specs.iter().any(|s| s.key == KeySource::Node(1)));
    }

    #[test]
    fn election_tables_are_consistent() {
        for w in WAVES {
            check_table(w);
        }
        assert_eq!(Wave::One.elections().len(), 8);
        assert!((18..=22).contains(&Wave::Two.elections().len()));
    }

    #[test]
    fn waves_are_selected_and_kept_apart() {
        assert_eq!(Wave::parse("").unwrap(), Wave::One);
        assert_eq!(Wave::parse("1").unwrap(), Wave::One);
        assert_eq!(Wave::parse(" 2 ").unwrap(), Wave::Two);
        assert!(Wave::parse("3").is_err());
        assert_ne!(Wave::One.secrets_file(), Wave::Two.secrets_file());
        assert_ne!(Wave::One.state_file(), Wave::Two.state_file());
        assert_eq!(Wave::One.secrets_file(), SECRETS_FILE);
        assert_eq!(Wave::One.state_file(), STATE_FILE);
    }

    /// The second wave covers every case it exists for.
    #[test]
    fn second_wave_covers_every_case() {
        let specs = Wave::Two.elections();
        let any = |f: &dyn Fn(&Spec) -> bool| specs.iter().any(f);
        let has = |b: BallotKind| any(&|s| s.ballot == b);
        let nf = |b: BallotKind, n: usize| any(&|s| s.ballot == b && s.num_fields() == n);
        assert!(has(BallotKind::SingleChoice { abstain: true }));
        assert!(nf(BallotKind::MultipleChoice { min: 2, max: 2 }, 5));
        assert!(nf(BallotKind::MultipleChoice { min: 1, max: 3 }, 6));
        assert!(nf(BallotKind::RatingFrom { min: 1, max: 5 }, 4));
        assert!(nf(
            BallotKind::Budget {
                total: 100,
                cap: 50
            },
            5
        ));
        assert!(any(&|s| matches!(
            s.ballot,
            BallotKind::QuadraticBudget { min_spend, .. } if min_spend > 0
        )));
        assert!(nf(BallotKind::Ranking, 5));
        assert!(has(BallotKind::Weighted));
        assert!(any(&|s| matches!(
            s.ballot,
            BallotKind::CostExponent { exp: 3, .. }
        )));
        assert!(nf(BallotKind::Numeric { max: 100 }, 1));
        assert!(nf(BallotKind::ApproveAny, 16));
        // Censuses: an updatable one with a reweight and new members, a
        // contract that grows, CSP with weights.
        assert!(any(&|s| s.census == CensusKind::Static));
        assert!(any(&|s| matches!(
            s.census,
            CensusKind::Updatable { added, reweight: Some(_) } if added > 0
        )));
        assert!(any(
            &|s| matches!(s.census, CensusKind::Contract { added } if added > 0)
        ));
        assert!(any(
            &|s| s.census == CensusKind::Csp && s.weights.0 < s.weights.1
        ));
        // Keys.
        assert!(any(&|s| s.key == KeySource::DkgLockedEarly));
        assert!(any(&|s| s.key == KeySource::DkgLocked && s.tallied()));
        // Lifecycles and organizer actions.
        let timed = |f: &dyn Fn(&Spec) -> bool| {
            any(&|s| matches!(s.lifecycle, Lifecycle::Timed { .. }) && f(s))
        };
        assert!(timed(&|s| matches!(s.key, KeySource::Node(_))));
        assert!(timed(&|s| s.dkg()));
        assert!(any(&|s| s.lifecycle == Lifecycle::Tally && s.has_votes()));
        assert!(any(&|s| s.pauses()));
        assert!(any(&|s| s
            .actions
            .iter()
            .any(|a| matches!(a, Action::Extend(_)))));
        assert!(any(&|s| s
            .actions
            .iter()
            .any(|a| matches!(a, Action::Shorten(_)))));
        assert!(any(&|s| s
            .actions
            .iter()
            .any(|a| matches!(a, Action::MaxVoters(_)))));
        assert!(any(&|s| s.lifecycle == Lifecycle::Canceled && s.votes_in(1)));
        assert!(any(&|s| matches!(
            s.lifecycle,
            Lifecycle::CanceledEarly { .. }
        )));
        assert!(any(&|s| matches!(s.lifecycle, Lifecycle::Later { .. })));
        assert!(any(&|s| matches!(s.lifecycle, Lifecycle::Upcoming { .. })));
        assert!(any(&|s| matches!(s.metadata, MetaPlan::BeforeStart(_))));
        assert!(any(&|s| matches!(s.metadata, MetaPlan::WhileOpen(_))));
        assert!(any(&|s| matches!(s.metadata, MetaPlan::Mismatch(_))));
        let zero = |f: &dyn Fn(&Spec) -> bool| {
            any(&|s| s.lifecycle == Lifecycle::Tally && !s.has_votes() && f(s))
        };
        assert!(zero(&|s| matches!(s.key, KeySource::Node(_))));
        assert!(zero(&|s| s.dkg()));
        // Votes: revotes through both nodes, a voter with three ballots, a
        // large election in chunks, and every refusal.
        assert!(any(&|s| s.same_node > 0 && s.same_node < s.revotes));
        assert!(any(&|s| s.revotes3 > 0));
        assert!(any(&|s| s.round1 + s.round2.1 - s.round2.0 >= 350
            && s.revotes >= 50
            && s.chunk > 0));
        let refusals: BTreeSet<Refusal> = specs.iter().flat_map(Spec::refusals).collect();
        assert_eq!(refusals.len(), 6);
        // Most are in three languages.
        let multilingual = specs.iter().filter(|s| s.i18n.len() == 2).count();
        assert!(multilingual * 3 >= specs.len() * 2, "{multilingual}");
        for s in &specs {
            let codes: Vec<_> = s.i18n.iter().map(|l| l.code).collect();
            assert!(
                codes.is_empty() || codes == ["es", "ca"],
                "election {}",
                s.n
            );
        }
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

    /// Mode tuple `(min, max, unique, exponent, minSum, maxSum)`.
    fn tuple(m: &BallotMode) -> (u64, u64, bool, u8, u64, u64) {
        (
            m.min_value,
            m.max_value,
            m.unique_values,
            m.cost_exponent,
            m.min_value_sum,
            m.max_value_sum,
        )
    }

    /// The second wave's modes are the ones davinci-sdk's
    /// `resolveElectionPreset` builds, and the metadata names the preset.
    #[test]
    fn second_wave_follows_the_sdk() {
        let at = |n| spec(Wave::Two, n);
        for s in Wave::Two.elections() {
            assert_eq!(s.ballot_mode().group_size as usize, s.num_fields());
        }
        assert_eq!(tuple(&at(2).ballot_mode()), (0, 1, false, 1, 0, 1));
        assert_eq!(tuple(&at(3).ballot_mode()), (0, 1, false, 1, 2, 2));
        assert_eq!(tuple(&at(4).ballot_mode()), (0, 1, false, 1, 1, 3));
        assert_eq!(tuple(&at(1).ballot_mode()), (0, 1, false, 1, 0, 16));
        assert_eq!(tuple(&at(5).ballot_mode()), (1, 5, false, 1, 4, 20));
        assert_eq!(tuple(&at(8).ballot_mode()), (1, 5, true, 1, 15, 15));
        assert_eq!(tuple(&at(7).ballot_mode()), (0, 100, false, 2, 50, 100));
        assert_eq!(tuple(&at(6).ballot_mode()), (0, 50, false, 1, 0, 100));
        assert_eq!(tuple(&at(10).ballot_mode()), (0, 5, false, 3, 0, 0));
        assert_eq!(tuple(&at(11).ballot_mode()), (0, 100, false, 1, 0, 100));
        assert_eq!(tuple(&at(9).ballot_mode()), (0, 400, false, 1, 0, 0));
        let preset = |n| {
            let j: serde_json::Value = serde_json::from_slice(&metadata(&at(n)).unwrap()).unwrap();
            j["meta"]["electionPreset"].clone()
        };
        assert_eq!(
            preset(2),
            serde_json::json!({"type": "single_choice", "allowAbstain": true})
        );
        assert_eq!(preset(12), serde_json::json!({"type": "single_choice"}));
        assert_eq!(
            preset(3),
            serde_json::json!({"type": "multiple_choice", "maxSelections": 2, "minSelections": 2})
        );
        assert_eq!(preset(1), serde_json::json!({"type": "approval"}));
        assert_eq!(
            preset(5),
            serde_json::json!({"type": "rating", "maxValue": 5, "minValue": 1})
        );
        assert_eq!(preset(8), serde_json::json!({"type": "ranking"}));
        assert_eq!(
            preset(7),
            serde_json::json!({"type": "quadratic", "budget": 100, "minValueSum": 50})
        );
        // Not presets: no claim.
        for n in [6, 9, 10, 11] {
            assert!(preset(n).is_null(), "election {n}");
        }
        assert!(elections().iter().all(|s| {
            !String::from_utf8(metadata(s).unwrap())
                .unwrap()
                .contains("electionPreset")
        }));
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

        // The second wave's rules.
        let at = |n| spec(Wave::Two, n).ballot_mode();
        let abstain = at(2);
        assert!(fits(&abstain, 1, &[0, 0, 0, 0]));
        assert!(fits(&abstain, 1, &[0, 0, 1, 0]));
        assert!(!fits(&abstain, 1, &[1, 0, 1, 0]));
        let exactly_two = at(3);
        assert!(fits(&exactly_two, 1, &[1, 0, 0, 1, 0]));
        assert!(!fits(&exactly_two, 1, &[1, 0, 0, 0, 0]));
        assert!(!fits(&exactly_two, 1, &[1, 1, 0, 1, 0]));
        let rating = at(5);
        assert!(fits(&rating, 1, &[1, 5, 3, 2]));
        assert!(!fits(&rating, 1, &[0, 5, 3, 2]));
        let points = at(6);
        assert!(fits(&points, 1, &[50, 50, 0, 0, 0]));
        assert!(!fits(&points, 1, &[55, 45, 0, 0, 0]));
        assert!(!fits(&points, 1, &[50, 40, 20, 0, 0]));
        let credits = at(7);
        assert!(fits(&credits, 1, &[5, 5, 0, 0, 0]));
        assert!(
            !fits(&credits, 1, &[5, 4, 0, 0, 0]),
            "41 is under the floor"
        );
        assert!(
            !fits(&credits, 1, &[10, 1, 0, 0, 0]),
            "101 is over the budget"
        );
        let cubes = at(10);
        assert!(fits(&cubes, 36, &[3, 2, 1, 0, 0]));
        assert!(!fits(&cubes, 35, &[3, 2, 1, 0, 0]));
        assert!(!fits(&cubes, 1000, &[6, 0, 0, 0, 0]));
        let number = at(11);
        assert!(fits(&number, 1, &[100]) && fits(&number, 1, &[0]));
        assert!(!fits(&number, 1, &[101]));
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
        for s in WAVES.iter().flat_map(|w| w.elections()) {
            let m = s.ballot_mode();
            for _ in 0..500 {
                let w = rng.gen_range(s.weights.0..=s.weights.1);
                // A reweighted member's new weight too.
                for w in [w, reweighted(w, s.weights.1)] {
                    let c = choose(s.ballot, s.lean, w, &mut rng);
                    assert!(fits(&m, u128::from(w), &c), "{}: {c:?} w={w}", s.dir);
                    let r = revote(s.ballot, s.lean, w, &c, &mut rng);
                    assert_ne!(r, c, "{}", s.dir);
                    assert!(fits(&m, u128::from(w), &r), "{}: {r:?} w={w}", s.dir);
                }
            }
        }
    }

    #[test]
    fn choices_are_varied() {
        let mut rng = StdRng::seed_from_u64(2);
        for s in WAVES.iter().flat_map(|w| w.elections()) {
            let w = s.weights.1;
            let ballots: Vec<Vec<u64>> = (0..200)
                .map(|_| choose(s.ballot, s.lean, w, &mut rng))
                .collect();
            let distinct: BTreeSet<_> = ballots.iter().collect();
            assert!(distinct.len() >= s.num_fields(), "{}", s.dir);
            // Totals follow the lean, not a flat split.
            let mut t = vec![0u64; s.num_fields()];
            for b in &ballots {
                for (o, x) in t.iter_mut().zip(b) {
                    *o += x;
                }
            }
            if s.lean.iter().any(|l| *l != s.lean[0]) {
                assert!(t.iter().any(|x| *x != t[0]), "{}: {t:?}", s.dir);
            }
        }
        // Quadratic voters spend most of their credits.
        let s = &elections()[2];
        for w in [25, 60, 100] {
            let c = choose(s.ballot, s.lean, w, &mut rng);
            let cost: u64 = c.iter().map(|v| v * v).sum();
            assert!(cost > 0 && cost <= w);
        }
        // Some blank ballots where abstaining is allowed, none elsewhere.
        let mut blank = |n| {
            let s = spec(Wave::Two, n);
            (0..600)
                .filter(|_| {
                    choose(s.ballot, s.lean, 1, &mut rng)
                        .iter()
                        .all(|x| *x == 0)
                })
                .count()
        };
        let (abstain, strict) = (blank(2), blank(12));
        assert!((20..=100).contains(&abstain), "{abstain}");
        assert_eq!(strict, 0);
        // The fee answers spread around the lean.
        let s = spec(Wave::Two, 11);
        let fees: Vec<u64> = (0..400)
            .map(|_| choose(s.ballot, s.lean, 1, &mut rng)[0])
            .collect();
        let mean = fees.iter().sum::<u64>() as f64 / fees.len() as f64;
        assert!((34.0..=50.0).contains(&mean), "{mean}");
        assert!(fees.iter().any(|f| *f < 30) && fees.iter().any(|f| *f > 55));
    }

    #[test]
    fn plan_follows_the_table() {
        for wave in WAVES {
            let sec = secrets_of(wave);
            let seed = sec.seed().unwrap();
            for s in wave.elections() {
                let at = s.dir;
                let w = sec.weights(s.n).unwrap();
                let p = plan(&s, &w, &seed, 2);
                assert_eq!(p, plan(&s, &w, &seed, 2), "deterministic");
                let round = |r: u8| p.iter().filter(move |v| v.round == r);
                assert_eq!(round(1).count(), s.round1, "{at}");
                assert_eq!(
                    round(2).count(),
                    s.round2.1 - s.round2.0 + s.revotes,
                    "{at}"
                );
                assert_eq!(
                    round(3).count(),
                    s.round3.1 - s.round3.0 + s.revotes3 + usize::from(s.reweight().is_some()),
                    "{at}"
                );
                // One ballot per voter and round.
                let keys: BTreeSet<_> = p.iter().map(|v| (v.round, v.voter)).collect();
                assert_eq!(keys.len(), p.len(), "{at}");
                // A later ballot of a voter changes something and, for a
                // revote, goes through the other node unless it is one of
                // the first `same_node`.
                let mut same = 0;
                for v in round(2).chain(round(3)) {
                    let prev = p.iter().rfind(|x| x.voter == v.voter && x.round < v.round);
                    match prev {
                        Some(f) => {
                            assert_ne!(v.fields, f.fields, "{at}: a revote changes the ballot");
                            if v.round == 2 {
                                same += usize::from(v.node == f.node);
                            } else {
                                assert_ne!(v.node, f.node, "{at}");
                            }
                        }
                        None if s.reweight() == Some(v.voter) => {}
                        None => {
                            let range = if v.round == 2 { s.round2 } else { s.round3 };
                            assert!((range.0..range.1).contains(&v.voter), "{at}");
                        }
                    }
                }
                assert_eq!(same, s.same_node, "{at}");
                // Three ballots from `revotes3` voters.
                let thrice = (0..s.total_members())
                    .filter(|m| p.iter().filter(|v| v.voter == *m).count() == 3)
                    .count();
                assert_eq!(thrice, s.revotes3, "{at}");
                for v in &p {
                    assert_eq!(v.weight, s.weight_at(&w, v.voter, v.round), "{at}");
                    assert!(
                        fits(&s.ballot_mode(), u128::from(v.weight), &v.fields),
                        "{at}"
                    );
                }
                if let Route::One(i) = s.route {
                    assert!(round(1).all(|v| v.node == i), "{at}");
                } else if p.len() > 4 {
                    let nodes: BTreeSet<_> = p.iter().map(|v| v.node).collect();
                    assert_eq!(nodes.len(), 2, "{at}: votes spread over both nodes");
                }
                // The members who cast refused votes vote in no round.
                for r in s.refusals() {
                    if let Some(m) = s.refusal_member(r) {
                        assert!(p.iter().all(|v| v.voter != m), "{at}: {r:?}");
                        assert!(m >= s.round2.1.max(s.round3.1), "{at}");
                    } else {
                        assert!(!r.needs_member(), "{at}");
                    }
                }
            }
        }
        // Another seed, another plan.
        let sec = secrets();
        let seed = sec.seed().unwrap();
        let other = [9u8; 32];
        let s = &elections()[0];
        let w = sec.weights(1).unwrap();
        assert_ne!(plan(s, &w, &seed, 2), plan(s, &w, &other, 2));
        // One node takes every vote, revotes included.
        assert!(plan(s, &w, &seed, 1).iter().all(|v| v.node == 0));
    }

    /// The first wave's plans, ballot modes, timings and public files are
    /// what they were before the second wave: its elections run on them.
    #[test]
    fn first_wave_is_unchanged() {
        use std::fmt::Write as _;
        let sec = secrets();
        let seed = sec.seed().unwrap();
        let mut all = String::new();
        for s in elections() {
            let w = sec.weights(s.n).unwrap();
            for v in plan(&s, &w, &seed, 2) {
                write!(
                    all,
                    "{} {} {} {} {:?};",
                    s.n, v.round, v.voter, v.node, v.fields
                )
                .unwrap();
            }
            let m = s.ballot_mode();
            write!(
                all,
                "{} {} {} {} {} {} {} {};",
                m.num_fields,
                m.group_size,
                m.unique_values,
                m.cost_exponent,
                m.max_value,
                m.min_value,
                m.max_value_sum,
                m.min_value_sum
            )
            .unwrap();
            write!(all, "{:?};", s.timing(1_000_000)).unwrap();
        }
        for f in public_files(&elections(), &sec).unwrap() {
            write!(all, "{} {};", f.path, hex::encode(keccak256(&f.body))).unwrap();
        }
        assert_eq!(
            hex::encode(keccak256(all.as_bytes())),
            "184370f5b7549312e1e345f19454c05ee7e53e05879e8cd50754be95b6d18b72"
        );
    }

    #[test]
    fn refusals_have_their_voters_and_ballots() {
        let s = spec(Wave::Two, 2);
        assert_eq!(s.refusal_member(Refusal::BadSignature), Some(29));
        assert_eq!(s.refusal_member(Refusal::AfterEnd), Some(28));
        assert_eq!(s.refusal_member(Refusal::NotInCensus), None);
        assert_eq!(s.refusal_member(Refusal::ReusedVoteId), None);
        assert_eq!(s.refusal_member(Refusal::OverMaxVoters), None);
        let s = spec(Wave::Two, 3);
        let (fields, looser) = breaking_ballot(&s).unwrap();
        assert_eq!(fields, [1, 1, 1, 0, 0]);
        assert!(!fits(&s.ballot_mode(), 1, &fields));
        assert!(fits(&looser, 1, &fields));
        assert_ne!(looser, s.ballot_mode());
        assert!(
            breaking_ballot(&spec(Wave::Two, 1)).is_none(),
            "any number fits"
        );
        assert!(breaking_ballot(&spec(Wave::Two, 11)).is_none());
        assert_eq!(Refusal::NotInCensus.expected(), (400, 40001));
        assert_eq!(Refusal::BadSignature.expected(), (400, 40002));
        assert_eq!(Refusal::BreaksRules.expected(), (400, 40002));
        assert_eq!(Refusal::ReusedVoteId.expected(), (409, 40901));
        assert_eq!(Refusal::OverMaxVoters.expected(), (412, 41202));
        assert_eq!(Refusal::AfterEnd.expected(), (412, 41201));
        // Their ballot secrets are no round's.
        let seed = [1u8; 32];
        let k = refusal_k(&seed, 2, 29, Refusal::BadSignature);
        for round in 1..=3 {
            assert_ne!(k, vote_k(&seed, 2, 29, round));
        }
        assert_ne!(k, refusal_k(&seed, 2, 29, Refusal::AfterEnd));
        assert_eq!(outsider(&seed, 2).to_bytes(), outsider(&seed, 2).to_bytes());
        assert_ne!(outsider(&seed, 2).to_bytes(), outsider(&seed, 3).to_bytes());
    }

    #[test]
    fn a_reweight_changes_one_leaf() {
        let s = spec(Wave::Two, 10);
        let x = s.reweight().unwrap();
        let sec = secrets_of(Wave::Two);
        let before = census_parts(&s, &sec, false).unwrap();
        let after = census_parts(&s, &sec, true).unwrap();
        assert_eq!(before.len(), s.members);
        assert_eq!(after.len(), s.total_members());
        for (i, (b, a)) in before.iter().zip(&after).enumerate() {
            assert_eq!(b.0, a.0);
            assert_eq!(b.1 == a.1, i != x, "member {i}");
        }
        let base = sec.weights(s.n).unwrap()[x];
        assert_eq!(after[x].1, u128::from(reweighted(base, s.weights.1)));
        assert_eq!(s.weight_at(&sec.weights(s.n).unwrap(), x, 2), base);
        assert_eq!(
            s.weight_at(&sec.weights(s.n).unwrap(), x, 3),
            reweighted(base, s.weights.1)
        );
        for w in 1..=300 {
            let r = reweighted(w, 150);
            assert_ne!(r, w);
            assert!(r >= 1 && (r <= 150 || w > 150));
        }
        // The member's round-2 ballot goes in under the first census, every
        // other ballot after round 1 under the updated one.
        let v = |round, voter| Planned {
            round,
            voter,
            node: 0,
            weight: 1,
            fields: vec![],
        };
        assert!(!under_updated_census(&s, &v(1, 3)));
        assert!(!under_updated_census(&s, &v(2, x)));
        assert!(under_updated_census(&s, &v(2, x + 1)));
        assert!(under_updated_census(&s, &v(3, x)));
        assert!(!under_updated_census(&spec(Wave::Two, 2), &v(2, 20)));
    }

    /// The large election's first chunk fits one transition, and every
    /// later one is more than two blobs hold, so the node splits it.
    #[test]
    fn the_large_election_spans_blobs() {
        use davinci_zkvm_sdk::blob::{blob_count, max_votes_for_cap};
        let s = Wave::Two
            .elections()
            .into_iter()
            .find(|s| s.chunk > 0)
            .unwrap();
        let nf = s.num_fields() as u8;
        assert_eq!(nf, 16);
        assert_eq!(blob_count(s.chunk, s.chunk, nf), 2);
        let steady = max_votes_for_cap(nf, 0, s.chunk, 2);
        assert!(steady < s.chunk, "{steady}");
        assert!(steady > s.chunk / 2);
        // The revotes come with refreshes too.
        assert!(s.revotes >= 50 && s.round1 / s.chunk >= 3);
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
        // A secrets file of one wave does not fit the other.
        assert!(secrets_of(Wave::Two).check(&specs).is_err());
        assert!(secrets().check(&Wave::Two.elections()).is_err());
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
        for wave in WAVES {
            let specs = wave.elections();
            let sec = secrets_of(wave);
            let files = public_files(&specs, &sec).unwrap();
            let seed = sec.seed().unwrap();
            let mut secret_hex = vec![sec.seed.0.clone(), sec.csp_key.0.clone()];
            for m in sec.elections.values().flatten() {
                secret_hex.push(m.key.0.clone());
            }
            for s in &specs {
                secret_hex.push(hex::encode(outsider(&seed, s.n).to_bytes()));
            }
            for f in &files {
                let body = String::from_utf8(f.body.clone()).unwrap().to_lowercase();
                for s in &secret_hex {
                    assert!(!body.contains(&s[..16]), "{} leaks a secret", f.path);
                }
                assert!(f.body.ends_with(b"}\n"));
            }
            let paths: BTreeSet<_> = files.iter().map(|f| f.path.as_str()).collect();
            assert_eq!(paths.len(), files.len());
            // Every path is under its election's directory.
            for f in &files {
                assert!(
                    specs
                        .iter()
                        .any(|s| f.path.starts_with(&format!("{}/", s.dir)))
                );
            }
        }
        let files = public_files(&elections(), &secrets()).unwrap();
        let paths: BTreeSet<_> = files.iter().map(|f| f.path.as_str()).collect();
        assert!(paths.contains("3-budget-2027/census-2.json"));
        assert!(paths.contains("6-shareholder-resolution/csp.json"));
        assert!(
            !paths
                .iter()
                .any(|p| p.starts_with("5-") && p.contains("census"))
        );
        let files = public_files(&Wave::Two.elections(), &secrets_of(Wave::Two)).unwrap();
        let paths: BTreeSet<_> = files.iter().map(|f| f.path.as_str()).collect();
        assert!(paths.contains("wave2/10-irrigation-works/census-2.json"));
        assert!(paths.contains("wave2/11-membership-fee/metadata-2.json"));
        assert!(paths.contains("wave2/15-community-kitchen/metadata-2.json"));
        assert!(paths.contains("wave2/9-water-tariff/csp.json"));
        // The draft behind a mismatch is never served.
        assert!(!paths.contains("wave2/17-watering-schedule/metadata-2.json"));
        assert!(
            !paths
                .iter()
                .any(|p| p.starts_with("wave2/7-") && p.contains("census"))
        );
    }

    #[test]
    fn census_files_match_their_roots() {
        for wave in WAVES {
            let specs = wave.elections();
            let sec = secrets_of(wave);
            let files = public_files(&specs, &sec).unwrap();
            let body = |p: &str| {
                files
                    .iter()
                    .find(|f| f.path == p)
                    .map(|f| f.body.clone())
                    .unwrap()
            };
            for s in &specs {
                let versions: &[bool] = match s.census {
                    CensusKind::Static => &[false],
                    CensusKind::Updatable { .. } => &[false, true],
                    _ => continue,
                };
                for &updated in versions {
                    let raw = body(&s.census_path(updated).unwrap());
                    let file: CensusFile = serde_json::from_slice(&raw).unwrap();
                    let parts = census_parts(s, &sec, updated).unwrap();
                    assert_eq!(file.participants.len(), parts.len());
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
        }
        // The updated census keeps the first members as they were.
        let sec = secrets();
        let s = &elections()[2];
        let before = census_tree(&census_parts(s, &sec, false).unwrap()).unwrap();
        let after = census_tree(&census_parts(s, &sec, true).unwrap()).unwrap();
        assert_ne!(before.root(), after.root());
        for i in 0..s.members {
            assert_eq!(before.proof(i).unwrap().leaf, after.proof(i).unwrap().leaf);
        }
        let files = public_files(&elections(), &sec).unwrap();
        let csp = files
            .iter()
            .find(|f| f.path == "6-shareholder-resolution/csp.json")
            .unwrap();
        let csp: serde_json::Value = serde_json::from_slice(&csp.body).unwrap();
        assert_eq!(
            csp["signer"],
            format!(
                "0x{}",
                hex::encode(eth_address(sec.csp().unwrap().verifying_key()))
            )
        );
    }

    /// `v["default"]` and, for a multilingual election, `en` equal to it and
    /// every other language present and different from English.
    fn check_text(v: &serde_json::Value, en: &str, others: &[&str], at: &str) {
        assert_eq!(v["default"], en, "{at}");
        let o = v.as_object().unwrap();
        if others.is_empty() {
            assert_eq!(o.len(), 1, "{at}");
            return;
        }
        assert_eq!(v["en"], en, "{at}");
        let keys: Vec<_> = o.keys().map(String::as_str).collect();
        assert_eq!(keys.len(), 2 + others.len(), "{at}");
        for (i, code) in ["es", "ca"].iter().enumerate().take(others.len()) {
            assert_eq!(v[*code], others[i], "{at} {code}");
        }
    }

    #[test]
    fn metadata_lines_up_with_the_fields() {
        for s in WAVES.iter().flat_map(|w| w.elections()) {
            let at = s.dir;
            let raw = metadata(&s).unwrap();
            let j: serde_json::Value = serde_json::from_slice(&raw).unwrap();
            assert_eq!(j["version"], "1.1");
            let tr = |f: fn(&Lang) -> &'static str| s.i18n.iter().map(f).collect::<Vec<_>>();
            check_text(&j["title"], s.title, &tr(|l| l.title), at);
            check_text(&j["description"], s.description, &tr(|l| l.description), at);
            let q = j["questions"].as_array().unwrap();
            assert_eq!(q.len(), 1);
            check_text(&q[0]["title"], s.question, &tr(|l| l.question), at);
            check_text(
                &q[0]["description"],
                s.question_description,
                &tr(|l| l.question_description),
                at,
            );
            let c = q[0]["choices"].as_array().unwrap();
            assert_eq!(c.len(), s.num_fields());
            for (i, c) in c.iter().enumerate() {
                assert_eq!(c["value"], i);
                let others: Vec<&str> = s.i18n.iter().map(|l| l.choices[i]).collect();
                check_text(&c["title"], s.choices[i], &others, at);
            }
            // No empty text, no placeholder words; only the mismatch says it
            // is a demonstration.
            for t in [s.title, s.description, s.question, s.question_description] {
                assert!(!t.trim().is_empty(), "{at}");
            }
            let text = String::from_utf8(raw).unwrap().to_lowercase();
            for word in ["test", "lorem", "ipsum"] {
                assert!(!text.contains(word), "{at} says {word}");
            }
            let mismatch = matches!(s.metadata, MetaPlan::Mismatch(_));
            assert_eq!(text.contains("demo"), mismatch, "{at}");
            // Spanish and Catalan differ from English and from each other.
            for l in s.i18n {
                assert_ne!(l.title, s.title, "{at} {}", l.code);
                assert_ne!(l.description, s.description, "{at} {}", l.code);
            }
            if let [es, ca] = s.i18n {
                assert_ne!(es.description, ca.description, "{at}");
            }
        }
    }

    #[test]
    fn metadata_updates_and_the_mismatch() {
        for s in Wave::Two.elections() {
            let served = metadata(&s).unwrap();
            let hash = registered_hash(&s).unwrap();
            match s.metadata {
                MetaPlan::Mismatch(r) => {
                    assert_ne!(hash, metadata_hash(&served), "{}", s.dir);
                    assert_eq!(hash, metadata_hash(&metadata_revised(&s, r).unwrap()));
                    assert!(metadata_update(&s).unwrap().is_none());
                }
                MetaPlan::BeforeStart(r) | MetaPlan::WhileOpen(r) => {
                    assert_eq!(hash, metadata_hash(&served));
                    let (path, h2) = metadata_update(&s).unwrap().unwrap();
                    assert_eq!(path, s.metadata_update_path());
                    let revised = metadata_revised(&s, r).unwrap();
                    assert_eq!(h2, metadata_hash(&revised));
                    assert_ne!(h2, hash);
                    // Only the description moves.
                    let a: serde_json::Value = serde_json::from_slice(&served).unwrap();
                    let b: serde_json::Value = serde_json::from_slice(&revised).unwrap();
                    assert_ne!(a["description"], b["description"]);
                    assert_eq!(a["title"], b["title"]);
                    assert_eq!(a["questions"], b["questions"]);
                    check_text(&b["description"], r.description, r.i18n, s.dir);
                }
                MetaPlan::Fixed => {
                    assert_eq!(hash, metadata_hash(&served));
                    assert!(metadata_update(&s).unwrap().is_none());
                }
            }
        }
    }

    /// The committed documents are the ones the code writes: a process
    /// registers the hash of these bytes.
    #[test]
    fn committed_metadata_is_what_the_code_writes() {
        for s in WAVES.iter().flat_map(|w| w.elections()) {
            let mut docs = vec![(s.metadata_path(), metadata(&s).unwrap())];
            if let MetaPlan::BeforeStart(r) | MetaPlan::WhileOpen(r) = s.metadata {
                docs.push((s.metadata_update_path(), metadata_revised(&s, r).unwrap()));
            }
            for (path, body) in docs {
                let on_disk = fs::read(public_dir().join(&path))
                    .unwrap_or_else(|e| panic!("{path}: {e}; run the prepare phase"));
                assert!(on_disk == body, "{path} differs from what prepare writes");
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
        let path = dir.join(SECRETS_FILE);
        let (a, fresh) = Secrets::load_or_generate(&path, &specs).unwrap();
        assert!(fresh);
        assert_eq!(mode(&path), 0o600);
        let (b, fresh) = Secrets::load_or_generate(&path, &specs).unwrap();
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
        assert!(Secrets::load_or_generate(&path, &more).is_err());
        // The second wave gets its own file next to the first.
        let two = dir.join(Wave::Two.secrets_file());
        let (c, fresh) = Secrets::load_or_generate(&two, &Wave::Two.elections()).unwrap();
        assert!(fresh && c.seed != a.seed);
        assert_eq!(Secrets::load(&path).unwrap(), a);
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
            weight: 1,
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
        assert_eq!(e.counts(), (2, 1));
        e.votes[4].state = VoteState::Sent;
        assert_eq!(e.expected_tally(2), [0, 2], "only settled votes count");
        // A third ballot of voter 0 and a voter whose first vote errored.
        e.record(&v(3, 0, vec![1, 0]), 1 << 63 | 11);
        e.record(&v(3, 2, vec![0, 1]), 1 << 63 | 12);
        e.votes[5].state = VoteState::Settled;
        e.votes[6].state = VoteState::Settled;
        assert_eq!(e.expected_tally(2), [1, 2]);
        assert_eq!(e.counts(), (3, 2));
        e.mark("pause");
        e.mark("pause");
        assert!(e.is_done("pause") && !e.is_done("resume"));
        assert_eq!(e.done.len(), 1);
        e.note("paused: 3 votes held");
        e.refused.push(RefusedVote {
            case: Refusal::OverMaxVoters,
            node: 1,
            vote_id: 1 << 63 | 13,
            status: 412,
            code: Some(41202),
            error: "max voters reached".into(),
        });
        assert!(e.refusal(Refusal::OverMaxVoters).unwrap().as_expected());
        assert!(e.refusal(Refusal::AfterEnd).is_none());
        let wrong = RefusedVote {
            case: Refusal::AfterEnd,
            status: 200,
            code: None,
            ..e.refused[0].clone()
        };
        assert!(!wrong.as_expected());
        st.save(&path).unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let back = State::load(&path).unwrap();
        assert_eq!(back, st);
        let raw = fs::read_to_string(&path).unwrap();
        assert!(raw.contains("\"voteId\": \"0x8000000000000005\""));
        assert!(raw.contains("\"case\": \"overMaxVoters\""));
        assert!(!format!("{back:?}").contains(&"09".repeat(32)));
        // A state from before these fields still loads.
        let old = r#"{"version": 1, "chainId": 100, "registry": "0xabc", "baseUrl": "https://x",
            "elections": {"1": {"pid": null, "censusUpdated": false, "revealed": false, "votes": []}}}"#;
        let old: State = serde_json::from_str(old).unwrap();
        assert!(old.elections[&1].done.is_empty() && old.elections[&1].refused.is_empty());
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
