//! Chaos for the live benchmark: when to inject (a pure trigger check over
//! what the run observed), what to inject (kills, proxy modes, knobs) and
//! how to undo it, with a row per injection for the report.

use std::time::{Duration, Instant};

use alloy::primitives::Address;
use alloy::providers::Provider;
use anyhow::{Context, Result, bail};

use super::metrics::Kind;
use super::plan::NodeId;
use super::rig::{Knobs, Rig, Role};
use crate::net::Net;
use crate::proxy::Proxy;

/// Which upstream list of a node a proxy fronts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rpc {
    Op,
    L1,
    Send,
    Beacon,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Trigger {
    /// From the scenario start.
    At(Duration),
    /// The node's pending Ethereum nonce is above its latest.
    L1Pending(NodeId),
    /// The node logged a new `batch sealed`.
    Sealed(NodeId),
    /// Since the election's end.
    AfterEnd(Duration),
}

#[derive(Clone, Debug)]
pub enum Action {
    Kill {
        node: Role,
        down: Duration,
    },
    ProverDown {
        node: NodeId,
        for_: Duration,
    },
    ProverDelay {
        node: NodeId,
        delay: Duration,
        for_: Duration,
    },
    RpcDown {
        node: Role,
        rpc: Rpc,
        index: usize,
        for_: Duration,
    },
    RpcFail {
        node: Role,
        rpc: Rpc,
        index: usize,
        status: u16,
        for_: Duration,
    },
    /// Restart with other knobs; `None` keeps them.
    Knobs {
        node: NodeId,
        knobs: Knobs,
        for_: Option<Duration>,
    },
}

#[derive(Clone, Debug)]
pub struct Chaos {
    pub name: String,
    pub trigger: Trigger,
    pub action: Action,
}

/// What the run saw at one poll.
#[derive(Clone, Debug)]
pub struct Observed {
    /// Since the scenario start.
    pub elapsed: Duration,
    /// Since the election's end, once it ended.
    pub ended_for: Option<Duration>,
    /// Per node A, B: pending nonce above latest.
    pub pending_nonce_gap: [bool; 2],
    /// Per node A, B: `batch sealed` lines since the scenario start.
    pub new_seals: [u64; 2],
}

fn idx(n: NodeId) -> usize {
    match n {
        NodeId::A => 0,
        NodeId::B => 1,
    }
}

fn role(n: NodeId) -> Role {
    match n {
        NodeId::A => Role::A,
        NodeId::B => Role::B,
    }
}

pub fn due(t: &Trigger, o: &Observed) -> bool {
    match t {
        Trigger::At(d) => o.elapsed >= *d,
        Trigger::L1Pending(n) => o.pending_nonce_gap[idx(*n)],
        Trigger::Sealed(n) => o.new_seals[idx(*n)] > 0,
        Trigger::AfterEnd(d) => o.ended_for.is_some_and(|e| e >= *d),
    }
}

/// The node an action touches.
fn target(a: &Action) -> Role {
    match a {
        Action::Kill { node, .. } | Action::RpcDown { node, .. } | Action::RpcFail { node, .. } => {
            *node
        }
        Action::ProverDown { node, .. }
        | Action::ProverDelay { node, .. }
        | Action::Knobs { node, .. } => role(*node),
    }
}

/// Whether the action's node and proxy index exist, given each role's URL
/// counts [OP, L1, Send, Beacon].
pub fn references_exist(a: &Action, urls: &[[usize; 4]; 3]) -> bool {
    let (node, rpc, index) = match a {
        Action::RpcDown {
            node, rpc, index, ..
        }
        | Action::RpcFail {
            node, rpc, index, ..
        } => (*node, *rpc, *index),
        // Every signing node has a prover proxy.
        _ => return true,
    };
    urls[node as usize][rpc as usize] > index
}

/// What to put back.
#[derive(Clone, Debug)]
pub enum Restore {
    Start(Role),
    ProxyUp { role: Role, rpc: Rpc, index: usize },
    ProxyOk { role: Role, rpc: Rpc, index: usize },
    ProverUp(Role),
    ProverOnTime(Role),
    Knobs { role: Role, knobs: Knobs },
    Nothing,
}

/// What to restore and when; `at` is `None` for a change that stays.
#[derive(Clone, Debug)]
pub struct Undo {
    pub at: Option<Instant>,
    pub what: Restore,
}

