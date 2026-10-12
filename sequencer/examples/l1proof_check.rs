//! Live check of L1BlobVerifier on OP Mainnet. Secrets are read from files and never printed;
//! errors from the OP endpoint are reduced to a generic message because they may embed the URL.
//! Run: cargo run -p davinci-sequencer --example l1proof_check -- --op-rpc-file F --key-file F
//! [--l1-rpc URL] deploy | check --verifier ADDR [--txs N]

use alloy::eips::eip2718::Encodable2718;
use alloy::network::{EthereumWallet, TransactionBuilder};
use alloy::primitives::{Address, B256, Bytes, U256, address, keccak256};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::rpc::types::TransactionRequest;
use alloy::signers::local::PrivateKeySigner;
use alloy::sol_types::{SolCall, SolValue};
use davinci_sequencer::web3::l1proof::{L1BlobProof, build, verify_local};
use std::time::Duration;

alloy::sol! {
    struct TxProof { uint256 headerIndex; bytes key; bytes[] proof; }
    struct L1Proof { bytes[] headers; TxProof[] txs; }
    function verify(L1Proof p) external view returns (bytes32[] vhs, uint256[] txBlocks);
    function number() external view returns (uint64);
    function hash() external view returns (bytes32);
}

const L1_BLOCK: Address = address!("4200000000000000000000000000000000000015");

type R<T> = Result<T, String>;

/// Generic error: the source may embed the endpoint URL.
fn g<E>(what: &'static str) -> impl FnOnce(E) -> String {
    move |_| format!("{what} failed")
}

fn arg(args: &[String], flag: &str) -> Option<String> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1).cloned())
}

fn to_sol(p: &L1BlobProof) -> L1Proof {
    L1Proof {
        headers: p.headers.clone(),
        txs: p
            .txs
            .iter()
            .map(|t| TxProof {
                headerIndex: U256::from(t.header_index),
                key: t.key.clone(),
                proof: t.proof.clone(),
            })
            .collect(),
    }
}

/// Origin number and hash read at one OP block, so they are consistent.
async fn origin<P: Provider>(op: &P, at: u64) -> R<(u64, B256)> {
    let call = |data: Vec<u8>| {
        TransactionRequest::default()
            .to(L1_BLOCK)
            .input(Bytes::from(data).into())
    };
    let n = op
        .call(call(numberCall {}.abi_encode()))
        .block(at.into())
        .await
        .map_err(g("L1Block.number"))?;
    let h = op
        .call(call(hashCall {}.abi_encode()))
        .block(at.into())
        .await
        .map_err(g("L1Block.hash"))?;
    let n = numberCall::abi_decode_returns(&n).map_err(g("decode number"))?;
    let h = hashCall::abi_decode_returns(&h).map_err(g("decode hash"))?;
    Ok((n, h))
}

struct Outcome {
    origin: u64,
    gas: u64,
}

async fn check_once<P: Provider>(op: &P, verifier: Address, proof: &L1BlobProof) -> R<Outcome> {
    let at = op.get_block_number().await.map_err(g("OP block number"))?;
    let (n, h) = origin(op, at).await?;
    let (vhs, blocks) = verify_local(proof, h).map_err(|e| format!("verify_local: {e}"))?;
    let data = verifyCall { p: to_sol(proof) }.abi_encode();
    let req = TransactionRequest::default()
        .to(verifier)
        .input(Bytes::from(data).into());
    let out = op
        .call(req.clone())
        .block(at.into())
        .await
        .map_err(g("eth_call verify"))?;
    let r = verifyCall::abi_decode_returns(&out).map_err(g("decode verify"))?;
    let got_blocks: Vec<u64> = r.txBlocks.iter().map(|b| b.to::<u64>()).collect();
    if r.vhs != vhs || got_blocks != blocks {
        return Err(format!(
            "on-chain result differs: vhs {:?} vs {vhs:?}, blocks {got_blocks:?} vs {blocks:?}",
            r.vhs
        ));
    }
    let gas = op
        .estimate_gas(req)
        .block(at.into())
        .await
        .map_err(g("estimateGas"))?;
    println!("  ok at origin {n}: vhs {vhs:?} tx blocks {blocks:?} estimateGas {gas}");
    Ok(Outcome { origin: n, gas })
}

