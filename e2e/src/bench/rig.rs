//! The benchmark rig: node A (sequencer 0), node B (sequencer 1) and an
//! observer O on fixed ports, every upstream behind its own loopback proxy
//! so an outage or a restart never changes the URLs a node was given.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::{Context, Result, bail};

use crate::bench::plan::Plan;
use crate::net::Net;
use crate::node::{Node, NodeConfig, sequencer_bin};
use crate::proxy::{Exchange, Proxy, Tap};

/// The three nodes of a rig.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    A,
    B,
    O,
}

impl Role {
    fn idx(self) -> usize {
        self as usize
    }

    fn name(self) -> &'static str {
        ["a", "b", "o"][self.idx()]
    }
}

/// The batching knobs a scenario varies.
#[derive(Clone, Debug)]
pub struct Knobs {
    pub batch_max: usize,
    pub batch_time: String,
    pub min_mix: Option<u32>,
    pub confirmations: Option<u64>,
    pub l1_max_post_fee: Option<String>,
}

impl Knobs {
    /// What a production node runs.
    pub fn production() -> Knobs {
        Knobs {
            batch_max: 512,
            batch_time: "15m".into(),
            min_mix: None,
            confirmations: None,
            l1_max_post_fee: None,
        }
    }

    /// `None` keeps the config's value, except `min_mix`, which the network
    /// config never sets: `None` there is the node's default.
    pub fn apply(&self, c: &mut NodeConfig) {
        c.batch_max = self.batch_max;
        c.batch_time = self.batch_time.clone();
        c.min_mix = self.min_mix;
        if let Some(n) = self.confirmations {
            c.confirmations = n;
        }
        if let Some(f) = &self.l1_max_post_fee {
            c.l1_max_post_fee = Some(f.clone());
        }
    }
}

pub struct RigSpec {
    /// Holds `plan.json`; datadirs and logs go to `nodes/`, the census to `census/`.
    pub campaign: PathBuf,
    /// Prover URLs of A and B; the observer never proves.
    pub prover: [String; 2],
    /// API ports of A, B and O (default 9101-9103).
    pub ports: [u16; 3],
    /// Knobs of A and B.
    pub knobs: [Knobs; 2],
}

/// The proxies in front of one node's upstreams, in list order.
#[derive(Default)]
pub struct NodeProxies {
    pub prover: Option<Proxy>,
    pub op: Vec<Proxy>,
    pub l1: Vec<Proxy>,
    pub send: Vec<Proxy>,
    pub beacon: Vec<Proxy>,
}

/// A `POST /prove` a node made.
#[derive(Clone, Debug, PartialEq)]
pub struct ProveCall {
    /// Seconds since the rig came up.
    pub at: f64,
    /// The prover's job id, when its answer carried one.
    pub job: Option<String>,
}

pub struct Rig {
    nodes: [Option<Node>; 3],
    cfgs: [NodeConfig; 3],
    ports: [u16; 3],
    proxies: [NodeProxies; 3],
    bin: PathBuf,
    calls: Arc<Mutex<[Vec<ProveCall>; 3]>>,
}

/// Starts one proxy per entry of the `,` list `list`.
async fn proxy_list(list: &str) -> Result<Vec<Proxy>> {
    let mut v = Vec::new();
    for e in entries(list) {
        v.push(Proxy::start(e).await?);
    }
    Ok(v)
}

fn entries(list: &str) -> Vec<&str> {
    list.split(',')
        .map(str::trim)
        .filter(|e| !e.is_empty())
        .collect()
}

/// `list` with its i-th entry replaced by `urls[i]`; an entry without a
/// replacement stays.
pub fn wrap_list(list: &str, urls: &[String]) -> String {
    entries(list)
        .iter()
        .enumerate()
        .map(|(i, e)| urls.get(i).map_or(*e, String::as_str))
        .collect::<Vec<_>>()
        .join(",")
}

fn urls(p: &[Proxy]) -> Vec<String> {
    p.iter().map(|p| p.url.clone()).collect()
}

