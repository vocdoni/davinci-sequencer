//! Progress lines and the JSON and Markdown reports of a run and a campaign.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use super::audit::AuditResult;
use super::metrics::{CellRate, Kind};
use super::plan::NodeId;

/// The counters behind one progress line (spec §12).
#[derive(Clone, Debug, Default)]
pub struct Progress {
    /// `w1/R`: wave and scenario.
    pub tag: String,
    pub sent: u64,
    pub planned: u64,
    pub accepted: u64,
    pub refused: u64,
    pub settled: u64,
    pub errored: u64,
    pub open: u64,
    /// Votes per minute over the last five minutes.
    pub per_min: f64,
    pub tx: [u64; 2],
    pub lost: [u64; 2],
    /// Ethereum spend per node, in µETH.
    pub spent_l1_ueth: [u64; 2],
    /// Gwei.
    pub base: f64,
    pub tip: f64,
    pub blob: f64,
}

impl Progress {
    pub fn line(&self, elapsed: Duration) -> String {
        let s = elapsed.as_secs();
        format!(
            "[bench {:02}:{:02}:{:02} {}] sent {}/{} acc {} ref {} | settled {} err {} open {} | 5m {:.0} v/min | \
             A tx {} lost {} | B tx {} lost {} | spent L1 A {} B {} µETH | fees {}+{} gwei, blob {} gwei",
            s / 3600,
            s / 60 % 60,
            s % 60,
            self.tag,
            self.sent,
            self.planned,
            self.accepted,
            self.refused,
            self.settled,
            self.errored,
            self.open,
            self.per_min,
            self.tx[0],
            self.lost[0],
            self.tx[1],
            self.lost[1],
            self.spent_l1_ueth[0],
            self.spent_l1_ueth[1],
            g(self.base),
            g(self.tip),
            g(self.blob),
        )
    }
}

/// A gwei figure with up to three significant decimals, no trailing zeros.
fn g(v: f64) -> String {
    let s = format!("{v:.3}");
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

/// Per node: the vote-loss metrics (log Kinds, cross-checked with /info
/// `bisections`, `isolated_votes`, `held_batches`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct VoteLoss {
    pub bisections: u64,
    pub isolated: u64,
    pub held: u64,
    pub refresh_overflow: u64,
    pub guest_rejection: u64,
    pub finalize_failed: u64,
}

pub fn vote_loss(events: &[(NodeId, Kind)]) -> BTreeMap<NodeId, VoteLoss> {
    let mut out: BTreeMap<NodeId, VoteLoss> = BTreeMap::new();
    for (n, k) in events {
        let v = out.entry(*n).or_default();
        match k {
            Kind::Bisect { .. } => v.bisections += 1,
            Kind::Isolated { .. } => v.isolated += 1,
            Kind::Held { .. } => v.held += 1,
            Kind::RefreshOverflow { .. } => v.refresh_overflow += 1,
            Kind::GuestRejected { .. } => v.guest_rejection += 1,
            Kind::FinalizeFailed { .. } => v.finalize_failed += 1,
            _ => {}
        }
    }
    out
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FindingKind {
    UnexpectedError,
    StaleError,
    Bisections,
    Isolated,
    Held,
    RefreshOverflow,
    GuestRejection,
    ResultsFailed,
    ObserverIncomplete,
    PendingAfterWait,
}

/// One row of the findings table: a kind of trouble in one election.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Finding {
    pub scenario: String,
    pub tag: String,
    pub kind: FindingKind,
    pub count: u64,
    pub detail: String,
}

fn node_name(n: NodeId) -> &'static str {
    match n {
        NodeId::A => "A",
        NodeId::B => "B",
    }
}

