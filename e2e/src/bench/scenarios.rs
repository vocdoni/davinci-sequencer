//! The election specs and scenarios of each wave (spec §8). Wave 1: the
//! realistic elections R1-R4 and the ceiling groups S-2, S-8, S-16 and S-c.
//! Wave 2: the races X1-X5.

use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::{Result, ensure};

use super::chaos::Chaos;
use super::plan::{ElectionSpec, NodeId};
use super::race::RaceSpec;
use super::rig::Knobs;
use super::schedule::{Gap, Phase, Schedule};
use super::submit::Route;

/// Ceiling cells, in ascending B: (B, bursts per election). Each S election
/// sends 4 bursts at 128, 4 at 256 and 3 at 512, 3072 votes at scale 1.
pub const S_CELLS: [(usize /* B */, usize /* bursts per election */); 3] =
    [(128, 4), (256, 4), (512, 3)];

/// S-c: (B, bursts per election), sent to both nodes.
pub const SC_CELL: (usize, usize) = (256, 3);

/// The OP registry's default grace window; R2 asks for more.
const GRACE: u32 = 300;

/// The fewest voters a scaled-down election keeps.
const MIN_VOTERS: u32 = 4;

/// ⌈scale·n⌉, at least 1: a burst of B, or an election's voters, scaled
/// down for `dry`.
pub fn scaled(n: usize, scale: f64) -> usize {
    ((scale * n as f64).ceil() as usize).max(1)
}

/// The election specs of wave `n` at `scale` (1 live, below 1 for `dry`).
/// Waves 2 and 3 come with their scenarios.
pub fn wave_specs(n: u32, scale: f64) -> Vec<ElectionSpec> {
    let mut v = match n {
        1 => wave1(scale),
        2 => wave2(scale),
        _ => Vec::new(),
    };
    // The key holder alternates, so both nodes publish results and each S
    // group puts two elections on each node.
    for (i, s) in v.iter_mut().enumerate() {
        s.key_node = if i % 2 == 0 { NodeId::A } else { NodeId::B };
    }
    v
}

#[allow(clippy::too_many_arguments)]
fn spec(
    tag: &str,
    scenario: &str,
    nf: u8,
    origin: u8,
    voters: u32,
    rounds: u8,
    revote_permille: u16,
    grace: u32,
) -> ElectionSpec {
    ElectionSpec {
        tag: tag.into(),
        scenario: scenario.into(),
        nf,
        origin,
        voters,
        rounds,
        revote_permille,
        key_node: NodeId::A,
        grace,
    }
}

fn wave1(scale: f64) -> Vec<ElectionSpec> {
    let mut v = Vec::new();
    // R: Merkle (1), CSP (4) and on-chain (3) censuses; half the revoters
    // overwrite and half revote, both as round 2.
    for (tag, nf, origin, voters, permille, grace) in [
        ("R1", 2, 1, 2000, 150, GRACE),
        ("R2", 8, 4, 1500, 100, 600),
        ("R3", 16, 1, 1500, 200, GRACE),
        ("R4", 16, 3, 128, 120, GRACE),
    ] {
        let voters = (scaled(voters, scale) as u32).max(MIN_VOTERS);
        v.push(spec(tag, "R", nf, origin, voters, 2, permille, grace));
    }
    let s_voters: usize = S_CELLS.iter().map(|&(b, k)| k * scaled(b, scale)).sum();
    for nf in [2u8, 8, 16] {
        for i in 1..=4 {
            let (tag, sc) = (format!("S{nf}-{i}"), format!("S-{nf}"));
            v.push(spec(&tag, &sc, nf, 1, s_voters as u32, 1, 0, GRACE));
        }
    }
    let (b, k) = SC_CELL;
    for i in 1..=2 {
        let voters = (k * scaled(b, scale)) as u32;
        v.push(spec(&format!("Sc-{i}"), "S-c", 16, 1, voters, 1, 0, GRACE));
    }
    v
}

/// Wave 2's races (spec §8, §9): fresh votes per round, and how many
/// races X1, X2 (each) and X3 collect.
const RACE_K: usize = 64;
const X1_ROUNDS: usize = 10;
const X23_ROUNDS: usize = 5;
/// X2's and X3's hold on the loser's `POST /prove`: longer than the
/// Ethereum leg plus the OP settlement minus the proof.
const SKEW: Duration = Duration::from_secs(150);
/// X3's loser runs this many OP blocks (about 5 min) behind on events.
const LAG: u64 = 150;
/// X4's two AGM ends are this far apart, so their flushes do not share a
/// node's Ethereum window.
const X4_STAGGER: u64 = 360;

/// K at `scale`: below 1 it shrinks with the elections, to at least 8.
fn race_k(scale: f64) -> usize {
    scaled(RACE_K, scale).max(8)
}

/// Race rounds at `scale`, at least 2.
fn race_rounds(n: usize, scale: f64) -> usize {
    scaled(n, scale).max(2)
}

