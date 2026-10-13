//! The audit: one verdict per vote id against the chain's transitions, and
//! the tally and counters the chain order implies. Pure: `chain_view` reads.
//!
//! Classify only once the observer holds every transition the registry
//! logs show and A and B report `pendingVotes == 0` (`chain_view::wait_quiet`),
//! or the wait timed out: a vote still open after it is a real finding.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Result, anyhow, ensure};
use davinci_client::api::{ProcessId, vote_id_hex};
use serde::{Deserialize, Serialize};

use super::chain_view::TransitionRow;
use super::ledger::Entry;
use super::plan::NodeId;

/// A node's answer for one vote id.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum NodeStatus {
    Settled,
    /// `pending`, `aggregated` or `processed`.
    Open(String),
    /// `error`, with the node's reason.
    Error(String),
    /// 404: the node does not know the vote.
    Unknown,
}

/// What became of one vote id. The variants are in spec §11 order: the
/// first that holds applies.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// In two or more transitions (their indices).
    Twice(Vec<u64>),
    /// In a transition, yet no node accepted it and no send was uncertain.
    Inconsistent(String),
    /// In exactly one transition. `counted`: its voter's latest settled
    /// ballot in chain order. `stale_error`: nodes that took it and still
    /// report `error`.
    Settled {
        transition: u64,
        counted: bool,
        stale_error: Vec<NodeId>,
    },
    /// In no transition, and every node that took it reports `error`.
    /// `expected`: every reason is on the scenario's allowed list.
    Errored { reason: String, expected: bool },
    /// Not an accepted vote: every send was refused, or uncertain with every
    /// node answering 404.
    NeverAdmitted,
    /// Accepted, in no transition, and some node still holds it open, or
    /// reports nothing. `status` is the first non-error status, or `none`.
    Lost { status: String },
}