/// Findings of one election (`tag`) from its audit and the nodes' log events.
pub fn findings(
    scenario: &str,
    tag: &str,
    audit: &AuditResult,
    events: &[(NodeId, Kind)],
) -> Vec<Finding> {
    let mut out = Vec::new();
    let mut add = |kind, count: u64, detail: String| {
        if count > 0 {
            out.push(Finding {
                scenario: scenario.into(),
                tag: tag.into(),
                kind,
                count,
                detail,
            });
        }
    };
    for (reason, n) in &audit.unexpected {
        add(FindingKind::UnexpectedError, *n as u64, reason.clone());
    }
    for (node, n) in &audit.stale_errors {
        add(FindingKind::StaleError, *n as u64, node_name(*node).into());
    }
    let (mut bis, mut iso, mut held, mut ovf, mut guest, mut fin) = (0, 0, 0, 0, 0, 0);
    let (mut iso_d, mut held_d, mut guest_d) = (Vec::new(), Vec::new(), Vec::new());
    for (n, k) in events {
        let nn = node_name(*n);
        match k {
            Kind::Bisect { .. } => bis += 1,
            Kind::Isolated { vid, .. } => {
                iso += 1;
                iso_d.push(format!("{nn}:{}", davinci_client::api::vote_id_hex(*vid)));
            }
            Kind::Held { error, .. } => {
                held += 1;
                held_d.push(format!("{nn}: {error}"));
            }
            Kind::RefreshOverflow { votes, .. } => ovf += votes,
            Kind::GuestRejected { mask, .. } => {
                guest += 1;
                guest_d.push(format!("{nn}: {mask}"));
            }
            Kind::FinalizeFailed { .. } => fin += 1,
            _ => {}
        }
    }
    let closed_held: u64 = audit
        .errors
        .iter()
        .filter(|(r, _)| r.starts_with("process closed (held:"))
        .map(|(_, n)| *n as u64)
        .sum();
    add(FindingKind::Bisections, bis, String::new());
    add(FindingKind::Isolated, iso, iso_d.join(", "));
    let mut d = held_d.join("; ");
    if closed_held > 0 {
        let _ = write!(d, " ({closed_held} votes closed as held)");
    }
    add(
        FindingKind::Held,
        if held > 0 { held } else { closed_held },
        d,
    );
    add(FindingKind::RefreshOverflow, ovf, "votes errored".into());
    add(FindingKind::GuestRejection, guest, guest_d.join("; "));
    let fin = if fin == 0 && !audit.results_ok {
        1
    } else {
        fin
    };
    add(FindingKind::ResultsFailed, fin, String::new());
    add(
        FindingKind::ObserverIncomplete,
        audit.observer_incomplete.into(),
        audit.notes.join("; "),
    );
    let lost = audit.counts.get("lost").copied().unwrap_or(0) as u64;
    add(FindingKind::PendingAfterWait, lost, String::new());
    out
}

