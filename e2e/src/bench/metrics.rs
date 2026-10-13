//! Log events, rates and percentiles for the benchmark's reports. Pure,
//! except [`LogTail`], which reads a node log incrementally.

use std::collections::BTreeMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Seconds since the epoch of an RFC 3339 UTC time, as the prover's chrono
/// writes it (`2026-09-28T10:34:32.105321716Z`).
pub fn rfc3339(s: &str) -> Option<f64> {
    let s = s.strip_suffix('Z').or_else(|| s.strip_suffix("+00:00"))?;
    let (date, time) = s.split_once('T')?;
    let mut d = date.splitn(3, '-').map(|x| x.parse::<i64>().ok());
    let (y, m, day) = (d.next()??, d.next()??, d.next()??);
    let (hms, frac) = time.split_once('.').unwrap_or((time, ""));
    let mut t = hms.splitn(3, ':').map(|x| x.parse::<i64>().ok());
    let (h, mi, se) = (t.next()??, t.next()??, t.next()??);
    let frac: f64 = if frac.is_empty() {
        0.0
    } else {
        format!("0.{frac}").parse().ok()?
    };
    // Days from the civil date (Hinnant).
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * ((m + 9) % 12) + 2) / 5 + day - 1;
    let days = era * 146_097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719_468;
    Some((days * 86_400 + h * 3600 + mi * 60 + se) as f64 + frac)
}

/// One line of a node log.
#[derive(Clone, Debug, PartialEq)]
pub struct LogEvent {
    /// Unix seconds.
    pub at: f64,
    pub level: String,
    pub msg: String,
    pub fields: BTreeMap<String, String>,
}

fn is_ident(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '.'
}

/// `<rfc3339>  LEVEL msg k=v ...`. A value with spaces runs to the next
/// ` key=` or the end of the line.
pub fn parse_line(l: &str) -> Option<LogEvent> {
    let (ts, rest) = l.trim().split_once(char::is_whitespace)?;
    let at = rfc3339(ts)?;
    let rest = rest.trim_start();
    let (level, rest) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
    if !matches!(level, "TRACE" | "DEBUG" | "INFO" | "WARN" | "ERROR") {
        return None;
    }
    let mut rest = rest.trim_start();
    // `module::path: message`
    if let Some((target, tail)) = rest.split_once(": ")
        && target.contains("::")
        && !target.contains(char::is_whitespace)
    {
        rest = tail;
    }
    // Start of every ` key=` token.
    let mut starts = Vec::new();
    for (i, c) in rest.char_indices() {
        if i > 0 && !rest[..i].ends_with(' ') {
            continue;
        }
        if !c.is_ascii_alphabetic() && c != '_' {
            continue;
        }
        let key_len = rest[i..]
            .find(|c: char| !is_ident(c))
            .unwrap_or(rest.len() - i);
        if rest[i + key_len..].starts_with('=') {
            starts.push((i, i + key_len));
        }
    }
    let msg_end = starts.first().map_or(rest.len(), |s| s.0);
    let mut fields = BTreeMap::new();
    for (n, &(s, k_end)) in starts.iter().enumerate() {
        let end = starts.get(n + 1).map_or(rest.len(), |x| x.0);
        let v = rest[k_end + 1..end].trim();
        let v = v
            .strip_prefix('"')
            .and_then(|v| v.strip_suffix('"'))
            .unwrap_or(v);
        fields.insert(rest[s..k_end].to_string(), v.to_string());
    }
    Some(LogEvent {
        at,
        level: level.into(),
        msg: rest[..msg_end].trim().into(),
        fields,
    })
}

/// Follows a log file, returning only whole lines that are new.
pub struct LogTail {
    path: PathBuf,
    offset: u64,
}

impl LogTail {
    pub fn new(p: &Path) -> Self {
        LogTail {
            path: p.to_path_buf(),
            offset: 0,
        }
    }

    /// Events appended since the last call. A missing file, a partial last
    /// line or a truncated file yields nothing (a truncation restarts).
    pub fn poll(&mut self) -> Vec<LogEvent> {
        let Ok(mut f) = std::fs::File::open(&self.path) else {
            return Vec::new();
        };
        let len = f.metadata().map_or(0, |m| m.len());
        if len < self.offset {
            self.offset = 0;
        }
        if f.seek(SeekFrom::Start(self.offset)).is_err() {
            return Vec::new();
        }
        let mut buf = Vec::new();
        if f.read_to_end(&mut buf).is_err() {
            return Vec::new();
        }
        let Some(end) = buf.iter().rposition(|b| *b == b'\n') else {
            return Vec::new();
        };
        self.offset += end as u64 + 1;
        String::from_utf8_lossy(&buf[..end])
            .lines()
            .filter_map(parse_line)
            .collect()
    }
}

