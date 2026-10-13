//! The live benchmark's controller: `keygen`, `fund`, `gate`, `plan`,
//! `gen`, `merge`, `run`, `audit`, `serve`, `cancel` and `dry`. Keys come
//! from files (`DAVINCI_E2E_ORGANIZER_KEY` names one) and are never printed.
//!
//! A step that brings the rig up (`plan`, `run`, `audit`, `serve`, `dry`)
//! tears it down on every exit, SIGINT and SIGTERM included: the nodes die
//! only when their handles drop.

use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use alloy::primitives::utils::parse_ether;
use alloy::primitives::{Address, U256};
use alloy::providers::{Provider, ProviderBuilder};
use anyhow::{Context, Result, anyhow, bail, ensure};
use clap::{Parser, Subcommand};
use davinci_client::organizer::Organizer;
use davinci_e2e::bench::chaos::Controller;
use davinci_e2e::bench::r#gen::{GenOpts, gen_wave};
use davinci_e2e::bench::merge::{Verify, check_snapshots, merge_wave};
use davinci_e2e::bench::money;
use davinci_e2e::bench::plan::{
    self, CancelScope, ElectionSpec, NodeId, Plan, Seed, cancel_candidates, cancel_open,
};
use davinci_e2e::bench::rig::{Knobs, Rig, RigSpec, Role};
use davinci_e2e::bench::run::{self as bench_run, ChaosSlot, Ctx};
use davinci_e2e::bench::scenarios::{Scenario, scenario_specs, scenarios, wave_specs};
use davinci_e2e::chain;
use davinci_e2e::fixture;
use davinci_e2e::net::{self, Net};
use tokio::signal::unix::{SignalKind, signal};