/// One ceiling cell: `nf` ballot fields at batch size `b`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Cell {
    pub nf: u32,
    pub b: u64,
    pub rate: CellRate,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Throughput {
    pub overall_per_min: Option<f64>,
    pub steady_per_min: Option<f64>,
    /// Busy fraction per prover.
    pub prover_busy: BTreeMap<String, f64>,
    /// Share of transitions per node.
    pub node_share: BTreeMap<String, f64>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ScenarioReport {
    pub name: String,
    pub params: BTreeMap<String, String>,
    pub audit: AuditResult,
    /// Stage → seconds at p50, p90, p99 and max.
    pub latency: BTreeMap<String, [f64; 4]>,
    pub throughput: Throughput,
    pub cells: Vec<Cell>,
    pub batches: u64,
    /// Name → count or outcome.
    pub races: BTreeMap<String, String>,
    pub chaos: BTreeMap<String, String>,
    /// Node → `/info` counters last read.
    pub nodes: BTreeMap<String, BTreeMap<String, u64>>,
    pub vote_loss: BTreeMap<NodeId, VoteLoss>,
    pub findings: Vec<Finding>,
    /// Account → µETH spent.
    pub spend: BTreeMap<String, i64>,
}

fn table(out: &mut String, head: &[&str], rows: &[Vec<String>]) {
    let _ = writeln!(out, "| {} |", head.join(" | "));
    let _ = writeln!(out, "|{}", "---|".repeat(head.len()));
    for r in rows {
        let _ = writeln!(out, "| {} |", r.join(" | "));
    }
    out.push('\n');
}

fn opt(v: Option<f64>) -> String {
    v.map_or("n/a".into(), |v| format!("{v:.1}"))
}

/// nf rows × B columns, each cell `votes/min (mean batch)`.
pub fn ceiling_table(all: &[ScenarioReport]) -> String {
    let cells: Vec<&Cell> = all.iter().flat_map(|r| &r.cells).collect();
    let mut nfs: Vec<u32> = cells.iter().map(|c| c.nf).collect();
    nfs.sort_unstable();
    nfs.dedup();
    let mut bs: Vec<u64> = cells.iter().map(|c| c.b).collect();
    bs.sort_unstable();
    bs.dedup();
    let mut head = vec!["nf \\ B".to_string()];
    head.extend(bs.iter().map(|b| b.to_string()));
    let head: Vec<&str> = head.iter().map(String::as_str).collect();
    let rows: Vec<Vec<String>> = nfs
        .iter()
        .map(|nf| {
            let mut row = vec![nf.to_string()];
            for b in &bs {
                row.push(match cells.iter().find(|c| c.nf == *nf && c.b == *b) {
                    None => "-".into(),
                    Some(c) => format!(
                        "{} ({:.0}){}",
                        c.rate
                            .steady_per_min
                            .map_or("n/a".into(), |v| format!("{v:.0}")),
                        c.rate.mean_batch,
                        if c.rate.floor { " floor" } else { "" }
                    ),
                });
            }
            row
        })
        .collect();
    let mut out = String::new();
    table(&mut out, &head, &rows);
    out
}

fn verdicts(out: &mut String, all: &[&ScenarioReport]) {
    let rows: Vec<Vec<String>> = all
        .iter()
        .map(|r| {
            let a = &r.audit;
            let mut row = vec![r.name.clone()];
            row.extend(
                [
                    a.no_vote_lost,
                    a.clean,
                    a.tally_ok,
                    a.results_ok,
                    a.counters_ok,
                ]
                .map(|b| if b { "yes" } else { "NO" }.to_string()),
            );
            row.push(
                if a.observer_incomplete {
                    "observer incomplete"
                } else {
                    ""
                }
                .into(),
            );
            row
        })
        .collect();
    table(
        out,
        &[
            "scenario",
            "no_vote_lost",
            "clean",
            "tally_ok",
            "results_ok",
            "counters_ok",
            "note",
        ],
        &rows,
    );
}

fn details(out: &mut String, all: &[&ScenarioReport]) {
    let lat: Vec<Vec<String>> = all
        .iter()
        .flat_map(|r| {
            r.latency.iter().map(|(stage, p)| {
                let mut row = vec![r.name.clone(), stage.clone()];
                row.extend(p.iter().map(|v| format!("{v:.1}")));
                row
            })
        })
        .collect();
    if !lat.is_empty() {
        out.push_str("### Latency (s)\n\n");
        table(
            out,
            &["scenario", "stage", "p50", "p90", "p99", "max"],
            &lat,
        );
    }
    let thr: Vec<Vec<String>> = all
        .iter()
        .map(|r| {
            let t = &r.throughput;
            vec![
                r.name.clone(),
                opt(t.overall_per_min),
                opt(t.steady_per_min),
                format!("{:?}", t.prover_busy),
                format!("{:?}", t.node_share),
            ]
        })
        .collect();
    out.push_str("### Throughput (votes/min)\n\n");
    table(
        out,
        &["scenario", "overall", "steady", "prover busy", "node share"],
        &thr,
    );
    for (title, pick) in [
        (
            "Races",
            (|r: &ScenarioReport| r.races.clone()) as fn(&ScenarioReport) -> _,
        ),
        ("Chaos", |r| r.chaos.clone()),
    ] {
        let rows: Vec<Vec<String>> = all
            .iter()
            .flat_map(|r| pick(r).into_iter().map(|(k, v)| vec![r.name.clone(), k, v]))
            .collect();
        if !rows.is_empty() {
            let _ = writeln!(out, "### {title}\n");
            table(out, &["scenario", "name", "result"], &rows);
        }
    }
    let loss: Vec<Vec<String>> = all
        .iter()
        .flat_map(|r| {
            r.vote_loss.iter().map(|(n, v)| {
                vec![
                    r.name.clone(),
                    node_name(*n).into(),
                    v.bisections.to_string(),
                    v.isolated.to_string(),
                    v.held.to_string(),
                    v.refresh_overflow.to_string(),
                    v.guest_rejection.to_string(),
                    v.finalize_failed.to_string(),
                ]
            })
        })
        .collect();
    if !loss.is_empty() {
        out.push_str("### Vote loss\n\n");
        table(
            out,
            &[
                "scenario",
                "node",
                "bisections",
                "isolated",
                "held",
                "refresh_overflow",
                "guest_rejection",
                "finalize_failed",
            ],
            &loss,
        );
    }
    out.push_str("### Findings\n\n");
    let rows: Vec<Vec<String>> = all
        .iter()
        .flat_map(|r| &r.findings)
        .map(|f| {
            vec![
                f.scenario.clone(),
                f.tag.clone(),
                serde_json::to_value(f.kind)
                    .ok()
                    .and_then(|v| v.as_str().map(String::from))
                    .unwrap_or_default(),
                f.count.to_string(),
                f.detail.clone(),
            ]
        })
        .collect();
    if rows.is_empty() {
        out.push_str("None.\n\n");
    } else {
        table(
            out,
            &["scenario", "election", "kind", "count", "detail"],
            &rows,
        );
    }
    let spend: Vec<Vec<String>> = all
        .iter()
        .flat_map(|r| {
            r.spend
                .iter()
                .map(|(k, v)| vec![r.name.clone(), k.clone(), v.to_string()])
        })
        .collect();
    if !spend.is_empty() {
        out.push_str("### Spend (µETH)\n\n");
        table(out, &["scenario", "account", "spent"], &spend);
    }
}

fn write_pair(dir: &Path, stem: &str, json: &impl Serialize, md: &str) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    std::fs::write(
        dir.join(format!("{stem}.json")),
        serde_json::to_vec_pretty(json)?,
    )?;
    std::fs::write(dir.join(format!("{stem}.md")), md)?;
    Ok(())
}