/// The undo of `a` applied at `now`; `prev` are the node's knobs before.
pub fn undo_for(a: &Action, prev: Option<&Knobs>, now: Instant) -> Undo {
    let after = |d: Duration| Some(now + d);
    match a {
        Action::Kill { node, down } => Undo {
            at: after(*down),
            what: Restore::Start(*node),
        },
        Action::ProverDown { node, for_ } => Undo {
            at: after(*for_),
            what: Restore::ProverUp(role(*node)),
        },
        Action::ProverDelay { node, for_, .. } => Undo {
            at: after(*for_),
            what: Restore::ProverOnTime(role(*node)),
        },
        Action::RpcDown {
            node,
            rpc,
            index,
            for_,
        } => Undo {
            at: after(*for_),
            what: Restore::ProxyUp {
                role: *node,
                rpc: *rpc,
                index: *index,
            },
        },
        Action::RpcFail {
            node,
            rpc,
            index,
            for_,
            ..
        } => Undo {
            at: after(*for_),
            what: Restore::ProxyOk {
                role: *node,
                rpc: *rpc,
                index: *index,
            },
        },
        Action::Knobs { node, for_, .. } => Undo {
            at: for_.and_then(after),
            what: prev.map_or(Restore::Nothing, |k| Restore::Knobs {
                role: role(*node),
                knobs: k.clone(),
            }),
        },
    }
}

fn proxy(rig: &Rig, r: Role, rpc: Rpc, i: usize) -> Result<&Proxy> {
    let p = rig.proxies(r);
    let list = match rpc {
        Rpc::Op => &p.op,
        Rpc::L1 => &p.l1,
        Rpc::Send => &p.send,
        Rpc::Beacon => &p.beacon,
    };
    list.get(i)
        .with_context(|| format!("{r:?} has no {rpc:?} proxy {i}"))
}

fn prover(rig: &Rig, r: Role) -> Result<&Proxy> {
    rig.proxies(r).prover.as_ref().context("no prover proxy")
}

/// Injects `a` now; the returned [`Undo`] puts it back.
pub async fn apply(rig: &mut Rig, _net: &Net, a: &Action) -> Result<Undo> {
    let prev = match a {
        Action::Knobs { node, .. } => rig.knobs(role(*node)).cloned(),
        _ => None,
    };
    match a {
        Action::Kill { node, .. } => rig.kill(*node).await?,
        Action::ProverDown { node, .. } => prover(rig, role(*node))?.set_down(true),
        Action::ProverDelay { node, delay, .. } => {
            prover(rig, role(*node))?.set_delay("/prove", *delay)
        }
        Action::RpcDown {
            node, rpc, index, ..
        } => proxy(rig, *node, *rpc, *index)?.set_down(true),
        Action::RpcFail {
            node,
            rpc,
            index,
            status,
            ..
        } => proxy(rig, *node, *rpc, *index)?.set_fail(Some(*status)),
        Action::Knobs { node, knobs, .. } => rig.restart_with(role(*node), knobs.clone()).await?,
    }
    Ok(undo_for(a, prev.as_ref(), Instant::now()))
}

impl Undo {
    pub async fn run(&self, rig: &mut Rig) -> Result<()> {
        match self.what.clone() {
            Restore::Start(r) => rig.start(r).await?,
            Restore::ProxyUp { role, rpc, index } => proxy(rig, role, rpc, index)?.set_down(false),
            Restore::ProxyOk { role, rpc, index } => proxy(rig, role, rpc, index)?.set_fail(None),
            Restore::ProverUp(r) => prover(rig, r)?.set_down(false),
            Restore::ProverOnTime(r) => prover(rig, r)?.set_delay("/prove", Duration::ZERO),
            Restore::Knobs { role, knobs } => rig.restart_with(role, knobs).await?,
            Restore::Nothing => {}
        }
        Ok(())
    }
}

/// Per node A, B: whether the Ethereum pending nonce of the node's key is
/// above its latest (a post is in the pool).
pub async fn pending_gap(net: &Net) -> Result<[bool; 2]> {
    let Some(l1) = &net.l1 else {
        return Ok([false; 2]);
    };
    if l1.keys.len() < 2 {
        bail!("fewer than two Ethereum keys");
    }
    let p = l1.provider()?;
    let mut out = [false; 2];
    for (i, o) in out.iter_mut().enumerate() {
        let a: Address = l1.keys[i].1;
        *o = p.get_transaction_count(a).pending().await? > p.get_transaction_count(a).await?;
    }
    Ok(out)
}

