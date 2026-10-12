//! DAVINCI "blobs on L1, proven on OP through the beacon root" PoC driver. See ../README.md.
mod da;
mod rpc;

use crate::da::*;
use crate::rpc::*;
use alloy::consensus::{SignableTransaction, TxEip1559};
use alloy::eips::eip2718::Encodable2718;
use alloy::eips::eip4844::env_settings::EnvKzgSettings;
use alloy::eips::eip4844::Blob as AlloyBlob;
use alloy::eips::eip7594::BlobTransactionSidecarVariant;
use alloy::consensus::BlobTransactionSidecarEip7594;
use alloy::network::{EthereumWallet, NetworkTransactionBuilder, TransactionBuilder, TransactionBuilder4844};
use alloy::primitives::{address, Address, Bytes, FixedBytes, TxKind, B256, U256};
use alloy::rpc::types::TransactionRequest;
use alloy::signers::local::PrivateKeySigner;
use alloy::signers::SignerSync;
use alloy::sol;
use alloy::sol_types::{SolCall, SolEvent, SolValue};
use anyhow::{anyhow, bail, Context, Result};
use clap::{Args, Parser, Subcommand};
use rand::RngCore;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

sol! {
    struct BeaconHeader { uint64 slot; uint64 proposerIndex; bytes32 parentRoot; bytes32 stateRoot; bytes32 bodyRoot; }
    struct BlobItem { bytes commitment; bytes32 y; bytes kzgProof; uint64 index; bytes32[17] branch; }
    function settle(bytes31 pid, bytes32 rootBefore, bytes32 rootAfter, BeaconHeader h, uint256 l2Timestamp, BlobItem[] blobs, bytes publicValues, bytes proofBytes) external returns (bytes32);
    event Settled(bytes31 indexed processId, bytes32 rootBefore, bytes32 rootAfter, bytes32 beaconRoot, bytes32 blobsDigest, uint256 nBlobs);
}

const BEACON_ROOTS: Address = address!("000F3df6D732807Ef1319fB7B8bB8522d0Beac02");
const L1_BLOCK: Address = address!("4200000000000000000000000000000000000015");
const GAS_PRICE_ORACLE: Address = address!("420000000000000000000000000000000000000F");
const OP_VERIFIER: &str = "0x6781FFF50D615CB3F331CCD3Bdad3416d4ce0d5c";
const MIN_L1_TIP_WEI: u128 = 1_000_000; // 0.001 gwei
/// Floor of the blob fee cap: 0.0001 gwei. Worst-case blob spend of a 6-blob tx at this cap is 7.9e13 wei (0.00008 ETH).
const MIN_BLOB_CAP_WEI: u128 = 100_000;
const BLOB_CAP_MULT: u128 = 3;

// ---------------------------------------------------------------- cli

#[derive(Parser)]
#[command(about = "DAVINCI L1-blob DA PoC driver")]
struct Cli {
    #[arg(long, global = true, default_value = "https://ethereum-rpc.publicnode.com")]
    l1_rpc: String,
    /// L1 endpoint used only for eth_sendRawTransaction (multi-blob txs exceed some public RPCs' body limits)
    #[arg(long, global = true)]
    l1_send_rpc: Option<String>,
    /// File whose first line is the OP RPC URL
    #[arg(long, global = true)]
    op_rpc_file: Option<PathBuf>,
    #[arg(long, global = true, default_value = "https://ethereum-beacon-api.publicnode.com")]
    beacon: String,
    /// File with the 0x-prefixed hex private key (same key on L1 and OP)
    #[arg(long, global = true)]
    key_file: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Signer address, balances, chain ids, fees, OP L1-origin lag
    Address,
    /// Deploy BlobDAPoC on OP (broadcasts)
    DeployOp(DeployArgs),
    /// Offline: compute (kzgProof, y) for the forge test from fixtures/beacon_sidecar.json
    Fixture {
        #[arg(long, default_value = "fixtures/beacon_sidecar.json")]
        sidecar: PathBuf,
        #[arg(long, default_value = "fixtures/settle_fixture.json")]
        out: PathBuf,
    },
    /// Full flow: L1 blob tx, beacon proof, OP settle (broadcasts on both chains)
    Run(RunArgs),
    /// Build and sign the L1 blob tx without sending; estimate costs from current fees
    DryRun(RunArgs),
    /// Read-only: locate and verify an existing L1 blob tx's commitments in the beacon block
    VerifyL1 {
        #[arg(long)]
        tx: String,
    },
    /// Read-only: find an OP timestamp T whose 4788 root equals the latest OP block's
    ProbeOp,
}

#[derive(Args)]
struct DeployArgs {
    #[arg(long)]
    poc_artifact: PathBuf,
    #[arg(long)]
    fixture: PathBuf,
    #[arg(long, default_value = OP_VERIFIER)]
    verifier: Address,
}

#[derive(Args, Clone)]
struct RunArgs {
    #[arg(long, default_value = "")]
    poc: String,
    #[arg(long)]
    scenario: String,
    /// Stream length override (max6 defaults to 761856)
    #[arg(long)]
    bytes: Option<usize>,
    #[arg(long, default_value_t = 1)]
    repeat: usize,
    /// Simulate proving during the post: send the L1 tx first, proof ready S seconds later
    #[arg(long)]
    post_delay_secs: Option<u64>,
    /// L1 priority fee (default: max(RPC suggestion, 0.001 gwei))
    #[arg(long)]
    l1_tip_wei: Option<u128>,
    /// Blob fee cap override (default: max(3 x blob base fee, 0.0001 gwei))
    #[arg(long)]
    blob_fee_cap_wei: Option<u128>,
    #[arg(long, default_value_t = 10_000)]
    op_tip_wei: u128,
    #[arg(long)]
    no_proof: bool,
    #[arg(long)]
    fixture: Option<PathBuf>,
    #[arg(long, default_value_t = 2500.0)]
    eth_usd: f64,
    #[arg(long, default_value = "results/run.json")]
    out: PathBuf,
}