/// What a log line tells the report.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Kind {
    Sealed {
        pid: String,
        votes: u64,
        reason: String,
    },
    Settled {
        pid: String,
        votes: u64,
        root: String,
    },
    Synced {
        pid: String,
        root: String,
    },
    LostEvent {
        pid: String,
    },
    LostRevert {
        pid: String,
    },
    Posted {
        nonce: u64,
        tx: String,
        blobs: u64,
    },
    PostRetry {
        pid: String,
        error: String,
    },
    Pause {
        pid: String,
        secs: u64,
    },
    RpcFailover {
        from: String,
        to: String,
    },
    BeaconFailover,
    /// A failed batch requeued at half size.
    Bisect {
        pid: String,
        votes: u64,
        cap: u64,
    },
    /// A single vote the prover rejects, errored.
    Isolated {
        pid: String,
        vid: u64,
    },
    /// A batch failing a node-side check, held pending.
    Held {
        pid: String,
        error: String,
    },
    /// Votes errored because the exposed refresh set outgrew a batch.
    RefreshOverflow {
        pid: String,
        votes: u64,
    },
    FinalizeFailed {
        pid: String,
        error: String,
    },
    GuestRejected {
        pid: String,
        mask: String,
    },
}

pub fn kind(e: &LogEvent) -> Option<Kind> {
    let f = |k: &str| e.fields.get(k).cloned().unwrap_or_default();
    let n = |k: &str| {
        e.fields
            .get(k)
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(0)
    };
    let pid = f("pid");
    let m = e.msg.as_str();
    Some(match m {
        "batch sealed" => Kind::Sealed {
            pid,
            votes: n("votes"),
            reason: f("reason"),
        },
        "transition settled" => Kind::Settled {
            pid,
            votes: n("votes"),
            root: f("root"),
        },
        "synced transition from another sequencer" => Kind::Synced {
            pid,
            root: f("root"),
        },
        "foreign transition beat the in-flight batch" => Kind::LostEvent { pid },
        "lost the settlement race, waiting for sync" => Kind::LostRevert { pid },
        "blobs posted on L1" => Kind::Posted {
            nonce: n("nonce"),
            tx: f("tx"),
            blobs: n("blobs"),
        },
        "L1 post failed; retrying" => Kind::PostRetry {
            pid,
            error: f("error"),
        },
        "settlement refused while the process is not open, requeueing" => Kind::Pause {
            pid,
            secs: n("retry_in"),
        },
        "RPC failover" => Kind::RpcFailover {
            from: f("from"),
            to: f("to"),
        },
        "beacon failover" => Kind::BeaconFailover,
        "batch proof failed; requeued for bisection" => Kind::Bisect {
            pid,
            votes: n("votes"),
            cap: n("cap"),
        },
        "vote isolated by bisection; errored" => {
            let vids = f("vids");
            let vid = vids
                .trim_matches(|c| c == '[' || c == ']')
                .split(',')
                .find_map(|x| x.trim().parse().ok())
                .unwrap_or(0);
            Kind::Isolated { pid, vid }
        }
        "batch failed a node-side check; votes held pending" => Kind::Held {
            pid,
            error: f("msg"),
        },
        "exposed refresh set too large; cleared, its votes must recast" => Kind::RefreshOverflow {
            pid,
            votes: n("votes"),
        },
        "batch failed in the guest" => {
            let msg = f("msg");
            let mask = msg
                .rsplit_once("(mask ")
                .map(|(_, t)| t.trim_end_matches(')').to_string())
                .unwrap_or_default();
            Kind::GuestRejected { pid, mask }
        }
        "results_request failed" | "dkg_results_inputs failed" => {
            Kind::FinalizeFailed { pid, error: f("e") }
        }
        _ => return None,
    })
}

/// Nearest-rank percentiles (`ps` in 0..=100) of `v`; empty input gives NaN.
pub fn percentiles(v: &[f64], ps: &[f64]) -> Vec<f64> {
    let mut s = v.to_vec();
    s.sort_by(f64::total_cmp);
    ps.iter()
        .map(|p| {
            if s.is_empty() {
                return f64::NAN;
            }
            let rank = (p * s.len() as f64 / 100.0).ceil() as usize;
            s[rank.clamp(1, s.len()) - 1]
        })
        .collect()
}

