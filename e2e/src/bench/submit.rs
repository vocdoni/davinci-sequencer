//! Sends ballots to the nodes on schedule, per route, with retries, and
//! writes one ledger line per send.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use davinci_client::SequencerClient;
use davinci_client::api::vote_id_hex;
use sha2::{Digest, Sha256};
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

use super::ballots::BallotRecord;
use super::ledger::{Attempt, Ledger, Line, Outcome, code_of, judge};
use super::plan::NodeId;

/// Which node gets a ballot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Route {
    /// A hash of `(tag, voter)` picks the node; every round of a voter goes
    /// there, as `docs/api.md` asks of clients.
    ByVoter,
    Only(NodeId),
    /// Both nodes at the same moment.
    Both,
    /// Odd rounds to the voter's `ByVoter` node, even rounds to the other.
    Cross,
}

fn other(n: NodeId) -> NodeId {
    match n {
        NodeId::A => NodeId::B,
        NodeId::B => NodeId::A,
    }
}

fn home(tag: &str, voter: u32) -> NodeId {
    let h = Sha256::new()
        .chain_update(b"davinci-bench/v1/route")
        .chain_update([0])
        .chain_update(tag)
        .chain_update([0])
        .chain_update(voter.to_be_bytes())
        .finalize();
    if h[0] & 1 == 0 { NodeId::A } else { NodeId::B }
}

pub fn targets(route: &Route, tag: &str, voter: u32, round: u8) -> Vec<NodeId> {
    match route {
        Route::ByVoter => vec![home(tag, voter)],
        Route::Only(n) => vec![*n],
        Route::Both => vec![NodeId::A, NodeId::B],
        Route::Cross if round % 2 == 1 => vec![home(tag, voter)],
        Route::Cross => vec![other(home(tag, voter))],
    }
}

/// In-flight requests per node.
const IN_FLIGHT: usize = 64;
const BACKOFF_MIN: Duration = Duration::from_millis(50);
const BACKOFF_MAX: Duration = Duration::from_secs(2);
/// A send still unanswered after this is recorded as it stands.
const GIVE_UP: Duration = Duration::from_secs(600);
/// How often a paused run checks whether it may send again.
const PAUSE_POLL: Duration = Duration::from_millis(250);

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SubmitStats {
    /// Sends, one per ballot and target node (a `Both` ballot is two).
    pub sent: usize,
    pub accepted: usize,
    pub refused: BTreeMap<u32, usize>,
    pub uncertain: usize,
}

pub struct Submitter {
    clients: [SequencerClient; 2],
    ledger: Arc<Ledger>,
    slots: [Arc<Semaphore>; 2],
    paused: Arc<AtomicBool>,
}

fn idx(n: NodeId) -> usize {
    match n {
        NodeId::A => 0,
        NodeId::B => 1,
    }
}

