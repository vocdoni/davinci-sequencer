//! The vote ledger: one JSONL line per send, append-only, and the per-vote
//! view the audit classifies.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail, ensure};
use davinci_client::api::{ProcessId, parse_vote_id};
use serde::{Deserialize, Serialize};

use super::plan::NodeId;

#[cfg(test)]
use davinci_client::api::vote_id_hex;

/// How one send ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Accepted,
    /// The code of a definitive refusal.
    Refused(u32),
    /// The vote may or may not have been admitted.
    Uncertain,
}

/// What to do after one try.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Attempt {
    Done(Outcome),
    /// The node is busy and the vote is not in: try again after a backoff.
    Retry,
    /// No definitive answer: the vote may be in. Send it again.
    Resend,
}

/// Maps one try's answer. `resent` is whether an earlier try of this send
/// was resent.
///
/// A refusal on a resend is uncertain: the first attempt may have admitted
/// the vote and the retry met a later condition (41201 after the end, 40002
/// on a replay). Only 40901 on a resend means the first attempt got in; 429
/// and 40902 are retried whether or not a try was resent.
pub fn judge(r: &Result<(), davinci_client::Error>, resent: bool) -> Attempt {
    use davinci_client::Error as E;
    let (status, code) = match r {
        Ok(()) => return Attempt::Done(Outcome::Accepted),
        Err(E::Api { status, code, .. }) => (*status, *code),
        // A transport failure, or an answer that did not decode: the request
        // may have reached the node.
        Err(_) => return Attempt::Resend,
    };
    match (status, code) {
        (_, Some(40901)) if resent => Attempt::Done(Outcome::Accepted),
        (_, Some(40901)) => Attempt::Done(Outcome::Refused(40901)),
        // The actor checks the vote id before the slot depth, so a 40902
        // always means the vote is not in.
        (_, Some(40902)) | (429, _) => Attempt::Retry,
        (408, _) => Attempt::Resend,
        (s, _) if s >= 500 => Attempt::Resend,
        (400..=499, _) if resent => Attempt::Done(Outcome::Uncertain),
        (400..=499, c) => Attempt::Done(Outcome::Refused(code_of(status, c))),
        // Any other status the client flagged is no definitive answer.
        _ => Attempt::Resend,
    }
}

/// The API code, or `status * 100` for an answer without one (a proxy's).
pub fn code_of(status: u16, code: Option<u32>) -> u32 {
    code.unwrap_or(u32::from(status) * 100)
}

/// One send, as written to `ledger.jsonl`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Line {
    /// Seconds from the run's start to the first try.
    pub t: f64,
    pub pid: ProcessId,
    /// `0x` + 16 hex digits.
    pub vid: String,
    pub tag: String,
    pub voter: u32,
    pub round: u8,
    pub node: NodeId,
    /// `accepted`, `refused` or `uncertain`.
    pub res: String,
    /// The final answer's code; none for a plain success or a transport error.
    pub code: Option<u32>,
    pub tries: u32,
    /// From the first try to the final answer.
    pub ms: u64,
}

/// Every send of one vote id.
#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    pub pid: ProcessId,
    pub tag: String,
    pub voter: u32,
    pub round: u8,
    pub accepted: BTreeSet<NodeId>,
    pub uncertain: BTreeSet<NodeId>,
    pub refused: Vec<(NodeId, u32)>,
    /// Submit time (`t`) of the earliest accepted send.
    pub first_accept: Option<f64>,
}

impl Entry {
    /// An entry with no sends, for tests.
    #[cfg(test)]
    pub fn test() -> Entry {
        Entry {
            pid: ProcessId([0; 31]),
            tag: "T".into(),
            voter: 0,
            round: 1,
            accepted: BTreeSet::new(),
            uncertain: BTreeSet::new(),
            refused: Vec::new(),
            first_accept: None,
        }
    }
}