/// Report row of one injection.
#[derive(Clone, Debug, PartialEq)]
pub struct ChaosRow {
    pub name: String,
    pub due_s: f64,
    pub applied_s: f64,
    pub undone_s: Option<f64>,
    pub effects: Vec<String>,
    /// From the undo to the node's first settlement or sync.
    pub recovery_s: Option<f64>,
}

/// Counts of the log kinds an injection provoked, as report lines.
pub fn effects(kinds: &[Kind]) -> Vec<String> {
    let mut c = [0usize; 6];
    for k in kinds {
        let i = match k {
            Kind::RpcFailover { .. } => 0,
            Kind::BeaconFailover => 1,
            Kind::PostRetry { .. } => 2,
            Kind::Pause { .. } => 3,
            Kind::Posted { .. } => 4,
            Kind::Bisect { .. } => 5,
            _ => continue,
        };
        c[i] += 1;
    }
    [
        "RPC failover",
        "beacon failover",
        "L1 post retry",
        "pause",
        "post",
        "bisection",
    ]
    .iter()
    .zip(c)
    .filter(|(_, n)| *n > 0)
    .map(|(l, n)| format!("{l} x{n}"))
    .collect()
}

struct Active {
    row: ChaosRow,
    role: Role,
    undo: Undo,
    kinds: Vec<Kind>,
}

/// Whether anvil mining should be held now: an injection waits on an
/// `L1Pending` node that has sealed, so its post must stay in the pool.
fn should_pause(waiting: &[Chaos], o: &Observed) -> bool {
    waiting
        .iter()
        .any(|c| matches!(c.trigger, Trigger::L1Pending(n) if o.new_seals[idx(n)] > 0))
}

/// Indices of the active injections whose undo is due at `now`; an undo
/// without a deadline stays until [`Controller::finish`].
fn due_undos(active: &[Active], now: Instant) -> Vec<usize> {
    active
        .iter()
        .enumerate()
        .filter(|(_, a)| a.undo.at.is_some_and(|at| now >= at))
        .map(|(i, _)| i)
        .collect()
}

/// Runs a scenario's injections: fires each when due, undoes it at its
/// deadline, and times the node's recovery from the log events.
///
/// The scenario driver must call [`Controller::finish`] on every exit path
/// (normal end, error, the SIGTERM handler): nothing else undoes an active
/// kill, proxy mode, prover delay or knob change, or resumes anvil mining.
pub struct Controller {
    waiting: Vec<Chaos>,
    active: Vec<Active>,
    recovering: Vec<(ChaosRow, Role)>,
    done: Vec<ChaosRow>,
    /// Anvil only: mining is paused so a post stays pending.
    paused: bool,
}

impl Controller {
    pub fn new(chaos: Vec<Chaos>) -> Controller {
        Controller {
            waiting: chaos,
            active: Vec::new(),
            recovering: Vec::new(),
            done: Vec::new(),
            paused: false,
        }
    }

    /// One poll. `events` are the nodes' new log lines as (node, seconds
    /// since the scenario start, kind). An error leaves what is applied
    /// recorded, for [`Controller::finish`].
    pub async fn step(
        &mut self,
        rig: &mut Rig,
        net: &Net,
        o: &Observed,
        events: &[(Role, f64, Kind)],
    ) -> Result<()> {
        let now = o.elapsed.as_secs_f64();
        let anvil = net.l1.as_ref().and_then(|l| l.anvil.as_ref());
        // On anvil nothing stays pending unless mining is held: hold it
        // from the node's first seal until the L1Pending injection fires.
        if let Some(l1) = anvil
            && !self.paused
            && should_pause(&self.waiting, o)
        {
            l1.pause_mining().await?;
            self.paused = true;
        }
        for a in &mut self.active {
            for (r, _, k) in events {
                if *r == a.role {
                    a.kinds.push(k.clone());
                }
            }
        }
        let mut i = 0;
        while i < self.waiting.len() {
            if !due(&self.waiting[i].trigger, o) {
                i += 1;
                continue;
            }
            let c = self.waiting.remove(i);
            let unpause = self.paused && matches!(c.trigger, Trigger::L1Pending(_));
            let undo = apply(rig, net, &c.action).await?;
            self.active.push(Active {
                row: ChaosRow {
                    name: c.name,
                    due_s: now,
                    applied_s: o.elapsed.as_secs_f64(),
                    undone_s: None,
                    effects: Vec::new(),
                    recovery_s: None,
                },
                role: target(&c.action),
                undo,
                kinds: Vec::new(),
            });
            if unpause && let Some(l1) = anvil {
                l1.resume_mining().await?;
                self.paused = false;
            }
        }
        for i in due_undos(&self.active, Instant::now()).into_iter().rev() {
            // Run first: a failed undo stays active for `finish` to retry.
            self.active[i].undo.run(rig).await?;
            self.undone(i, now);
        }
        let mut i = 0;
        while i < self.recovering.len() {
            let (row, r) = &mut self.recovering[i];
            let undone = row.undone_s.unwrap_or(now);
            let hit = events.iter().find(|(er, at, k)| {
                er == r && *at >= undone && matches!(k, Kind::Settled { .. } | Kind::Synced { .. })
            });
            if let Some((_, at, _)) = hit {
                row.recovery_s = Some(at - undone);
                let (row, _) = self.recovering.remove(i);
                self.done.push(row);
            } else {
                i += 1;
            }
        }
        Ok(())
    }