fn wave2(scale: f64) -> Vec<ElectionSpec> {
    let k = race_k(scale);
    // Every round up to twice the target sends K fresh votes.
    let race = |tag: &str, rounds| {
        let voters = (2 * k * race_rounds(rounds, scale)) as u32;
        spec(tag, tag, 2, 1, voters, 1, 0, GRACE)
    };
    let mut v = vec![
        race("X1", X1_ROUNDS),
        race("X2-a", X23_ROUNDS),
        race("X2-b", X23_ROUNDS),
        race("X3", X23_ROUNDS),
    ];
    let x4 = (scaled(300, scale) as u32).max(MIN_VOTERS);
    for i in 1..=2 {
        v.push(spec(&format!("X4-{i}"), "X4", 16, 1, x4, 1, 0, GRACE));
    }
    // Every X5 voter revotes; a 600 s grace leaves the loser of the flush
    // room to land its re-attempt.
    let x5 = (scaled(200, scale) as u32).max(MIN_VOTERS);
    v.push(spec("X5", "X5", 8, 1, x5, 2, 1000, 600));
    v
}

/// The specs of the named scenarios, by wave; fails on a name no wave has.
pub fn scenario_specs(names: &[String], scale: f64) -> Result<Vec<(u32, Vec<ElectionSpec>)>> {
    let mut out = Vec::new();
    for n in 1..=3 {
        let specs: Vec<_> = wave_specs(n, scale)
            .into_iter()
            .filter(|s| names.contains(&s.scenario))
            .collect();
        if !specs.is_empty() {
            out.push((n, specs));
        }
    }
    for name in names {
        ensure!(
            out.iter()
                .any(|(_, v)| v.iter().any(|s| &s.scenario == name)),
            "no scenario {name:?} in waves 1 to 3"
        );
    }
    Ok(out)
}

/// How an election ends. `after` counts from its lane's start.
#[derive(Clone, Debug, PartialEq)]
pub enum End {
    /// The end moves to `notice` seconds ahead (`setProcessDuration`): the
    /// nodes flush during the notice.
    Shorten { after: Duration, notice: u64 },
    /// Ended at once (`endProcess`, an AGM end).
    Now { after: Duration },
    /// Ended once every vote it took has settled.
    AfterSettled,
}

/// One ceiling cell of an election: `bursts` bursts of ⌈scale·B⌉ votes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cell {
    pub b: usize,
    pub bursts: usize,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Kind {
    /// Each lane sends on its schedule.
    Plain,
    /// Each lane sends its cells in ascending B, a burst at a time, the next
    /// once the previous one has sealed.
    Ceiling(Vec<Cell>),
    /// One lane, sent in rounds of K fresh votes to both nodes, each once
    /// the previous round has landed and every node has synced it.
    Race(RaceSpec),
}

/// One election of a scenario. R's elections differ in rate, end and start,
/// and an S group routes each election to its own key node, so these are
/// per election rather than per scenario.
#[derive(Clone, Debug, PartialEq)]
pub struct Lane {
    pub tag: String,
    /// From the scenario's start.
    pub start: Duration,
    pub route: Route,
    /// Round-1 arrivals (a `Ceiling` lane has none).
    pub schedule: Schedule,
    /// Later rounds: the i-th revoter in voter order follows `gaps[i % len]`,
    /// so `[Overwrite, Revote]` sends half of them each way.
    pub gaps: Vec<Gap>,
    pub end: End,
}

#[derive(Clone, Debug)]
pub struct Scenario {
    pub name: String,
    pub wave: u32,
    pub lanes: Vec<Lane>,
    /// Of A and B.
    pub knobs: [Knobs; 2],
    /// Allowed error reasons per tag (substrings).
    pub allowed: BTreeMap<String, Vec<&'static str>>,
    pub chaos: Vec<Chaos>,
    pub kind: Kind,
}

impl Scenario {
    pub fn tags(&self) -> Vec<&str> {
        self.lanes.iter().map(|l| l.tag.as_str()).collect()
    }

    pub fn allowed(&self, tag: &str) -> &[&'static str] {
        self.allowed.get(tag).map_or(&[], Vec::as_slice)
    }
}

/// Scenarios that send past their election's end on purpose, so the 41201
/// refusals are measured: their schedule may hold fewer than their voters.
pub const SENDS_PAST_END: &[&str] = &["R2"];

/// The registry's least notice for a shortened end (OP: 60 s).
pub const NOTICE: u64 = 60;

/// The scenarios of wave `n` at `scale`, in run order. Waves 2 and 3 come
/// with their tasks.
pub fn scenarios(n: u32, scale: f64) -> Vec<Scenario> {
    let specs = wave_specs(n, scale);
    match n {
        1 => wave1_scenarios(&specs, scale),
        2 => wave2_scenarios(&specs, scale),
        _ => Vec::new(),
    }
}

/// Fees a spend estimate is quoted at (wei per gas): 0.1 gwei execution,
/// 0.004 gwei blob.
pub const EST_EXEC_WEI: u128 = 100_000_000;
pub const EST_BLOB_WEI: u128 = 4_000_000;

/// Ethereum posts of one node in a scenario: transactions and blobs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PostBound {
    pub txs: u64,
    pub blobs: u64,
}