/// Adds one line to the per-vote map. A vote id seen with another identity
/// is an error: merging two ballots would hide one from the audit.
fn apply(map: &mut BTreeMap<u64, Entry>, l: &Line) -> Result<()> {
    let vid = parse_vote_id(&l.vid).map_err(|e| anyhow!("vote id {:?}: {e}", l.vid))?;
    let e = map.entry(vid).or_insert_with(|| Entry {
        pid: l.pid,
        tag: l.tag.clone(),
        voter: l.voter,
        round: l.round,
        accepted: BTreeSet::new(),
        uncertain: BTreeSet::new(),
        refused: Vec::new(),
        first_accept: None,
    });
    ensure!(
        (e.pid, e.tag.as_str(), e.voter, e.round) == (l.pid, l.tag.as_str(), l.voter, l.round),
        "vote id {} already belongs to {} voter {} round {}",
        l.vid,
        e.tag,
        e.voter,
        e.round
    );
    match l.res.as_str() {
        "accepted" => {
            e.accepted.insert(l.node);
            e.first_accept = Some(e.first_accept.map_or(l.t, |t| t.min(l.t)));
        }
        "uncertain" => {
            e.uncertain.insert(l.node);
        }
        "refused" => e.refused.push((l.node, l.code.unwrap_or(0))),
        r => bail!("vote id {}: unknown result {r:?}", l.vid),
    }
    Ok(())
}

/// Lines between flushes, and the longest a line waits to be flushed.
const FLUSH_LINES: usize = 100;
const FLUSH_EVERY: Duration = Duration::from_secs(1);

struct Inner {
    out: BufWriter<File>,
    map: BTreeMap<u64, Entry>,
    unflushed: usize,
    last_flush: Instant,
    /// A failed background flush, reported by the next append.
    failed: Option<String>,
}

impl Inner {
    fn flush(&mut self) -> Result<()> {
        self.out.flush().context("flush the ledger")?;
        self.unflushed = 0;
        self.last_flush = Instant::now();
        Ok(())
    }
}

/// The append-only `ledger.jsonl` and its in-memory map. Lines are flushed
/// every 100 lines or 1 s, so a crash loses at most the last second.
pub struct Ledger {
    inner: Arc<Mutex<Inner>>,
}

impl Ledger {
    /// A new ledger; an existing file is never reopened, since its entries
    /// would be missing from the map.
    pub fn create(path: &Path) -> Result<Ledger> {
        let f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .with_context(|| format!("create ledger {}", path.display()))?;
        let inner = Arc::new(Mutex::new(Inner {
            out: BufWriter::new(f),
            map: BTreeMap::new(),
            unflushed: 0,
            last_flush: Instant::now(),
            failed: None,
        }));
        let weak: Weak<Mutex<Inner>> = Arc::downgrade(&inner);
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(FLUSH_EVERY);
                let Some(i) = weak.upgrade() else { return };
                let Ok(mut g) = i.lock() else { return };
                if g.unflushed > 0
                    && let Err(e) = g.flush()
                {
                    g.failed = Some(format!("{e:#}"));
                }
            }
        });
        Ok(Ledger { inner })
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Inner>> {
        self.inner
            .lock()
            .map_err(|_| anyhow!("ledger lock poisoned"))
    }

    pub fn append(&self, l: Line) -> Result<()> {
        let mut g = self.lock()?;
        if let Some(e) = g.failed.take() {
            bail!("ledger flush failed: {e}");
        }
        apply(&mut g.map, &l)?;
        let mut s = serde_json::to_vec(&l)?;
        s.push(b'\n');
        g.out.write_all(&s).context("write the ledger")?;
        g.unflushed += 1;
        if g.unflushed >= FLUSH_LINES || g.last_flush.elapsed() >= FLUSH_EVERY {
            g.flush()?;
        }
        Ok(())
    }

    pub fn flush(&self) -> Result<()> {
        self.lock()?.flush()
    }

    /// A poisoned lock is an error: an empty map would read as no votes.
    pub fn entries(&self) -> Result<BTreeMap<u64, Entry>> {
        Ok(self.lock()?.map.clone())
    }

    /// The entries of a ledger file. A last line without its newline is a
    /// write cut short by a crash and is ignored unless it parses; any
    /// other bad line is an error.
    pub fn replay(path: &Path) -> Result<BTreeMap<u64, Entry>> {
        let body = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
        let mut map = BTreeMap::new();
        let mut lines: Vec<&[u8]> = body.split(|b| *b == b'\n').collect();
        // `split` yields the part after the last newline: empty when the
        // file ends cleanly.
        let tail = lines.pop().unwrap_or_default();
        for (i, l) in lines.iter().enumerate() {
            let line: Line = serde_json::from_slice(l)
                .with_context(|| format!("{}: line {}", path.display(), i + 1))?;
            apply(&mut map, &line)
                .with_context(|| format!("{}: line {}", path.display(), i + 1))?;
        }
        if let Ok(line) = serde_json::from_slice::<Line>(tail) {
            apply(&mut map, &line)?;
        }
        Ok(map)
    }
}