impl Rig {
    /// A = sequencer 0 on RPC 0, B = sequencer 1 on RPC 1, O = observer on RPC 2.
    pub async fn up(net: &Net, spec: RigSpec) -> Result<Rig> {
        let plan = Plan::load(&spec.campaign)?;
        let dir = spec.campaign.join("nodes");
        std::fs::create_dir_all(&dir)?;
        let census = spec.campaign.join("census");
        let t0 = Instant::now();
        let calls: Arc<Mutex<[Vec<ProveCall>; 3]>> = Default::default();
        let mut cfgs = Vec::new();
        let mut proxies = Vec::new();
        for (i, role) in [Role::A, Role::B, Role::O].into_iter().enumerate() {
            let seq = (i < 2).then_some(i);
            let mut px = NodeProxies::default();
            let prover_url = if i < 2 {
                let c = calls.clone();
                let tap: Tap = Arc::new(move |x: &Exchange<'_>| {
                    if x.method != "POST" || x.path != "/prove" {
                        return;
                    }
                    let job = serde_json::from_slice::<serde_json::Value>(x.response)
                        .ok()
                        .and_then(|v| v.get("job_id").and_then(|j| j.as_str().map(String::from)));
                    if let Ok(mut c) = c.lock() {
                        c[i].push(ProveCall {
                            at: x.at.duration_since(t0).as_secs_f64(),
                            job,
                        });
                    }
                });
                let p = Proxy::tapped(&spec.prover[i], tap).await?;
                let u = p.url.clone();
                px.prover = Some(p);
                u
            } else {
                spec.prover[0].clone()
            };
            let mut c = net.node_config(role.name(), seq, i, &prover_url, &census, &dir)?;
            c.start_block = Some(plan.start_block);
            if i < 2 {
                spec.knobs[i].apply(&mut c);
            }
            px.op = proxy_list(&c.rpc_url).await?;
            c.rpc_url = wrap_list(&c.rpc_url, &urls(&px.op));
            if let Some(l) = c.l1_rpc_url.clone() {
                px.l1 = proxy_list(&l).await?;
                c.l1_rpc_url = Some(wrap_list(&l, &urls(&px.l1)));
            }
            if let Some(l) = c.l1_send_rpc_url.clone() {
                px.send = proxy_list(&l).await?;
                c.l1_send_rpc_url = Some(wrap_list(&l, &urls(&px.send)));
            }
            if let Some(l) = c.blob_source.strip_prefix("beacon:").map(String::from) {
                px.beacon = proxy_list(&l).await?;
                c.blob_source = format!("beacon:{}", wrap_list(&l, &urls(&px.beacon)));
            }
            cfgs.push(c);
            proxies.push(px);
        }
        let bin = sequencer_bin()?;
        let mut rig = Rig {
            nodes: [None, None, None],
            cfgs: cfgs
                .try_into()
                .map_err(|_| anyhow::anyhow!("three configs"))?,
            ports: spec.ports,
            proxies: proxies
                .try_into()
                .map_err(|_| anyhow::anyhow!("three proxy sets"))?,
            bin,
            calls,
        };
        for r in [Role::A, Role::B, Role::O] {
            rig.start(r).await?;
        }
        Ok(rig)
    }

    pub fn node(&self, r: Role) -> Result<&Node> {
        self.nodes[r.idx()]
            .as_ref()
            .with_context(|| format!("node {} is not running", r.name()))
    }

    pub fn proxies(&self, r: Role) -> &NodeProxies {
        &self.proxies[r.idx()]
    }

    /// Stops the node, releasing its datadir and port.
    pub async fn kill(&mut self, r: Role) -> Result<()> {
        // Dropping a node kills and reaps its process.
        if self.nodes[r.idx()].take().is_none() {
            bail!("node {} is not running", r.name());
        }
        Ok(())
    }

    /// Starts a stopped node on its port with its config, on its old datadir.
    pub async fn start(&mut self, r: Role) -> Result<()> {
        let i = r.idx();
        if self.nodes[i].is_some() {
            bail!("node {} is already running", r.name());
        }
        self.nodes[i] = Some(Node::start_on(&self.bin, &self.cfgs[i], self.ports[i]).await?);
        Ok(())
    }

    /// Stops the node and starts it again with `k`.
    pub async fn restart_with(&mut self, r: Role, k: Knobs) -> Result<()> {
        if self.nodes[r.idx()].is_some() {
            self.kill(r).await?;
        }
        k.apply(&mut self.cfgs[r.idx()]);
        self.start(r).await
    }

    /// The node's `POST /prove` calls so far, oldest first.
    pub fn prove_calls(&self, r: Role) -> Vec<ProveCall> {
        self.calls
            .lock()
            .map(|c| c[r.idx()].clone())
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node::NodeConfig;

    #[test]
    fn wrap_list_keeps_order() {
        let u = ["p", "q", "r"].map(String::from);
        assert_eq!(wrap_list("a, b,,c", &u), "p,q,r");
    }

    #[test]
    fn knobs_reach_the_config() {
        let mut c = NodeConfig::new(
            "n",
            None,
            None,
            "http://r",
            Default::default(),
            "http://p",
            std::path::Path::new("/c"),
            std::path::Path::new("/d"),
        );
        let p = Knobs::production();
        assert_eq!((p.batch_max, p.batch_time.as_str()), (512, "15m"));
        assert!(p.min_mix.is_none() && p.confirmations.is_none() && p.l1_max_post_fee.is_none());
        Knobs {
            batch_max: 7,
            batch_time: "9s".into(),
            min_mix: Some(3),
            confirmations: Some(2),
            l1_max_post_fee: Some("1".into()),
        }
        .apply(&mut c);
        assert_eq!((c.batch_max, c.batch_time.as_str()), (7, "9s"));
        assert_eq!((c.min_mix, c.confirmations), (Some(3), 2));
        assert_eq!(c.l1_max_post_fee.as_deref(), Some("1"));
    }
}