#[derive(Parser)]
#[command(name = "davinci-bench")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Create `<name>.op.key` and `<name>.l1.key` (mode 0600) in `dir`.
    Keygen {
        #[arg(long)]
        dir: PathBuf,
        #[arg(long)]
        name: String,
    },
    /// Send capped transfers from the organizer key.
    Fund {
        #[arg(long)]
        to_op: Option<Address>,
        #[arg(long)]
        op_eth: Option<String>,
        #[arg(long)]
        to_l1: Option<Address>,
        #[arg(long)]
        l1_eth: Option<String>,
        #[arg(long, value_delimiter = ',', required = true)]
        confirm: Vec<Address>,
        /// Record the transfers as inflows in `<campaign>/budget.json`.
        #[arg(long)]
        campaign: Option<PathBuf>,
    },
    /// Print the go/no-go gate; exit 0 on a go, 3 on a no-go.
    Gate {
        #[arg(long)]
        campaign: Option<PathBuf>,
    },
    /// Write a campaign seed and a stand-in plan (no chain) for dry runs.
    StandIn {
        #[arg(long, env = "DAVINCI_BENCH_DIR")]
        campaign: PathBuf,
        #[arg(long)]
        wave: u32,
        /// `tag:nf:origin:voters:rounds:permille,...`
        #[arg(long)]
        elections: String,
    },
    /// Prove this host's ballot shards of one or more waves.
    Gen {
        #[arg(long, env = "DAVINCI_BENCH_DIR")]
        campaign: PathBuf,
        #[arg(long, value_delimiter = ',', required = true)]
        wave: Vec<u32>,
        #[arg(long)]
        host: usize,
        #[arg(long)]
        hosts: usize,
        #[arg(long, default_value_t = 8)]
        threads: usize,
        #[arg(long, env = "CIRCOM_ARTIFACTS")]
        artifacts: PathBuf,
        /// Recorded in each shard's meta; defaults to /etc/hostname.
        #[arg(long)]
        host_name: Option<String>,
    },
    /// Verify and join both hosts' shards into `ballots/w<N>/`.
    Merge {
        #[arg(long, env = "DAVINCI_BENCH_DIR")]
        campaign: PathBuf,
        #[arg(long)]
        wave: u32,
        /// Skip the Groth16 check of every proof.
        #[arg(long)]
        skip_proofs: bool,
        /// Also compare the plan's snapshots with the registry at this RPC.
        #[arg(long)]
        rpc: Option<String>,
    },
    /// Create the elections of one or more waves, live. The first wave
    /// writes the seed and records the start block; the rig comes up only
    /// for the election keys.
    Plan {
        #[arg(long, env = "DAVINCI_BENCH_DIR")]
        campaign: PathBuf,
        #[arg(long, value_delimiter = ',', required = true)]
        wave: Vec<u32>,
        /// Every election scaled down (1 live).
        #[arg(long, default_value_t = 1.0)]
        scale: f64,
        /// Create the origin-3 elections (R4): deploy and fill a census
        /// contract per election.
        #[arg(long)]
        onchain_census: bool,
        #[command(flatten)]
        rig: RigArgs,
    },
    /// Run scenarios of a planned, generated and merged wave, live: each
    /// from the fee gate to its audited report.
    Run {
        #[arg(long, env = "DAVINCI_BENCH_DIR")]
        campaign: PathBuf,
        #[arg(long)]
        wave: u32,
        /// Scenario names, or `all` for the wave's in order.
        #[arg(long, value_delimiter = ',', required = true)]
        scenario: Vec<String>,
        #[command(flatten)]
        rig: RigArgs,
    },
    /// Rerun the audit of a run that crashed, from its ledger, the nodes'
    /// datadirs and the chain, live.
    Audit {
        #[arg(long, env = "DAVINCI_BENCH_DIR")]
        campaign: PathBuf,
        /// The run's directory, `runs/w<N>-<scenario>`.
        #[arg(long)]
        run: PathBuf,
        #[command(flatten)]
        rig: RigArgs,
    },
    /// Hold the rig up for external drivers until SIGTERM or SIGINT.
    Serve {
        #[arg(long, env = "DAVINCI_BENCH_DIR")]
        campaign: PathBuf,
        #[command(flatten)]
        rig: RigArgs,
    },
    /// Cancel the READY and PAUSED processes of a wave (`plan.json`), or with
    /// `--all` every one the organizer created since the start block.
    Cancel {
        #[arg(long, env = "DAVINCI_BENCH_DIR")]
        campaign: PathBuf,
        #[arg(long, conflicts_with = "all", required_unless_present = "all")]
        wave: Option<u32>,
        #[arg(long)]
        all: bool,
    },
    /// A scaled-down run of some scenarios on two anvils: plan, gen (this
    /// host alone), merge and run.
    Dry {
        #[arg(long, value_delimiter = ',', required = true)]
        scenario: Vec<String>,
        #[arg(long, default_value_t = 0.05)]
        scale: f64,
        /// Stop after this stage: `plan` (then cancel what it created).
        #[arg(long)]
        stop_after: Option<String>,
        /// Ballot proving threads.
        #[arg(long, default_value_t = 8)]
        threads: usize,
        /// Default: `../davinci-circom/artifacts` next to the workspace.
        #[arg(long, env = "CIRCOM_ARTIFACTS")]
        artifacts: Option<PathBuf>,
        /// Default: a new `~/.cache/davinci-bench/dry-<unix time>`.
        #[arg(long)]
        campaign: Option<PathBuf>,
        #[command(flatten)]
        rig: RigArgs,
    },
}

#[derive(clap::Args)]
struct RigArgs {
    /// API ports of nodes A, B and O.
    #[arg(long, value_delimiter = ',', default_value = "9101,9102,9103")]
    ports: Vec<u16>,
    #[arg(
        long,
        env = "DAVINCI_BENCH_PROVER_A",
        default_value = "http://127.0.0.1:8080"
    )]
    prover_a: String,
    #[arg(
        long,
        env = "DAVINCI_BENCH_PROVER_B",
        default_value = "http://10.200.0.27:8080"
    )]
    prover_b: String,
}