/// A blob post's execution gas (the node's `BLOB_TX_GAS`).
const POST_GAS: u128 = 21_000;

impl PostBound {
    /// µETH at `exec` wei per gas and `blob` wei per blob gas.
    pub fn ueth(&self, exec: u128, blob: u128) -> f64 {
        let wei = u128::from(self.txs) * POST_GAS * exec
            + u128::from(self.blobs) * u128::from(crate::cost::GAS_PER_BLOB) * blob;
        wei as f64 / 1e12
    }

    /// `n` attempts at a batch of `votes` fresh votes at `nf` fields.
    fn add(&mut self, n: u64, votes: usize, nf: u8) {
        use davinci_zkvm_sdk::blob::blob_count;
        use davinci_zkvm_sdk::limits::{TX_BLOB_CAP, refresh_target};
        if votes == 0 {
            return;
        }
        let blobs = blob_count(votes, votes + refresh_target(votes, 0), nf) as u64;
        self.blobs += n * blobs;
        self.txs += n * blobs.div_ceil(TX_BLOB_CAP as u64);
    }
}

/// An upper bound of each node's (A, B) Ethereum posts in `sc`, for the
/// spend estimate. A race round costs each node one post, winner and loser
/// alike, for up to twice its target rounds; a ceiling burst one post per
/// node it went to. A plain lane seals at least once per timer period over
/// its sends, plus the end's flush, and any of those batches may lose a
/// race and post again.
pub fn post_bound(sc: &Scenario, specs: &[ElectionSpec], scale: f64) -> [PostBound; 2] {
    let mut out = [PostBound::default(); 2];
    for l in &sc.lanes {
        let Some(s) = specs.iter().find(|s| s.tag == l.tag) else {
            continue;
        };
        let on: Vec<usize> = match l.route {
            Route::Only(n) => vec![node_idx(n)],
            _ => vec![0, 1],
        };
        for i in on {
            let b = &mut out[i];
            match &sc.kind {
                Kind::Race(r) => b.add(2 * r.rounds as u64, r.k, s.nf),
                Kind::Ceiling(cells) => {
                    for c in cells {
                        b.add(c.bursts as u64, scaled(c.b, scale), s.nf);
                    }
                }
                Kind::Plain => {
                    let revoters =
                        (u64::from(s.voters) * u64::from(s.revote_permille)).div_ceil(1000);
                    let ballots = u64::from(s.voters) + revoters * u64::from(s.rounds.max(1) - 1);
                    // `ByVoter` and `Cross` split a lane between the nodes.
                    let mine = match l.route {
                        Route::Both | Route::Only(_) => ballots,
                        Route::ByVoter | Route::Cross => ballots.div_ceil(2),
                    };
                    let gap = l
                        .gaps
                        .iter()
                        .map(|g| match g {
                            Gap::Overwrite { max, .. } => *max,
                            Gap::Revote { secs } => *secs,
                        })
                        .max()
                        .unwrap_or(0);
                    let span = l
                        .schedule
                        .offsets(s.voters as usize, 0)
                        .last()
                        .map_or(0, Duration::as_secs)
                        + gap;
                    let knobs = &sc.knobs[i];
                    let timer = secs_of(&knobs.batch_time).unwrap_or(900).max(1);
                    let batches = mine
                        .div_ceil(knobs.batch_max.max(1) as u64)
                        .max(span / timer + 1);
                    let size = mine.div_ceil(batches) as usize;
                    b.add(2 * batches, size, s.nf);
                }
            }
        }
    }
    out
}

/// What every S group runs with: production's but `batch_max 512` and a
/// 60 s timer.
pub fn ceiling_knobs() -> Knobs {
    Knobs {
        batch_max: 512,
        batch_time: "60s".into(),
        ..Knobs::production()
    }
}

fn secs(s: u64) -> Duration {
    Duration::from_secs(s)
}

fn rate(per_min: f64, secs: u64, poisson: bool) -> Phase {
    Phase::Rate {
        per_min,
        secs,
        poisson,
    }
}