impl Verdict {
    pub fn name(&self) -> &'static str {
        match self {
            Verdict::Twice(_) => "twice",
            Verdict::Inconsistent(_) => "inconsistent",
            Verdict::Settled { .. } => "settled",
            Verdict::Errored { .. } => "errored",
            Verdict::NeverAdmitted => "never_admitted",
            Verdict::Lost { .. } => "lost",
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Classified {
    pub verdicts: BTreeMap<u64, Verdict>,
    /// `(vote id, transition)` of every vote id on chain that the ledger
    /// never sent.
    pub phantoms: Vec<(u64, u64)>,
    /// Planned vote ids with no ledger line: their fate is unknown.
    pub missing: Vec<u64>,
}

/// Allowed reasons match by substring, except that a held batch closed at
/// the end (`process closed (held: …)`) never matches.
pub fn allowed_reason(reason: &str, allowed: &[&str]) -> bool {
    !reason.contains("(held:") && allowed.iter().any(|a| !a.is_empty() && reason.contains(a))
}

/// The nodes that took the vote or may have.
fn takers(en: &Entry) -> BTreeSet<NodeId> {
    en.accepted.union(&en.uncertain).copied().collect()
}

/// The verdict of a vote in no transition.
fn off_chain(
    vid: u64,
    en: &Entry,
    st: &BTreeMap<(u64, NodeId), NodeStatus>,
    allowed: &[&str],
) -> Verdict {
    let status = |n: NodeId| st.get(&(vid, n));
    let takers = takers(en);
    let reasons: Vec<&str> = takers
        .iter()
        .filter_map(|n| match status(*n) {
            Some(NodeStatus::Error(r)) => Some(r.as_str()),
            _ => None,
        })
        .collect();
    let accepted_errored = en
        .accepted
        .iter()
        .all(|n| matches!(status(*n), Some(NodeStatus::Error(_))));
    // An uncertain node that answers 404 never had it; one that errored it
    // did. Any other answer, or none, leaves the vote possibly alive there.
    let uncertain_closed = en
        .uncertain
        .difference(&en.accepted)
        .all(|n| matches!(status(*n), Some(NodeStatus::Error(_) | NodeStatus::Unknown)));
    if !reasons.is_empty() && accepted_errored && uncertain_closed {
        let expected = reasons.iter().all(|r| allowed_reason(r, allowed));
        let reason = reasons
            .iter()
            .find(|r| !allowed_reason(r, allowed))
            .unwrap_or(&reasons[0]);
        return Verdict::Errored {
            reason: (*reason).to_string(),
            expected,
        };
    }
    if en.accepted.is_empty()
        && en
            .uncertain
            .iter()
            .all(|n| status(*n) == Some(&NodeStatus::Unknown))
    {
        return Verdict::NeverAdmitted;
    }
    let status = takers
        .iter()
        .find_map(|n| match status(*n) {
            Some(NodeStatus::Open(s)) => Some(s.clone()),
            Some(NodeStatus::Settled) => Some("settled".into()),
            _ => None,
        })
        .unwrap_or_else(|| "none".into());
    Verdict::Lost { status }
}

/// One verdict per ledger vote id against `rows` and the statuses the nodes
/// that took each vote report. `e`, `rows` and `planned` (the vote ids the
/// run scheduled) are one election's: entries of another election would
/// read as never landed.
pub fn classify(
    e: &BTreeMap<u64, Entry>,
    rows: &[TransitionRow],
    st: &BTreeMap<(u64, NodeId), NodeStatus>,
    allowed: &[&str],
    planned: &BTreeSet<u64>,
) -> Classified {
    let mut landed: BTreeMap<u64, Vec<u64>> = BTreeMap::new();
    let mut phantoms = Vec::new();
    for r in rows {
        for &v in &r.vids {
            if e.contains_key(&v) {
                landed.entry(v).or_default().push(r.index);
            } else {
                phantoms.push((v, r.index));
            }
        }
    }
    // Each voter's latest landing over the ballots that landed once; a tie
    // (two ballots of a voter in one transition) leaves none counted.
    let mut latest: BTreeMap<(ProcessId, &str, u32), (u64, Option<u64>)> = BTreeMap::new();
    for (vid, at) in &landed {
        let ([t], Some(en)) = (at.as_slice(), e.get(vid)) else {
            continue;
        };
        let k = (en.pid, en.tag.as_str(), en.voter);
        match latest.get(&k) {
            Some((l, _)) if *l > *t => {}
            Some((l, _)) if *l == *t => {
                latest.insert(k, (*t, None));
            }
            _ => {
                latest.insert(k, (*t, Some(*vid)));
            }
        }
    }
    let verdicts = e
        .iter()
        .map(|(&vid, en)| {
            let at = landed.get(&vid).map(Vec::as_slice).unwrap_or_default();
            let v = match at {
                [] => off_chain(vid, en, st, allowed),
                [t] if en.accepted.is_empty() && en.uncertain.is_empty() => {
                    let refused: Vec<String> = en
                        .refused
                        .iter()
                        .map(|(n, c)| format!("{n:?} {c}"))
                        .collect();
                    Verdict::Inconsistent(format!(
                        "in transition {t}, but no node accepted it and no send was uncertain (refused: {})",
                        refused.join(", ")
                    ))
                }
                [t] => Verdict::Settled {
                    transition: *t,
                    counted: latest.get(&(en.pid, en.tag.as_str(), en.voter))
                        == Some(&(*t, Some(vid))),
                    stale_error: takers(en)
                        .into_iter()
                        .filter(|n| matches!(st.get(&(vid, *n)), Some(NodeStatus::Error(_))))
                        .collect(),
                },
                _ => {
                    let mut t = at.to_vec();
                    t.sort_unstable();
                    Verdict::Twice(t)
                }
            };
            (vid, v)
        })
        .collect();
    let missing = planned
        .iter()
        .filter(|v| !e.contains_key(v))
        .copied()
        .collect();
    Classified {
        verdicts,
        phantoms,
        missing,
    }
}

/// The tally and counters the chain order implies.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Expect {
    pub tally: Vec<u64>,
    /// Distinct voters with a settled ballot.
    pub voters: u64,
    /// Settled ballots minus distinct voters.
    pub overwrites: u64,
}

/// Each voter's latest ballot in transition order (across nodes, the later
/// landing stands), summed over `nf` fields. `who` maps a vote id to its
/// voter and choices; a vote id it does not know is an error.
pub fn expect(
    rows: &[TransitionRow],
    who: &dyn Fn(u64) -> Option<(u32, Vec<u64>)>,
    nf: usize,
) -> Result<Expect> {
    let mut order: Vec<&TransitionRow> = rows.iter().collect();
    order.sort_by_key(|r| r.index);
    ensure!(
        order.windows(2).all(|w| w[0].index < w[1].index),
        "a transition index repeats"
    );
    let mut last: BTreeMap<u32, Vec<u64>> = BTreeMap::new();
    let mut votes = 0u64;
    for r in order {
        let mut seen = BTreeSet::new();
        for &vid in &r.vids {
            let (voter, choices) = who(vid).ok_or_else(|| {
                anyhow!(
                    "vote id {} in transition {} is no known ballot",
                    vote_id_hex(vid),
                    r.index
                )
            })?;
            ensure!(
                choices.len() == nf,
                "vote id {} has {} choices, want {nf}",
                vote_id_hex(vid),
                choices.len()
            );
            // One batch never holds two ballots of a slot, so the order
            // within a transition is never needed.
            ensure!(
                seen.insert(voter),
                "voter {voter} twice in transition {}",
                r.index
            );
            last.insert(voter, choices);
            votes += 1;
        }
    }
    let mut tally = vec![0u64; nf];
    for c in last.values() {
        for (t, x) in tally.iter_mut().zip(c) {
            *t = t.checked_add(*x).ok_or_else(|| anyhow!("tally overflow"))?;
        }
    }
    let voters = last.len() as u64;
    Ok(Expect {
        tally,
        voters,
        overwrites: votes - voters,
    })
}

/// What the run reads once the results wait ends, for the criteria
/// `classify` cannot see.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Published {
    /// The registry's `ProcessResultsSet` result; `None` when none landed by
    /// `grace_end + 15 min`.
    pub results: Option<Vec<u64>>,
    /// Every node's view of the results (`/processes/{pid}` `result`), by name.
    pub views: Vec<(String, Option<Vec<u64>>)>,
    /// The registry's `votersCount` and `overwrittenVotesCount`.
    pub voters_count: u64,
    pub overwritten_votes_count: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct AuditResult {
    /// Votes per verdict name, plus `phantom`.
    pub counts: BTreeMap<String, usize>,
    /// Every `Errored` reason and its votes.
    pub errors: BTreeMap<String, usize>,
    /// The `Errored` reasons not on the allowed list: findings.
    pub unexpected: BTreeMap<String, usize>,
    pub no_vote_lost: bool,
    pub clean: bool,
    /// The next three stay false until [`AuditResult::check`] runs.
    pub tally_ok: bool,
    pub results_ok: bool,
    pub counters_ok: bool,
    pub observer_incomplete: bool,
    pub stale_errors: BTreeMap<NodeId, usize>,
    pub notes: Vec<String>,
}

/// Notes kept per kind of failing vote.
const MAX_NOTES: usize = 20;

impl Classified {
    pub fn summary(&self) -> AuditResult {
        let mut a = AuditResult::default();
        let mut noted: BTreeMap<&str, usize> = BTreeMap::new();
        let mut note = |a: &mut AuditResult, kind: &'static str, s: String| {
            let n = noted.entry(kind).or_default();
            *n += 1;
            if *n <= MAX_NOTES {
                a.notes.push(s);
            }
        };
        for (vid, v) in &self.verdicts {
            *a.counts.entry(v.name().into()).or_default() += 1;
            let id = vote_id_hex(*vid);
            match v {
                Verdict::Settled { stale_error, .. } => {
                    for n in stale_error {
                        *a.stale_errors.entry(*n).or_default() += 1;
                    }
                }
                Verdict::Errored { reason, expected } => {
                    *a.errors.entry(reason.clone()).or_default() += 1;
                    if !expected {
                        *a.unexpected.entry(reason.clone()).or_default() += 1;
                    }
                }
                Verdict::NeverAdmitted => {}
                Verdict::Lost { status } => note(&mut a, "lost", format!("lost {id}: {status}")),
                Verdict::Twice(t) => {
                    note(&mut a, "twice", format!("twice {id}: transitions {t:?}"))
                }
                Verdict::Inconsistent(m) => {
                    note(&mut a, "inconsistent", format!("inconsistent {id}: {m}"))
                }
            }
        }
        for (vid, t) in &self.phantoms {
            *a.counts.entry("phantom".into()).or_default() += 1;
            note(
                &mut a,
                "phantom",
                format!("phantom {} in transition {t}", vote_id_hex(*vid)),
            );
        }
        for vid in &self.missing {
            *a.counts.entry("missing".into()).or_default() += 1;
            note(
                &mut a,
                "missing",
                format!("missing {}: planned, but no ledger line", vote_id_hex(*vid)),
            );
        }
        for (kind, n) in noted {
            if n > MAX_NOTES {
                a.notes.push(format!("{kind}: {} more", n - MAX_NOTES));
            }
        }
        let has = |k: &str| a.counts.get(k).is_some_and(|n| *n > 0);
        a.no_vote_lost = !(has("lost")
            || has("twice")
            || has("inconsistent")
            || has("phantom")
            || has("missing"));
        a.clean = a.unexpected.is_empty() && a.stale_errors.is_empty();
        a
    }
}