/// What a step holds that must go on every exit (end, error, SIGINT,
/// SIGTERM): the rig's nodes and proxies and the net's anvils die only when
/// they drop, and only `Controller::finish` undoes an active chaos
/// injection.
#[derive(Default)]
struct Held {
    net: Option<Net>,
    /// A run's context; it owns the net once the run starts.
    ctx: Option<Ctx>,
    rig: Option<Rig>,
    /// The controller and its scenario's start.
    chaos: Option<(Controller, Instant)>,
}

impl Held {
    /// Undoes chaos, then drops the rig, then the chain it runs on.
    async fn teardown(&mut self) -> Result<()> {
        let net = self.ctx.as_ref().map(|c| &c.net).or(self.net.as_ref());
        let r = match (&mut self.chaos, &mut self.rig, net) {
            (Some((c, t0)), Some(rig), Some(net)) => c.finish(rig, net, t0.elapsed()).await,
            _ => Ok(()),
        };
        self.chaos = None;
        self.rig = None;
        self.ctx = None;
        self.net = None;
        r
    }
}

/// Runs `step` until it ends or SIGINT or SIGTERM arrives, then tears down
/// what it holds. A signal is an error unless `signal_ends` (serve); a
/// teardown error never hides the step's.
async fn held_step<T>(
    signal_ends: bool,
    step: impl AsyncFnOnce(&mut Held) -> Result<T>,
) -> Result<Option<T>> {
    let mut int = signal(SignalKind::interrupt()).context("SIGINT handler")?;
    let mut term = signal(SignalKind::terminate()).context("SIGTERM handler")?;
    let mut held = Held::default();
    // The step's result, or the signal that ended it.
    let ended: std::result::Result<Result<T>, &str> = tokio::select! {
        r = step(&mut held) => Ok(r),
        _ = int.recv() => Err("SIGINT"),
        _ = term.recv() => Err("SIGTERM"),
    };
    let down = held.teardown().await;
    let r = match ended {
        Ok(r) => r.map(Some),
        Err(sig) if signal_ends => {
            println!("{sig}: rig down");
            Ok(None)
        }
        Err(sig) => Err(anyhow!("stopped by {sig}; rig down")),
    };
    match (r, down) {
        (Err(e), Err(d)) => Err(e.context(format!("teardown also failed: {d:#}"))),
        (r, Ok(())) => r,
        (Ok(_), Err(d)) => Err(d.context("teardown")),
    }
}

/// Spec §14: the rig's ports are free before it starts. A taken one may be
/// a leftover node holding a real key.
fn ports_free(ports: &[u16; 3]) -> Result<()> {
    let taken: Vec<u16> = ports
        .iter()
        .copied()
        .filter(|&p| std::net::TcpListener::bind(("127.0.0.1", p)).is_err())
        .collect();
    ensure!(
        taken.is_empty(),
        "ports {taken:?} are taken: stop the leftover nodes first"
    );
    Ok(())
}

/// Nodes A, B and O with production knobs on the campaign's datadirs.
async fn rig_up(net: &Net, campaign: &Path, a: &RigArgs) -> Result<Rig> {
    let ports: [u16; 3] = a
        .ports
        .clone()
        .try_into()
        .map_err(|_| anyhow!("--ports takes three ports"))?;
    // Else `Rig::up` builds one, which need not be the binary `gate` hashed.
    ensure!(
        std::env::var_os("DAVINCI_SEQUENCER_BIN").is_some(),
        "set DAVINCI_SEQUENCER_BIN to the gated node binary"
    );
    ports_free(&ports)?;
    std::fs::create_dir_all(campaign.join("census"))?;
    let rig = Rig::up(
        net,
        RigSpec {
            campaign: campaign.to_path_buf(),
            prover: [a.prover_a.clone(), a.prover_b.clone()],
            ports,
            knobs: [Knobs::production(), Knobs::production()],
        },
    )
    .await?;
    for (name, r) in [("a", Role::A), ("b", Role::B), ("o", Role::O)] {
        println!("node {name} at {}", rig.node(r)?.url);
    }
    Ok(rig)
}