/// R's four lanes. Rates and counts are spec §8's; below scale 1 the
/// elections shrink, so the stagger (5 min) and the revote gap (15 min)
/// shrink with them (at least 30 s and 2 min) to keep the four overlapping.
fn r_lanes(specs: &[ElectionSpec], scale: f64) -> Vec<Lane> {
    let stagger = (300.0 * scale).max(30.0).round() as u64;
    let revote = (900.0 * scale).max(120.0).round() as u64;
    let gaps = vec![
        Gap::Overwrite { min: 5, max: 30 },
        Gap::Revote { secs: revote },
    ];
    let mut out = Vec::new();
    for (i, s) in specs.iter().filter(|s| s.scenario == "R").enumerate() {
        let v = s.voters as usize;
        let (schedule, end) = match s.tag.as_str() {
            // Poisson 60/min (one a second) with a burst in the middle; a
            // shortened end once the revotes are out.
            "R1" => {
                let burst = scaled(300, scale).min(v / 2);
                let rest = (v - burst) as u64;
                let a = rest / 2;
                let phases = vec![
                    rate(60.0, a, true),
                    Phase::Burst { n: burst },
                    rate(60.0, rest - a, true),
                ];
                let after = secs(rest + revote + 120);
                (
                    phases,
                    End::Shorten {
                        after,
                        notice: NOTICE,
                    },
                )
            }
            // Overload at 600/min, then 30/min; ended right after the
            // overload, so later sends meet 41201.
            "R2" => {
                let overload = v.div_ceil(10) as u64;
                let phases = vec![rate(600.0, overload, false), rate(30.0, 600, false)];
                (
                    phases,
                    End::Now {
                        after: secs(overload),
                    },
                )
            }
            // Poisson 40/min; an AGM end with batches in flight.
            "R3" => {
                let span = (3 * v).div_ceil(2) as u64;
                (
                    vec![rate(40.0, span, true)],
                    End::Now {
                        after: secs(span + revote + 30),
                    },
                )
            }
            // 10/min.
            _ => {
                let span = 6 * v as u64;
                let after = secs(span + revote + 120);
                (
                    vec![rate(10.0, span, false)],
                    End::Shorten {
                        after,
                        notice: NOTICE,
                    },
                )
            }
        };
        out.push(Lane {
            tag: s.tag.clone(),
            start: secs(i as u64 * stagger),
            route: Route::ByVoter,
            schedule: Schedule { phases: schedule },
            gaps: gaps.clone(),
            end,
        });
    }
    out
}

/// One `Ceiling` scenario over the specs of `name`.
fn ceiling(specs: &[ElectionSpec], name: &str, cells: &[(usize, usize)], both: bool) -> Scenario {
    let lanes = specs
        .iter()
        .filter(|s| s.scenario == name)
        .map(|s| Lane {
            tag: s.tag.clone(),
            start: Duration::ZERO,
            route: if both {
                Route::Both
            } else {
                Route::Only(s.key_node)
            },
            schedule: Schedule { phases: Vec::new() },
            gaps: Vec::new(),
            end: End::AfterSettled,
        })
        .collect();
    Scenario {
        name: name.into(),
        wave: 1,
        lanes,
        knobs: [ceiling_knobs(), ceiling_knobs()],
        allowed: BTreeMap::new(),
        chaos: Vec::new(),
        kind: Kind::Ceiling(
            cells
                .iter()
                .map(|&(b, bursts)| Cell { b, bursts })
                .collect(),
        ),
    }
}

fn wave1_scenarios(specs: &[ElectionSpec], scale: f64) -> Vec<Scenario> {
    let mut out = vec![Scenario {
        name: "R".into(),
        wave: 1,
        lanes: r_lanes(specs, scale),
        knobs: [Knobs::production(), Knobs::production()],
        allowed: ["R2", "R3"]
            .into_iter()
            .map(|t| (t.to_string(), vec!["process closed"]))
            .collect(),
        chaos: Vec::new(),
        kind: Kind::Plain,
    }];
    for nf in [2, 8, 16] {
        out.push(ceiling(specs, &format!("S-{nf}"), &S_CELLS, false));
    }
    out.push(ceiling(specs, "S-c", &[SC_CELL], true));
    out
}

/// Both nodes seal at K, never on the timer.
pub fn race_knobs(k: usize) -> Knobs {
    Knobs {
        batch_max: k,
        batch_time: "15m".into(),
        ..Knobs::production()
    }
}

fn other(n: NodeId) -> NodeId {
    match n {
        NodeId::A => NodeId::B,
        NodeId::B => NodeId::A,
    }
}

fn node_idx(n: NodeId) -> usize {
    usize::from(n == NodeId::B)
}