/// The registry's results equal `want` on its fields, the rest zero.
fn same_tally(r: &[u64], want: &[u64]) -> bool {
    r.len() >= want.len() && r[..want.len()] == *want && r[want.len()..].iter().all(|x| *x == 0)
}

impl AuditResult {
    /// Fills `results_ok`, `tally_ok`, `observer_incomplete` and the
    /// registry and per-transition part of `counters_ok` (the run ANDs in the
    /// scenario-wide `/info` counters). `x` is [`expect`] over `rows`;
    /// without it nothing passes.
    pub fn check(
        &mut self,
        x: &Result<Expect>,
        rows: &[TransitionRow],
        p: &Published,
        from_fallback: &[u64],
    ) {
        self.observer_incomplete = !from_fallback.is_empty();
        if self.observer_incomplete {
            self.notes.push(format!(
                "observer incomplete: transitions {from_fallback:?} read from A or B"
            ));
        }
        let x = match x {
            Ok(x) => x,
            Err(e) => {
                (self.tally_ok, self.results_ok, self.counters_ok) = (false, false, false);
                self.notes.push(format!("no expected tally: {e:#}"));
                return;
            }
        };
        self.results_ok = p
            .results
            .as_deref()
            .is_some_and(|r| same_tally(r, &x.tally));
        match &p.results {
            None => self
                .notes
                .push("no results on the registry by the deadline".into()),
            Some(r) if !self.results_ok => self
                .notes
                .push(format!("registry results {r:?}, expected {:?}", x.tally)),
            Some(_) => {}
        }
        let mut views_ok = true;
        for (name, v) in &p.views {
            if !v.as_deref().is_some_and(|v| same_tally(v, &x.tally)) {
                views_ok = false;
                self.notes.push(format!(
                    "node {name} shows results {v:?}, expected {:?}",
                    x.tally
                ));
            }
        }
        self.tally_ok = self.results_ok && views_ok;

        let mut new_voters = 0u64;
        let mut overwrites = 0u64;
        let mut rows_ok = true;
        for r in rows {
            match r.voters.checked_sub(r.overwrites) {
                Some(n) => new_voters += n,
                None => rows_ok = false,
            }
            overwrites += r.overwrites;
        }
        let checks = [
            (
                p.voters_count,
                x.voters,
                "registry votersCount vs distinct settled voters",
            ),
            (
                p.overwritten_votes_count,
                x.overwrites,
                "registry overwrittenVotesCount vs settled ballots minus voters",
            ),
            (
                p.voters_count,
                new_voters,
                "registry votersCount vs transitions' voters minus overwrites",
            ),
            (
                p.overwritten_votes_count,
                overwrites,
                "registry overwrittenVotesCount vs transitions' overwrites",
            ),
        ];
        self.counters_ok = rows_ok;
        if !rows_ok {
            self.notes
                .push("a transition records more overwrites than votes".into());
        }
        for (got, want, what) in checks {
            if got != want {
                self.counters_ok = false;
                self.notes.push(format!("{what}: {got} != {want}"));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use NodeId::{A, B};

    fn acc(n: NodeId, voter: u32) -> Entry {
        Entry {
            accepted: [n].into(),
            voter,
            ..Entry::test()
        }
    }
    fn unc(n: NodeId, voter: u32) -> Entry {
        Entry {
            uncertain: [n].into(),
            voter,
            ..Entry::test()
        }
    }
    fn refd(n: NodeId, voter: u32) -> Entry {
        Entry {
            refused: vec![(n, 40002)],
            voter,
            ..Entry::test()
        }
    }
    /// Every ledger vote id was planned.
    fn keys(e: &BTreeMap<u64, Entry>) -> BTreeSet<u64> {
        e.keys().copied().collect()
    }
    fn row(i: u64, vids: &[u64]) -> TransitionRow {
        TransitionRow {
            index: i,
            vids: vids.to_vec(),
            ..TransitionRow::test()
        }
    }

    #[test]
    #[allow(clippy::bool_comparison)] // the brief's assertion, kept verbatim
    fn classify_every_case() {
        let e: BTreeMap<u64, Entry> = [
            (1, acc(A, 1)),
            (2, acc(B, 2)),
            (3, acc(A, 3)),
            (4, acc(A, 4)),
            (5, unc(A, 5)),
            (6, unc(B, 6)),
            (7, acc(A, 7)),
            (8, acc(B, 8)),
            (9, refd(A, 9)),
        ]
        .into();
        let rows = vec![row(0, &[1, 7]), row(1, &[2, 7, 8]), row(2, &[99, 9])];
        let st: BTreeMap<_, _> = [
            ((3, A), NodeStatus::Error("process closed".into())),
            ((4, A), NodeStatus::Open("pending".into())),
            ((5, A), NodeStatus::Unknown),
            ((6, B), NodeStatus::Open("pending".into())),
            ((8, B), NodeStatus::Error("settlement reverted: X".into())),
        ]
        .into();
        let c = classify(&e, &rows, &st, &["process closed"], &keys(&e));
        assert_eq!(
            c.verdicts[&1],
            Verdict::Settled {
                transition: 0,
                counted: true,
                stale_error: vec![]
            }
        );
        assert_eq!(
            c.verdicts[&2],
            Verdict::Settled {
                transition: 1,
                counted: true,
                stale_error: vec![]
            }
        );
        assert_eq!(
            c.verdicts[&3],
            Verdict::Errored {
                reason: "process closed".into(),
                expected: true
            }
        );
        assert_eq!(
            c.verdicts[&4],
            Verdict::Lost {
                status: "pending".into()
            }
        );
        assert_eq!(c.verdicts[&5], Verdict::NeverAdmitted);
        assert_eq!(
            c.verdicts[&6],
            Verdict::Lost {
                status: "pending".into()
            }
        ); // uncertain, but the node holds it
        assert_eq!(c.verdicts[&7], Verdict::Twice(vec![0, 1]));
        assert_eq!(
            c.verdicts[&8],
            Verdict::Settled {
                transition: 1,
                counted: true,
                stale_error: vec![B]
            }
        );
        assert!(matches!(c.verdicts[&9], Verdict::Inconsistent(_))); // refused everywhere, yet on chain
        assert_eq!(c.phantoms, vec![(99, 2)]);
        assert!(c.summary().no_vote_lost == false); // because of 4, 6, 7, 9 and the phantom, not 8
        let e8: BTreeMap<u64, Entry> = [(8, acc(B, 8))].into();
        let s8 = classify(&e8, &[row(1, &[8])], &st, &[], &keys(&e8)).summary();
        assert!(s8.no_vote_lost && !s8.clean); // stale error is a finding only

        // Ruling 20: an isolated vote errors with the prover's reason; a finding, not a loss.
        let only3: BTreeMap<u64, Entry> = [(3, acc(A, 3))].into();
        let st2: BTreeMap<_, _> = [(
            (3, A),
            NodeStatus::Error("prover: job failed (isolated by bisection)".into()),
        )]
        .into();
        let s = classify(&only3, &[], &st2, &["process closed"], &keys(&only3)).summary();
        assert!(s.no_vote_lost && !s.clean);
        // A held batch closed at the end is never the allowed `process closed`.
        let held = "process closed (held: proved publics do not match)";
        let st3: BTreeMap<_, _> = [((3, A), NodeStatus::Error(held.into()))].into();
        let c3 = classify(&only3, &[], &st3, &["process closed"], &keys(&only3));
        assert_eq!(
            c3.verdicts[&3],
            Verdict::Errored {
                reason: held.into(),
                expected: false
            }
        );
    }

    #[test]
    fn tally_and_counters_follow_chain_order() {
        // Voter 1's revote (vid 12) lands before its first ballot (vid 11): across nodes the later landing stands.
        let rows = vec![row(0, &[10, 12]), row(1, &[11])];
        let who = |vid: u64| match vid {
            10 => Some((0, vec![1, 0])),
            11 => Some((1, vec![0, 2])),
            12 => Some((1, vec![3, 0])),
            _ => None,
        };
        let x = expect(&rows, &who, 2).unwrap();
        assert_eq!((x.tally, x.voters, x.overwrites), (vec![1, 2], 2, 1));
        assert!(expect(&[row(0, &[77])], &who, 2).is_err()); // an unknown vote id is an error
    }

    #[test]
    fn ruling_20_to_22_reasons_and_uncertain_sends() {
        let allowed = ["process closed"];
        let one = |s: NodeStatus| -> BTreeMap<_, _> { [((3, A), s)].into() };
        let only3: BTreeMap<u64, Entry> = [(3, acc(A, 3))].into();
        for r in [
            "guest rejected the batch: ballot proof (mask 0x4)",
            "exposed refresh set too large, recast",
            "prover: job failed: oom (isolated by bisection)",
            "settlement reverted: InvalidStateRoot",
        ] {
            let c = classify(
                &only3,
                &[],
                &one(NodeStatus::Error(r.into())),
                &allowed,
                &keys(&only3),
            );
            assert_eq!(
                c.verdicts[&3],
                Verdict::Errored {
                    reason: r.into(),
                    expected: false
                }
            );
            let s = c.summary();
            assert!(s.no_vote_lost && !s.clean, "{r}");
            assert_eq!(s.unexpected[r], 1);
        }
        // Accepted, then nothing on the node: lost, not "never admitted".
        let c = classify(
            &only3,
            &[],
            &one(NodeStatus::Unknown),
            &allowed,
            &keys(&only3),
        );
        assert_eq!(
            c.verdicts[&3],
            Verdict::Lost {
                status: "none".into()
            }
        );
        // An uncertain send the node errored was admitted: errored, not never admitted.
        let u3: BTreeMap<u64, Entry> = [(3, unc(A, 3))].into();
        let c = classify(
            &u3,
            &[],
            &one(NodeStatus::Error("process closed".into())),
            &allowed,
            &keys(&u3),
        );
        assert_eq!(
            c.verdicts[&3],
            Verdict::Errored {
                reason: "process closed".into(),
                expected: true
            }
        );
        // An uncertain send with no answer from the node is not proven absent.
        let c = classify(&u3, &[], &BTreeMap::new(), &allowed, &keys(&u3));
        assert!(matches!(c.verdicts[&3], Verdict::Lost { .. }));
        // An uncertain send that settled is settled, and a refused-only vote
        // that never landed is not an accepted vote.
        let c = classify(&u3, &[row(0, &[3])], &BTreeMap::new(), &allowed, &keys(&u3));
        assert!(matches!(c.verdicts[&3], Verdict::Settled { .. }));
        let r3: BTreeMap<u64, Entry> = [(3, refd(A, 3))].into();
        let s = classify(&r3, &[], &BTreeMap::new(), &allowed, &keys(&r3)).summary();
        assert!(s.no_vote_lost && s.clean);
        // Errored on one node, still open on the other: lost.
        let both: BTreeMap<u64, Entry> = [(
            3,
            Entry {
                accepted: [A, B].into(),
                ..Entry::test()
            },
        )]
        .into();
        let st: BTreeMap<_, _> = [
            ((3, A), NodeStatus::Error("process closed".into())),
            ((3, B), NodeStatus::Open("aggregated".into())),
        ]
        .into();
        let c = classify(&both, &[], &st, &allowed, &keys(&both));
        assert_eq!(
            c.verdicts[&3],
            Verdict::Lost {
                status: "aggregated".into()
            }
        );
        // Only the later of a voter's two settled ballots counts.
        let two: BTreeMap<u64, Entry> = [(3, acc(A, 5)), (4, acc(B, 5))].into();
        let c = classify(
            &two,
            &[row(0, &[4]), row(1, &[3])],
            &BTreeMap::new(),
            &[],
            &keys(&two),
        );
        assert_eq!(
            (c.verdicts[&3].clone(), c.verdicts[&4].clone()),
            (
                Verdict::Settled {
                    transition: 1,
                    counted: true,
                    stale_error: vec![]
                },
                Verdict::Settled {
                    transition: 0,
                    counted: false,
                    stale_error: vec![]
                }
            )
        );
    }

    #[test]
    fn a_planned_vote_without_a_ledger_line_fails() {
        let e: BTreeMap<u64, Entry> = [(1, acc(A, 1))].into();
        let rows = [row(0, &[1])];
        let c = classify(&e, &rows, &BTreeMap::new(), &[], &[1, 2].into());
        assert_eq!(c.missing, vec![2]);
        let s = c.summary();
        assert!(!s.no_vote_lost && s.clean);
        assert_eq!(s.counts["missing"], 1);
        assert!(
            s.notes.iter().any(|n| n.contains("missing")),
            "{:?}",
            s.notes
        );
        // An empty ledger against a plan is no pass either.
        let s = classify(&BTreeMap::new(), &[], &BTreeMap::new(), &[], &[1].into()).summary();
        assert!(!s.no_vote_lost);
        // The full plan passes.
        let s = classify(&e, &rows, &BTreeMap::new(), &[], &[1].into()).summary();
        assert!(s.no_vote_lost);
    }

    #[test]
    fn results_and_counters_checks() {
        let rows = vec![
            TransitionRow {
                voters: 2,
                overwrites: 0,
                ..row(0, &[10, 12])
            },
            TransitionRow {
                voters: 1,
                overwrites: 1,
                ..row(1, &[11])
            },
        ];
        let x = Expect {
            tally: vec![1, 2],
            voters: 2,
            overwrites: 1,
        };
        let good = Published {
            results: Some(vec![1, 2, 0, 0]),
            views: vec![
                ("a".into(), Some(vec![1, 2])),
                ("o".into(), Some(vec![1, 2, 0])),
            ],
            voters_count: 2,
            overwritten_votes_count: 1,
        };
        let run = |x: Result<Expect>, p: &Published, fb: &[u64]| {
            let mut a = Classified::default().summary();
            assert!(!(a.tally_ok || a.results_ok || a.counters_ok)); // unchecked is not a pass
            a.check(&x, &rows, p, fb);
            a
        };
        let a = run(Ok(x.clone()), &good, &[]);
        assert!(a.tally_ok && a.results_ok && a.counters_ok && !a.observer_incomplete);
        // No results by the deadline.
        let a = run(
            Ok(x.clone()),
            &Published {
                results: None,
                ..good.clone()
            },
            &[],
        );
        assert!(!a.results_ok && !a.tally_ok && a.counters_ok);
        // Wrong results.
        let a = run(
            Ok(x.clone()),
            &Published {
                results: Some(vec![1, 3]),
                ..good.clone()
            },
            &[],
        );
        assert!(!a.results_ok && !a.tally_ok);
        // A node that does not show them.
        let mut p = good.clone();
        p.views.push(("b".into(), None));
        let a = run(Ok(x.clone()), &p, &[]);
        assert!(a.results_ok && !a.tally_ok);
        // Registry counters off by one, then per-transition sums off.
        let a = run(
            Ok(x.clone()),
            &Published {
                voters_count: 3,
                ..good.clone()
            },
            &[],
        );
        assert!(!a.counters_ok && a.results_ok);
        let a = run(
            Ok(Expect {
                voters: 3,
                overwrites: 0,
                ..x.clone()
            }),
            &Published {
                voters_count: 3,
                overwritten_votes_count: 0,
                ..good.clone()
            },
            &[],
        );
        assert!(!a.counters_ok);
        // No expectation (an unknown vote id) passes nothing.
        let a = run(Err(anyhow::anyhow!("unknown vote id")), &good, &[1]);
        assert!(!(a.tally_ok || a.results_ok || a.counters_ok) && a.observer_incomplete);
    }
}