/// `p` created with mode 0700, as an absolute path: the nodes confine
/// `file://` censuses to its `census/`.
fn campaign_dir(p: &Path) -> Result<PathBuf> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(p)
        .with_context(|| format!("create {}", p.display()))?;
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o700))?;
    Ok(p.canonicalize()?)
}

fn check_plan(plan: &Plan, net: &Net) -> Result<()> {
    ensure!(
        (plan.chain_id, plan.registry) == (net.chain_id, net.registry),
        "plan.json is for chain {} registry {}, not chain {} registry {}",
        plan.chain_id,
        plan.registry,
        net.chain_id,
        net.registry
    );
    Ok(())
}

/// The campaign's seed. Before the first wave it writes the seed and a
/// plan whose start block is the head, before any creation.
async fn open_campaign(net: &Net, dir: &Path) -> Result<Seed> {
    let seed_path = dir.join("secret").join("seed");
    if dir.join("plan.json").exists() {
        check_plan(&Plan::load(dir)?, net)?;
        return Seed::load(&seed_path);
    }
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir.join("secret"))
        .context("create secret dir")?;
    // A seed without a plan.json predates any creation, so nothing used it.
    let seed = if seed_path.exists() {
        Seed::load(&seed_path)?
    } else {
        Seed::create(&seed_path)?
    };
    let head = ProviderBuilder::new()
        .connect_client(chain::rpc(&net.rpc)?)
        .get_block_number()
        .await
        .context("head block")?;
    let name = dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    Plan {
        campaign: name.clone(),
        chain_id: net.chain_id,
        registry: net.registry,
        start_block: head,
        waves: Vec::new(),
    }
    .save(dir)?;
    println!("campaign {name}: start block {head}");
    Ok(seed)
}

/// `plan`'s body: seed and plan, pins, the rig for the keys, then each
/// wave's elections.
#[allow(clippy::too_many_arguments)]
async fn plan_waves(
    net: &Net,
    rig_slot: &mut Option<Rig>,
    campaign: &Path,
    waves: &[(u32, Vec<ElectionSpec>)],
    scale: f64,
    onchain_census: bool,
    rig: &RigArgs,
) -> Result<()> {
    let seed = open_campaign(net, campaign).await?;
    plan::write_pins(campaign)?;
    let rig = rig_slot.insert(rig_up(net, campaign, rig).await?);
    for (n, specs) in waves {
        plan::create_wave(net, rig, campaign, &seed, *n, scale, specs, onchain_census).await?;
        let p = Plan::load(campaign)?;
        println!(
            "plan.json: wave {n}, {} elections",
            p.wave(*n)?.elections.len()
        );
    }
    Ok(())
}

/// Runs `scs` in order; a budget stop ends the run after its audit.
async fn run_scenarios(
    ctx: &Ctx,
    rig: &mut Rig,
    chaos: &mut ChaosSlot,
    scs: &[Scenario],
) -> Result<()> {
    for sc in scs {
        let r = bench_run::run_scenario_in(ctx, rig, sc, chaos).await?;
        if r.params
            .get("stopped")
            .is_some_and(|s| s.starts_with("budget"))
        {
            println!("stopped: budget; the run ends here");
            break;
        }
    }
    Ok(())
}

/// CPU-bound work on a blocking thread: `held_step` keeps polling for a
/// signal, so the rig goes down at once (the thread finishes on its own).
async fn off_runtime<T: Send + 'static>(
    f: impl FnOnce() -> Result<T> + Send + 'static,
) -> Result<T> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| anyhow!("blocking task: {e}"))?
}