struct Ctx {
    l1: Rpc,
    l1_send: Option<Rpc>,
    op: Option<Rpc>,
    beacon: Beacon,
    signer: Option<PrivateKeySigner>,
}

impl Ctx {
    fn new(cli: &Cli) -> Result<Self> {
        let op = match &cli.op_rpc_file {
            Some(f) => {
                let u = std::fs::read_to_string(f).context("read --op-rpc-file")?;
                Some(Rpc::new(u.lines().next().unwrap_or("").trim().to_string())?)
            }
            None => None,
        };
        let signer = match &cli.key_file {
            Some(f) => {
                let k = std::fs::read_to_string(f).context("read --key-file")?;
                Some(k.trim().parse::<PrivateKeySigner>().map_err(|_| anyhow!("invalid private key in key file"))?)
            }
            None => None,
        };
        let l1_send = cli.l1_send_rpc.clone().map(Rpc::new).transpose()?;
        Ok(Self { l1: Rpc::new(cli.l1_rpc.clone())?, l1_send, op, beacon: Beacon::new(cli.beacon.clone())?, signer })
    }
    fn op(&self) -> Result<&Rpc> {
        self.op.as_ref().context("need --op-rpc-file")
    }
    fn signer(&self) -> Result<&PrivateKeySigner> {
        self.signer.as_ref().context("need --key-file")
    }
}

fn eth(w: u128) -> f64 {
    w as f64 / 1e18
}

fn wei(m: &Value, k: &str) -> u128 {
    m[k].as_str().and_then(|s| s.parse().ok()).unwrap_or(0)
}

fn b32(x: [u8; 32]) -> String {
    format!("0x{}", hex::encode(x))
}

// ---------------------------------------------------------------- OP txs

#[allow(clippy::too_many_arguments)]
fn sign_1559(signer: &PrivateKeySigner, chain_id: u64, nonce: u64, gas: u64, max_fee: u128, prio: u128, to: TxKind, input: Vec<u8>) -> Result<(Vec<u8>, B256)> {
    let tx = TxEip1559 {
        chain_id,
        nonce,
        gas_limit: gas,
        max_fee_per_gas: max_fee,
        max_priority_fee_per_gas: prio,
        to,
        value: U256::ZERO,
        access_list: Default::default(),
        input: Bytes::from(input),
    };
    let sig = signer.sign_hash_sync(&tx.signature_hash())?;
    let signed = tx.into_signed(sig);
    Ok((signed.encoded_2718(), *signed.hash()))
}

fn receipt_metrics(r: &Value) -> Value {
    let g = r.get("gasUsed").and_then(q).unwrap_or(0);
    let egp = r.get("effectiveGasPrice").and_then(q).unwrap_or(0);
    let l1_fee = r.get("l1Fee").and_then(q).unwrap_or(0);
    let op_scalar = r.get("operatorFeeScalar").and_then(q).unwrap_or(0);
    let op_const = r.get("operatorFeeConstant").and_then(q).unwrap_or(0);
    let op_fee = if r.get("operatorFeeScalar").is_some() || r.get("operatorFeeConstant").is_some() { g * op_scalar / 1_000_000 + op_const } else { 0 };
    let l2_fee = g * egp;
    let mut m = json!({
        "block": r.get("blockNumber").and_then(q),
        "gas_used": g as u64,
        "effective_gas_price": egp.to_string(),
        "status": r.get("status").and_then(q),
        "l2_fee_wei": l2_fee.to_string(),
        "l1_fee_wei": l1_fee.to_string(),
        "operator_fee_wei": op_fee.to_string(),
        "cost_wei": (l2_fee + l1_fee + op_fee).to_string(),
    });
    for k in ["l1Fee", "l1GasUsed", "l1GasPrice", "l1BlobBaseFee", "l1BaseFeeScalar", "l1BlobBaseFeeScalar", "operatorFeeScalar", "operatorFeeConstant"] {
        if let Some(v) = r.get(k) {
            m[k] = v.clone();
        }
    }
    m
}

fn artifact_bytecode(p: &PathBuf) -> Result<Vec<u8>> {
    let v: Value = serde_json::from_slice(&std::fs::read(p).with_context(|| format!("read {}", p.display()))?)?;
    let o = v["bytecode"]["object"].as_str().or_else(|| v["bytecode"].as_str()).context("no bytecode in artifact")?;
    Ok(hex::decode(o.trim_start_matches("0x"))?)
}

// ---------------------------------------------------------------- L1 blob tx

struct L1Tx {
    raw: Vec<u8>,
    hash: B256,
    nonce: u64,
    base_fee: u128,
    tip: u128,
    max_fee: u128,
    blob_base_fee: u128,
    blob_cap: u128,
    blob_gas: u128,
}

fn build_sidecar(blobs: &[Vec<u8>]) -> Result<BlobTransactionSidecarVariant> {
    let raw: Vec<AlloyBlob> = blobs.iter().map(|b| AlloyBlob::from_slice(b)).collect();
    let sc = BlobTransactionSidecarEip7594::try_from_blobs_with_settings(raw, EnvKzgSettings::Default.get())
        .map_err(|e| anyhow!("sidecar: {e}"))?;
    Ok(BlobTransactionSidecarVariant::Eip7594(sc))
}