    /// Moves active injection `i` to recovering, undone at `now` seconds.
    fn undone(&mut self, i: usize, now: f64) {
        let mut a = self.active.remove(i);
        a.row.undone_s = Some(now);
        a.row.effects = effects(&a.kinds);
        self.recovering.push((a.row, a.role));
    }

    /// Undoes every active injection and resumes anvil mining, continuing
    /// past errors; fails with all of them.
    pub async fn finish(&mut self, rig: &mut Rig, net: &Net, now: Duration) -> Result<()> {
        let anvil = net.l1.as_ref().and_then(|l| l.anvil.as_ref());
        self.finish_with(
            now.as_secs_f64(),
            async |u: &Undo| u.run(rig).await,
            async || match anvil {
                Some(l1) => l1.resume_mining().await,
                None => Ok(()),
            },
        )
        .await
    }

    async fn finish_with(
        &mut self,
        now: f64,
        mut undo: impl AsyncFnMut(&Undo) -> Result<()>,
        resume: impl AsyncFn() -> Result<()>,
    ) -> Result<()> {
        let mut errs = Vec::new();
        while !self.active.is_empty() {
            if let Err(e) = undo(&self.active[0].undo).await {
                errs.push(format!("{}: {e:#}", self.active[0].row.name));
            }
            // Failed or not, the injection leaves the active list.
            self.undone(0, now);
        }
        if self.paused {
            match resume().await {
                Ok(()) => self.paused = false,
                Err(e) => errs.push(format!("resume mining: {e:#}")),
            }
        }
        if errs.is_empty() {
            Ok(())
        } else {
            bail!("chaos teardown: {}", errs.join("; "))
        }
    }