fn load_seed(campaign: &Path) -> Result<Seed> {
    Seed::load(&campaign.join("secret").join("seed"))
}

fn check_scale(scale: f64) -> Result<()> {
    ensure!(scale > 0.0 && scale <= 1.0, "--scale must be in (0, 1]");
    Ok(())
}

fn require_live(step: &str) -> Result<()> {
    ensure!(
        net::is_live(),
        "{step} runs live (DAVINCI_E2E_LIVE=1); `dry` plans on anvil"
    );
    Ok(())
}

fn parse_specs(s: &str) -> Result<Vec<ElectionSpec>> {
    s.split(',')
        .map(|e| {
            let f: Vec<&str> = e.split(':').collect();
            let [tag, nf, origin, voters, rounds, pm] = f[..] else {
                bail!("election {e:?}: want tag:nf:origin:voters:rounds:permille");
            };
            Ok(ElectionSpec {
                tag: tag.into(),
                scenario: "stand-in".into(),
                nf: nf.parse().context("nf")?,
                origin: origin.parse().context("origin")?,
                voters: voters.parse().context("voters")?,
                rounds: rounds.parse().context("rounds")?,
                revote_permille: pm.parse().context("permille")?,
                key_node: NodeId::A,
                grace: 60,
            })
        })
        .collect()
}

fn pair(
    to: Option<Address>,
    eth: &Option<String>,
    max: &str,
    confirm: &[Address],
) -> Result<Option<(Address, U256)>> {
    match (to, eth) {
        (None, None) => Ok(None),
        (Some(to), Some(eth)) => {
            let amount = parse_ether(eth).context("amount")?;
            money::check_fund(amount, parse_ether(max)?, to, confirm)?;
            Ok(Some((to, amount)))
        }
        _ => bail!("--to-* and --*-eth go together"),
    }
}