/// Type-3 tx to the signer's own address, EIP-7594 v1 sidecar (cell proofs), signed locally.
async fn build_l1(c: &Ctx, blobs: &[Vec<u8>], a: &RunArgs) -> Result<L1Tx> {
    let signer = c.signer()?.clone();
    let me = signer.address();
    let chain_id = c.l1.chain_id().await?;
    let nonce = c.l1.nonce(me).await?;
    let base_fee = c.l1.base_fee().await?;
    let blob_base_fee = c.l1.blob_base_fee().await?;
    let tip = match a.l1_tip_wei {
        Some(t) => t,
        None => c.l1.priority_suggestion().await?.max(MIN_L1_TIP_WEI),
    };
    let max_fee = 2 * base_fee + tip;
    let blob_cap = a.blob_fee_cap_wei.unwrap_or((BLOB_CAP_MULT * blob_base_fee).max(MIN_BLOB_CAP_WEI));
    let owned = blobs.to_vec();
    let sidecar = tokio::task::spawn_blocking(move || build_sidecar(&owned)).await??;
    let mut req = TransactionRequest::default()
        .with_from(me)
        .with_to(me)
        .with_value(U256::ZERO)
        .with_nonce(nonce)
        .with_chain_id(chain_id)
        .with_gas_limit(21_000)
        .with_max_fee_per_gas(max_fee)
        .with_max_priority_fee_per_gas(tip);
    req.set_max_fee_per_blob_gas(blob_cap);
    req.set_blob_sidecar(sidecar);
    let env = req.build(&EthereumWallet::from(signer)).await.map_err(|e| anyhow!("signing: {e}"))?;
    Ok(L1Tx {
        raw: env.encoded_2718(),
        hash: *env.tx_hash(),
        nonce,
        base_fee,
        tip,
        max_fee,
        blob_base_fee,
        blob_cap,
        blob_gas: (blobs.len() * BLOB_BYTES) as u128,
    })
}

// ---------------------------------------------------------------- beacon proof

struct Located {
    slot: u64,
    root: [u8; 32],
    hdr: (u64, u64, [u8; 32], [u8; 32], [u8; 32]),
    commitments: Vec<Vec<u8>>,
    indices: Vec<u64>,
    branches: Vec<[[u8; 32]; 17]>,
    next_block: u64,
    checks: Value,
}

fn parse_sidecar(sc: &Value) -> Result<(u64, Vec<u8>, [[u8; 32]; 17], (u64, u64, [u8; 32], [u8; 32], [u8; 32]))> {
    let index = q(&sc["index"]).context("index")? as u64;
    let com = hex_bytes(&sc["kzg_commitment"])?;
    let br: Vec<[u8; 32]> = sc["kzg_commitment_inclusion_proof"].as_array().context("inclusion proof")?.iter().map(h32).collect::<Result<_>>()?;
    let br: [[u8; 32]; 17] = br.try_into().map_err(|_| anyhow!("inclusion proof is not 17 nodes"))?;
    let m = &sc["signed_block_header"]["message"];
    let hdr = (
        q(&m["slot"]).context("slot")? as u64,
        q(&m["proposer_index"]).context("proposer")? as u64,
        h32(&m["parent_root"])?,
        h32(&m["state_root"])?,
        h32(&m["body_root"])?,
    );
    Ok((index, com, br, hdr))
}

