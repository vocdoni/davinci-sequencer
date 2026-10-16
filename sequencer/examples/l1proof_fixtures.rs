//! Writes L1 proof fixtures from Ethereum mainnet for the forge tests of L1BlobVerifier and the
//! offline Rust tests. Run: cargo run -p davinci-sequencer --example l1proof_fixtures -- [--rpc URL]
//! [--out-contracts DIR] [--out-rust DIR]

use alloy::eips::eip2718::Encodable2718;
use alloy::primitives::{B256, Bytes, keccak256};
use alloy::providers::{Provider, ProviderBuilder};
use davinci_sequencer::web3::l1proof::{
    L1BlobProof, L1TxProof, build, header_rlp, tx_key, tx_trie_proof, verify_local, verify_tx,
};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::time::Duration;

struct Block {
    number: u64,
    hash: B256,
    header: Bytes,
    root: B256,
    envelopes: Vec<Bytes>,
}

/// Full block with its envelopes, retried with a small backoff.
async fn fetch<P: Provider>(l1: &P, n: u64) -> Block {
    let mut last = String::new();
    for attempt in 0..6u64 {
        match l1.get_block_by_number(n.into()).full().await {
            Ok(Some(b)) => {
                return Block {
                    number: n,
                    hash: b.header.hash,
                    header: header_rlp(&b.header.inner, b.header.hash).unwrap(),
                    root: b.header.transactions_root,
                    envelopes: b
                        .transactions
                        .txns()
                        .map(|t| t.inner.encoded_2718().into())
                        .collect(),
                };
            }
            Ok(None) => last = "block not found".into(),
            Err(e) => last = e.to_string(),
        }
        tokio::time::sleep(Duration::from_millis(500 * (attempt + 1))).await;
    }
    panic!("block {n}: {last}");
}

/// Envelope type: the first byte of a typed envelope, 0 for legacy (RLP list).
fn ty(env: &Bytes) -> u8 {
    if env[0] < 0x80 { env[0] } else { 0 }
}

/// A blob tx in `from..to`, scanning upward: (block, index, hash).
async fn find_blob_tx<P: Provider>(
    l1: &P,
    from: u64,
    to: u64,
    want_index: impl Fn(usize) -> bool,
) -> (u64, usize, B256) {
    for n in from..to {
        let b = fetch(l1, n).await;
        for (i, e) in b.envelopes.iter().enumerate() {
            if ty(e) == 3 && want_index(i) {
                return (n, i, keccak256(e));
            }
        }
    }
    panic!("no blob tx found in {from}..{to}");
}

fn fixture(
    name: &str,
    p: &L1BlobProof,
    numbers: &[u64],
    tx_type: u8,
    vhs: &[B256],
    blocks: &[u64],
) -> Value {
    json!({
        "name": name,
        "headers": p.headers,
        "header_hashes": p.headers.iter().map(keccak256).collect::<Vec<_>>(),
        "header_numbers": numbers,
        "txs": p.txs.iter().map(|t| json!({"header_index": t.header_index, "key": t.key, "proof": t.proof})).collect::<Vec<_>>(),
        "tx_type": tx_type,
        "expected_vhs": vhs,
        "expected_tx_blocks": blocks,
    })
}

struct Out {
    contracts: PathBuf,
}

impl Out {
    fn write(&self, name: &str, v: &Value) {
        std::fs::create_dir_all(&self.contracts).unwrap();
        let path = self.contracts.join(format!("{name}.json"));
        std::fs::write(path, serde_json::to_string_pretty(v).unwrap() + "\n").unwrap();
    }
}

fn summary(name: &str, block: u64, index: usize, p: &L1BlobProof) {
    let nodes: usize = p.txs.iter().map(|t| t.proof.len()).sum();
    println!(
        "{name}: block {block} index {index} headers {} proof_nodes {nodes}",
        p.headers.len()
    );
}