    /// Every row so far; an injection still active or unrecovered has no
    /// recovery time, one that never fired is absent.
    pub fn rows(&self) -> Vec<ChaosRow> {
        let mut v = self.done.clone();
        v.extend(self.recovering.iter().map(|(r, _)| r.clone()));
        v.extend(self.active.iter().map(|a| {
            let mut r = a.row.clone();
            r.effects = effects(&a.kinds);
            r
        }));
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bench::plan::NodeId;
    use crate::bench::rig::{Knobs, Role};
    use std::time::{Duration, Instant};

    fn obs() -> Observed {
        Observed {
            elapsed: Duration::from_secs(10),
            ended_for: None,
            pending_nonce_gap: [false; 2],
            new_seals: [0; 2],
        }
    }

    #[test]
    fn triggers_fire_once_their_condition_holds() {
        let at = Trigger::At(Duration::from_secs(11));
        assert!(!due(&at, &obs()));
        let o = Observed {
            elapsed: Duration::from_secs(11),
            ..obs()
        };
        assert!(due(&at, &o));

        let t = Trigger::L1Pending(NodeId::B);
        let o = Observed {
            pending_nonce_gap: [true, false],
            ..obs()
        };
        assert!(!due(&t, &o));
        let o = Observed {
            pending_nonce_gap: [false, true],
            ..obs()
        };
        assert!(due(&t, &o));

        let t = Trigger::Sealed(NodeId::A);
        let o = Observed {
            new_seals: [0, 3],
            ..obs()
        };
        assert!(!due(&t, &o));
        let o = Observed {
            new_seals: [1, 0],
            ..obs()
        };
        assert!(due(&t, &o));

        let t = Trigger::AfterEnd(Duration::from_secs(60));
        assert!(!due(&t, &obs()));
        let o = Observed {
            ended_for: Some(Duration::from_secs(59)),
            ..obs()
        };
        assert!(!due(&t, &o));
        let o = Observed {
            ended_for: Some(Duration::from_secs(60)),
            ..obs()
        };
        assert!(due(&t, &o));
    }

    #[test]
    fn undo_restores_the_knobs() {
        let prev = Knobs::production();
        let k = Knobs {
            l1_max_post_fee: Some("0.000000001eth".into()),
            ..Knobs::production()
        };
        let a = Action::Knobs {
            node: NodeId::B,
            knobs: k,
            for_: Some(Duration::from_secs(900)),
        };
        let now = Instant::now();
        let u = undo_for(&a, Some(&prev), now);
        assert_eq!(u.at, Some(now + Duration::from_secs(900)));
        match u.what {
            Restore::Knobs { role, knobs } => {
                assert_eq!(role, Role::B);
                assert_eq!(knobs.l1_max_post_fee, None);
                assert_eq!(knobs.batch_max, prev.batch_max);
            }
            _ => panic!("not a knobs restore"),
        }
    }

    #[test]
    fn references_are_checked() {
        let down = |index| Action::RpcDown {
            node: Role::O,
            rpc: Rpc::Beacon,
            index,
            for_: Duration::from_secs(300),
        };
        // [OP, L1, Send, Beacon] per role A, B, O.
        let two = [[1, 1, 1, 1], [1, 1, 1, 1], [1, 1, 1, 2]];
        let one = [[1, 1, 1, 1]; 3];
        assert!(references_exist(&down(1), &two));
        assert!(!references_exist(&down(1), &one));
        let p = Action::ProverDown {
            node: NodeId::A,
            for_: Duration::from_secs(600),
        };
        assert!(references_exist(&p, &[[0; 4]; 3]));
    }

    fn active(name: &str, role: Role, at: Option<Instant>, what: Restore) -> Active {
        Active {
            row: ChaosRow {
                name: name.into(),
                due_s: 0.0,
                applied_s: 0.0,
                undone_s: None,
                effects: vec![],
                recovery_s: None,
            },
            role,
            undo: Undo { at, what },
            kinds: vec![],
        }
    }

    #[tokio::test]
    async fn finish_undoes_everything_and_resumes_mining() {
        let mut c = Controller::new(vec![]);
        c.paused = true;
        c.active
            .push(active("kill", Role::A, None, Restore::Start(Role::A)));
        c.active
            .push(active("bad", Role::B, None, Restore::ProverUp(Role::B)));
        let ran = std::cell::RefCell::new(Vec::new());
        let resumed = std::cell::Cell::new(false);
        let r = c
            .finish_with(
                5.0,
                async |u: &Undo| {
                    ran.borrow_mut().push(format!("{:?}", u.what));
                    if matches!(u.what, Restore::Start(_)) {
                        anyhow::bail!("start failed");
                    }
                    Ok(())
                },
                async || {
                    resumed.set(true);
                    Ok(())
                },
            )
            .await;
        // The first failure did not stop the second undo or the resume.
        assert_eq!(ran.borrow().len(), 2);
        assert!(r.unwrap_err().to_string().contains("start failed"));
        assert!(c.active.is_empty() && !c.paused && resumed.get());
        assert!(c.rows().iter().all(|r| r.undone_s == Some(5.0)));
    }

    #[test]
    fn pause_only_for_a_sealed_pending_trigger() {
        let w = |t| {
            vec![Chaos {
                name: "c".into(),
                trigger: t,
                action: Action::Kill {
                    node: Role::A,
                    down: Duration::ZERO,
                },
            }]
        };
        let sealed = Observed {
            new_seals: [1, 0],
            ..obs()
        };
        assert!(should_pause(&w(Trigger::L1Pending(NodeId::A)), &sealed));
        assert!(!should_pause(&w(Trigger::L1Pending(NodeId::B)), &sealed));
        assert!(!should_pause(&w(Trigger::Sealed(NodeId::A)), &sealed));
        assert!(!should_pause(&w(Trigger::L1Pending(NodeId::A)), &obs()));
    }

    #[test]
    fn only_deadlined_undos_are_swept() {
        let now = Instant::now();
        let a = vec![
            active("kill", Role::A, Some(now), Restore::Start(Role::A)),
            active("stays", Role::B, None, Restore::Nothing),
            active(
                "later",
                Role::B,
                Some(now + Duration::from_secs(9)),
                Restore::Nothing,
            ),
        ];
        assert_eq!(due_undos(&a, now), [0]);
        assert_eq!(due_undos(&a, now + Duration::from_secs(10)), [0, 2]);
        let k = Action::Kill {
            node: Role::A,
            down: Duration::from_secs(30),
        };
        assert!(matches!(
            undo_for(&k, None, now).what,
            Restore::Start(Role::A)
        ));
    }
}