/// Finds the blobs with the given versioned hashes in the beacon block at `slot`, checks the inclusion
/// proofs, the header against the beacon API's block root and against L1 block n+1's parentBeaconBlockRoot.
async fn locate(c: &Ctx, slot: u64, l1_block: u64, vhs: &[[u8; 32]]) -> Result<Located> {
    let sc = c.beacon.get_wait(&format!("/eth/v1/beacon/blob_sidecars/{slot}"), Duration::from_secs(90)).await?;
    let list = sc["data"].as_array().context("sidecars data")?;
    let parsed: Vec<_> = list.iter().map(parse_sidecar).collect::<Result<_>>()?;
    let mut commitments = vec![];
    let mut indices = vec![];
    let mut branches = vec![];
    let mut hdr = None;
    let mut inclusion_ok = true;
    for vh in vhs {
        let (idx, com, br, h) = parsed
            .iter()
            .find(|(_, com, _, _)| &versioned_hash(com) == vh)
            .with_context(|| format!("blob {} not in the sidecars of slot {slot}", b32(*vh)))?;
        inclusion_ok &= commitment_root(com, *idx, br) == h.4;
        if let Some(prev) = hdr {
            if prev != *h {
                bail!("sidecars of slot {slot} carry different headers");
            }
        }
        hdr = Some(*h);
        commitments.push(com.clone());
        indices.push(*idx);
        branches.push(*br);
    }
    let h = hdr.context("no blobs requested")?;
    if h.0 != slot {
        bail!("header slot {} != {slot}", h.0);
    }
    let root = header_root(h.0, h.1, &h.2, &h.3, &h.4);
    let bh = c.beacon.get_wait(&format!("/eth/v1/beacon/headers/{slot}"), Duration::from_secs(90)).await?;
    let api_root = h32(&bh["data"]["root"])?;
    let header_ok = root == api_root;
    // L1 block n+1 carries the root of the previous beacon block, i.e. this one (also when the next slot is missed)
    let next = l1_block + 1;
    let t0 = std::time::Instant::now();
    let nb = loop {
        let b = c.l1.block(&format!("0x{next:x}")).await?;
        if !b.is_null() {
            break b;
        }
        if t0.elapsed() > Duration::from_secs(120) {
            bail!("L1 block {next} not available after 120 s");
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    };
    let next_ok = h32(&nb["parentBeaconBlockRoot"])? == root;
    let checks = json!({"inclusion_proof_ok": inclusion_ok, "header_root_matches_beacon_api": header_ok, "l1_next_parent_root_ok": next_ok});
    if !(inclusion_ok && header_ok && next_ok) {
        bail!("beacon proof checks failed: {checks}");
    }
    Ok(Located { slot, root, hdr: h, commitments, indices, branches, next_block: next, checks })
}

// ---------------------------------------------------------------- OP helpers

async fn op_origin(op: &Rpc) -> Result<u64> {
    let sel = hex::decode("8381f58a")?; // number()
    let r = op.eth_call(L1_BLOCK, &sel).await?;
    Ok(U256::from_be_slice(&r).to::<u64>())
}

async fn root_at(op: &Rpc, t: u64) -> Result<Option<[u8; 32]>> {
    match op.eth_call(BEACON_ROOTS, &U256::from(t).to_be_bytes::<32>()).await {
        Ok(r) if r.len() == 32 => Ok(Some(r.try_into().unwrap())),
        Ok(_) => Ok(None),
        Err(e) if e.to_string().to_lowercase().contains("revert") => Ok(None),
        Err(e) => Err(e),
    }
}

/// Scans OP timestamps backwards from the latest block in 2 s steps (at most 600 s) for 4788(T) == root.
async fn find_op_timestamp(op: &Rpc, root: &[u8; 32]) -> Result<u64> {
    let latest = op.block("latest").await?;
    let top = q(&latest["timestamp"]).context("op timestamp")? as u64;
    let steps: Vec<u64> = (0..=300).map(|i| top - 2 * i).collect();
    for chunk in steps.chunks(10) {
        let mut set = tokio::task::JoinSet::new();
        for (i, t) in chunk.iter().enumerate() {
            let (op, t) = (op.clone(), *t);
            set.spawn(async move { (i, t, root_at(&op, t).await) });
        }
        let mut hits = vec![];
        while let Some(r) = set.join_next().await {
            let (i, t, res) = r?;
            if res?.as_ref() == Some(root) {
                hits.push((i, t));
            }
        }
        if let Some((_, t)) = hits.into_iter().min_by_key(|(i, _)| *i) {
            return Ok(t);
        }
    }
    bail!("no OP timestamp in the last 600 s has 4788 == {}", b32(*root))
}

// ---------------------------------------------------------------- run

struct Proof {
    public_values: Vec<u8>,
    proof_bytes: Vec<u8>,
}

fn load_proof(a: &RunArgs) -> Result<Option<Proof>> {
    if a.no_proof {
        return Ok(None);
    }
    let f: Value = serde_json::from_slice(&std::fs::read(a.fixture.as_ref().context("need --fixture or --no-proof")?)?)?;
    Ok(Some(Proof {
        public_values: hex_bytes(&f["public_values"])?,
        proof_bytes: hex_bytes(&f["proof_bytes"])?,
    }))
}

struct Opened {
    items: Vec<BlobItem>,
    digest: [u8; 32],
}

/// Step 7: z, (proof, y) per blob, digest. `loc` supplies index and branch (random in dry-run).
fn open_all(blobs: &[Vec<u8>], coms: &[Vec<u8>], pid: &[u8; 31], rb: &[u8; 32], indices: &[u64], branches: &[[[u8; 32]; 17]]) -> Result<Opened> {
    let mut items = vec![];
    let mut pairs = vec![];
    for (i, blob) in blobs.iter().enumerate() {
        let com: [u8; 48] = coms[i].clone().try_into().map_err(|_| anyhow!("commitment length"))?;
        let z = challenge_z(pid, rb, &com);
        let (proof, y) = open(blob, &com, &z)?;
        pairs.push((com.to_vec(), y));
        items.push(BlobItem {
            commitment: Bytes::copy_from_slice(&com),
            y: B256::from(y),
            kzgProof: Bytes::copy_from_slice(&proof),
            index: indices[i],
            branch: branches[i].map(B256::from),
        });
    }
    Ok(Opened { items, digest: blobs_digest(&pairs) })
}

fn rand_arr<const N: usize>() -> [u8; N] {
    let mut a = [0u8; N];
    rand::thread_rng().fill_bytes(&mut a);
    a
}

#[allow(clippy::too_many_arguments)]
fn settle_calldata(pid: [u8; 31], rb: [u8; 32], ra: [u8; 32], hdr: &(u64, u64, [u8; 32], [u8; 32], [u8; 32]), t: u64, items: Vec<BlobItem>, proof: &Option<Proof>) -> Vec<u8> {
    let (pv, pb) = match proof {
        Some(p) => (p.public_values.clone(), p.proof_bytes.clone()),
        None => (vec![], vec![]),
    };
    settleCall {
        pid: FixedBytes::<31>::from(pid),
        rootBefore: B256::from(rb),
        rootAfter: B256::from(ra),
        h: BeaconHeader { slot: hdr.0, proposerIndex: hdr.1, parentRoot: B256::from(hdr.2), stateRoot: B256::from(hdr.3), bodyRoot: B256::from(hdr.4) },
        l2Timestamp: U256::from(t),
        blobs: items,
        publicValues: pv.into(),
        proofBytes: pb.into(),
    }
    .abi_encode()
}

fn l1_cost(rc: &Value) -> Value {
    let g = rc.get("gasUsed").and_then(q).unwrap_or(0);
    let egp = rc.get("effectiveGasPrice").and_then(q).unwrap_or(0);
    let bg = rc.get("blobGasUsed").and_then(q).unwrap_or(0);
    let bgp = rc.get("blobGasPrice").and_then(q).unwrap_or(0);
    json!({
        "block": rc.get("blockNumber").and_then(q),
        "gas_used": g as u64, "effective_gas_price": egp.to_string(),
        "blob_gas_used": bg as u64, "blob_gas_price": bgp.to_string(),
        "exec_fee_wei": (g * egp).to_string(), "blob_fee_wei": (bg * bgp).to_string(),
        "cost_wei": (g * egp + bg * bgp).to_string(),
    })
}

async fn run_once(c: &Ctx, a: &RunArgs, proof: &Option<Proof>) -> Result<Value> {
    let op = c.op()?;
    let signer = c.signer()?;
    let poc: Address = a.poc.parse().context("--poc address")?;
    let stream = build_stream(&a.scenario, a.bytes)?;
    let s = &stream.data;
    let blobs = embed(s)?;
    let (pid, rb, ra) = (rand_arr::<31>(), rand_arr::<32>(), rand_arr::<32>());

    // 1. proof ready (default: the owner posts after the proof)
    let t_start = now_ms();
    // 3. build + send the blob tx
    let tx = build_l1(c, &blobs, a).await?;
    let t_build_done = now_ms();
    let sent = c.l1_send.as_ref().unwrap_or(&c.l1).send_raw(&tx.raw).await.context("sending the L1 blob tx; not retrying")?;
    if !sent.eq_ignore_ascii_case(&tx.hash.to_string()) {
        bail!("node returned hash {sent}, expected {}", tx.hash);
    }
    let t_l1_sent = now_ms();
    eprintln!("  L1 blob tx {} sent ({} bytes, {} blobs)", tx.hash, tx.raw.len(), blobs.len());
    let t_proof_ready = match a.post_delay_secs {
        Some(d) => t_l1_sent + d as u128 * 1000,
        None => t_start,
    };
    // 4. L1 receipt
    let rc = c.l1.wait_receipt(&tx.hash.to_string(), Duration::from_secs(600)).await?;
    if rc.get("status").and_then(q) != Some(1) {
        bail!("L1 tx reverted: {}", tx.hash);
    }
    let n = rc.get("blockNumber").and_then(q).context("block number")? as u64;
    let l1_block = c.l1.block(&format!("0x{n:x}")).await?;
    let l1_ts = q(&l1_block["timestamp"]).context("l1 timestamp")? as u64;
    let t_l1_seen = now_ms();
    let l1m = l1_cost(&rc);
    // 5. beacon proof
    let slot = (l1_ts - BEACON_GENESIS) / SLOT_SECS;
    let vhs: Vec<[u8; 32]> = blobs.iter().map(|b| commitment(b).map(|cm| versioned_hash(&cm))).collect::<Result<_>>()?;
    let loc = locate(c, slot, n, &vhs).await?;
    let t_beacon = now_ms();
    // 6. OP origin catches up, then find T
    let t0 = std::time::Instant::now();
    let mut origin_polls = 0;
    let origin = loop {
        let o = op_origin(op).await?;
        origin_polls += 1;
        if o >= loc.next_block {
            break o;
        }
        if t0.elapsed() > Duration::from_secs(900) {
            bail!("OP L1 origin stuck below {}", loc.next_block);
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    };
    let t_origin = now_ms();
    let l2_t = find_op_timestamp(op, &loc.root).await?;
    // 7. openings
    let opened = open_all(&blobs, &loc.commitments, &pid, &rb, &loc.indices, &loc.branches)?;
    let data = settle_calldata(pid, rb, ra, &loc.hdr, l2_t, opened.items.clone(), proof);
    // simulated "post during proving": cannot settle before the proof exists
    let now = now_ms();
    if t_proof_ready > now {
        tokio::time::sleep(Duration::from_millis((t_proof_ready - now) as u64)).await;
    }
    // 8. settle on OP
    let me = signer.address();
    let est = op.call("eth_estimateGas", json!([{"from": me.to_string(), "to": poc.to_string(), "data": format!("0x{}", hex::encode(&data))}])).await.context("estimateGas of settle (would revert)")?;
    let gas = (q(&est).context("estimate")? * 12 / 10) as u64;
    let op_chain = op.chain_id().await?;
    let nonce = op.nonce(me).await?;
    let op_base = op.base_fee().await?;
    let (raw, hash) = sign_1559(signer, op_chain, nonce, gas, 2 * op_base + a.op_tip_wei, a.op_tip_wei, TxKind::Call(poc), data.clone())?;
    let sent = op.send_raw(&raw).await.context("sending settle; not retrying")?;
    if !sent.eq_ignore_ascii_case(&hash.to_string()) {
        bail!("OP node returned hash {sent}, expected {hash}");
    }
    let t_op_sent = now_ms();
    eprintln!("  OP settle {hash} sent (calldata {} bytes, gas limit {gas})", data.len());
    let orc = op.wait_receipt(&hash.to_string(), Duration::from_secs(300)).await?;
    let t_op_seen = now_ms();
    if orc.get("status").and_then(q) != Some(1) {
        bail!("OP settle reverted: {hash}");
    }
    let ob = op.block(&format!("0x{:x}", orc.get("blockNumber").and_then(q).context("op block")?)).await?;
    let op_ts = q(&ob["timestamp"]).context("op ts")? as u64;
    let mut om = receipt_metrics(&orc);
    om["tx_hash"] = json!(hash.to_string());
    om["calldata_bytes"] = json!(data.len());
    om["gas_limit"] = json!(gas);
    let ev = orc["logs"].as_array().context("logs")?.iter().find(|l| l["topics"][0].as_str().map(|t| t.eq_ignore_ascii_case(&Settled::SIGNATURE_HASH.to_string())).unwrap_or(false)).context("no Settled event")?;
    let ed = hex_bytes(&ev["data"])?;
    if ed.len() != 160 {
        bail!("Settled data length {}", ed.len());
    }
    let digest_ok = ed[96..128] == opened.digest;
    let beacon_ok = ed[64..96] == loc.root;
    // 9. follower rebuild from the beacon API only
    let fetched = fetch_blobs(c, slot, &loc.indices).await?;
    let rebuilt = extract(&fetched, s.len())?;
    let follower_ok = rebuilt == *s;
    if !(digest_ok && beacon_ok && follower_ok) {
        bail!("post-settle checks failed: digest_ok={digest_ok} beacon_ok={beacon_ok} follower_ok={follower_ok}");
    }
    // 10. report
    let total_wei = wei(&l1m, "cost_wei") + wei(&om, "cost_wei");
    let latency = t_op_seen as i128 - t_proof_ready as i128;
    let mut checks = loc.checks.clone();
    checks["op_origin_reached"] = json!(origin >= loc.next_block);
    checks["op_4788_matches_root"] = json!(true);
    checks["settled_digest_ok"] = json!(digest_ok);
    checks["settled_beacon_root_ok"] = json!(beacon_ok);
    checks["follower_rebuild_ok"] = json!(follower_ok);
    checks["op_status_ok"] = json!(true);
    checks["l1_status_ok"] = json!(true);
    Ok(json!({
        "scenario": stream.name, "len_s": s.len(), "n_blobs": blobs.len(),
        "pid": hex::encode(pid), "beacon_slot": slot, "l1_block": n, "op_l2_timestamp_T": l2_t,
        "beacon_block_root": b32(loc.root), "blob_indices": loc.indices, "op_origin_at_ready": origin, "origin_polls": origin_polls,
        "timestamps_ms": {
            "proof_ready": t_proof_ready, "l1_tx_built": t_build_done, "l1_sent": t_l1_sent, "l1_included_block_ts": l1_ts as u128 * 1000,
            "l1_receipt_seen": t_l1_seen, "beacon_proof_ready": t_beacon, "origin_ready": t_origin, "op_sent": t_op_sent,
            "op_included_block_ts": op_ts as u128 * 1000, "op_receipt_seen": t_op_seen,
        },
        "latency_proof_ready_to_settled_ms": latency,
        "latency_proof_ready_to_op_block_ts_ms": op_ts as i128 * 1000 - t_proof_ready as i128,
        "post_delay_secs": a.post_delay_secs,
        "l1": { "tx_hash": tx.hash.to_string(), "raw_tx_bytes": tx.raw.len(), "base_fee": tx.base_fee.to_string(), "tip": tx.tip.to_string(), "max_fee": tx.max_fee.to_string(),
                 "blob_base_fee": tx.blob_base_fee.to_string(), "max_fee_per_blob_gas": tx.blob_cap.to_string(), "metrics": l1m },
        "op": om,
        "total_cost_wei": total_wei.to_string(), "total_cost_eth": eth(total_wei), "total_cost_usd": eth(total_wei) * a.eth_usd,
        "checks": checks,
    }))
}

/// Blobs of `indices` (in order) from the beacon API: sidecars first, then /blobs.
async fn fetch_blobs(c: &Ctx, slot: u64, indices: &[u64]) -> Result<Vec<Vec<u8>>> {
    if let Some(sc) = c.beacon.get(&format!("/eth/v1/beacon/blob_sidecars/{slot}")).await? {
        let list = sc["data"].as_array().context("sidecars")?;
        let mut out = vec![];
        for i in indices {
            let e = list.iter().find(|e| q(&e["index"]) == Some(*i as u128)).context("sidecar index missing")?;
            out.push(hex_bytes(&e["blob"])?);
        }
        return Ok(out);
    }
    let v = c.beacon.get_wait(&format!("/eth/v1/beacon/blobs/{slot}"), Duration::from_secs(30)).await?;
    let list = v["data"].as_array().context("blobs data")?;
    indices.iter().map(|i| hex_bytes(list.get(*i as usize).context("blob index")?)).collect()
}

fn stats(runs: &[Value], f: impl Fn(&Value) -> f64) -> Value {
    let mut v: Vec<f64> = runs.iter().map(f).collect();
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    json!({"min": v[0], "median": v[v.len() / 2], "max": v[v.len() - 1]})
}

async fn cmd_run(c: &Ctx, a: &RunArgs) -> Result<()> {
    let proof = load_proof(a)?;
    let mut runs = vec![];
    for i in 0..a.repeat {
        let r = run_once(c, a, &proof).await.with_context(|| format!("run {}", i + 1))?;
        println!(
            "run {}: {} len={} blobs={} latency={} ms cost={:.6} ETH (${:.4}) L1={} wei OP={} wei",
            i + 1, r["scenario"], r["len_s"], r["n_blobs"], r["latency_proof_ready_to_settled_ms"], r["total_cost_eth"].as_f64().unwrap(),
            r["total_cost_usd"].as_f64().unwrap(), r["l1"]["metrics"]["cost_wei"], r["op"]["cost_wei"]
        );
        runs.push(r);
    }
    let out = json!({
        "scenario": runs[0]["scenario"], "len_s": runs[0]["len_s"], "n_blobs": runs[0]["n_blobs"], "with_proof": proof.is_some(),
        "repeat": a.repeat, "eth_usd": a.eth_usd,
        "latency_ms": stats(&runs, |r| r["latency_proof_ready_to_settled_ms"].as_f64().unwrap()),
        "cost_eth": stats(&runs, |r| r["total_cost_eth"].as_f64().unwrap()),
        "runs": runs,
    });
    if let Some(p) = a.out.parent() {
        std::fs::create_dir_all(p)?;
    }
    std::fs::write(&a.out, serde_json::to_string_pretty(&out)?)?;
    println!("wrote {}", a.out.display());
    Ok(())
}

async fn cmd_dry_run(c: &Ctx, a: &RunArgs) -> Result<()> {
    let proof = load_proof(a)?;
    let stream = build_stream(&a.scenario, a.bytes)?;
    let blobs = embed(&stream.data)?;
    let (pid, rb, ra) = (rand_arr::<31>(), rand_arr::<32>(), rand_arr::<32>());
    let t0 = now_ms();
    let tx = build_l1(c, &blobs, a).await?;
    let t_build = now_ms() - t0;
    let coms: Vec<Vec<u8>> = blobs.iter().map(|b| commitment(b).map(|x| x.to_vec())).collect::<Result<_>>()?;
    let t1 = now_ms();
    // no beacon block exists yet: index and branch are placeholders
    let indices: Vec<u64> = (0..blobs.len() as u64).collect();
    let branches: Vec<[[u8; 32]; 17]> = blobs.iter().map(|_| std::array::from_fn(|_| rand_arr::<32>())).collect();
    let opened = open_all(&blobs, &coms, &pid, &rb, &indices, &branches)?;
    let t_open = now_ms() - t1;
    let hdr = (1u64, 1u64, rand_arr::<32>(), rand_arr::<32>(), rand_arr::<32>());
    let data = settle_calldata(pid, rb, ra, &hdr, 1_700_000_000, opened.items, &proof);
    let l1_exec = 21_000 * (tx.base_fee + tx.tip);
    let l1_blob = tx.blob_gas * tx.blob_base_fee;
    // approximate settle gas: tx base + calldata + 0x0A opening, sha256 and branch hashing per blob + 3 SSTORE-ish writes + log (+ verifier)
    let nz = data.iter().filter(|b| **b != 0).count() as u64;
    let z = data.len() as u64 - nz;
    let gas_est = 21_000 + 16 * nz + 4 * z + blobs.len() as u64 * 70_000 + 90_000 + if proof.is_some() { 310_000 } else { 0 };
    let mut report = json!({
        "scenario": stream.name, "len_s": stream.data.len(), "n_blobs": blobs.len(),
        "l1_tx": { "signed_not_sent": true, "hash": tx.hash.to_string(), "raw_bytes": tx.raw.len(), "nonce": tx.nonce,
                    "base_fee_wei": tx.base_fee.to_string(), "tip_wei": tx.tip.to_string(), "max_fee_wei": tx.max_fee.to_string(),
                    "blob_base_fee_wei": tx.blob_base_fee.to_string(), "max_fee_per_blob_gas_wei": tx.blob_cap.to_string(), "blob_gas": tx.blob_gas.to_string(),
                    "est_exec_fee_wei": l1_exec.to_string(), "est_blob_fee_wei": l1_blob.to_string(), "est_cost_eth": eth(l1_exec + l1_blob),
                    "worst_case_cap_eth": eth(21_000 * tx.max_fee + tx.blob_gas * tx.blob_cap) },
        "timings_ms": { "build_and_sign_l1_sidecar": t_build as u64, "kzg_openings": t_open as u64 },
        "op_settle_est": { "calldata_bytes": data.len(), "gas_estimate_approx": gas_est },
    });
    let mut op_cost = None;
    if let Some(op) = &c.op {
        let base = op.base_fee().await?;
        let l2 = gas_est as u128 * (base + a.op_tip_wei);
        // GasPriceOracle.getL1Fee(bytes): the calldata stands in for the unsigned tx
        let mut sel = hex::decode("49948e0e")?;
        sel.extend((U256::from(32u64), U256::from(data.len())).abi_encode_params());
        sel.extend(&data);
        sel.resize(sel.len().div_ceil(32) * 32, 0);
        let l1fee = U256::from_be_slice(&op.eth_call(GAS_PRICE_ORACLE, &sel).await?).to::<u128>();
        report["op_settle_est"]["op_base_fee_wei"] = json!(base.to_string());
        report["op_settle_est"]["l2_fee_wei"] = json!(l2.to_string());
        report["op_settle_est"]["l1_data_fee_wei"] = json!(l1fee.to_string());
        report["op_settle_est"]["est_cost_eth"] = json!(eth(l2 + l1fee));
        op_cost = Some(l2 + l1fee);
    }
    if let Some(oc) = op_cost {
        let tot = l1_exec + l1_blob + oc;
        report["total_est_eth"] = json!(eth(tot));
        report["total_est_usd"] = json!(eth(tot) * a.eth_usd);
    }
    println!("{}", serde_json::to_string_pretty(&report)?);
    println!(
        "dry-run {}: {} B stream, {} blobs, signed L1 tx {} B (NOT sent), est L1 {:.8} ETH{}",
        stream.name, stream.data.len(), blobs.len(), tx.raw.len(), eth(l1_exec + l1_blob),
        op_cost.map(|o| format!(", est OP {:.8} ETH", eth(o))).unwrap_or_default()
    );
    Ok(())
}

// ---------------------------------------------------------------- other commands

async fn cmd_address(c: &Ctx) -> Result<()> {
    let a = c.signer()?.address();
    let op = c.op()?;
    println!("address:        {a}");
    println!("L1 host:        {}  chain id {}", c.l1.host(), c.l1.chain_id().await?);
    println!("L1 balance:     {:.6} ETH", eth(c.l1.balance(a).await?));
    let head = c.l1.block_number().await?;
    println!("L1 head:        {head}");
    println!("L1 base fee:    {} wei", c.l1.base_fee().await?);
    println!("L1 blob base:   {} wei", c.l1.blob_base_fee().await?);
    println!("OP host:        {}  chain id {}", op.host(), op.chain_id().await?);
    println!("OP balance:     {:.6} ETH", eth(op.balance(a).await?));
    println!("OP base fee:    {} wei", op.base_fee().await?);
    let o = op_origin(op).await?;
    println!("OP L1 origin:   {o}  (lag {} L1 blocks)", head as i64 - o as i64);
    Ok(())
}

async fn cmd_deploy(c: &Ctx, d: &DeployArgs) -> Result<()> {
    let op = c.op()?;
    let signer = c.signer()?;
    let fx: Value = serde_json::from_slice(&std::fs::read(&d.fixture)?)?;
    let vk = h32(&fx["program_vk"])?;
    let rc = h32(&fx["root_c_vadcop_final"])?;
    let mut code = artifact_bytecode(&d.poc_artifact)?;
    code.extend((d.verifier, B256::from(vk), B256::from(rc)).abi_encode_params());
    let from = signer.address();
    let chain_id = op.chain_id().await?;
    let est = op.call("eth_estimateGas", json!([{"from": from.to_string(), "data": format!("0x{}", hex::encode(&code))}])).await?;
    let gas = (q(&est).context("estimate")? * 13 / 10) as u64;
    let nonce = op.nonce(from).await?;
    let base = op.base_fee().await?;
    let prio = 10_000u128;
    let (raw, hash) = sign_1559(signer, chain_id, nonce, gas, 2 * base + prio, prio, TxKind::Create, code)?;
    op.send_raw(&raw).await?;
    let r = op.wait_receipt(&hash.to_string(), Duration::from_secs(600)).await?;
    if r.get("status").and_then(q) != Some(1) {
        bail!("deployment reverted: {hash}");
    }
    let addr: Address = r["contractAddress"].as_str().context("contractAddress")?.parse()?;
    let m = receipt_metrics(&r);
    println!("BlobDAPoC: {addr} gas={} cost={:.8} ETH", m["gas_used"], eth(wei(&m, "cost_wei")));
    let report = json!({"poc": addr.to_string(), "verifier": d.verifier.to_string(), "program_vk": b32(vk), "root_c": b32(rc), "chain_id": chain_id, "deploy": m});
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(format!("deployments-{chain_id}.json"));
    std::fs::write(&path, serde_json::to_string_pretty(&report)?)?;
    println!("wrote {}", path.display());
    Ok(())
}

fn cmd_fixture(sidecar: &PathBuf, out: &PathBuf) -> Result<()> {
    let f: Value = serde_json::from_slice(&std::fs::read(sidecar)?)?;
    let blob = hex_bytes(&f["blob"])?;
    let com: [u8; 48] = hex_bytes(&f["kzg_commitment"])?.try_into().map_err(|_| anyhow!("commitment length"))?;
    if commitment(&blob)? != com {
        bail!("fixture blob does not match its commitment");
    }
    let mut pid = [0u8; 31];
    pid[30] = 1;
    let mut rb = [0u8; 32];
    rb[31] = 2;
    let z = challenge_z(&pid, &rb, &com);
    let (proof, y) = open(&blob, &com, &z)?;
    // the exact calldata the run flow would send (no proof), for an ABI cross-check in forge: T = 1_800_000_000, rootAfter = 3
    let h = &f["header"];
    let hdr = (
        q(&h["slot"]).context("slot")? as u64,
        q(&h["proposer_index"]).context("proposer")? as u64,
        h32(&h["parent_root"])?,
        h32(&h["state_root"])?,
        h32(&h["body_root"])?,
    );
    let br: Vec<[u8; 32]> = f["kzg_commitment_inclusion_proof"].as_array().context("proof")?.iter().map(h32).collect::<Result<_>>()?;
    let br: [[u8; 32]; 17] = br.try_into().map_err(|_| anyhow!("17 nodes"))?;
    let mut ra = [0u8; 32];
    ra[31] = 3;
    let opened = open_all(&[blob.clone()], &[com.to_vec()], &pid, &rb, &[q(&f["index"]).context("index")? as u64], &[br])?;
    let calldata = settle_calldata(pid, rb, ra, &hdr, 1_800_000_000, opened.items, &None);
    let o = json!({
        "settle_calldata": format!("0x{}", hex::encode(calldata)),
        "pid": format!("0x{}", hex::encode(pid)), "root_before": b32(rb), "z": b32(z), "y": b32(y),
        "kzg_proof": format!("0x{}", hex::encode(proof)), "commitment": format!("0x{}", hex::encode(com)),
        "digest": b32(blobs_digest(&[(com.to_vec(), y)])),
    });
    std::fs::write(out, serde_json::to_string_pretty(&o)?)?;
    println!("wrote {} (z {}, y {})", out.display(), b32(z), b32(y));
    Ok(())
}

async fn cmd_verify_l1(c: &Ctx, tx: &str) -> Result<()> {
    let t = c.l1.call("eth_getTransactionByHash", json!([tx])).await?;
    let vhs: Vec<[u8; 32]> = t["blobVersionedHashes"].as_array().context("not a blob tx")?.iter().map(h32).collect::<Result<_>>()?;
    let n = t.get("blockNumber").and_then(q).context("not mined")? as u64;
    let b = c.l1.block(&format!("0x{n:x}")).await?;
    let ts = q(&b["timestamp"]).context("timestamp")? as u64;
    let slot = (ts - BEACON_GENESIS) / SLOT_SECS;
    let loc = locate(c, slot, n, &vhs).await?;
    println!("tx {tx}: L1 block {n}, slot {}, beacon root {}", loc.slot, b32(loc.root));
    println!("  blobs {} at indices {:?}; checks {}", vhs.len(), loc.indices, loc.checks);
    Ok(())
}

async fn cmd_probe_op(c: &Ctx) -> Result<()> {
    let op = c.op()?;
    let latest = op.block("latest").await?;
    let root = h32(&latest["parentBeaconBlockRoot"])?;
    let t = find_op_timestamp(op, &root).await?;
    println!("OP header parentBeaconBlockRoot {} found via 4788 at T={t} (latest block ts {})", b32(root), q(&latest["timestamp"]).unwrap_or(0));
    println!("OP L1 origin {}", op_origin(op).await?);
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let c = Arc::new(Ctx::new(&cli)?);
    match &cli.cmd {
        Cmd::Address => cmd_address(&c).await,
        Cmd::DeployOp(d) => cmd_deploy(&c, d).await,
        Cmd::Fixture { sidecar, out } => cmd_fixture(sidecar, out),
        Cmd::Run(a) => cmd_run(&c, a).await,
        Cmd::DryRun(a) => cmd_dry_run(&c, a).await,
        Cmd::VerifyL1 { tx } => cmd_verify_l1(&c, tx).await,
        Cmd::ProbeOp => cmd_probe_op(&c).await,
    }
}