/// Blob fixture: build, verify locally, write.
async fn blob_fixture<P: Provider>(
    l1: &P,
    out: &Out,
    name: &str,
    (n, i, hash): (u64, usize, B256),
    top: u64,
) {
    let p = build(l1, &[hash], top).await.unwrap();
    let (vhs, blocks) = verify_local(&p, keccak256(&p.headers[0])).unwrap();
    let numbers: Vec<u64> = (n..=top).rev().collect();
    out.write(name, &fixture(name, &p, &numbers, 3, &vhs, &blocks));
    summary(name, n, i, &p);
}

/// Hand-built single-tx fixture on `b`.
fn plain_fixture(out: &Out, name: &str, b: &Block, i: usize) {
    let (root, proof) = tx_trie_proof(&b.envelopes, i);
    assert_eq!(root, b.root, "{name}: root mismatch");
    let key = tx_key(i);
    verify_tx(root, &key, &proof, &b.envelopes[i]).unwrap();
    let p = L1BlobProof {
        headers: vec![b.header.clone()],
        txs: vec![L1TxProof {
            header_index: 0,
            key,
            proof,
        }],
    };
    let t = ty(&b.envelopes[i]);
    let (vhs, blocks) = if t == 3 {
        verify_local(&p, b.hash).unwrap()
    } else {
        (vec![], vec![])
    };
    out.write(name, &fixture(name, &p, &[b.number], t, &vhs, &blocks));
    summary(name, b.number, i, &p);
}

fn arg(args: &[String], flag: &str) -> Option<String> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1).cloned())
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    let rpc = arg(&args, "--rpc").unwrap_or_else(|| "https://ethereum-rpc.publicnode.com".into());
    let out = Out {
        contracts: arg(&args, "--out-contracts")
            .unwrap_or_else(|| "../davinci-contracts/test/vectors/l1blob".into())
            .into(),
    };
    let out_rust: PathBuf = arg(&args, "--out-rust")
        .unwrap_or_else(|| "sequencer/tests/fixtures/l1proof".into())
        .into();
    let l1 = ProviderBuilder::new().connect_http(rpc.parse().unwrap());
    let head = l1.get_block_number().await.unwrap();

    // recent
    let (n, i, hash) = find_blob_tx(&l1, head - 600, head - 10, |_| true).await;
    blob_fixture(&l1, &out, "recent", (n, i, hash), n + 3).await;

    // cancun
    let c = find_blob_tx(&l1, 20_000_000, 20_001_000, |_| true).await;
    blob_fixture(&l1, &out, "cancun", c, c.0 + 2).await;

    // index128plus
    let x = find_blob_tx(&l1, head - 20_000, head - 10, |i| i >= 128).await;
    blob_fixture(&l1, &out, "index128plus", x, x.0).await;

    // index0 and not_blob on the recent block
    let rb = fetch(&l1, n).await;
    plain_fixture(&out, "index0", &rb, 0);
    let nb = rb
        .envelopes
        .iter()
        .position(|e| ty(e) != 3)
        .expect("recent block has a non-blob tx");
    plain_fixture(&out, "not_blob", &rb, nb);

    // big_block
    let mut big = None;
    for m in (head - 5000..head).rev() {
        let b = fetch(&l1, m).await;
        if b.envelopes.len() > 257 {
            big = Some(b);
            break;
        }
    }
    plain_fixture(
        &out,
        "big_block",
        &big.expect("no block with more than 256 txs"),
        257,
    );

    // Rust offline fixture
    std::fs::create_dir_all(&out_rust).unwrap();
    let v = json!({
        "number": rb.number,
        "hash": rb.hash,
        "header_rlp": rb.header,
        "transactions_root": rb.root,
        "envelopes": rb.envelopes,
    });
    std::fs::write(
        out_rust.join("recent.json"),
        serde_json::to_string_pretty(&v).unwrap() + "\n",
    )
    .unwrap();
    println!("wrote 6 fixtures and recent.json");
}