/// Votes per minute over the landings after the first, from the first
/// landing's time to the last. `(unix seconds, votes)`.
pub fn steady_rate(landings: &[(f64, u64)]) -> Option<f64> {
    let first = landings.first()?;
    let last = landings.last()?;
    let span = last.0 - first.0;
    if landings.len() < 2 || span <= 0.0 {
        return None;
    }
    let votes: u64 = landings[1..].iter().map(|l| l.1).sum();
    Some(votes as f64 / span * 60.0)
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CellRate {
    pub mean_batch: f64,
    pub steady_per_min: Option<f64>,
    /// The median batch fell short of 0.9·B: the grace floor, not the
    /// prover, bounded the cell.
    pub floor: bool,
}

/// The steady rate over batches of at least 0.9·`b` votes.
pub fn cell_rate(landings: &[(f64, u64)], b: u64) -> CellRate {
    let sizes: Vec<f64> = landings.iter().map(|l| l.1 as f64).collect();
    let mean_batch = if sizes.is_empty() {
        0.0
    } else {
        sizes.iter().sum::<f64>() / sizes.len() as f64
    };
    let min = 0.9 * b as f64;
    let full: Vec<(f64, u64)> = landings
        .iter()
        .copied()
        .filter(|l| l.1 as f64 >= min)
        .collect();
    let median = percentiles(&sizes, &[50.0])[0];
    CellRate {
        mean_batch,
        steady_per_min: steady_rate(&full),
        floor: sizes.is_empty() || median < min,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(l: &str) -> (LogEvent, Kind) {
        let e = parse_line(l).unwrap_or_else(|| panic!("unparsed: {l}"));
        let k = kind(&e).unwrap_or_else(|| panic!("no kind: {l}"));
        (e, k)
    }

    #[test]
    fn parses_real_log_lines() {
        let pid = "f39f0000000000000000000000000000000000000000000000000000000000";
        let (e, k) = ev(&format!(
            "2026-10-13T17:24:24.614943Z  INFO batch sealed pid={pid} votes=225 overwrites=0 reason=at_max"
        ));
        assert!((e.at - (rfc3339("2026-10-13T17:24:24Z").unwrap() + 0.614943)).abs() < 1e-6);
        assert_eq!(
            k,
            Kind::Sealed {
                pid: pid.into(),
                votes: 225,
                reason: "at_max".into()
            }
        );
        let root = "0x69000000000000000000000000000000000000000000000000000000000cfd5c";
        let (_, k) = ev(&format!(
            "2026-10-13T17:25:36.321979Z  INFO transition settled pid={pid} votes=225 overwrites=0 root={root}"
        ));
        assert_eq!(
            k,
            Kind::Settled {
                pid: pid.into(),
                votes: 225,
                root: root.into()
            }
        );
        let tx = "0xf126000000000000000000000000000000000000000000000000000000008800";
        let (_, k) = ev(&format!(
            "2026-10-13T17:24:29.845987Z  INFO blobs posted on L1 nonce=0 tx={tx} block=98 blobs=2"
        ));
        assert_eq!(
            k,
            Kind::Posted {
                nonce: 0,
                tx: tx.into(),
                blobs: 2
            }
        );
        let (_, k) = ev(&format!(
            "2026-10-13T17:57:53.963997Z  WARN foreign transition beat the in-flight batch pid={pid}"
        ));
        assert_eq!(k, Kind::LostEvent { pid: pid.into() });
        let pid1 = "f39f0000000000000000000000000000000000000000000000000000000001";
        let root = "0xd2ef000000000000000000000000000000000000000000000000000000000001";
        let (_, k) = ev(&format!(
            "2026-10-13T17:55:29.977773Z  INFO synced transition from another sequencer pid={pid1} votes=3 root={root}"
        ));
        assert_eq!(
            k,
            Kind::Synced {
                pid: pid1.into(),
                root: root.into()
            }
        );
        let (_, k) = ev(&format!(
            "2026-10-13T17:57:54.000001Z  WARN lost the settlement race, waiting for sync pid={pid}"
        ));
        assert_eq!(k, Kind::LostRevert { pid: pid.into() });
        let (_, k) =
            ev("2026-10-13T17:57:54.000001Z  WARN RPC failover from=127.0.0.1:1 to=127.0.0.1:2");
        assert_eq!(
            k,
            Kind::RpcFailover {
                from: "127.0.0.1:1".into(),
                to: "127.0.0.1:2".into()
            }
        );
        let (_, k) = ev("2026-10-13T17:57:54.000001Z  WARN beacon failover error=timed out");
        assert_eq!(k, Kind::BeaconFailover);
        let (_, k) = ev(&format!(
            "2026-10-13T17:57:54.000001Z  WARN L1 post failed; retrying pid={pid} chunk=0 error=replacement transaction underpriced: a b"
        ));
        assert_eq!(
            k,
            Kind::PostRetry {
                pid: pid.into(),
                error: "replacement transaction underpriced: a b".into()
            }
        );
        let (_, k) = ev(&format!(
            "2026-10-13T17:57:54.000001Z  INFO settlement refused while the process is not open, requeueing pid={pid} name=InvalidStatus retry_in=12"
        ));
        assert_eq!(
            k,
            Kind::Pause {
                pid: pid.into(),
                secs: 12
            }
        );
    }

    #[test]
    fn parses_loss_lines() {
        let pid = "f39f0000000000000000000000000000000000000000000000000000000000";
        let (_, k) = ev(&format!(
            "2026-10-13T18:00:00.000000Z ERROR batch proof failed; requeued for bisection pid={pid} votes=8 cap=4 retry_in=5 reason=prover: boom"
        ));
        assert_eq!(
            k,
            Kind::Bisect {
                pid: pid.into(),
                votes: 8,
                cap: 4
            }
        );
        let (_, k) = ev(&format!(
            "2026-10-13T18:00:00.000000Z ERROR vote isolated by bisection; errored pid={pid} vids=[12345] msg=prover: boom (isolated by bisection)"
        ));
        assert_eq!(
            k,
            Kind::Isolated {
                pid: pid.into(),
                vid: 12345
            }
        );
        let (_, k) = ev(&format!(
            "2026-10-13T18:00:00.000000Z ERROR batch failed a node-side check; votes held pending pid={pid} votes=9 retry_in=30 msg=check failed: root"
        ));
        assert_eq!(
            k,
            Kind::Held {
                pid: pid.into(),
                error: "check failed: root".into()
            }
        );
        let (_, k) = ev(&format!(
            "2026-10-13T18:00:00.000000Z ERROR exposed refresh set too large; cleared, its votes must recast pid={pid} slots=700 votes=3"
        ));
        assert_eq!(
            k,
            Kind::RefreshOverflow {
                pid: pid.into(),
                votes: 3
            }
        );
        let (_, k) = ev(&format!(
            "2026-10-13T18:00:00.000000Z ERROR batch failed in the guest pid={pid} msg=guest rejected the batch: ok, census (mask 0x3)"
        ));
        assert_eq!(
            k,
            Kind::GuestRejected {
                pid: pid.into(),
                mask: "0x3".into()
            }
        );
        for m in ["results_request failed", "dkg_results_inputs failed"] {
            let (_, k) = ev(&format!(
                "2026-10-13T18:00:00.000000Z ERROR {m} pid={pid} e=no secret key"
            ));
            assert_eq!(
                k,
                Kind::FinalizeFailed {
                    pid: pid.into(),
                    error: "no secret key".into()
                }
            );
        }
        assert!(parse_line("not a log line").is_none());
    }

    #[test]
    fn tail_returns_new_whole_lines() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("n.log");
        let mut t = LogTail::new(&p);
        assert!(t.poll().is_empty());
        std::fs::write(
            &p,
            "2026-10-13T17:24:24.614943Z  INFO batch sealed pid=a votes=1 reason=timer\n2026-10-13T17:24:25Z  INFO batch sea",
        )
        .unwrap();
        assert_eq!(t.poll().len(), 1);
        assert!(t.poll().is_empty());
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new().append(true).open(&p).unwrap();
        writeln!(f, "led pid=a votes=2 reason=timer").unwrap();
        let v = t.poll();
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].fields["votes"], "2");
    }

    #[test]
    fn percentiles_and_steady_rate() {
        let v: Vec<f64> = (1..=100).map(f64::from).collect();
        assert_eq!(percentiles(&v, &[50.0, 90.0, 99.0]), vec![50.0, 90.0, 99.0]);
        assert_eq!(
            steady_rate(&[(0.0, 100), (60.0, 100), (120.0, 100)]),
            Some(100.0)
        );
        assert_eq!(steady_rate(&[(0.0, 100)]), None);
        let l = [(0.0, 128), (60.0, 128), (90.0, 40), (120.0, 128)];
        let c = cell_rate(&l, 128);
        assert_eq!(c.mean_batch, 106.0);
        assert_eq!(c.steady_per_min, Some(128.0));
        assert!(!c.floor);
        let c = cell_rate(&[(0.0, 100), (60.0, 100), (120.0, 100)], 128);
        assert!(c.floor);
    }
}
