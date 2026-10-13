//! The live benchmark's controller. Money steps only for now: `keygen`,
//! `fund` and `gate`. Keys come from files (`DAVINCI_E2E_ORGANIZER_KEY` names
//! one) and are never printed.

use std::path::PathBuf;
use std::process::ExitCode;

use alloy::primitives::utils::parse_ether;
use alloy::primitives::{Address, U256};
use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use davinci_e2e::bench::money;
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
    },
    /// Print the go/no-go gate; exit 0 on a go, 3 on a no-go.
    Gate {
        #[arg(long)]
        campaign: Option<PathBuf>,
    },
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
            money::fund(&owner, &op_rpc, &l1_rpc, op, l1).await?;
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