/// `report.json` and `report.md` of one run.
pub fn write_run(dir: &Path, r: &ScenarioReport) -> Result<()> {
    let mut md = format!("# {}\n\n", r.name);
    if !r.params.is_empty() {
        for (k, v) in &r.params {
            let _ = writeln!(md, "- {k}: {v}");
        }
        md.push('\n');
    }
    md.push_str("## Audit\n\n");
    verdicts(&mut md, &[r]);
    let _ = writeln!(md, "Batches: {}\n", r.batches);
    for n in &r.audit.notes {
        let _ = writeln!(md, "- {n}");
    }
    md.push_str("\n## Details\n\n");
    details(&mut md, &[r]);
    write_pair(dir, "report", r, &md)
}

/// The campaign roll-up (`summary.json`, `summary.md`), rewritten after every scenario.
pub fn write_campaign(campaign: &Path, all: &[ScenarioReport]) -> Result<()> {
    let refs: Vec<&ScenarioReport> = all.iter().collect();
    let mut md =
        String::from("# Campaign summary\n\n## Ceiling (steady votes/min, mean batch)\n\n");
    md.push_str(&ceiling_table(all));
    md.push_str("## Audit verdicts\n\n");
    verdicts(&mut md, &refs);
    md.push_str("## Details\n\n");
    details(&mut md, &refs);
    write_pair(campaign, "summary", &all, &md)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_line_format() {
        let p = Progress {
            tag: "w1/R".into(),
            sent: 1830,
            planned: 2300,
            accepted: 1828,
            refused: 2,
            settled: 1500,
            errored: 0,
            open: 328,
            per_min: 61.0,
            tx: [9, 7],
            lost: [2, 1],
            spent_l1_ueth: [210, 180],
            base: 0.21,
            tip: 0.002,
            blob: 0.003,
        };
        assert_eq!(
            p.line(Duration::from_secs(5025)),
            "[bench 01:23:45 w1/R] sent 1830/2300 acc 1828 ref 2 | settled 1500 err 0 open 328 | 5m 61 v/min | \
             A tx 9 lost 2 | B tx 7 lost 1 | spent L1 A 210 B 180 µETH | fees 0.21+0.002 gwei, blob 0.003 gwei"
        );
    }

    fn report(name: &str, nf: u32, cells: &[(u64, f64, f64, bool)]) -> ScenarioReport {
        ScenarioReport {
            name: name.into(),
            cells: cells
                .iter()
                .map(|&(b, steady, mean, floor)| Cell {
                    nf,
                    b,
                    rate: CellRate {
                        mean_batch: mean,
                        steady_per_min: Some(steady),
                        floor,
                    },
                })
                .collect(),
            ..Default::default()
        }
    }

    #[test]
    fn ceiling_table_renders() {
        let t = ceiling_table(&[
            report("a", 4, &[(64, 40.0, 63.0, false), (128, 55.0, 100.0, true)]),
            report("b", 8, &[(64, 30.0, 64.0, false)]),
        ]);
        let lines: Vec<&str> = t.lines().collect();
        assert_eq!(lines[0], "| nf \\ B | 64 | 128 |");
        assert_eq!(lines[2], "| 4 | 40 (63) | 55 (100) floor |");
        assert_eq!(lines[3], "| 8 | 30 (64) | - |");
    }

    #[test]
    fn findings_from_events_and_audit() {
        let pid = || "p".to_string();
        let ev = vec![
            (
                NodeId::A,
                Kind::Bisect {
                    pid: pid(),
                    votes: 8,
                    cap: 4,
                },
            ),
            (NodeId::A, Kind::Isolated { pid: pid(), vid: 7 }),
            (
                NodeId::A,
                Kind::Held {
                    pid: pid(),
                    error: "check".into(),
                },
            ),
            (
                NodeId::A,
                Kind::RefreshOverflow {
                    pid: pid(),
                    votes: 3,
                },
            ),
            (
                NodeId::A,
                Kind::GuestRejected {
                    pid: pid(),
                    mask: "0x3".into(),
                },
            ),
            (
                NodeId::A,
                Kind::FinalizeFailed {
                    pid: pid(),
                    error: "x".into(),
                },
            ),
        ];
        let mut audit = AuditResult::default();
        audit.unexpected.insert("weird".into(), 2);
        audit.errors.insert("weird".into(), 2);
        audit.stale_errors.insert(NodeId::B, 1);
        let f = findings("s", "e1", &audit, &ev);
        let get = |k| {
            f.iter()
                .find(|x| x.kind == k)
                .unwrap_or_else(|| panic!("{k:?}"))
        };
        assert_eq!(get(FindingKind::UnexpectedError).count, 2);
        assert_eq!(get(FindingKind::UnexpectedError).detail, "weird");
        assert_eq!(get(FindingKind::StaleError).detail, "B");
        for k in [
            FindingKind::Bisections,
            FindingKind::Isolated,
            FindingKind::Held,
            FindingKind::GuestRejection,
            FindingKind::ResultsFailed,
        ] {
            assert_eq!(get(k).count, 1, "{k:?}");
        }
        assert_eq!(get(FindingKind::RefreshOverflow).count, 3);
        assert!(get(FindingKind::Isolated).detail.contains("A:"));
        assert_eq!(f.len(), 8);
        // Results not ok with no event is still one results failure.
        let f = findings("s", "e1", &audit, &[]);
        assert_eq!(
            f.iter()
                .filter(|x| x.kind == FindingKind::ResultsFailed)
                .count(),
            1
        );

        let loss = vote_loss(&ev);
        assert_eq!(
            loss[&NodeId::A],
            VoteLoss {
                bisections: 1,
                isolated: 1,
                held: 1,
                refresh_overflow: 1,
                guest_rejection: 1,
                finalize_failed: 1
            }
        );
        let d = tempfile::tempdir().unwrap();
        let r = ScenarioReport {
            name: "s".into(),
            audit,
            vote_loss: loss,
            findings: findings("s", "e1", &AuditResult::default(), &ev),
            ..Default::default()
        };
        write_run(d.path(), &r).unwrap();
        let md = std::fs::read_to_string(d.path().join("report.md")).unwrap();
        assert!(
            md.contains("### Findings") && md.contains("refresh_overflow"),
            "{md}"
        );
        assert!(d.path().join("report.json").exists());
        write_campaign(d.path(), &[r]).unwrap();
        assert!(d.path().join("summary.md").exists());
    }
}
