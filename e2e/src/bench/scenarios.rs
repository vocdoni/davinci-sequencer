//! The election specs and scenarios of each wave (spec §8). Wave 1: the
//! realistic elections R1-R4 and the ceiling groups S-2, S-8, S-16 and S-c.

use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::{Result, ensure};

use super::chaos::Chaos;
use super::plan::{ElectionSpec, NodeId};
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
        _ => Vec::new(),
    }
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
        assert!(!wave_specs(1, 1.0).is_empty());
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