async fn deploy<P: Provider>(op: &P, from: Address, bytecode: Vec<u8>) -> R<()> {
    let mut code = bytecode;
    code.extend_from_slice(&L1_BLOCK.abi_encode());
    let block = op
        .get_block_by_number(Default::default())
        .await
        .map_err(g("latest block"))?
        .ok_or("no latest block")?;
    let base = block.header.base_fee_per_gas.ok_or("no base fee")? as u128;
    let tip = 10_000u128;
    let req = TransactionRequest::default()
        .from(from)
        .into_create()
        .with_input(Bytes::from(code))
        .with_max_priority_fee_per_gas(tip)
        .with_max_fee_per_gas(2 * base + tip);
    let pending = op.send_transaction(req).await.map_err(g("send deploy"))?;
    let rc = pending.get_receipt().await.map_err(g("deploy receipt"))?;
    if !rc.status() {
        return Err(format!("deploy reverted, tx {}", rc.transaction_hash));
    }
    println!("verifier {}", rc.contract_address.ok_or("no address")?);
    println!("deploy tx {}", rc.transaction_hash);
    println!(
        "gas used {} effective gas price {} cost wei {}",
        rc.gas_used,
        rc.effective_gas_price,
        rc.gas_used as u128 * rc.effective_gas_price
    );
    println!("(L2 cost only; the L1 data fee is extra and shows in the balance change)");
    Ok(())
}

async fn run() -> R<()> {
    let args: Vec<String> = std::env::args().collect();
    let rpc_file = arg(&args, "--op-rpc-file").ok_or("--op-rpc-file required")?;
    let key_file = arg(&args, "--key-file").ok_or("--key-file required")?;
    let l1_rpc =
        arg(&args, "--l1-rpc").unwrap_or_else(|| "https://ethereum-rpc.publicnode.com".into());
    let op_url = std::fs::read_to_string(rpc_file).map_err(g("read op rpc file"))?;
    let op_url = op_url.lines().next().unwrap_or("").trim().to_string();
    let key = std::fs::read_to_string(key_file).map_err(g("read key file"))?;
    let signer: PrivateKeySigner = key.trim().parse().map_err(g("parse key"))?;
    let from = signer.address();
    let op = ProviderBuilder::new()
        .wallet(EthereumWallet::from(signer))
        .connect_http(op_url.parse().map_err(g("parse op url"))?);
    let l1 = ProviderBuilder::new().connect_http(l1_rpc.parse().map_err(g("parse l1 url"))?);
    let cmd = args
        .iter()
        .find(|a| *a == "deploy" || *a == "check")
        .ok_or("subcommand deploy|check required")?;

    if cmd == "deploy" {
        let art = std::fs::read_to_string(
            "../davinci-contracts/out/L1BlobVerifier.sol/L1BlobVerifier.json",
        )
        .map_err(g("read artifact"))?;
        let v: serde_json::Value = serde_json::from_str(&art).map_err(g("parse artifact"))?;
        let hex = v["bytecode"]["object"].as_str().ok_or("no bytecode")?;
        let code = hex::decode(hex.trim_start_matches("0x")).map_err(g("decode bytecode"))?;
        return deploy(&op, from, code).await;
    }

    let verifier: Address = arg(&args, "--verifier")
        .ok_or("--verifier required")?
        .parse()
        .map_err(g("parse verifier"))?;
    let want: usize = arg(&args, "--txs")
        .map(|s| s.parse().map_err(g("parse --txs")))
        .transpose()?
        .unwrap_or(3);
    let head = l1.get_block_number().await.map_err(g("L1 head"))?;
    let at = op.get_block_number().await.map_err(g("OP block number"))?;
    let (o, _) = origin(&op, at).await?;
    println!("L1 head {head}, OP origin {o}");

    // Latest `want` blob txs at or below the origin, scanning down.
    let mut picks: Vec<(u64, usize, B256, usize)> = vec![];
    let mut n = o.min(head);
    while picks.len() < want && n > 0 {
        let b = l1
            .get_block_by_number(n.into())
            .full()
            .await
            .map_err(g("L1 block"))?
            .ok_or("L1 block missing")?;
        let txs: Vec<_> = b.transactions.txns().collect();
        for (i, t) in txs.into_iter().enumerate().rev() {
            if t.inner.tx_type() as u8 == 3 && picks.len() < want {
                let env = t.inner.encoded_2718();
                picks.push((n, i, keccak256(&env), env.len()));
            }
        }
        n -= 1;
    }

    for (blk, idx, hash, _) in picks {
        println!("tx {hash} L1 block {blk} index {idx}");
        let proof = build(&l1, &[hash], head)
            .await
            .map_err(|e| format!("build: {e}"))?;
        let calldata = verifyCall { p: to_sol(&proof) }.abi_encode().len();
        println!(
            "  headers {} calldata bytes {calldata}",
            proof.headers.len()
        );
        let a = check_once(&op, verifier, &proof).await?;
        tokio::time::sleep(Duration::from_secs(15)).await;
        let mut waited = 0;
        loop {
            let at = op.get_block_number().await.map_err(g("OP block number"))?;
            let (n2, _) = origin(&op, at).await?;
            if n2 != a.origin || waited >= 60 {
                break;
            }
            tokio::time::sleep(Duration::from_secs(5)).await;
            waited += 5;
        }
        let b = check_once(&op, verifier, &proof).await?;
        println!(
            "  summary: origin {} then {}, estimateGas {}",
            a.origin, b.origin, a.gas
        );
    }
    Ok(())
}

#[tokio::main]
async fn main() {
    if let Err(e) = run().await {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}
