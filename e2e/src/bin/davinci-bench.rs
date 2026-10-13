//! The live benchmark's controller. Money steps only for now: `keygen`,
//! `fund` and `gate`. Keys come from files (`DAVINCI_E2E_ORGANIZER_KEY` names
//! one) and are never printed.

use std::path::PathBuf;
use std::process::ExitCode;

use alloy::primitives::utils::parse_ether;
use alloy::primitives::{Address, U256};
use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use davinci_e2e::bench::r#gen::{GenOpts, gen_wave};
use davinci_e2e::bench::merge::{Verify, check_snapshots, merge_wave};
use davinci_e2e::bench::money;
use davinci_e2e::bench::plan::{ElectionSpec, NodeId, Plan, Seed};
use davinci_e2e::fixture;
use davinci_e2e::net;

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
    /// Prove this host's ballot shards of a wave.
    Gen {
        #[arg(long, env = "DAVINCI_BENCH_DIR")]
        campaign: PathBuf,
        #[arg(long)]
        wave: u32,
        #[arg(long)]
        host: usize,
        #[arg(long)]
        hosts: usize,
        #[arg(long, default_value_t = 8)]
        threads: usize,
        #[arg(long, env = "CIRCOM_ARTIFACTS")]
        artifacts: PathBuf,
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
            use std::os::unix::fs::DirBuilderExt;
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
        } => {
            let plan = Plan::load(&campaign)?;
            let seed = Seed::load(&campaign.join("secret").join("seed"))?;
            let prover = fixture::load_prover_from(&artifacts)?;
            let host_name = std::env::var("HOSTNAME").unwrap_or_else(|_| format!("host{host}"));
            let o = GenOpts {
                host,
                hosts,
                threads,
                host_name,
                artifacts,
            };
            let st = tokio::task::block_in_place(|| {
                gen_wave(&campaign, &plan, wave, &seed, &prover, &o)
            })?;
            println!(
                "gen host {host}/{hosts}: {} shards ({} skipped), {} ballots in {:.0}s",
                st.shards, st.skipped, st.ballots, st.secs
            );
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