async fn real_main() -> Result<ExitCode> {
    match Cli::parse().cmd {
        Cmd::Keygen { dir, name } => {
            let (op, l1) = money::keygen(&dir, &name)?;
            println!("{name} OP {op}");
            println!("{name} Ethereum {l1}");
        }
        Cmd::Fund {
            to_op,
            op_eth,
            to_l1,
            l1_eth,
            confirm,
            campaign,
        } => {
            let op = pair(to_op, &op_eth, money::FUND_MAX_OP, &confirm)?;
            let l1 = pair(to_l1, &l1_eth, money::FUND_MAX_L1, &confirm)?;
            if op.is_none() && l1.is_none() {
                bail!("nothing to send");
            }
            let key = std::env::var_os("DAVINCI_E2E_ORGANIZER_KEY")
                .context("DAVINCI_E2E_ORGANIZER_KEY (a key file)")?;
            let owner = net::read_key(std::path::Path::new(&key))?;
            let (op_rpc, ..) = net::live_target()?;
            let l1_rpc = std::env::var("DAVINCI_E2E_L1_RPC")
                .unwrap_or_default()
                .split(',')
                .next()
                .unwrap_or("")
                .trim()
                .to_string();
            if l1.is_some() && l1_rpc.is_empty() {
                bail!("DAVINCI_E2E_L1_RPC");
            }
            if let Some(dir) = &campaign {
                for (t, l1_side) in [(op, false), (l1, true)] {
                    if let Some((to, a)) = t {
                        money::record_inflow(dir, to, l1_side, a)?;
                    }
                }
            }
            money::fund(&owner, &op_rpc, &l1_rpc, op, l1).await?;
        }
        Cmd::StandIn {
            campaign,
            wave,
            elections,
        } => {
            let specs = parse_specs(&elections)?;
            let sdir = campaign.join("secret");
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(&sdir)
                .context("create secret dir")?;
            let seed = Seed::create(&sdir.join("seed"))?;
            let plan = Plan::stand_in(&seed, wave, &specs)?;
            plan.save(&campaign)?;
            println!("stand-in plan for wave {wave}: {} elections", specs.len());
        }
        Cmd::Gen {
            campaign,
            wave,
            host,
            hosts,
            threads,
            artifacts,
            host_name,
        } => {
            let plan = Plan::load(&campaign)?;
            let seed = Seed::load(&campaign.join("secret").join("seed"))?;
            for &n in &wave {
                plan.wave(n)?;
            }
            let prover = fixture::load_prover_from(&artifacts)?;
            let host_name = match host_name {
                Some(n) => n,
                None => std::fs::read_to_string("/etc/hostname")
                    .map(|s| s.trim().to_string())
                    .unwrap_or_else(|_| format!("host{host}")),
            };
            let o = GenOpts {
                host,
                hosts,
                threads,
                host_name,
                artifacts,
            };
            for n in wave {
                let st = tokio::task::block_in_place(|| {
                    gen_wave(&campaign, &plan, n, &seed, &prover, &o)
                })?;
                println!(
                    "gen wave {n} host {host}/{hosts}: {} shards ({} skipped), {} ballots in {:.0}s",
                    st.shards, st.skipped, st.ballots, st.secs
                );
            }
        }
        Cmd::Merge {
            campaign,
            wave,
            skip_proofs,
            rpc,
        } => {
            let plan = Plan::load(&campaign)?;
            let seed = Seed::load(&campaign.join("secret").join("seed"))?;
            if let Some(rpc) = rpc {
                check_snapshots(&plan, wave, &rpc).await?;
                println!("snapshots match the registry");
            }
            let v = if skip_proofs {
                Verify::Skip
            } else {
                Verify::Full
            };
            let idx = tokio::task::block_in_place(|| merge_wave(&campaign, &plan, wave, &seed, v))?;
            for (tag, i) in &idx {
                println!(
                    "{tag}: {} ballots {:?} sha256 {}",
                    i.ballots, i.rounds, i.sha256
                );
            }
        }
        Cmd::Plan {
            campaign,
            wave,
            scale,
            onchain_census,
            rig,
        } => {
            require_live("plan")?;
            check_scale(scale)?;
            let waves = wave
                .iter()
                .map(|&n| {
                    let specs = wave_specs(n, scale);
                    ensure!(!specs.is_empty(), "wave {n} has no elections yet");
                    Ok((n, specs))
                })
                .collect::<Result<Vec<_>>>()?;
            let campaign = campaign_dir(&campaign)?;
            held_step(false, async |h: &mut Held| {
                let net = h.net.insert(Net::open(&campaign).await?);
                plan_waves(
                    net,
                    &mut h.rig,
                    &campaign,
                    &waves,
                    scale,
                    onchain_census,
                    &rig,
                )
                .await
            })
            .await?;
        }
        Cmd::Run {
            campaign,
            wave,
            scenario,
            rig,
        } => {
            require_live("run")?;
            let campaign = campaign
                .canonicalize()
                .with_context(|| format!("campaign {}", campaign.display()))?;
            let plan = Plan::load(&campaign)?;
            // The wave's own scale: the ceiling bursts must match its voters.
            let scs = bench_run::select(scenarios(wave, plan.scale(wave)?), &scenario)?;
            let seed = load_seed(&campaign)?;
            let path = campaign.join("budget.json");
            let budget: money::Budget = serde_json::from_slice(
                &std::fs::read(&path).context("budget.json: run `gate --campaign` first")?,
            )
            .with_context(|| format!("parse {}", path.display()))?;
            held_step(false, async move |h: &mut Held| {
                let net = Net::open(&campaign).await?;
                check_plan(&plan, &net)?;
                let ctx = h.ctx.insert(Ctx {
                    net,
                    plan,
                    campaign: campaign.clone(),
                    seed,
                    budget: Some(budget),
                });
                let rig = h.rig.insert(rig_up(&ctx.net, &campaign, &rig).await?);
                run_scenarios(ctx, rig, &mut h.chaos, &scs).await
            })
            .await?;
        }
        Cmd::Audit { campaign, run, rig } => {
            require_live("audit")?;
            let campaign = campaign
                .canonicalize()
                .with_context(|| format!("campaign {}", campaign.display()))?;
            let plan = Plan::load(&campaign)?;
            let seed = load_seed(&campaign)?;
            held_step(false, async move |h: &mut Held| {
                let net = Net::open(&campaign).await?;
                check_plan(&plan, &net)?;
                let ctx = h.ctx.insert(Ctx {
                    net,
                    plan,
                    campaign: campaign.clone(),
                    seed,
                    budget: None,
                });
                let rig = h.rig.insert(rig_up(&ctx.net, &campaign, &rig).await?);
                let a = bench_run::audit_run(ctx, rig, &run).await?;
                println!(
                    "audit {}: no_vote_lost={} clean={} tally_ok={} results_ok={} counters_ok={}",
                    run.display(),
                    a.no_vote_lost,
                    a.clean,
                    a.tally_ok,
                    a.results_ok,
                    a.counters_ok
                );
                Ok(())
            })
            .await?;
        }
        Cmd::Serve { campaign, rig } => {
            require_live("serve")?;
            let campaign = campaign
                .canonicalize()
                .with_context(|| format!("campaign {}", campaign.display()))?;
            let plan = Plan::load(&campaign)?;
            held_step(true, async |h: &mut Held| -> Result<()> {
                let net = h.net.insert(Net::open(&campaign).await?);
                check_plan(&plan, net)?;
                let rig = h.rig.insert(rig_up(net, &campaign, &rig).await?);
                println!("serving until SIGTERM");
                loop {
                    tokio::time::sleep(Duration::from_secs(10)).await;
                    for r in [Role::A, Role::B, Role::O] {
                        rig.node(r)?.check_alive()?;
                    }
                }
            })
            .await?;
        }
        Cmd::Cancel {
            campaign,
            wave,
            all: _,
        } => {
            let plan = Plan::load(&campaign)?;
            let (rpc, ..) = net::live_target()?;
            let key = std::env::var_os("DAVINCI_E2E_ORGANIZER_KEY")
                .context("DAVINCI_E2E_ORGANIZER_KEY (a key file)")?;
            let org = Organizer::connect(&rpc, net::read_key(Path::new(&key))?, plan.registry)?
                .with_receipt_timeout(net::scaled(Duration::from_secs(180)));
            let chain_id = org.provider().get_chain_id().await.context("chain id")?;
            ensure!(
                chain_id == plan.chain_id,
                "the RPC is on chain {chain_id}, the plan on {}",
                plan.chain_id
            );
            // clap makes `--wave` and `--all` exclusive and one required.
            let scope = match wave {
                Some(n) => CancelScope::Wave(n),
                None => CancelScope::All,
            };
            let pids = cancel_candidates(&plan, &rpc, org.address(), scope).await?;
            let n = cancel_open(&org, &pids).await?;
            println!("canceled {n} of {} processes", pids.len());
        }
        Cmd::Dry {
            scenario,
            scale,
            stop_after,
            threads,
            artifacts,
            campaign,
            rig,
        } => {
            ensure!(!net::is_live(), "dry runs on anvil: unset DAVINCI_E2E_LIVE");
            check_scale(scale)?;
            let only_plan = match stop_after.as_deref() {
                Some("plan") => true,
                Some(s) => bail!("--stop-after {s}: only plan"),
                None => false,
            };
            let waves = scenario_specs(&scenario, scale)?;
            let artifacts = artifacts.unwrap_or_else(|| {
                PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../davinci-circom/artifacts")
            });
            let campaign = match campaign {
                Some(c) => c,
                None => PathBuf::from(std::env::var_os("HOME").context("HOME")?)
                    .join(".cache/davinci-bench")
                    .join(format!(
                        "dry-{}",
                        std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)?
                            .as_secs()
                    )),
            };
            // Its anvil is new, with the same registry address: an old
            // plan would name processes that do not exist.
            ensure!(
                !campaign.join("plan.json").exists(),
                "dry needs a fresh campaign dir"
            );
            let campaign = campaign_dir(&campaign)?;
            println!("dry campaign {}", campaign.display());
            let chain_dir = campaign.join("chain");
            std::fs::create_dir_all(&chain_dir)?;
            held_step(false, async move |h: &mut Held| {
                let net = h.net.insert(Net::open(&chain_dir).await?);
                plan_waves(net, &mut h.rig, &campaign, &waves, scale, true, &rig).await?;
                if only_plan {
                    // Exercises `cancel --all` on what the plan created.
                    let plan = Plan::load(&campaign)?;
                    let org = net.organizer()?;
                    let pids =
                        cancel_candidates(&plan, &net.rpc, org.address(), CancelScope::All).await?;
                    let n = cancel_open(&org, &pids).await?;
                    println!("cancel --all: canceled {n} of {} processes", pids.len());
                    println!("dry stopped after plan");
                    return Ok(());
                }
                let plan = Plan::load(&campaign)?;
                let seed = load_seed(&campaign)?;
                for (n, _) in &waves {
                    let n = *n;
                    let (c, p, s, a) = (
                        campaign.clone(),
                        plan.clone(),
                        seed.clone(),
                        artifacts.clone(),
                    );
                    let st = off_runtime(move || {
                        let prover = fixture::load_prover_from(&a)?;
                        let o = GenOpts {
                            host: 0,
                            hosts: 1,
                            threads,
                            host_name: "dry".into(),
                            artifacts: a,
                        };
                        gen_wave(&c, &p, n, &s, &prover, &o)
                    })
                    .await?;
                    println!("gen wave {n}: {} ballots in {:.0}s", st.ballots, st.secs);
                    let (c, p, s) = (campaign.clone(), plan.clone(), seed.clone());
                    let idx = off_runtime(move || merge_wave(&c, &p, n, &s, Verify::Full)).await?;
                    let total: usize = idx.values().map(|i| i.ballots).sum();
                    println!("merge wave {n}: {} elections, {total} ballots", idx.len());
                }
                // The net moves into the run's context; no await in between.
                let net = h.net.take().context("net")?;
                let ctx = h.ctx.insert(Ctx {
                    net,
                    plan,
                    campaign: campaign.clone(),
                    seed,
                    budget: None,
                });
                let rig = h.rig.as_mut().context("rig")?;
                for (n, _) in &waves {
                    // The wave's own scale, as `run` reads it.
                    let all = scenarios(*n, ctx.plan.scale(*n)?);
                    let names: Vec<String> = scenario
                        .iter()
                        .filter(|s| all.iter().any(|x| &x.name == *s))
                        .cloned()
                        .collect();
                    let scs = bench_run::select(all, &names)?;
                    run_scenarios(ctx, rig, &mut h.chaos, &scs).await?;
                }
                println!("dry done: {}", campaign.display());
                Ok(())
            })
            .await?;
        }
        Cmd::Gate { campaign } => {
            let r = money::gate_check(campaign.as_deref(), &[]).await?;
            for l in &r.lines {
                println!("{l}");
            }
            for p in &r.problems {
                println!("NO-GO: {p}");
            }
            println!("{}", if r.go() { "GO" } else { "NO-GO" });
            return Ok(ExitCode::from(if r.go() { 0 } else { 3 }));
        }
    }
    Ok(ExitCode::SUCCESS)
}

#[tokio::main]
async fn main() -> ExitCode {
    match real_main().await {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: {}", davinci_e2e::redact_urls(&format!("{e:#}")));
            ExitCode::from(1)
        }
    }
}