/// How one send ended: its outcome, the final answer's code and the tries.
async fn send(c: &SequencerClient, rec: &BallotRecord) -> (Outcome, Option<u32>, u32) {
    let start = Instant::now();
    let mut backoff = BACKOFF_MIN;
    let (mut tries, mut resent) = (0u32, false);
    loop {
        tries += 1;
        let r = c.submit_vote(&rec.req).await;
        let code = match &r {
            Err(davinci_client::Error::Api { status, code, .. }) => Some(code_of(*status, *code)),
            _ => None,
        };
        match judge(&r, resent) {
            Attempt::Done(o) => return (o, code, tries),
            Attempt::Retry => {}
            Attempt::Resend => resent = true,
        }
        if start.elapsed() >= GIVE_UP {
            // Only busy answers (429, 40902) and resends get here: a
            // resent vote may be in, a busy one is not.
            let o = if resent {
                Outcome::Uncertain
            } else {
                Outcome::Refused(code.unwrap_or(0))
            };
            return (o, code, tries);
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(BACKOFF_MAX);
    }
}

impl Submitter {
    pub fn new(a: &str, b: &str, ledger: Arc<Ledger>) -> Submitter {
        Submitter {
            clients: [SequencerClient::new(a), SequencerClient::new(b)],
            ledger,
            slots: [
                Arc::new(Semaphore::new(IN_FLIGHT)),
                Arc::new(Semaphore::new(IN_FLIGHT)),
            ],
            paused: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Holds back sends not yet started (the fee gate).
    pub fn pause(&self, on: bool) {
        self.paused.store(on, Ordering::SeqCst);
    }

    /// Sends each ballot at `t0 + offset` to its route's nodes and waits for
    /// every send to end. `plan` is in offset order.
    pub async fn run(
        &self,
        plan: Vec<(Duration, BallotRecord)>,
        route: Route,
        t0: Instant,
    ) -> Result<SubmitStats> {
        let mut set: JoinSet<Result<Outcome>> = JoinSet::new();
        let mut stats = SubmitStats::default();
        let mut first_err: Option<anyhow::Error> = None;
        let mut tally = |r: Result<Result<Outcome>, tokio::task::JoinError>,
                         stats: &mut SubmitStats| {
            match r.map_err(|e| anyhow!("send task: {e}")).and_then(|r| r) {
                Ok(o) => {
                    stats.sent += 1;
                    match o {
                        Outcome::Accepted => stats.accepted += 1,
                        Outcome::Refused(c) => *stats.refused.entry(c).or_default() += 1,
                        Outcome::Uncertain => stats.uncertain += 1,
                    }
                }
                Err(e) => {
                    first_err.get_or_insert(e);
                }
            }
        };
        for (at, rec) in plan {
            tokio::time::sleep_until(tokio::time::Instant::from_std(t0 + at)).await;
            while self.paused.load(Ordering::SeqCst) {
                tokio::time::sleep(PAUSE_POLL).await;
            }
            let rec = Arc::new(rec);
            for n in targets(&route, &rec.tag, rec.voter, rec.round) {
                let (c, slots, ledger, rec) = (
                    self.clients[idx(n)].clone(),
                    self.slots[idx(n)].clone(),
                    self.ledger.clone(),
                    rec.clone(),
                );
                // The permit is taken inside the task, so a node that is down
                // never holds back sends to the other.
                set.spawn(async move {
                    let _permit = slots.acquire_owned().await?;
                    let start = Instant::now();
                    let t = start.duration_since(t0).as_secs_f64();
                    let (o, code, tries) = send(&c, &rec).await;
                    let res = match o {
                        Outcome::Accepted => "accepted",
                        Outcome::Refused(_) => "refused",
                        Outcome::Uncertain => "uncertain",
                    };
                    ledger.append(Line {
                        t,
                        pid: rec.req.process_id,
                        vid: vote_id_hex(rec.req.vote_id),
                        tag: rec.tag.clone(),
                        voter: rec.voter,
                        round: rec.round,
                        node: n,
                        res: res.into(),
                        code,
                        tries,
                        ms: u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX),
                    })?;
                    Ok(o)
                });
            }
            while let Some(r) = set.try_join_next() {
                tally(r, &mut stats);
            }
        }
        while let Some(r) = set.join_next().await {
            tally(r, &mut stats);
        }
        self.ledger.flush()?;
        match first_err {
            Some(e) => Err(e),
            None => Ok(stats),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use super::*;
    use crate::bench::ballots::fake_req;
    use crate::bench::plan::NodeId::{A, B};

    #[test]
    fn routes_are_stable() {
        let mut on_a = 0;
        for v in 0..10_000u32 {
            let t = targets(&Route::ByVoter, "R1", v, 1);
            assert_eq!(t.len(), 1);
            for r in 2..=3 {
                assert_eq!(targets(&Route::ByVoter, "R1", v, r), t);
            }
            if t[0] == A {
                on_a += 1;
            }
            let (c1, c2) = (
                targets(&Route::Cross, "R1", v, 1),
                targets(&Route::Cross, "R1", v, 2),
            );
            assert_eq!((c1.len(), c2.len()), (1, 1));
            assert_ne!(c1, c2);
        }
        assert!((4800..=5200).contains(&on_a), "{on_a}");
        assert_eq!(targets(&Route::Both, "R1", 3, 1), vec![A, B]);
        assert_eq!(targets(&Route::Only(B), "R1", 3, 1), vec![B]);
    }

    /// A loopback node answering the n-th `POST /votes` with `script[n]`
    /// (the last one repeats), one request per connection.
    async fn scripted(script: Vec<(u16, &'static str)>) -> (String, Arc<AtomicUsize>) {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", l.local_addr().unwrap());
        let n = Arc::new(AtomicUsize::new(0));
        let seen = n.clone();
        tokio::spawn(async move {
            while let Ok((mut s, _)) = l.accept().await {
                let i = seen.fetch_add(1, Ordering::SeqCst);
                let (status, body) = script[i.min(script.len() - 1)];
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 1 << 16];
                    let mut got = Vec::new();
                    loop {
                        let n = s.read(&mut buf).await.unwrap_or(0);
                        got.extend_from_slice(&buf[..n]);
                        let text = String::from_utf8_lossy(&got).to_string();
                        if let Some((h, b)) = text.split_once("\r\n\r\n") {
                            let len = h
                                .lines()
                                .find_map(|l| l.strip_prefix("content-length: "))
                                .and_then(|v| v.parse::<usize>().ok())
                                .unwrap_or(0);
                            if b.len() >= len || n == 0 {
                                let resp = format!(
                                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                                    body.len()
                                );
                                let _ = s.write_all(resp.as_bytes()).await;
                                return;
                            }
                        }
                        if n == 0 {
                            return;
                        }
                    }
                });
            }
        });
        (url, n)
    }

    #[tokio::test]
    async fn submit_retries_then_records() {
        let (url, calls) = scripted(vec![
            (429, r#"{"error":"busy","code":42903}"#),
            (408, r#"{"error":"request timed out","code":40801}"#),
            (409, r#"{"error":"vote already submitted","code":40901}"#),
        ])
        .await;
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("ledger.jsonl");
        let ledger = Arc::new(Ledger::create(&p).unwrap());
        let s = Submitter::new(&url, "http://127.0.0.1:1", ledger.clone());
        let vid = 1 << 63 | 42;
        let rec = BallotRecord {
            tag: "R1".into(),
            voter: 4,
            round: 1,
            choices: vec![1, 0],
            req: fake_req(vid, [4; 20]),
        };
        let stats = s
            .run(vec![(Duration::ZERO, rec)], Route::Only(A), Instant::now())
            .await
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 3);
        assert_eq!((stats.sent, stats.accepted, stats.uncertain), (1, 1, 0));
        assert!(stats.refused.is_empty());
        let body = std::fs::read_to_string(&p).unwrap();
        let lines: Vec<Line> = body
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(lines.len(), 1);
        let l = &lines[0];
        assert_eq!((l.res.as_str(), l.tries, l.node), ("accepted", 3, A));
        assert_eq!((l.vid.clone(), l.code), (vote_id_hex(vid), Some(40901)));
        assert_eq!(ledger.entries()[&vid].accepted, [A].into());
    }
}