fn wave2_scenarios(specs: &[ElectionSpec], scale: f64) -> Vec<Scenario> {
    let k = race_k(scale);
    let spec = |t: &str| specs.iter().find(|s| s.tag == t);
    let lane = |tag: &str, start, route, schedule, gaps, end| Lane {
        tag: tag.into(),
        start: secs(start),
        route,
        schedule: Schedule { phases: schedule },
        gaps,
        end,
    };
    let race = |name: &str, rounds, skew: Option<NodeId>, lag: bool| {
        let r = RaceSpec {
            k,
            rounds: race_rounds(rounds, scale),
            skew: skew.map(|n| (n, SKEW)),
            lag_confirmations: lag.then_some(LAG),
        };
        let mut knobs = [race_knobs(k), race_knobs(k)];
        // The lag holds for the whole scenario rather than a restart per
        // round: a restart re-bootstraps every campaign election and zeroes
        // the node's counters.
        if let (Some(n), Some(c)) = (skew, r.lag_confirmations) {
            knobs[node_idx(n)].confirmations = Some(c);
        }
        Scenario {
            name: name.into(),
            wave: 2,
            lanes: vec![lane(
                name,
                0,
                Route::Both,
                Vec::new(),
                Vec::new(),
                End::AfterSettled,
            )],
            knobs,
            allowed: BTreeMap::new(),
            chaos: Vec::new(),
            kind: Kind::Race(r),
        }
    };
    // X3's lagging loser is not its key node, which would see the end and
    // publish the results 5 min late.
    let x3_loser = spec("X3").map_or(NodeId::A, |s| other(s.key_node));
    let mut out = vec![
        race("X1", X1_ROUNDS, None, false),
        race("X2-a", X23_ROUNDS, Some(NodeId::B), false),
        race("X2-b", X23_ROUNDS, Some(NodeId::A), false),
        race("X3", X23_ROUNDS, Some(x3_loser), true),
    ];
    // X4: 300 votes to both nodes at 2 a second, then an AGM end: both
    // flush the same queue on the same root inside the grace window.
    let mut x4 = Vec::new();
    for (i, t) in ["X4-1", "X4-2"].into_iter().enumerate() {
        let v = spec(t).map_or(0, |s| s.voters as u64);
        let span = v.div_ceil(2);
        x4.push(lane(
            t,
            i as u64 * X4_STAGGER,
            Route::Both,
            vec![rate(120.0, span, false)],
            Vec::new(),
            End::Now {
                after: secs(span + 30),
            },
        ));
    }
    out.push(Scenario {
        name: "X4".into(),
        wave: 2,
        lanes: x4,
        knobs: [Knobs::production(), Knobs::production()],
        allowed: ["X4-1", "X4-2"]
            .into_iter()
            .map(|t| (t.to_string(), vec!["process closed"]))
            .collect(),
        chaos: Vec::new(),
        kind: Kind::Plain,
    });
    // X5: round 1 to the voter's node and the revote 5 s later to the
    // other, then a shortened end: whichever flush lands last stands.
    let v = spec("X5").map_or(0, |s| s.voters as u64);
    out.push(Scenario {
        name: "X5".into(),
        wave: 2,
        lanes: vec![lane(
            "X5",
            0,
            Route::Cross,
            vec![rate(60.0, v, false)],
            vec![Gap::Overwrite { min: 5, max: 5 }],
            End::Shorten {
                after: secs(v + 5 + 30),
                notice: NOTICE,
            },
        )],
        knobs: [Knobs::production(), Knobs::production()],
        allowed: BTreeMap::new(),
        chaos: Vec::new(),
        kind: Kind::Plain,
    });
    out
}