impl Drop for Ledger {
    fn drop(&mut self) {
        if let Ok(mut g) = self.inner.lock() {
            let _ = g.flush();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bench::plan::NodeId::{A, B};

    #[test]
    fn judge_maps_every_answer() {
        use davinci_client::Error as E;
        let api = |status, code| {
            Err(E::Api {
                status,
                code: Some(code),
                message: String::new(),
            })
        };
        assert_eq!(judge(&Ok(()), false), Attempt::Done(Outcome::Accepted));
        assert_eq!(judge(&api(429, 42903), false), Attempt::Retry);
        assert_eq!(judge(&api(409, 40902), true), Attempt::Retry); // never ours: vid is checked first
        assert_eq!(
            judge(&api(409, 40901), true),
            Attempt::Done(Outcome::Accepted)
        );
        assert_eq!(
            judge(&api(409, 40901), false),
            Attempt::Done(Outcome::Refused(40901))
        );
        assert_eq!(
            judge(&api(412, 41201), false),
            Attempt::Done(Outcome::Refused(41201))
        );
        assert_eq!(
            judge(&api(400, 40002), false),
            Attempt::Done(Outcome::Refused(40002))
        );
        assert_eq!(judge(&api(408, 40801), false), Attempt::Resend);
        assert_eq!(judge(&Err(E::Http("reset".into())), false), Attempt::Resend);
        assert_eq!(judge(&api(503, 50001), false), Attempt::Resend);
        // A refusal on a resend is uncertain: the first attempt may have admitted the vote and the
        // retry met a later condition (41201 after the end, 42903, 40002 on a replay). Only 40901 on
        // a resend means the first attempt got in.
        assert_eq!(
            judge(&api(412, 41201), true),
            Attempt::Done(Outcome::Uncertain)
        );
        assert_eq!(judge(&api(429, 42903), true), Attempt::Retry);
        assert_eq!(
            judge(&api(400, 40002), true),
            Attempt::Done(Outcome::Uncertain)
        );
    }

    fn line(t: f64, vid: u64, node: NodeId, res: &str, code: Option<u32>) -> Line {
        Line {
            t,
            pid: ProcessId([3; 31]),
            vid: vote_id_hex(vid),
            tag: "R1".into(),
            voter: (vid & 0xff) as u32,
            round: 1,
            node,
            res: res.into(),
            code,
            tries: 1,
            ms: 12,
        }
    }

    #[test]
    fn ledger_replays_and_ignores_a_torn_line() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("ledger.jsonl");
        let l = Ledger::create(&p).unwrap();
        let (v1, v2) = (1 << 63 | 7, 1 << 63 | 8);
        l.append(line(1.0, v1, A, "accepted", None)).unwrap();
        l.append(line(1.5, v1, B, "refused", Some(40901))).unwrap();
        l.append(line(2.0, v2, A, "uncertain", Some(40801)))
            .unwrap();
        l.append(line(3.0, v2, A, "accepted", Some(40901))).unwrap();
        l.flush().unwrap();
        // A crash mid-write leaves half a line.
        let torn = serde_json::to_string(&line(4.0, 1 << 63 | 9, B, "accepted", None)).unwrap();
        let mut f = std::fs::OpenOptions::new().append(true).open(&p).unwrap();
        std::io::Write::write_all(&mut f, &torn.as_bytes()[..torn.len() / 2]).unwrap();
        drop(f);

        let e = l.entries().unwrap();
        assert_eq!(e.len(), 2);
        assert_eq!(e[&v1].accepted, [A].into());
        assert_eq!(e[&v1].refused, vec![(B, 40901)]);
        assert_eq!(e[&v1].first_accept, Some(1.0));
        assert_eq!(e[&v2].uncertain, [A].into());
        assert_eq!(e[&v2].accepted, [A].into());
        assert_eq!(e[&v2].first_accept, Some(3.0));
        assert_eq!(Ledger::replay(&p).unwrap(), e);
        // A ledger is never reopened over an old one.
        assert!(Ledger::create(&p).is_err());
        // A damaged line before the last is an error, not a skipped vote.
        let body = std::fs::read_to_string(&p).unwrap();
        std::fs::write(&p, body.replacen("accepted", "acc\"epted", 1)).unwrap();
        assert!(Ledger::replay(&p).is_err());
    }
}
