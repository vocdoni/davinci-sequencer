//! The election specs of each wave (spec §8). Wave 1: the realistic
//! elections R1-R4 and the ceiling groups S-2, S-8, S-16 and S-c.

use anyhow::{Result, ensure};

use super::plan::{ElectionSpec, NodeId};

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
}