/// `15m`, `60s`, `1h` in seconds.
fn secs_of(s: &str) -> Option<u64> {
    let (n, unit) = s.split_at(s.find(|c: char| !c.is_ascii_digit())?);
    let n: u64 = n.parse().ok()?;
    Some(match unit {
        "s" => n,
        "m" => n * 60,
        "h" => n * 3600,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;
    use crate::bench::plan::{Plan, Seed};

    fn seed() -> Seed {
        Seed::from_bytes([11; 32])
    }

    /// Round-1 voters plus each later round's revoters.
    fn ballots(s: &ElectionSpec, seed: &Seed) -> usize {
        let revoters = (0..s.voters)
            .filter(|&v| seed.revoter(&s.tag, v, s.revote_permille))
            .count();
        s.voters as usize + revoters * usize::from(s.rounds.saturating_sub(1))
    }

    #[test]
    fn wave1_is_well_formed() {
        // Σ bursts × ⌈scale·B⌉ over S_CELLS, worked out by hand.
        for (scale, s_voters, sc_voters) in [(1.0, 3072, 768), (0.02, 4 * 3 + 4 * 6 + 3 * 11, 18)] {
            let w = wave_specs(1, scale);
            assert!(!w.is_empty(), "wave 1 at {scale} is empty");
            let tags: HashSet<_> = w.iter().map(|s| s.tag.as_str()).collect();
            assert_eq!(tags.len(), w.len(), "tags repeat at {scale}");
            for nf in [2u8, 8, 16] {
                let g: Vec<_> = w
                    .iter()
                    .filter(|s| s.scenario == format!("S-{nf}"))
                    .collect();
                assert_eq!(g.len(), 4, "S-{nf} at {scale}");
                let on_a = g.iter().filter(|s| s.key_node == NodeId::A).count();
                assert_eq!(on_a, 2, "S-{nf} at {scale}: two per key node");
                for s in g {
                    assert_eq!((s.nf, s.origin, s.rounds), (nf, 1, 1), "{}", s.tag);
                    assert_eq!(s.voters, s_voters, "{} at {scale}", s.tag);
                }
            }
            let sc: Vec<_> = w.iter().filter(|s| s.scenario == "S-c").collect();
            assert_eq!(sc.len(), 2);
            assert!(sc.iter().all(|s| s.nf == 16 && s.voters == sc_voters));
            let r: Vec<_> = w.iter().filter(|s| s.scenario == "R").collect();
            let got: Vec<_> = r.iter().map(|s| (s.tag.as_str(), s.nf, s.origin)).collect();
            assert_eq!(
                got,
                [("R1", 2, 1), ("R2", 8, 4), ("R3", 16, 1), ("R4", 16, 3)]
            );
            assert!(
                w.iter().all(|s| s.voters >= 4),
                "fewer than 4 voters at {scale}"
            );
            for p in w.windows(2) {
                assert_ne!(p[0].key_node, p[1].key_node, "{} {}", p[0].tag, p[1].tag);
            }
        }
        // The seed derivations ignore the wave: a tag must not repeat in any
        // wave.
        for scale in [1.0, 0.02] {
            let all: Vec<_> = (1..=3).flat_map(|n| wave_specs(n, scale)).collect();
            let tags: HashSet<_> = all.iter().map(|s| s.tag.as_str()).collect();
            assert_eq!(tags.len(), all.len(), "tags repeat across waves at {scale}");
        }
        let total: usize = wave_specs(1, 1.0).iter().map(|s| ballots(s, &seed())).sum();
        assert!(
            (42_000..=46_000).contains(&total),
            "wave 1 holds {total} ballots"
        );
    }

    #[test]
    fn max_voters_equal_planned_voters() {
        let seed = seed();
        for n in [1, 2] {
            assert!(!wave_specs(n, 1.0).is_empty(), "wave {n}");
        }
        for n in 1..=3 {
            for scale in [1.0, 0.02] {
                let specs = wave_specs(n, scale);
                if specs.is_empty() {
                    continue;
                }
                let plan = Plan::stand_in(&seed, n, &specs).unwrap();
                for e in &plan.wave(n).unwrap().elections {
                    let distinct: HashSet<u32> =
                        e.ballots(&seed).into_iter().map(|(_, v)| v).collect();
                    assert_eq!(
                        distinct.len(),
                        e.spec.max_voters() as usize,
                        "wave {n} scale {scale} {}",
                        e.spec.tag
                    );
                }
            }
        }
    }

    #[test]
    fn wave1_scenarios_match_specs() {
        let ceiling = Knobs {
            batch_max: 512,
            batch_time: "60s".into(),
            ..Knobs::production()
        };
        let cells: Vec<Cell> = S_CELLS
            .iter()
            .map(|&(b, bursts)| Cell { b, bursts })
            .collect();
        assert!(cells.windows(2).all(|w| w[0].b < w[1].b), "S_CELLS ascend");
        for scale in [1.0, 0.05] {
            let specs = wave_specs(1, scale);
            let all = scenarios(1, scale);
            let names: Vec<&str> = all.iter().map(|s| s.name.as_str()).collect();
            assert_eq!(names, ["R", "S-2", "S-8", "S-16", "S-c"], "at {scale}");
            let spec = |t: &str| {
                specs
                    .iter()
                    .find(|s| s.tag == t)
                    .unwrap_or_else(|| panic!("{t} is not in wave_specs(1, {scale})"))
            };
            for sc in &all {
                assert_eq!(sc.wave, 1, "{}", sc.name);
                let want: Vec<&str> = specs
                    .iter()
                    .filter(|s| s.scenario == sc.name)
                    .map(|s| s.tag.as_str())
                    .collect();
                assert_eq!(sc.tags(), want, "{} at {scale}", sc.name);
                for t in sc.tags() {
                    assert_eq!(spec(t).scenario, sc.name, "{t}");
                }
                assert!(sc.chaos.is_empty(), "{}", sc.name);
            }
            for name in ["S-2", "S-8", "S-16"] {
                let sc = all.iter().find(|s| s.name == name).unwrap();
                assert_eq!(sc.kind, Kind::Ceiling(cells.clone()), "{name}");
                for l in &sc.lanes {
                    assert_eq!(l.route, Route::Only(spec(&l.tag).key_node), "{}", l.tag);
                    assert_eq!(l.end, End::AfterSettled, "{}", l.tag);
                }
            }
            // One set of knobs from S-2 to S-c: the rig never restarts
            // between them, so the proving estimate converges once per nf.
            for sc in all.iter().filter(|s| s.name.starts_with("S-")) {
                assert_eq!(sc.knobs, [ceiling.clone(), ceiling.clone()], "{}", sc.name);
                assert!(sc.allowed.is_empty(), "{}", sc.name);
            }
            let i16 = names.iter().position(|n| *n == "S-16").unwrap();
            assert_eq!(names[i16 + 1], "S-c");
            let sc = &all[i16 + 1];
            let (b, bursts) = SC_CELL;
            assert_eq!(sc.kind, Kind::Ceiling(vec![Cell { b, bursts }]));
            assert!(sc.lanes.iter().all(|l| l.route == Route::Both));
            assert!(sc.lanes.iter().all(|l| l.end == End::AfterSettled));

            let r = &all[0];
            assert_eq!(r.tags(), ["R1", "R2", "R3", "R4"]);
            assert_eq!(r.kind, Kind::Plain);
            assert_eq!(r.knobs, [Knobs::production(), Knobs::production()]);
            let allowed: Vec<&str> = r.allowed.keys().map(String::as_str).collect();
            assert_eq!(allowed, ["R2", "R3"], "process closed for R2 and R3 only");
            assert!(r.allowed.values().all(|v| v == &["process closed"]));
            for l in &r.lanes {
                assert_eq!(l.route, Route::ByVoter, "{}", l.tag);
                // Half the revoters overwrite, half revote.
                assert!(
                    matches!(
                        l.gaps[..],
                        [Gap::Overwrite { min: 5, max: 30 }, Gap::Revote { .. }]
                    ),
                    "{}",
                    l.tag
                );
            }
            let ends: Vec<&str> = r
                .lanes
                .iter()
                .map(|l| match l.end {
                    End::Shorten { notice: NOTICE, .. } => "shorten",
                    End::Now { .. } => "now",
                    _ => "other",
                })
                .collect();
            assert_eq!(ends, ["shorten", "now", "now", "shorten"]);
        }
        // Live, the four start 5 min apart and revotes come 15 min later.
        let r = &scenarios(1, 1.0)[0];
        let starts: Vec<u64> = r.lanes.iter().map(|l| l.start.as_secs()).collect();
        assert_eq!(starts, [0, 300, 600, 900]);
        assert!(
            r.lanes
                .iter()
                .all(|l| l.gaps[1] == Gap::Revote { secs: 900 })
        );
    }

    fn race_of(sc: &Scenario) -> &RaceSpec {
        match &sc.kind {
            Kind::Race(r) => r,
            k => panic!("{}: {k:?}", sc.name),
        }
    }

    #[test]
    fn wave2_specs() {
        let all = scenarios(2, 1.0);
        let specs = wave_specs(2, 1.0);
        let names: Vec<&str> = all.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["X1", "X2-a", "X2-b", "X3", "X4", "X5"]);
        let sc = |n: &str| all.iter().find(|s| s.name == n).unwrap();
        let spec = |t: &str| specs.iter().find(|s| s.tag == t).unwrap();
        let skew = Duration::from_secs(150);
        assert_eq!(
            race_of(sc("X1")),
            &RaceSpec {
                k: 64,
                rounds: 10,
                skew: None,
                lag_confirmations: None
            }
        );
        // X2: one scenario per loser, its proof held 150 s, so the other
        // node wins every round.
        let mut losers = Vec::new();
        for n in ["X2-a", "X2-b"] {
            let r = race_of(sc(n));
            assert_eq!((r.k, r.rounds, r.lag_confirmations), (64, 5, None), "{n}");
            let (loser, d) = r.skew.expect("X2 skews its loser");
            assert_eq!(d, skew, "{n}");
            losers.push(loser);
        }
        assert_eq!(losers, [NodeId::B, NodeId::A]);
        // X3: the loser is also 150 blocks behind on events, for the whole
        // scenario.
        let x3 = sc("X3");
        let r = race_of(x3);
        let (loser, d) = r.skew.expect("X3 skews its loser");
        assert_eq!((d, r.lag_confirmations, r.rounds), (skew, Some(150), 5));
        let i = |n: NodeId| usize::from(n == NodeId::B);
        assert_eq!(x3.knobs[i(loser)].confirmations, Some(150));
        assert_eq!(x3.knobs[1 - i(loser)].confirmations, None);
        assert_ne!(
            spec(&x3.lanes[0].tag).key_node,
            loser,
            "the lagging loser would publish the results late"
        );
        for n in ["X1", "X2-a", "X2-b", "X3"] {
            let s = sc(n);
            assert_eq!(s.lanes.len(), 1, "{n}");
            let l = &s.lanes[0];
            assert_eq!((l.route, &l.end), (Route::Both, &End::AfterSettled), "{n}");
            assert_eq!(spec(&l.tag).nf, 2, "{n}");
            // Both seal on the K-th vote: no timer in the way.
            for k in &s.knobs {
                assert_eq!((k.batch_max, k.batch_time.as_str()), (64, "15m"), "{n}");
            }
            assert!(s.allowed.is_empty(), "{n}");
        }
        // X4: two nf 16 elections of 300 votes to both nodes, then an AGM
        // end; a re-attempt that no longer fits the grace is expected.
        let x4 = sc("X4");
        assert_eq!(x4.kind, Kind::Plain);
        assert_eq!(x4.tags(), ["X4-1", "X4-2"]);
        for l in &x4.lanes {
            assert_eq!(l.route, Route::Both, "{}", l.tag);
            assert!(matches!(l.end, End::Now { .. }), "{}", l.tag);
            assert_eq!((spec(&l.tag).nf, spec(&l.tag).voters), (16, 300));
            assert_eq!(x4.allowed(&l.tag), ["process closed"]);
        }
        // X5: nf 8, 200 voters in two rounds, the revote 5 s later on the
        // other node.
        let x5 = sc("X5");
        assert_eq!(x5.kind, Kind::Plain);
        let l = &x5.lanes[0];
        assert_eq!(l.route, Route::Cross);
        assert_eq!(l.gaps, [Gap::Overwrite { min: 5, max: 5 }]);
        let s = spec(&l.tag);
        assert_eq!(
            (s.nf, s.voters, s.rounds, s.revote_permille),
            (8, 200, 2, 1000)
        );
        assert!(x5.allowed.is_empty());
        assert!(all.iter().all(|s| s.wave == 2 && s.chaos.is_empty()));
    }

    #[test]
    fn spend_bounds() {
        // X1 at scale 1: 20 tries of one 1-blob post per node.
        let all = scenarios(2, 1.0);
        let specs = wave_specs(2, 1.0);
        let b = post_bound(&all[0], &specs, 1.0);
        assert_eq!(b, [PostBound { txs: 20, blobs: 20 }; 2]);
        let u = b[0].ueth(EST_EXEC_WEI, EST_BLOB_WEI);
        // 20 × (21000 × 0.1 gwei + 131072 × 0.004 gwei).
        assert!((u - 52.48576).abs() < 1e-6, "{u}");
        // An `Only` lane posts on its node alone.
        let s2 = &scenarios(1, 1.0)[1];
        let b = post_bound(s2, &wave_specs(1, 1.0), 1.0);
        assert!(b[0].txs > 0 && b[1].txs > 0);
        let one = Scenario {
            lanes: s2.lanes[..1].to_vec(),
            ..s2.clone()
        };
        let b = post_bound(&one, &wave_specs(1, 1.0), 1.0);
        let on = usize::from(one.lanes[0].route == Route::Only(NodeId::B));
        assert!(b[on].txs > 0 && b[1 - on] == PostBound::default());
    }

    /// Finding 5: wave 2's lost races post too; at 0.1 gwei and a 0.004 gwei
    /// blob fee it stays within 300 µETH per sequencer key.
    #[test]
    fn wave2_fits_the_spend_cap() {
        let specs = wave_specs(2, 1.0);
        let mut total = [0.0f64; 2];
        for sc in scenarios(2, 1.0) {
            for (t, b) in total.iter_mut().zip(post_bound(&sc, &specs, 1.0)) {
                *t += b.ueth(EST_EXEC_WEI, EST_BLOB_WEI);
            }
        }
        assert!(total.iter().all(|t| *t > 100.0 && *t <= 300.0), "{total:?}");
    }

    #[test]
    fn scenarios_are_consistent() {
        let seed = seed();
        let mut seen = 0;
        for n in 1..=3 {
            for scale in [1.0, 0.05] {
                let specs = wave_specs(n, scale);
                for sc in scenarios(n, scale) {
                    seen += 1;
                    let ss: Vec<&ElectionSpec> = sc
                        .lanes
                        .iter()
                        .map(|l| specs.iter().find(|s| s.tag == l.tag).unwrap())
                        .collect();
                    let per_burst = |cells: &[Cell]| -> usize {
                        cells.iter().map(|c| c.bursts * scaled(c.b, scale)).sum()
                    };
                    let available: usize = ss.iter().map(|s| ballots(s, &seed)).sum();
                    let planned = match &sc.kind {
                        Kind::Plain => available,
                        Kind::Ceiling(cells) => ss.len() * per_burst(cells),
                        Kind::Race(r) => {
                            // Both nodes seal on the K-th vote, and every
                            // round up to twice the target has fresh votes.
                            assert!(
                                sc.knobs.iter().all(|k| r.k <= k.batch_max),
                                "{} at {scale}",
                                sc.name
                            );
                            assert_eq!(ss.len(), 1, "{} at {scale}", sc.name);
                            assert!(r.rounds > 0 && r.k > 0, "{} at {scale}", sc.name);
                            2 * r.rounds * r.k
                        }
                    };
                    assert!(planned <= available, "{} at {scale}", sc.name);
                    for (l, s) in sc.lanes.iter().zip(&ss) {
                        let at = format!("{} {} at {scale}", sc.name, l.tag);
                        assert!(l.schedule.validate().is_ok(), "{at}");
                        match &sc.kind {
                            Kind::Plain => {
                                if !SENDS_PAST_END.contains(&l.tag.as_str()) {
                                    assert!(
                                        l.schedule.capacity() >= s.voters as usize,
                                        "{at}: capacity {}",
                                        l.schedule.capacity()
                                    );
                                }
                                assert!(s.rounds == 1 || !l.gaps.is_empty(), "{at}");
                                // An end never cuts round 1 short.
                                let last = l.schedule.offsets(s.voters as usize, 1);
                                let last = last.last().copied().unwrap_or_default();
                                match l.end {
                                    End::Shorten { after, .. } | End::Now { after } => {
                                        assert!(
                                            after >= last,
                                            "{at}: ends at {after:?}, sends until {last:?}"
                                        )
                                    }
                                    End::AfterSettled => {}
                                }
                            }
                            Kind::Ceiling(cells) => {
                                assert_eq!(per_burst(cells), s.voters as usize, "{at}");
                                assert_eq!(s.rounds, 1, "{at}");
                            }
                            Kind::Race(r) => {
                                assert!(r.k * 2 * r.rounds <= s.voters as usize, "{at}");
                                assert_eq!(s.rounds, 1, "{at}");
                                assert_eq!(l.route, Route::Both, "{at}");
                            }
                        }
                        if let End::Shorten { notice, .. } = l.end {
                            assert!(notice >= NOTICE, "{at}");
                        }
                    }
                    for t in sc.allowed.keys() {
                        assert!(sc.tags().contains(&t.as_str()), "{}: {t}", sc.name);
                    }
                }
            }
        }
        assert!(seen > 0, "no scenarios");
    }
}
