//! Attacks on the L1-blob data path: the L1 proof parsers fed truncated,
//! overlong, non-canonical and hand-crafted RLP and trie nodes (plus a seeded
//! mutation fuzzer over honest proofs), the proof builder and beacon blob
//! sources behind hostile endpoints, and the poster on an Ethereum that
//! lies, lags or fails.

mod common;

use std::panic::{AssertUnwindSafe, catch_unwind};

use alloy::consensus::Header;
use alloy::primitives::{Bytes, keccak256};
use alloy::rlp::{Encodable, Header as RlpHeader};
use common::*;
use davinci_sequencer::web3::L1Chain;
use davinci_sequencer::web3::l1proof::{L1BlobProof, L1ProofError, L1TxProof, verify_local};
use davinci_zkvm_sdk::blob::BLOB_SIZE;

// ------------------------------------------------------------ helpers

/// xorshift64*: a seeded, reproducible stream for the mutation fuzzers.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn below(&mut self, n: usize) -> usize {
        if n == 0 {
            0
        } else {
            (self.next() % n as u64) as usize
        }
    }

    fn bytes(&mut self, n: usize) -> Vec<u8> {
        (0..n).map(|_| self.next() as u8).collect()
    }
}

/// Blobs whose content is never read: only the versioned hashes reach the
/// envelopes (each starts with the 0x01 version byte).
fn fake_blobs(tag: u8, n: usize) -> TransitionBlobs {
    let zero = || -> Blob { vec![0u8; BLOB_SIZE].into_boxed_slice().try_into().unwrap() };
    TransitionBlobs {
        blobs: (0..n).map(|_| zero()).collect(),
        commitments: vec![[0; 48]; n],
        zs: vec![[0; 32]; n],
        ys: vec![[0; 32]; n],
        proofs: vec![[0; 48]; n],
        versioned_hashes: (0..n)
            .map(|i| {
                let mut h = [tag; 32];
                h[0] = 1;
                h[1] = i as u8;
                h
            })
            .collect(),
        digest: [0; 32],
        cells_digest: [0; 32],
    }
}

fn fees() -> L1Fees {
    L1Fees {
        max_fee: 10,
        tip: 1,
        blob_fee: 1,
    }
}

/// An honest proof of three blob transactions over two blocks with several
/// transactions each (branch and extension nodes in the tries), anchored one
/// block below the head; with what it proves.
async fn honest() -> (L1BlobProof, B256, Proven) {
    let l1 = FakeL1::new();
    l1.set_auto_mine(false);
    l1.hold(true);
    let mut mine = Vec::new();
    for (k, byte) in [0x31u8, 0x32, 0x33, 0x34].into_iter().enumerate() {
        let s = l1.with_sender(byte);
        let b = fake_blobs(byte, 1 + k % 3);
        let tx = s.sign_blob_tx(0, &fees(), &b).await.unwrap();
        s.send_blob_tx(&tx).await.unwrap();
        if k != 1 {
            mine.push(tx.hash);
        }
    }
    // Mines the pool with a foreign post: five transactions in one block.
    l1.post_foreign(&fake_blobs(0x40, 2));
    for (k, byte) in [0x35u8, 0x36].into_iter().enumerate() {
        let s = l1.with_sender(byte);
        let tx = s
            .sign_blob_tx(0, &fees(), &fake_blobs(byte, 2 + k))
            .await
            .unwrap();
        s.send_blob_tx(&tx).await.unwrap();
        if k == 1 {
            mine.push(tx.hash);
        }
    }
    l1.post_foreign(&fake_blobs(0x41, 1));
    l1.mine(3);
    l1.set_lag(1);
    let top = l1.head_number();
    let proof = l1.build_proof(&mine, top).await.unwrap();
    let anchor = l1.origin().hash;
    let out = verify_local(&proof, anchor).expect("the honest proof verifies");
    assert_eq!(out.0.len(), 1 + 3 + 1 + 3);
    (proof, anchor, out)
}

/// What `verify_local` proves: versioned hashes and their blocks.
type Proven = (Vec<B256>, Vec<u64>);

/// Runs `verify_local`, turning a panic into `Err(description)`.
fn verify_caught(p: &L1BlobProof, anchor: B256) -> Result<Result<Proven, L1ProofError>, String> {
    catch_unwind(AssertUnwindSafe(|| verify_local(p, anchor))).map_err(|e| {
        e.downcast_ref::<String>()
            .cloned()
            .or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string()))
            .unwrap_or_else(|| "panic".into())
    })
}

/// RLP list of already-encoded items.
fn list(items: &[Vec<u8>]) -> Vec<u8> {
    let payload: Vec<u8> = items.concat();
    let mut out = vec![];
    RlpHeader {
        list: true,
        payload_length: payload.len(),
    }
    .encode(&mut out);
    out.extend(payload);
    out
}

fn string(b: &[u8]) -> Vec<u8> {
    alloy::rlp::encode(b)
}

/// One header whose transactions root is `tx_root`: a one-block chain
/// anchored at itself.
fn header_with_root(tx_root: B256) -> (Bytes, B256) {
    let h = Header {
        number: 77,
        transactions_root: tx_root,
        ..Default::default()
    };
    let mut rlp = vec![];
    h.encode(&mut rlp);
    let hash = keccak256(&rlp);
    (rlp.into(), hash)
}

/// A proof of one transaction under a header committing to `nodes[0]`.
fn crafted(key: Vec<u8>, nodes: Vec<Vec<u8>>) -> (L1BlobProof, B256) {
    let (header, anchor) = header_with_root(keccak256(&nodes[0]));
    (
        L1BlobProof {
            headers: vec![header],
            txs: vec![L1TxProof {
                header_index: 0,
                key: key.into(),
                proof: nodes.into_iter().map(Bytes::from).collect(),
            }],
        },
        anchor,
    )
}

// ------------------------------------------------- L1 proof: crafted input

// A leaf whose path is longer than any trie key (40 bytes): the decoder
// must refuse it, not overflow the nibble buffer.
// BUG: verify_local panics on a proof whose leaf carries an overlong path.
#[test]
fn an_overlong_leaf_path_is_an_error() {
    let leaf = list(&[
        string(&[[0x20u8].as_slice(), &[0xab; 40]].concat()),
        string(b"v"),
    ]);
    let (p, anchor) = crafted(vec![0x80], vec![leaf]);
    let got = verify_caught(&p, anchor);
    assert!(matches!(got, Ok(Err(_))), "{got:?}");
}

// A transaction key longer than 32 bytes (the trie's nibble capacity).
// BUG: verify_local panics on a key over 32 bytes.
#[test]
fn an_overlong_key_is_an_error() {
    let leaf = list(&[string(&[0x20]), string(b"v")]);
    let (p, anchor) = crafted(vec![0x55; 40], vec![leaf]);
    let got = verify_caught(&p, anchor);
    assert!(matches!(got, Ok(Err(_))), "{got:?}");
}

// A branch whose child for the key is an in-place extension over an
// in-place leaf: a shape no transactions trie has (every envelope is over
// 32 bytes), which the trie verifier treats as unreachable.
// BUG: verify_local panics on an in-place extension node with a leaf child.
#[test]
fn an_in_place_extension_over_a_leaf_is_an_error() {
    // tx_key(0) = 0x80: nibbles [8, 0].
    let leaf_inner = list(&[string(&[0x20]), string(b"x")]);
    let ext = list(&[string(&[0x10]), leaf_inner]);
    // Hashed siblings make the branch itself a hashed node (over 32 bytes).
    let mut items: Vec<Vec<u8>> = (0..17)
        .map(|i| match i {
            1 | 2 => string(&[i as u8; 32]),
            _ => vec![0x80],
        })
        .collect();
    items[8] = ext;
    let branch = list(&items);
    let tail = list(&[string(&[0x20]), string(b"x")]);
    let (p, anchor) = crafted(vec![0x80], vec![branch, tail]);
    let got = verify_caught(&p, anchor);
    assert!(matches!(got, Ok(Err(_))), "{got:?}");
}

// Extension paths that walk past 64 nibbles before reaching a leaf.
// BUG: verify_local panics when the walked trie path passes 64 nibbles.
#[test]
fn a_path_longer_than_any_key_is_an_error() {
    // A value long enough that the leaf is referenced by hash.
    let leaf = list(&[string(&[0x20, 0x01]), string(&[0x76; 40])]);
    let ext = list(&[
        string(&[[0x00u8].as_slice(), &[0x80; 32]].concat()),
        string(keccak256(&leaf).as_slice()),
    ]);
    let (p, anchor) = crafted(vec![0x80], vec![ext, leaf]);
    let got = verify_caught(&p, anchor);
    assert!(matches!(got, Ok(Err(_))), "{got:?}");
}

#[test]
fn malformed_headers_are_errors() {
    let good = Header {
        number: 5,
        ..Default::default()
    };
    let mut rlp = vec![];
    good.encode(&mut rlp);
    let mut cases: Vec<Vec<u8>> = vec![
        vec![],
        vec![0xc0],
        vec![0x80],
        rlp[..rlp.len() - 1].to_vec(),
        [rlp.as_slice(), &[0x00]].concat(),
        // Non-canonical: a long-form list length for a short payload.
        [&[0xf9u8, 0x00, 0x05][..], &[0x80; 5]].concat(),
        // A list header claiming far more than the buffer.
        vec![0xfb, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff],
        // Nine items, the hash fields lists, the number 9 bytes long.
        list(&(0..9).map(|_| vec![0xc0]).collect::<Vec<_>>()),
        list(
            &(0..9)
                .map(|i| match i {
                    8 => string(&[1; 9]),
                    _ => string(&[0; 32]),
                })
                .collect::<Vec<_>>(),
        ),
    ];
    // Deep nesting: 10,000 lists inside each other.
    let mut deep = vec![0xc0];
    for _ in 0..10_000 {
        deep = list(&[deep]);
    }
    cases.push(deep);
    for (i, h) in cases.into_iter().enumerate() {
        let anchor = keccak256(&h);
        let p = L1BlobProof {
            headers: vec![h.into()],
            txs: vec![],
        };
        let got = verify_caught(&p, anchor);
        assert!(matches!(got, Ok(Err(_))), "case {i}: {got:?}");
    }
}

// A chain whose headers are honest but out of order, with a gap, or with one
// repeated: never a chain.
#[tokio::test]
async fn header_chains_with_gaps_or_wrong_order_are_refused() {
    let (proof, anchor, _) = honest().await;
    let n = proof.headers.len();
    assert!(n >= 4, "{n}");
    let mut cases: Vec<Vec<Bytes>> = Vec::new();
    let h = &proof.headers;
    // A gap.
    cases.push([&h[..1], &h[2..]].concat());
    // Oldest first.
    cases.push(h.iter().rev().cloned().collect());
    // Two swapped.
    let mut s = h.clone();
    s.swap(1, 2);
    cases.push(s);
    // One repeated.
    cases.push([&h[..2], &h[1..]].concat());
    for (i, headers) in cases.into_iter().enumerate() {
        let p = L1BlobProof {
            headers,
            txs: proof.txs.clone(),
        };
        let got = verify_caught(&p, anchor);
        assert!(matches!(got, Ok(Err(_))), "case {i}: {got:?}");
    }
    // A transaction pointing above the anchor, or past the last header.
    for idx in [0usize, n, usize::MAX] {
        let mut p = proof.clone();
        p.txs[0].header_index = idx;
        let got = verify_caught(&p, anchor);
        assert!(
            matches!(got, Ok(Err(L1ProofError::AboveOrigin(0)))),
            "{idx}: {got:?}"
        );
    }
}

// ------------------------------------------------- L1 proof: fuzzing

// Every mutation of an honest proof is an error or proves exactly what the
// honest one does; none panics.
#[tokio::test]
async fn mutated_proofs_never_panic_nor_prove_more() {
    let (proof, anchor, want) = honest().await;
    let mut panics = Vec::new();
    for seed in [
        0x5eed_1234_abcd_0001u64,
        0xdead_beef_0bad_f00d,
        0x0123_4567_89ab_cdef,
    ] {
        let mut rng = Rng(seed);
        for round in 0..8_000 {
            let mut p = proof.clone();
            // Up to three mutations stacked.
            let what: Vec<String> = (0..1 + rng.below(3))
                .map(|_| mutate(&mut rng, &mut p))
                .collect();
            match verify_caught(&p, anchor) {
                Err(msg) => panics.push(format!("{seed:x}/{round} ({what:?}): {msg}")),
                Ok(Ok(out)) => assert_eq!(
                    out, want,
                    "{seed:x}/{round} ({what:?}) proved something else"
                ),
                Ok(Err(_)) => {}
            }
        }
    }
    assert!(
        panics.is_empty(),
        "{} panics: {:#?}",
        panics.len(),
        &panics[..panics.len().min(5)]
    );
}

fn mutate_bytes(rng: &mut Rng, b: &mut Vec<u8>) -> &'static str {
    match rng.below(7) {
        0 if !b.is_empty() => {
            let i = rng.below(b.len());
            b[i] ^= 1 << rng.below(8);
            "bit flip"
        }
        1 => {
            let n = rng.below(b.len() + 1);
            b.truncate(n);
            "truncate"
        }
        2 => {
            let i = rng.below(b.len() + 1);
            let n = 1 + rng.below(40);
            let extra = rng.bytes(n);
            b.splice(i..i, extra);
            "insert"
        }
        3 if !b.is_empty() => {
            // Lengths are where parsers break: hit the first bytes.
            let i = rng.below(b.len().min(4));
            b[i] = rng.next() as u8;
            "length byte"
        }
        4 => {
            let n = rng.below(80);
            *b = rng.bytes(n);
            "random"
        }
        5 if !b.is_empty() => {
            let i = rng.below(b.len());
            b[i] = [
                0x00, 0x7f, 0x80, 0x81, 0xb7, 0xb8, 0xbf, 0xc0, 0xf7, 0xf8, 0xff,
            ][rng.below(11)];
            "boundary byte"
        }
        _ => {
            b.push(rng.next() as u8);
            "append"
        }
    }
}

fn mutate(rng: &mut Rng, p: &mut L1BlobProof) -> String {
    // Stacked mutations may have emptied a list: nothing to index then.
    if p.headers.is_empty() || p.txs.is_empty() || p.txs.iter().any(|t| t.proof.is_empty()) {
        return "none".into();
    }
    match rng.below(8) {
        0 | 1 => {
            let i = rng.below(p.headers.len());
            let mut h = p.headers[i].to_vec();
            let m = mutate_bytes(rng, &mut h);
            p.headers[i] = h.into();
            format!("header {i} {m}")
        }
        2 => {
            let i = rng.below(p.headers.len());
            match rng.below(3) {
                0 => {
                    p.headers.remove(i);
                    format!("drop header {i}")
                }
                1 => {
                    let h = p.headers[i].clone();
                    p.headers.insert(i, h);
                    format!("repeat header {i}")
                }
                _ => {
                    let j = rng.below(p.headers.len());
                    p.headers.swap(i, j);
                    format!("swap headers {i} {j}")
                }
            }
        }
        3 => {
            let t = rng.below(p.txs.len());
            p.txs[t].header_index = match rng.below(3) {
                0 => rng.below(p.headers.len() + 2),
                1 => usize::MAX - rng.below(3),
                _ => p.txs[t].header_index.wrapping_add(1),
            };
            format!("tx {t} header index")
        }
        4 => {
            let t = rng.below(p.txs.len());
            let mut k = p.txs[t].key.to_vec();
            let m = mutate_bytes(rng, &mut k);
            p.txs[t].key = k.into();
            format!("tx {t} key {m}")
        }
        5 | 6 => {
            let t = rng.below(p.txs.len());
            let nodes = &mut p.txs[t].proof;
            let i = rng.below(nodes.len());
            let mut n = nodes[i].to_vec();
            let m = mutate_bytes(rng, &mut n);
            nodes[i] = n.into();
            format!("tx {t} node {i} {m}")
        }
        _ => {
            let t = rng.below(p.txs.len());
            let nodes = &mut p.txs[t].proof;
            let i = rng.below(nodes.len());
            match rng.below(4) {
                0 => {
                    nodes.remove(i);
                    format!("tx {t} drop node {i}")
                }
                1 => {
                    let n = nodes[i].clone();
                    nodes.insert(i, n);
                    format!("tx {t} repeat node {i}")
                }
                2 => {
                    nodes.reverse();
                    format!("tx {t} reversed nodes")
                }
                _ => {
                    // Another transaction's nodes: valid nodes, wrong path.
                    let o = rng.below(p.txs.len());
                    let other = p.txs[o].proof.clone();
                    p.txs[t].proof = other;
                    format!("tx {t} nodes of tx {o}")
                }
            }
        }
    }
}

// ------------------------------------------- L1 proof builder: hostile RPC

/// A JSON-RPC Ethereum that places every receipt in block `receipt_block`
/// and serves an empty block for any number; counts block reads.
async fn lying_l1(receipt_block: u64) -> (url::Url, Arc<AtomicU64>) {
    use axum::{Json, Router, routing::post};
    use serde_json::{Value, json};
    let reads = Arc::new(AtomicU64::new(0));
    let r = reads.clone();
    let app = Router::new().route(
        "/",
        post(move |Json(req): Json<Value>| {
            let r = r.clone();
            async move {
                let id = req["id"].clone();
                let result = match req["method"].as_str().unwrap_or_default() {
                    "eth_getTransactionReceipt" => json!({
                        "transactionHash": req["params"][0], "transactionIndex": "0x0",
                        "blockHash": B256::repeat_byte(9), "blockNumber": format!("{receipt_block:#x}"),
                        "from": Address::ZERO, "to": Address::ZERO,
                        "cumulativeGasUsed": "0x5208", "gasUsed": "0x5208", "effectiveGasPrice": "0x1",
                        "logs": [], "logsBloom": format!("0x{}", "0".repeat(512)), "status": "0x1",
                        "type": "0x3", "contractAddress": null,
                    }),
                    "eth_getBlockByNumber" => {
                        r.fetch_add(1, Ordering::SeqCst);
                        let hex_n = req["params"][0].as_str().unwrap().trim_start_matches("0x");
                        let n = u64::from_str_radix(hex_n, 16).unwrap();
                        let inner = Header {
                            number: n,
                            transactions_root: alloy::trie::EMPTY_ROOT_HASH,
                            ..Default::default()
                        };
                        json!(alloy::rpc::types::Block::<alloy::rpc::types::Transaction> {
                            header: alloy::rpc::types::Header {
                                hash: inner.hash_slow(),
                                inner,
                                total_difficulty: None,
                                size: None,
                            },
                            uncles: vec![],
                            transactions: Default::default(),
                            withdrawals: None,
                        })
                    }
                    m => panic!("unexpected {m}"),
                };
                Json(json!({"jsonrpc": "2.0", "id": id, "result": result}))
            }
        }),
    );
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    (format!("http://{addr}").parse().unwrap(), reads)
}

// An endpoint that moves a receipt far below the block the settle loop
// anchored on must not make the builder read every block in between: the
// settle loop never asks for more than MAX_L1_PROOF_HEADERS.
// BUG: l1proof::build reads top - oldest + 1 headers, unbounded, from an RPC's receipt.
#[tokio::test]
async fn a_lying_receipt_cannot_make_the_proof_unbounded() {
    use davinci_sequencer::l1da::MAX_L1_PROOF_HEADERS;
    let (url, reads) = lying_l1(0).await;
    let p = davinci_sequencer::web3::rpc_provider(&[url]);
    let top = 5_000;
    let got = davinci_sequencer::web3::l1proof::build(&p, &[B256::repeat_byte(1)], top).await;
    let n = reads.load(Ordering::SeqCst);
    assert!(
        n <= MAX_L1_PROOF_HEADERS + 1,
        "{n} block reads for one proof ({:?})",
        got.map(|p| p.headers.len())
    );
    assert!(got.is_err());
}

// ------------------------------------------------- poster and settle loop

use davinci_sequencer::l1da::{
    GONE_FOR, L1Da, L1Job, PostError, Poster, Posting, Settled, settle, with_post,
};
use davinci_sequencer::web3::{L1Receipt, L1SendError, SignedBlobTx};
use davinci_zkvm_sdk::limits::TX_BLOB_CAP;

const PID: [u8; 31] = [7; 31];
const ROOT: [u8; 32] = [1; 32];

/// An Ethereum endpoint that lies or fails over a [`FakeL1`]: receipt reads
/// erroring for a while, every read failing for a while, a head or a receipt
/// block of its choosing, or flapping between the truth and a view stuck
/// before the post (a failover between disagreeing endpoints).
struct Hostile {
    inner: FakeL1,
    receipts_fail_until: Mutex<Option<tokio::time::Instant>>,
    down_until: Mutex<Option<tokio::time::Instant>>,
    head: Option<u64>,
    receipt_block: Option<u64>,
    flap: bool,
    calls: AtomicU64,
    /// The next proof build proves this transaction instead (backends of a
    /// load-balanced endpoint on two forks for one build).
    swap_once: Mutex<Option<B256>>,
    /// Keep proving that transaction on every build.
    swap_keep: bool,
    /// Receipt reads of these transactions always fail (one bad backend).
    failing: Mutex<Vec<B256>>,
    /// Every other receipt read fails (an intermittent backend).
    alternate: bool,
    receipt_calls: AtomicU64,
}

impl Hostile {
    fn new(l1: &FakeL1) -> Self {
        Hostile {
            inner: l1.clone(),
            receipts_fail_until: Mutex::new(None),
            down_until: Mutex::new(None),
            head: None,
            receipt_block: None,
            flap: false,
            calls: AtomicU64::new(0),
            swap_once: Mutex::new(None),
            swap_keep: false,
            failing: Mutex::new(Vec::new()),
            alternate: false,
            receipt_calls: AtomicU64::new(0),
        }
    }

    fn fail_receipts_for(&self, d: Duration) {
        *self.receipts_fail_until.lock().unwrap() = Some(tokio::time::Instant::now() + d);
    }

    fn down_for(&self, d: Duration) {
        *self.down_until.lock().unwrap() = Some(tokio::time::Instant::now() + d);
    }

    fn before(m: &Mutex<Option<tokio::time::Instant>>) -> bool {
        m.lock()
            .unwrap()
            .is_some_and(|t| tokio::time::Instant::now() < t)
    }

    fn down(&self) -> Result<(), Web3Error> {
        if Self::before(&self.down_until) {
            return Err(Web3Error::Rpc("connection refused".into()));
        }
        Ok(())
    }

    // About half the reads answer from before the post when flapping, in a
    // seeded order (a load balancer over backends at different heads).
    fn stale(&self) -> bool {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        self.flap && Rng(n.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1).next() % 2 == 1
    }
}

#[async_trait]
impl L1Chain for Hostile {
    fn sender(&self) -> Option<Address> {
        self.inner.sender()
    }
    async fn head(&self) -> Result<u64, Web3Error> {
        self.down()?;
        let h = self.inner.head().await?;
        if self.stale() {
            return Ok(h.saturating_sub(5));
        }
        Ok(self.head.unwrap_or(h))
    }
    async fn nonce(&self) -> Result<u64, Web3Error> {
        self.down()?;
        let n = self.inner.nonce().await?;
        if self.stale() {
            return Ok(n.saturating_sub(1));
        }
        Ok(n)
    }
    async fn fees(&self) -> Result<L1Market, Web3Error> {
        self.down()?;
        self.inner.fees().await
    }
    async fn sign_blob_tx(
        &self,
        nonce: u64,
        fees: &L1Fees,
        blobs: &TransitionBlobs,
    ) -> Result<SignedBlobTx, Web3Error> {
        self.inner.sign_blob_tx(nonce, fees, blobs).await
    }
    async fn send_blob_tx(&self, tx: &SignedBlobTx) -> Result<(), L1SendError> {
        if self.down().is_err() {
            return Err(L1SendError::Unanswered("connection refused".into()));
        }
        self.inner.send_blob_tx(tx).await
    }
    async fn receipt(&self, tx: B256) -> Result<Option<L1Receipt>, Web3Error> {
        self.down()?;
        let odd = self.receipt_calls.fetch_add(1, Ordering::SeqCst) % 2 == 1;
        if Self::before(&self.receipts_fail_until)
            || self.failing.lock().unwrap().contains(&tx)
            || (self.alternate && odd)
        {
            return Err(Web3Error::Rpc("-32603 internal error".into()));
        }
        if self.stale() {
            return Ok(None);
        }
        let r = self.inner.receipt(tx).await?;
        Ok(r.map(|r| L1Receipt {
            block: self.receipt_block.unwrap_or(r.block),
            ..r
        }))
    }
    async fn tx_known(&self, tx: B256) -> Result<bool, Web3Error> {
        self.down()?;
        self.inner.tx_known(tx).await
    }
    async fn build_proof(&self, txs: &[B256], top: u64) -> Result<L1BlobProof, Web3Error> {
        self.down()?;
        let swap = match self.swap_keep {
            true => *self.swap_once.lock().unwrap(),
            false => self.swap_once.lock().unwrap().take(),
        };
        if let Some(other) = swap {
            return self.inner.build_proof(&[other], top).await;
        }
        self.inner.build_proof(txs, top).await
    }
}

fn fresh_db() -> (TempDir, Db) {
    let dir = TempDir::new().unwrap();
    let db = Db::open_in(dir.path()).unwrap();
    (dir, db)
}

fn later() -> tokio::time::Instant {
    tokio::time::Instant::now() + Duration::from_secs(600)
}

// Receipt reads that fail (a method an endpoint rate-limits, a node-side
// error) say nothing about the post: the account's nonce moving past it is
// our own transaction being mined, not a stranger's. No second post.
// BUG: the poster re-posts at the next nonce when receipt reads fail for GONE_FOR.
#[tokio::test(start_paused = true)]
async fn failing_receipt_reads_do_not_repost() {
    let l1 = FakeL1::new();
    let (_dir, db) = fresh_db();
    let h = Arc::new(Hostile::new(&l1));
    h.fail_receipts_for(GONE_FOR * 4);
    let p = Poster::new(h.clone(), db.clone(), u128::MAX, Duration::from_secs(1)).unwrap();
    let posted = p
        .post(&PID, &ROOT, &fake_blobs(1, 1), later())
        .await
        .unwrap();
    let s = l1.sends();
    assert!(
        s.iter().all(|s| s.nonce == 0),
        "posted again at another nonce: {s:?}"
    );
    assert_eq!(l1.mined_posts(), 1);
    assert_eq!((posted.nonce, posted.tx), (0, s[0].hash));
}

// Another holder of the key takes the nonce of our pending post while every
// other receipt read fails: the clean reads still say "unknown", so the
// poster declares the nonce taken after GONE_FOR and posts at the next one.
// BUG: any failed receipt read resets the GONE_FOR window, so intermittent failures hide a taken nonce.
#[tokio::test(start_paused = true)]
async fn intermittent_receipt_failures_still_detect_a_taken_nonce() {
    let l1 = FakeL1::new();
    let (_dir, db) = fresh_db();
    let h = Arc::new(Hostile {
        alternate: true,
        ..Hostile::new(&l1)
    });
    l1.hold(true);
    let p = Arc::new(Poster::new(h, db, u128::MAX, Duration::from_secs(1)).unwrap());
    let task = {
        let p = p.clone();
        tokio::spawn(async move { p.post(&PID, &ROOT, &fake_blobs(4, 1), later()).await })
    };
    wait_until("the post is pooled", async || !l1.sends().is_empty()).await;
    let theirs = l1.inject_orphan(0, fees().doubled().doubled().doubled());
    l1.hold(false);
    l1.mine(1);
    assert!(l1.block_of(&theirs).is_some());
    let t = tokio::time::Instant::now();
    let posted = task.await.unwrap().unwrap();
    assert_eq!(posted.nonce, 1);
    assert!(t.elapsed() >= GONE_FOR, "moved on after {:?}", t.elapsed());
    assert!(
        t.elapsed() < Duration::from_secs(300),
        "after {:?}",
        t.elapsed()
    );
    assert_eq!(l1.sends().iter().filter(|s| s.nonce == 1).count(), 1);
}

// Two sends at one nonce (a post and its replacement), the nonce moved past,
// one hash's receipt reads always failing and the other's answering null:
// the failing one may be the mined one, so the poster never concludes from
// that partial answer and posts nothing more.
#[tokio::test(start_paused = true)]
async fn a_hash_whose_receipt_always_fails_never_reposts() {
    let l1 = FakeL1::new();
    let (_dir, db) = fresh_db();
    let h = Arc::new(Hostile::new(&l1));
    l1.hold(true);
    let p = Arc::new(Poster::new(h.clone(), db, u128::MAX, Duration::from_secs(1)).unwrap());
    let task = {
        let p = p.clone();
        tokio::spawn(async move { p.post(&PID, &ROOT, &fake_blobs(4, 1), later()).await })
    };
    wait_until("a replacement", async || l1.sends().len() >= 2).await;
    let s = l1.sends();
    assert!(s.iter().all(|s| s.nonce == 0), "{s:?}");
    h.failing.lock().unwrap().push(s[0].hash);
    let theirs = l1.inject_orphan(0, fees().doubled().doubled().doubled());
    l1.hold(false);
    l1.mine(1);
    assert!(l1.block_of(&theirs).is_some());
    let e = task.await.unwrap().unwrap_err();
    assert!(matches!(e, PostError::Deadline), "{e}");
    assert_eq!(l1.sends().iter().filter(|s| s.nonce > 0).count(), 0);
}

// A head no chain has (an endpoint answering garbage) must not crash the
// poster's replacement clock.
// BUG: the poster overflows `head + REPLACE_AFTER` on an absurd head (a panic in debug).
#[tokio::test]
async fn an_absurd_head_does_not_crash_the_poster() {
    let l1 = FakeL1::new();
    let (_dir, db) = fresh_db();
    let h = Arc::new(Hostile {
        head: Some(u64::MAX),
        ..Hostile::new(&l1)
    });
    let p = Poster::new(h, db, u128::MAX, Duration::from_millis(5)).unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let task = tokio::spawn(async move { p.post(&PID, &ROOT, &fake_blobs(1, 1), deadline).await });
    let got = task.await;
    assert!(got.is_ok(), "the poster panicked: {got:?}");
    assert_eq!(got.unwrap().unwrap().nonce, 0);
}

// Endpoints flapping between the truth and a view from before the post (a
// sticky failover moving between disagreeing providers): one transaction,
// one nonce, the post comes back once it is mined.
#[tokio::test(start_paused = true)]
async fn flapping_endpoints_post_once() {
    let l1 = FakeL1::new();
    let (_dir, db) = fresh_db();
    let h = Arc::new(Hostile {
        flap: true,
        ..Hostile::new(&l1)
    });
    let p = Poster::new(h, db.clone(), u128::MAX, Duration::from_secs(1)).unwrap();
    for k in 0..3u8 {
        let posted = p
            .post(&PID, &ROOT, &fake_blobs(k + 1, 1), later())
            .await
            .unwrap();
        assert_eq!(posted.nonce, u64::from(k));
    }
    let s = l1.sends();
    assert_eq!(l1.mined_posts(), 3, "{s:?}");
    let mut nonces: Vec<u64> = s.iter().map(|s| s.nonce).collect();
    nonces.dedup();
    assert_eq!(nonces, [0, 1, 2], "{s:?}");
    assert!(db.l1_posts().unwrap().is_empty());
}

// Another holder of the key takes the nonce while ours waits in the pool:
// the post moves to the next nonce once that is certain, and its blobs are
// mined once.
#[tokio::test(start_paused = true)]
async fn a_nonce_taken_by_another_holder_of_the_key_moves_on() {
    let l1 = FakeL1::new();
    let (_dir, db) = fresh_db();
    l1.hold(true);
    let p =
        Arc::new(Poster::new(Arc::new(l1.clone()), db, u128::MAX, Duration::from_secs(1)).unwrap());
    let b = fake_blobs(3, 1);
    let task = {
        let p = p.clone();
        tokio::spawn(async move { p.post(&PID, &ROOT, &b, later()).await })
    };
    wait_until("the post is pooled", async || !l1.sends().is_empty()).await;
    // The other holder's tx replaces ours in the pool and is mined.
    let theirs = l1.inject_orphan(0, fees().doubled().doubled());
    l1.hold(false);
    l1.mine(1);
    assert_eq!(l1.block_of(&theirs), Some(l1.head_number()));
    let posted = task.await.unwrap().unwrap();
    assert_eq!(posted.nonce, 1);
    let ours: Vec<_> = l1.sends().into_iter().filter(|s| s.nonce == 1).collect();
    assert_eq!(ours.len(), 1, "one post at the next nonce: {ours:?}");
    assert_eq!(l1.mined_posts(), 2);
}

/// FakeChain in L1 mode over `l1` (wrapped by `wrap`), a fresh poster, and a
/// one-vote batch on the chain's root with the snark FakeProver returns.
struct Rig {
    s: TestSetup,
    l1: FakeL1,
    da: L1Da,
    snark: PlonkSnark,
    blobs: TransitionBlobs,
    _dir: TempDir,
}

fn rig(poll: Duration, wrap: impl FnOnce(&FakeL1) -> Arc<dyn L1Chain>) -> Rig {
    let s = setup(2, 8, None);
    let l1 = FakeL1::new();
    s.chain.attach_l1(&l1);
    let (dir, db) = fresh_db();
    let blobs = {
        use davinci_zkvm_sdk::blob::{TransitionData, build_blobs};
        use davinci_zkvm_sdk::limits::VOTE_ID_MIN;
        let t = TransitionData {
            vote_ids: vec![VOTE_ID_MIN + 9],
            updates: vec![],
            num_fields: 1,
        };
        build_blobs(&t, &Fr::from(9u64), &[9; 32]).unwrap()
    };
    let da = L1Da::new(wrap(&l1), db, u128::MAX, poll, TX_BLOB_CAP).unwrap();
    let p = BatchPublics {
        ok: true,
        fail_mask: 0,
        root_before: s.chain.root(),
        root_after: [0x42; 32],
        voters: 1,
        overwrites: 0,
        census_root: [0; 32],
        blobs_digest: blobs.digest,
        n_blobs: blobs.blobs.len() as u32,
        occupied_before: 0,
        nproofs: 1,
        n_public: 3,
        log_n: 0,
        cells_digest: blobs.cells_digest,
    };
    let snark = PlonkSnark {
        program_vk: release::BATCH_PROGRAM_VK,
        root_c_vadcop_final: release::ROOT_C_VADCOP_FINAL,
        public_values: encode_batch(&p),
        proof_bytes: vec![0; 768],
    };
    Rig {
        s,
        l1,
        da,
        snark,
        blobs,
        _dir: dir,
    }
}

impl Rig {
    fn job(&self, deadline: tokio::time::Instant) -> L1Job<'_> {
        L1Job {
            pid: pid31(),
            root_before: self.s.chain.root(),
            blobs: &self.blobs,
            deadline,
            cancel: CancellationToken::new(),
            posted: Default::default(),
        }
    }
}

// A receipt naming a block no chain has must not crash the settle loop; the
// settlement waits for sane reads until its deadline.
// BUG: settle overflows `newest + REORG_MARGIN` on an absurd receipt block (a panic in debug).
#[tokio::test]
async fn an_absurd_receipt_block_does_not_crash_the_settle_loop() {
    let l1 = FakeL1::new();
    let posted = {
        let (_dir, db) = fresh_db();
        Poster::new(
            Arc::new(l1.clone()),
            db,
            u128::MAX,
            Duration::from_millis(5),
        )
        .unwrap()
        .post(&PID, &ROOT, &fake_blobs(1, 1), later())
        .await
        .unwrap()
    };
    let r = rig(Duration::from_millis(5), |_| {
        Arc::new(Hostile {
            receipt_block: Some(u64::MAX),
            ..Hostile::new(&l1)
        })
    });
    let out = tokio::spawn(async move {
        let job = r.job(tokio::time::Instant::now() + Duration::from_millis(500));
        let posting = Posting {
            posts: vec![posted],
            ..Default::default()
        };
        let out = settle(&r.da, &r.s.chain, &job, &r.snark, posting).await;
        format!("{out:?}")
    })
    .await;
    let out = out.expect("the settle loop panicked");
    assert!(out.contains("deadline"), "{out}");
}

// Every Ethereum endpoint down for two minutes in the middle of a flight,
// then back: one post, one settlement.
#[tokio::test(start_paused = true)]
async fn all_endpoints_down_then_back_settles_once() {
    let mut hostile = None;
    let r = rig(Duration::from_secs(1), |l1| {
        let h = Arc::new(Hostile::new(l1));
        hostile = Some(h.clone());
        h
    });
    let h = hostile.unwrap();
    let job = r.job(later());
    let (_, posting) = with_post(&r.da, &r.s.chain, &job, std::future::ready(())).await;
    assert_eq!(posting.posts.len(), 1);
    h.down_for(Duration::from_secs(120));
    let out = settle(&r.da, &r.s.chain, &job, &r.snark, posting).await;
    assert!(matches!(out, Settled::Landed(_)), "{out:?}");
    assert_eq!(r.l1.sends().len(), 1);
    assert_eq!(r.l1.mined_posts(), 1);
    assert_eq!(r.s.chain.root(), [0x42; 32]);
}

// Down while posting: the post waits (nothing is spent twice) and goes out
// once the endpoints are back.
#[tokio::test(start_paused = true)]
async fn all_endpoints_down_while_posting_post_once() {
    let mut hostile = None;
    let r = rig(Duration::from_secs(1), |l1| {
        let h = Arc::new(Hostile::new(l1));
        hostile = Some(h.clone());
        h
    });
    let h = hostile.unwrap();
    h.down_for(Duration::from_secs(90));
    let job = r.job(later());
    let (_, posting) = with_post(&r.da, &r.s.chain, &job, std::future::ready(())).await;
    let out = settle(&r.da, &r.s.chain, &job, &r.snark, posting).await;
    assert!(matches!(out, Settled::Landed(_)), "{out:?}");
    assert_eq!(r.l1.mined_posts(), 1, "{:?}", r.l1.sends());
}

// A fee spike past the post cap: nothing is sent, the post waits, and it
// goes out at the market price once fees fall.
#[tokio::test(start_paused = true)]
async fn a_fee_spike_withholds_the_post_until_it_passes() {
    let r = rig(Duration::from_secs(1), |l1| Arc::new(l1.clone()));
    let calm = r.l1.market();
    r.l1.set_market(L1Market {
        base_fee: u128::MAX / 4,
        tip: u128::MAX,
        blob_base_fee: u128::MAX,
    });
    // A cap that fits the calm market only.
    let (_dir, db) = fresh_db();
    let cap = L1Fees::offer(&calm).cost(1) * 2;
    let da = L1Da::new(
        Arc::new(r.l1.clone()),
        db,
        cap,
        Duration::from_secs(1),
        TX_BLOB_CAP,
    )
    .unwrap();
    let job = r.job(later());
    let calm_later = async {
        tokio::time::sleep(Duration::from_secs(120)).await;
        assert!(r.l1.sends().is_empty(), "sent during the spike");
        r.l1.set_market(calm);
    };
    let ((_, posting), ()) = tokio::join!(
        with_post(&da, &r.s.chain, &job, std::future::ready(())),
        calm_later
    );
    assert!(posting.error.is_none(), "{:?}", posting.error);
    let s = r.l1.sends();
    assert_eq!(s.len(), 1, "{s:?}");
    assert_eq!(s[0].fees, L1Fees::offer(&calm));
}

// The deadline passes during a spike: the post reports it and spent nothing.
#[tokio::test(start_paused = true)]
async fn a_fee_spike_to_the_deadline_spends_nothing() {
    let l1 = FakeL1::new();
    let (_dir, db) = fresh_db();
    l1.set_market(L1Market {
        base_fee: 10_u128.pow(15),
        tip: 0,
        blob_base_fee: 1,
    });
    let p = Poster::new(
        Arc::new(l1.clone()),
        db,
        10_u128.pow(15),
        Duration::from_secs(1),
    )
    .unwrap();
    let e = p
        .post(&PID, &ROOT, &fake_blobs(1, 1), later())
        .await
        .unwrap_err();
    assert!(matches!(e, PostError::FeeCap { .. }), "{e}");
    assert!(l1.sends().is_empty());
}

// ------------------------------------------------------ beacon blob sources

use axum::http::StatusCode;
use davinci_sequencer::web3::BeaconBlobs;
use davinci_zkvm_sdk::blob::{TransitionData, build_blobs, decode_blobs, versioned_hash};

type Reply = Arc<dyn Fn() -> (StatusCode, String) + Send + Sync>;

fn reply(status: StatusCode, body: impl Into<String>) -> Reply {
    let body = body.into();
    Arc::new(move || (status, body.clone()))
}

fn hexs(b: &[u8]) -> String {
    format!("0x{}", hex::encode(b))
}

/// `{"data": [blob hex, ...]}` for the blobs endpoint.
fn blobs_body(blobs: &[&Blob]) -> String {
    let data: Vec<String> = blobs.iter().map(|b| hexs(&b[..])).collect();
    serde_json::json!({ "data": data }).to_string()
}

/// `{"data": [sidecar, ...]}` claiming `claims[i]` as each blob's commitment.
fn sidecars_body(blobs: &[&Blob], claims: &[Vec<u8>]) -> String {
    let data: Vec<_> = blobs
        .iter()
        .zip(claims)
        .enumerate()
        .map(|(i, (b, c))| {
            serde_json::json!({"index": i.to_string(), "blob": hexs(&b[..]),
                "kzg_commitment": hexs(c), "kzg_proof": hexs(&[0u8; 48])})
        })
        .collect();
    serde_json::json!({ "data": data }).to_string()
}

/// A beacon: genesis at 1000, 12 s slots, `blobs` and `sidecars` answering
/// every slot; counts the blob requests.
async fn beacon(blobs: Reply, sidecars: Reply) -> (url::Url, Arc<AtomicU64>) {
    use axum::{Router, routing::get};
    let hits = Arc::new(AtomicU64::new(0));
    let (h1, h2) = (hits.clone(), hits.clone());
    let app = Router::new()
        .route(
            "/eth/v1/beacon/genesis",
            get(|| async { r#"{"data":{"genesis_time":"1000"}}"# }),
        )
        .route(
            "/eth/v1/config/spec",
            get(|| async { r#"{"data":{"SECONDS_PER_SLOT":"12"}}"# }),
        )
        .route(
            "/eth/v1/beacon/blobs/:slot",
            get(move || {
                h1.fetch_add(1, Ordering::SeqCst);
                let r = blobs();
                async move { r }
            }),
        )
        .route(
            "/eth/v1/beacon/blob_sidecars/:slot",
            get(move || {
                h2.fetch_add(1, Ordering::SeqCst);
                let r = sidecars();
                async move { r }
            }),
        );
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    (format!("http://{addr}/").parse().unwrap(), hits)
}

fn kzg_commitment(b: &Blob) -> [u8; 48] {
    let s = alloy::eips::eip4844::env_settings::EnvKzgSettings::Default.get();
    let kb = alloy::eips::eip4844::c_kzg::Blob::from_bytes(&b[..]).unwrap();
    *s.blob_to_kzg_commitment(&kb).unwrap().to_bytes()
}

/// Two blobs of one transition, with their hashes, and an unrelated blob.
fn two_blobs() -> (Vec<Blob>, Vec<B256>, Blob) {
    use davinci_zkvm_sdk::ballot::Ballot;
    use davinci_zkvm_sdk::limits::VOTE_ID_MIN;
    let t = TransitionData {
        vote_ids: (0..500).map(|i| VOTE_ID_MIN + i).collect(),
        updates: (0..500).map(|i| (0x10 + i, Ballot::identity())).collect(),
        num_fields: 4,
    };
    let b = build_blobs(&t, &Fr::from(7u64), &[1u8; 32]).unwrap();
    assert_eq!(b.blobs.len(), 2);
    let other = TransitionData {
        vote_ids: vec![VOTE_ID_MIN + 1],
        updates: vec![],
        num_fields: 1,
    };
    let o = build_blobs(&other, &Fr::from(7u64), &[2u8; 32]).unwrap();
    let hashes = b.versioned_hashes.iter().map(|h| B256::from(*h)).collect();
    (b.blobs, hashes, o.blobs[0].clone())
}

const AT: u64 = 1000 + 12 * 5;

// The first beacon answers 200 without serving the blobs (pruned, not a
// supernode, or lying); a second one listed has them. The node must reach
// it, as it does after a 404.
// BUG: a 200 answer that lacks the blobs stops the beacon failover: the blobs never sync.
#[tokio::test]
async fn a_beacon_answering_without_the_blobs_is_not_the_last_word() {
    let (blobs, hashes, other) = two_blobs();
    let mut tampered = blobs[1].clone();
    tampered[200] ^= 1;
    let honest_claims = vec![
        kzg_commitment(&blobs[0]).to_vec(),
        kzg_commitment(&blobs[1]).to_vec(),
    ];
    let none = reply(StatusCode::NOT_FOUND, "");
    let cases: Vec<(&str, Reply, Reply)> = vec![
        (
            "empty",
            reply(StatusCode::OK, blobs_body(&[])),
            none.clone(),
        ),
        (
            "partial",
            reply(StatusCode::OK, blobs_body(&[&blobs[0]])),
            none.clone(),
        ),
        (
            "tampered",
            reply(StatusCode::OK, blobs_body(&[&blobs[0], &tampered])),
            none.clone(),
        ),
        (
            "unrelated",
            reply(StatusCode::OK, blobs_body(&[&other])),
            none.clone(),
        ),
        ("not json", reply(StatusCode::OK, "<html>"), none.clone()),
        (
            "sidecars lie",
            none.clone(),
            reply(
                StatusCode::OK,
                sidecars_body(&[&blobs[0], &tampered], &honest_claims),
            ),
        ),
        (
            "sidecar with a short commitment",
            none.clone(),
            reply(StatusCode::OK, sidecars_body(&[&blobs[0]], &[vec![1; 47]])),
        ),
        (
            "pruned, empty sidecars",
            none.clone(),
            reply(StatusCode::OK, r#"{"data":[]}"#),
        ),
    ];
    for (what, b, s) in cases {
        let (bad, _) = beacon(b, s).await;
        let (good, _) = beacon(
            reply(StatusCode::OK, blobs_body(&[&blobs[0], &blobs[1]])),
            none.clone(),
        )
        .await;
        let src = BeaconBlobs::beacon_only(vec![bad, good]);
        let got = src.blobs_at(AT, &hashes).await;
        assert!(
            got.as_ref().is_ok_and(|g| *g == blobs),
            "{what}: {:?}",
            got.err()
        );
    }
}

// Two beacons that pruned the slot ahead of the archive in the list: the
// 404 peek asks only the next one, so the third must still be reached.
// BUG: with two pruned beacons first, an archive listed third is never asked.
#[tokio::test]
async fn an_archive_after_two_pruned_beacons_is_reached() {
    let (blobs, hashes, _) = two_blobs();
    let none = reply(StatusCode::NOT_FOUND, "");
    let (a, _) = beacon(none.clone(), none.clone()).await;
    let (b, _) = beacon(none.clone(), none.clone()).await;
    let (archive, hits) = beacon(
        reply(StatusCode::OK, blobs_body(&[&blobs[1], &blobs[0]])),
        none.clone(),
    )
    .await;
    let src = BeaconBlobs::beacon_only(vec![a, b, archive]);
    let got = src.blobs_at(AT, &hashes).await;
    assert!(got.as_ref().is_ok_and(|g| *g == blobs), "{:?}", got.err());
    assert_eq!(hits.load(Ordering::SeqCst), 1);
}

// One beacon, every malformed answer: an error, never a panic, never the
// wrong blobs.
#[tokio::test]
async fn malformed_beacon_answers_are_errors() {
    let (blobs, hashes, other) = two_blobs();
    let none = reply(StatusCode::NOT_FOUND, "");
    let short = hexs(&blobs[0][..BLOB_SIZE - 1]);
    let long = format!("{}00", hexs(&blobs[0][..]));
    // Field elements at the BLS modulus: c-kzg refuses to commit.
    let mut noncanon = blobs[0].clone();
    noncanon[..32].copy_from_slice(&[0xff; 32]);
    let cases: Vec<(&str, Reply, Reply)> = vec![
        ("404 both", none.clone(), none.clone()),
        (
            "500 both",
            reply(StatusCode::INTERNAL_SERVER_ERROR, ""),
            reply(StatusCode::INTERNAL_SERVER_ERROR, ""),
        ),
        (
            "one blob twice",
            reply(StatusCode::OK, blobs_body(&[&blobs[0], &blobs[0]])),
            none.clone(),
        ),
        (
            "short blob",
            reply(StatusCode::OK, format!(r#"{{"data":["{short}"]}}"#)),
            none.clone(),
        ),
        (
            "long blob",
            reply(StatusCode::OK, format!(r#"{{"data":["{long}"]}}"#)),
            none.clone(),
        ),
        (
            "odd hex",
            reply(StatusCode::OK, r#"{"data":["0xabc"]}"#),
            none.clone(),
        ),
        (
            "empty hex",
            reply(StatusCode::OK, r#"{"data":["0x"]}"#),
            none.clone(),
        ),
        (
            "data not a list",
            reply(StatusCode::OK, r#"{"data":{"0":"0x00"}}"#),
            none.clone(),
        ),
        (
            "data missing",
            reply(StatusCode::OK, r#"{"blobs":[]}"#),
            none.clone(),
        ),
        (
            "non-canonical field element",
            reply(StatusCode::OK, blobs_body(&[&noncanon, &blobs[1]])),
            none.clone(),
        ),
        (
            "sidecar claims a blob it does not carry",
            none.clone(),
            reply(
                StatusCode::OK,
                sidecars_body(
                    &[&other, &blobs[1]],
                    &[
                        kzg_commitment(&blobs[0]).to_vec(),
                        kzg_commitment(&blobs[1]).to_vec(),
                    ],
                ),
            ),
        ),
        (
            "sidecar commitment not hex",
            none.clone(),
            reply(
                StatusCode::OK,
                r#"{"data":[{"index":"0","blob":"0x00","kzg_commitment":"zz","kzg_proof":"0x"}]}"#,
            ),
        ),
    ];
    for (what, b, s) in cases {
        let (url, _) = beacon(b, s).await;
        let src = BeaconBlobs::beacon_only(vec![url]);
        let got = src.blobs_at(AT, &hashes).await;
        assert!(got.is_err(), "{what}: accepted");
    }
    // Shuffled, duplicated and padded with an unrelated blob: the right
    // blobs, in hash order.
    let (url, _) = beacon(
        reply(
            StatusCode::OK,
            blobs_body(&[&other, &blobs[1], &blobs[1], &blobs[0], &other]),
        ),
        none,
    )
    .await;
    let src = BeaconBlobs::beacon_only(vec![url]);
    assert!(src.blobs_at(AT, &hashes).await.unwrap() == blobs);
}

// A body past the 64 MiB cap is refused while it streams in.
#[tokio::test]
async fn an_oversized_beacon_body_is_refused() {
    let (_, hashes, _) = two_blobs();
    // Valid JSON once complete: leading whitespace, then an empty list.
    let huge = format!("{}{}", " ".repeat(65 << 20), r#"{"data":[]}"#);
    let (url, _) = beacon(
        reply(StatusCode::OK, huge),
        reply(StatusCode::NOT_FOUND, ""),
    )
    .await;
    let src = BeaconBlobs::beacon_only(vec![url]);
    let e = src.blobs_at(AT, &hashes).await.unwrap_err();
    assert!(e.to_string().contains("too large"), "{e}");
}

/// A blob of valid field elements (each cell below 2^248) that matches its
/// own versioned hash but is no transition's layout.
fn junk_blob(rng: &mut Rng) -> (Blob, B256) {
    let mut b: Blob = vec![0u8; BLOB_SIZE].into_boxed_slice().try_into().unwrap();
    for cell in b.chunks_mut(32) {
        cell[1..].copy_from_slice(&rng.bytes(31));
    }
    let vh = B256::from(versioned_hash(&kzg_commitment(&b)));
    (b, vh)
}

// A blob that matches its hash yet does not decode is fetched (it is what
// the chain names) and then refused by the decoder, never a panic.
#[tokio::test]
async fn a_hash_correct_undecodable_blob_is_a_decode_error() {
    let mut rng = Rng(77);
    let (junk, vh) = junk_blob(&mut rng);
    let (url, _) = beacon(
        reply(StatusCode::OK, blobs_body(&[&junk])),
        reply(StatusCode::NOT_FOUND, ""),
    )
    .await;
    let src = BeaconBlobs::beacon_only(vec![url]);
    let got = src.blobs_at(AT, &[vh]).await.unwrap();
    for nf in [1, 4, 16] {
        assert!(decode_blobs(&got, nf).is_err());
    }
}

// The decoder over mutated honest blobs: every outcome an error or a
// transition, never a panic, and none for a blob the header does not size.
#[test]
fn mutated_blobs_never_panic_the_decoder() {
    let (blobs, _, _) = two_blobs();
    let mut rng = Rng(0xb10b);
    let mut panics = Vec::new();
    for round in 0..400 {
        let mut b = blobs.clone();
        let k = rng.below(b.len());
        let what = match rng.below(5) {
            0 => {
                // The header: counts and version.
                let i = rng.below(32);
                b[k][i] = rng.next() as u8;
                "header byte"
            }
            1 => {
                let i = rng.below(BLOB_SIZE);
                b[k][i] ^= 1 << rng.below(8);
                "bit flip"
            }
            2 => {
                let cell = rng.below(4096) * 32;
                let v = rng.bytes(32);
                b[k][cell..cell + 32].copy_from_slice(&v);
                "random cell"
            }
            3 => {
                b.truncate(1);
                "first blob only"
            }
            _ => {
                b.push(b[0].clone());
                "blob repeated"
            }
        };
        for nf in [0u8, 1, 4, 16, 17] {
            let blobs = b.clone();
            match catch_unwind(AssertUnwindSafe(move || decode_blobs(&blobs, nf))) {
                Err(_) => panics.push(format!("round {round} ({what}) nf {nf}")),
                Ok(Ok(t)) => assert!(nf == 4 || nf == 1, "{round} {what}: {t:?}"),
                Ok(Err(_)) => {}
            }
        }
    }
    assert!(panics.is_empty(), "{panics:?}");
}

// --------------------------------------- OP sends: fee cap, bumps, reverts

use davinci_sequencer::config::SecretString;
use davinci_sequencer::web3::Contracts;

/// anvil's first key: a well-known test key.
const ANVIL_KEY: &str = "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";

/// One `eth_sendRawTransaction` the OP stub saw.
#[derive(Clone, Debug)]
struct OpSend {
    nonce: u64,
    max_fee: u128,
    tip: u128,
    gas: u64,
    blob_fee: u128,
    hash: B256,
    accepted: bool,
}

/// A scripted OP chain behind JSON-RPC: one account, a fee market, sends
/// recorded and optionally mined at once (with a status), receipts for what
/// it mined.
#[derive(Default)]
struct Op {
    /// The account's mined count.
    count: u64,
    /// Subtracted from the count reads (a lagging backend).
    count_lag: u64,
    base_fee: u128,
    tip: u128,
    gas: u64,
    sends: Vec<OpSend>,
    /// Refusals for the next sends, in order (`None` accepts).
    refuse: std::collections::VecDeque<Option<String>>,
    /// Mine every accepted send at once with this status.
    mine: Option<bool>,
    mined: HashMap<B256, bool>,
    /// What `eth_call` reverts with (the replay of a mined revert).
    revert: Option<Vec<u8>>,
    /// Geth's blob pool rule: a blob replacement must double the fee cap,
    /// the tip and the blob fee cap of the pooled one.
    blob_pool: bool,
}

async fn op_chain(op: Arc<Mutex<Op>>) -> url::Url {
    use alloy::consensus::{Transaction as _, TxEnvelope};
    use alloy::eips::eip2718::Decodable2718;
    use axum::{Json, Router, routing::post};
    use serde_json::{Value, json};
    let app = Router::new().route(
        "/",
        post(move |Json(req): Json<Value>| {
            let op = op.clone();
            async move {
                let id = req["id"].clone();
                let mut o = op.lock().unwrap();
                let err = |m: String| json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32000, "message": m}});
                let result = match req["method"].as_str().unwrap_or_default() {
                    "eth_chainId" => json!("0xa"),
                    "eth_getCode" => json!("0x6000"),
                    "eth_blockNumber" => json!("0x10"),
                    "eth_getTransactionCount" => json!(format!("{:#x}", o.count.saturating_sub(o.count_lag))),
                    "eth_feeHistory" => json!({
                        "oldestBlock": "0x6",
                        "baseFeePerGas": vec![format!("{:#x}", o.base_fee); 11],
                        "gasUsedRatio": vec![0.5; 10],
                        "reward": vec![vec![format!("{:#x}", o.tip)]; 10],
                    }),
                    "eth_estimateGas" => json!(format!("{:#x}", o.gas)),
                    "eth_blobBaseFee" => json!("0x3"),
                    "eth_getTransactionByHash" => Value::Null,
                    "eth_call" => match &o.revert {
                        Some(d) => {
                            let d = hexs(d);
                            return Json(json!({"jsonrpc": "2.0", "id": id, "error":
                                {"code": 3, "message": "execution reverted", "data": d}}));
                        }
                        None => json!("0x"),
                    },
                    "eth_sendRawTransaction" => {
                        let raw: Bytes = serde_json::from_value(req["params"][0].clone()).unwrap();
                        let env = TxEnvelope::decode_2718(&mut raw.as_ref()).unwrap();
                        let mut s = OpSend {
                            nonce: env.nonce(),
                            max_fee: env.max_fee_per_gas(),
                            tip: env.max_priority_fee_per_gas().unwrap_or_default(),
                            gas: env.gas_limit(),
                            blob_fee: env.max_fee_per_blob_gas().unwrap_or_default(),
                            hash: *env.tx_hash(),
                            accepted: false,
                        };
                        let pooled = o
                            .sends
                            .iter()
                            .rev()
                            .find(|p| p.accepted && p.nonce == s.nonce && !o.mined.contains_key(&p.hash));
                        let short = pooled.is_some_and(|p| {
                            s.blob_fee > 0
                                && (s.max_fee < 2 * p.max_fee
                                    || s.tip < 2 * p.tip
                                    || s.blob_fee < 2 * p.blob_fee)
                        });
                        let mut refusal = o.refuse.pop_front().flatten();
                        if o.blob_pool && short {
                            refusal = Some("replacement transaction underpriced".into());
                        }
                        s.accepted = refusal.is_none();
                        o.sends.push(s.clone());
                        if let Some(m) = refusal {
                            return Json(err(m));
                        }
                        if let Some(status) = o.mine {
                            o.mined.insert(s.hash, status);
                            o.count = o.count.max(s.nonce + 1);
                        }
                        json!(s.hash)
                    }
                    "eth_getTransactionReceipt" => {
                        let h: B256 = serde_json::from_value(req["params"][0].clone()).unwrap();
                        match o.mined.get(&h) {
                            None => Value::Null,
                            Some(ok) => json!({
                                "transactionHash": h, "transactionIndex": "0x0",
                                "blockHash": B256::repeat_byte(9), "blockNumber": "0x10",
                                "from": Address::ZERO, "to": Address::repeat_byte(1),
                                "cumulativeGasUsed": "0x5208", "gasUsed": "0x5208",
                                "effectiveGasPrice": "0x1", "logs": [],
                                "logsBloom": format!("0x{}", "0".repeat(512)),
                                "status": if *ok { "0x1" } else { "0x0" },
                                "type": "0x2", "contractAddress": null,
                            }),
                        }
                    }
                    m => panic!("unexpected {m}"),
                };
                Json(json!({"jsonrpc": "2.0", "id": id, "result": result}))
            }
        }),
    );
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    format!("http://{addr}").parse().unwrap()
}

/// The node's registry handle on the stub, capped at `cap` wei a send.
async fn op_node(op: &Arc<Mutex<Op>>, cap: u128) -> Contracts {
    let url = op_chain(op.clone()).await;
    let key = SecretString::new(ANVIL_KEY);
    let mut c = Contracts::open(&[url], Address::repeat_byte(1), Some(&key), false)
        .await
        .unwrap();
    c.set_max_tx_fee(cap);
    c.set_receipt_timeout(Duration::from_millis(300));
    c
}

fn calm_op() -> Arc<Mutex<Op>> {
    Arc::new(Mutex::new(Op {
        count: 3,
        base_fee: 1_000_000,
        tip: 1_000,
        gas: 1_000_000,
        ..Default::default()
    }))
}

fn results_snark() -> PlonkSnark {
    PlonkSnark {
        program_vk: [0; 32],
        root_c_vadcop_final: [0; 32],
        public_values: vec![1; 64],
        proof_bytes: vec![2; 768],
    }
}

// A fee spike past `--op-max-tx-fee` before the first send: withheld before
// signing, nothing reaches the chain.
#[tokio::test]
async fn an_op_fee_spike_withholds_the_send() {
    let op = calm_op();
    op.lock().unwrap().base_fee = 10_u128.pow(13);
    let c = op_node(&op, 10_u128.pow(15)).await;
    let snark = results_snark();
    let e = c.submit_results(&PID, &snark).await.unwrap_err();
    assert!(matches!(e, Web3Error::FeeCap { .. }), "{e}");
    assert!(op.lock().unwrap().sends.is_empty());
}

// Fees spike while a send of ours waits unmined: its replacement would pass
// the cap, so none goes out (the stuck one waits at its price), and every
// send stays within the cap. Once fees calm the next send replaces it at the
// same nonce with both execution fees bumped at least 10 %.
#[tokio::test]
async fn a_spike_while_stuck_waits_at_the_price_and_the_next_send_outbids() {
    let op = calm_op();
    let cap = 10_u128.pow(13);
    let c = op_node(&op, cap).await;
    let snark = results_snark();
    let spike = async {
        wait_until("first send", async || !op.lock().unwrap().sends.is_empty()).await;
        op.lock().unwrap().base_fee = 10_u128.pow(12);
    };
    let (got, ()) = tokio::join!(c.submit_results(&PID, &snark), spike);
    let e = got.unwrap_err();
    assert!(matches!(e, Web3Error::Stuck { nonce: 3, .. }), "{e}");
    let sends = op.lock().unwrap().sends.clone();
    assert_eq!(
        sends.len(),
        1,
        "a replacement over the cap went out: {sends:?}"
    );
    for s in &sends {
        assert!(u128::from(s.gas) * s.max_fee <= cap, "{s:?}");
    }
    // Calm again, and mined at once: the stuck send is outbid at its nonce.
    {
        let mut o = op.lock().unwrap();
        o.base_fee = 1_000_000;
        o.mine = Some(true);
    }
    let r = c.submit_results(&PID, &snark).await.unwrap();
    let sends = op.lock().unwrap().sends.clone();
    let (a, b) = (&sends[0], &sends[1]);
    assert_eq!((a.nonce, b.nonce), (3, 3), "{sends:?}");
    assert!(
        b.max_fee * 10 >= a.max_fee * 11 && b.tip * 10 >= a.tip * 11,
        "{sends:?}"
    );
    assert_eq!(r.tx_hash, b.hash);
}

// A send that is never mined is replaced at its nonce, each time with both
// execution fees up at least 10 % (what nodes require), until one lands.
#[tokio::test]
async fn unmined_sends_are_replaced_with_every_fee_bumped() {
    let op = calm_op();
    let c = op_node(&op, u128::MAX).await;
    let snark = results_snark();
    let third = async {
        wait_until("two sends", async || op.lock().unwrap().sends.len() >= 2).await;
        op.lock().unwrap().mine = Some(true);
    };
    let (got, ()) = tokio::join!(c.submit_results(&PID, &snark), third);
    let r = got.unwrap();
    let sends = op.lock().unwrap().sends.clone();
    assert!(sends.len() >= 3, "{sends:?}");
    for w in sends.windows(2) {
        assert_eq!(w[0].nonce, w[1].nonce);
        assert!(w[1].max_fee * 10 >= w[0].max_fee * 11, "{sends:?}");
        assert!(w[1].tip * 10 >= w[0].tip * 11, "{sends:?}");
    }
    assert_eq!(r.tx_hash, sends.last().unwrap().hash);
    assert_eq!(r.replacements as usize, sends.len() - 1);
}

// A receipt with status 0: the revert is named by replaying the call, and
// nothing is sent again.
#[tokio::test]
async fn a_reverted_receipt_is_named_and_not_resent() {
    use alloy::sol_types::SolError;
    alloy::sol! { error InvalidStateRoot(); }
    let op = calm_op();
    {
        let mut o = op.lock().unwrap();
        o.mine = Some(false);
        o.revert = Some(InvalidStateRoot::SELECTOR.to_vec());
    }
    let c = op_node(&op, u128::MAX).await;
    let snark = results_snark();
    let e = c.submit_results(&PID, &snark).await.unwrap_err();
    assert!(
        matches!(&e, Web3Error::Revert(r) if r.name() == Some("InvalidStateRoot")),
        "{e}"
    );
    assert_eq!(op.lock().unwrap().sends.len(), 1);
    // A replay that passes names nothing: a lost race, still not resent.
    op.lock().unwrap().revert = None;
    let e = c.submit_results(&PID, &snark).await.unwrap_err();
    assert!(matches!(e, Web3Error::Lost { .. }), "{e}");
    assert_eq!(op.lock().unwrap().sends.len(), 2);
}

// The count read lags a nonce of ours already mined: the refusal moves the
// send to the next nonce, which lands; no nonce is skipped.
#[tokio::test]
async fn a_lagging_count_moves_to_the_next_nonce_without_a_gap() {
    let op = calm_op();
    {
        let mut o = op.lock().unwrap();
        o.count_lag = 1;
        o.mine = Some(true);
        o.refuse
            .push_back(Some("nonce too low: next nonce 3, tx nonce 2".into()));
    }
    let c = op_node(&op, u128::MAX).await;
    let snark = results_snark();
    c.submit_results(&PID, &snark).await.unwrap();
    let sends = op.lock().unwrap().sends.clone();
    let nonces: Vec<(u64, bool)> = sends.iter().map(|s| (s.nonce, s.accepted)).collect();
    assert_eq!(nonces, [(2, false), (3, true)], "{sends:?}");
    // The floor holds: the next send goes to 4 although the count still lags.
    c.submit_results(&PID, &snark).await.unwrap();
    assert_eq!(op.lock().unwrap().sends.last().unwrap().nonce, 4);
}

// Another holder of the key takes the nonce of a send of ours that waits
// unmined: that call gives up, and the next send goes to the following
// nonce (no replacement war at a nonce that is gone).
#[tokio::test]
async fn a_nonce_taken_by_another_key_holder_moves_the_next_send_on() {
    let op = calm_op();
    let c = op_node(&op, u128::MAX).await;
    let snark = results_snark();
    let take = async {
        wait_until("first send", async || !op.lock().unwrap().sends.is_empty()).await;
        let mut o = op.lock().unwrap();
        o.count = 4;
        // Every later send at nonce 3 is refused as taken.
        for _ in 0..8 {
            o.refuse
                .push_back(Some("nonce too low: next nonce 4, tx nonce 3".into()));
        }
    };
    let (got, ()) = tokio::join!(c.submit_results(&PID, &snark), take);
    assert!(got.is_err());
    {
        let mut o = op.lock().unwrap();
        o.refuse.clear();
        o.mine = Some(true);
    }
    c.submit_results(&PID, &snark).await.unwrap();
    let sends = op.lock().unwrap().sends.clone();
    let last = sends.last().unwrap();
    assert_eq!((last.nonce, last.accepted), (4, true), "{sends:?}");
    assert!(
        sends.iter().all(|s| s.nonce == 3 || s.nonce == 4),
        "{sends:?}"
    );
}

// --------------------------------------------------- poster restarts

/// A post left pending by a run that stopped at its deadline (the node going
/// down with the blob transaction in the pool): its record stays.
async fn left_pending(l1: &FakeL1, db: &Db, tag: u8) -> davinci_sequencer::storage::L1PostRecord {
    l1.hold(true);
    let p = Poster::new(
        Arc::new(l1.clone()),
        db.clone(),
        u128::MAX,
        Duration::from_millis(5),
    )
    .unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_millis(100);
    let e = p
        .post(&PID, &ROOT, &fake_blobs(tag, 1), deadline)
        .await
        .unwrap_err();
    assert!(matches!(e, PostError::Deadline), "{e}");
    db.l1_posts().unwrap().pop().unwrap()
}

// The pending post was mined while the node was down: the restarted poster
// moves to the next nonce and drops the record.
#[tokio::test]
async fn a_restart_after_the_pending_post_was_mined_moves_on() {
    let l1 = FakeL1::new();
    let (_dir, db) = fresh_db();
    let rec = left_pending(&l1, &db, 1).await;
    l1.hold(false);
    l1.mine(1);
    assert!(l1.block_of(&rec.tx_hash).is_some());
    let p = Poster::new(
        Arc::new(l1.clone()),
        db.clone(),
        u128::MAX,
        Duration::from_millis(5),
    )
    .unwrap();
    let posted = p
        .post(&PID, &ROOT, &fake_blobs(2, 1), later())
        .await
        .unwrap();
    assert_eq!(posted.nonce, rec.nonce + 1);
    assert_eq!(l1.mined_posts(), 2);
    assert!(db.l1_posts().unwrap().is_empty());
}

// A crash left a stale record of a mined post beside the pending one: the
// restarted poster replaces the newest at its nonce, every fee doubled, and
// the replaced one never lands.
#[tokio::test]
async fn a_restart_with_a_stale_record_replaces_the_newest() {
    let l1 = FakeL1::new();
    let (_dir, db) = fresh_db();
    let mined = Poster::new(
        Arc::new(l1.clone()),
        db.clone(),
        u128::MAX,
        Duration::from_millis(5),
    )
    .unwrap()
    .post(&PID, &ROOT, &fake_blobs(1, 1), later())
    .await
    .unwrap();
    let stale = davinci_sequencer::storage::L1PostRecord {
        pid: PID,
        root_before: ROOT,
        nonce: mined.nonce,
        tx_hash: mined.tx,
        fees: fees(),
        vhs: vec![],
        posted_at: 0,
        from: l1.sender().unwrap().0.0,
    };
    db.put_l1_post(&stale).unwrap();
    let pending = left_pending(&l1, &db, 2).await;
    assert_eq!(pending.nonce, mined.nonce + 1);
    l1.hold(false);
    l1.set_auto_mine(false);
    let p = Poster::new(
        Arc::new(l1.clone()),
        db.clone(),
        u128::MAX,
        Duration::from_millis(5),
    )
    .unwrap();
    l1.set_auto_mine(true);
    let posted = p
        .post(&PID, &ROOT, &fake_blobs(3, 1), later())
        .await
        .unwrap();
    assert_eq!(posted.nonce, pending.nonce);
    let last = l1.sends().pop().unwrap();
    assert_eq!(last.fees, pending.fees.doubled());
    assert_eq!(
        l1.block_of(&pending.tx_hash),
        None,
        "the replaced post landed"
    );
    assert_eq!(l1.mined_posts(), 2);
    assert!(db.l1_posts().unwrap().is_empty());
}

// ------------------------------------------------------------ RPC failover

/// An HTTP endpoint answering every POST with `pad` MiB of whitespace and
/// then a valid `eth_blockNumber` answer, streamed in 1 MiB writes.
async fn padded_rpc(pad_mib: usize) -> url::Url {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/", l.local_addr().unwrap())
        .parse()
        .unwrap();
    tokio::spawn(async move {
        while let Ok((mut s, _)) = l.accept().await {
            tokio::spawn(async move {
                let mut buf = vec![0u8; 64 << 10];
                let _ = s.read(&mut buf).await;
                let tail = br#"{"jsonrpc":"2.0","id":0,"result":"0x10"}"#;
                let len = (pad_mib << 20) + tail.len();
                let head = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {len}\r\n\r\n"
                );
                if s.write_all(head.as_bytes()).await.is_err() {
                    return;
                }
                let chunk = vec![b' '; 1 << 20];
                for _ in 0..pad_mib {
                    if s.write_all(&chunk).await.is_err() {
                        return;
                    }
                }
                let _ = s.write_all(tail).await;
            });
        }
    });
    url
}

// An RPC answer far larger than any block or log range is refused as it
// streams in (and fails over), not read whole into memory.
// BUG: the failover transport reads an RPC body whole, however large.
#[tokio::test]
async fn an_oversized_rpc_body_is_refused() {
    use alloy::providers::Provider;
    let p = davinci_sequencer::web3::rpc_provider(&[padded_rpc(160).await]);
    let got = p.get_block_number().await;
    assert!(
        got.as_ref()
            .is_err_and(|e| e.to_string().contains("too large")),
        "{got:?}"
    );
    // Small padding is just whitespace: answered.
    let p = davinci_sequencer::web3::rpc_provider(&[padded_rpc(1).await]);
    assert_eq!(p.get_block_number().await.unwrap(), 16);
}

// One proof build reads an inconsistent endpoint (its receipt index from one
// fork, the block from another) and proves another blob transaction of the
// same block: the next build reads consistently and the batch lands.
// BUG: one inconsistent proof read ends the settlement as held.
#[tokio::test]
async fn one_inconsistent_proof_read_is_retried() {
    let mut hostile = None;
    let r = rig(Duration::from_millis(5), |l1| {
        let h = Arc::new(Hostile::new(l1));
        hostile = Some(h.clone());
        h
    });
    let h = hostile.unwrap();
    // Another account's blob transaction in the block of ours.
    r.l1.hold(true);
    let other = r.l1.with_sender(0x55);
    let tx = other
        .sign_blob_tx(0, &fees(), &fake_blobs(0x55, 1))
        .await
        .unwrap();
    other.send_blob_tx(&tx).await.unwrap();
    let job = r.job(later());
    let release = async {
        wait_until("our post pooled", async || r.l1.sends().len() >= 2).await;
        r.l1.hold(false);
    };
    let ((_, posting), ()) = tokio::join!(
        with_post(&r.da, &r.s.chain, &job, std::future::ready(())),
        release
    );
    assert_eq!(r.l1.block_of(&tx.hash), r.l1.block_of(&posting.posts[0].tx));
    *h.swap_once.lock().unwrap() = Some(tx.hash);
    let out = settle(&r.da, &r.s.chain, &job, &r.snark, posting).await;
    assert!(matches!(out, Settled::Landed(_)), "{out:?}");
}

// The settle loop behind endpoints flapping between the truth and a view
// from before the post: receipts read null half the time, heads lag. One
// post, one landing.
#[tokio::test(start_paused = true)]
async fn flapping_endpoints_settle_once() {
    let r = rig(Duration::from_secs(1), |l1| {
        Arc::new(Hostile {
            flap: true,
            ..Hostile::new(l1)
        })
    });
    let job = r.job(later());
    let (_, posting) = with_post(&r.da, &r.s.chain, &job, std::future::ready(())).await;
    let out = settle(&r.da, &r.s.chain, &job, &r.snark, posting).await;
    assert!(matches!(out, Settled::Landed(_)), "{out:?}");
    assert_eq!(r.l1.sends().len(), 1, "{:?}", r.l1.sends());
    assert_eq!(r.l1.mined_posts(), 1);
    assert_eq!(r.s.chain.root(), [0x42; 32]);
}

// Every build proves the other transaction: the mismatch persists past
// PROOF_FAIL_FOR, and only then is the settlement held, without a re-post
// (no post fixes what the endpoint serves).
#[tokio::test(start_paused = true)]
async fn a_persistent_proof_of_other_blobs_is_held_after_proof_fail_for() {
    use davinci_sequencer::l1da::PROOF_FAIL_FOR;
    let mut hostile = None;
    let r = rig(Duration::from_secs(1), |l1| {
        let h = Arc::new(Hostile {
            swap_keep: true,
            ..Hostile::new(l1)
        });
        hostile = Some(h.clone());
        h
    });
    let h = hostile.unwrap();
    r.l1.hold(true);
    let other = r.l1.with_sender(0x55);
    let tx = other
        .sign_blob_tx(0, &fees(), &fake_blobs(0x55, 1))
        .await
        .unwrap();
    other.send_blob_tx(&tx).await.unwrap();
    let job = r.job(later());
    let release = async {
        wait_until("our post pooled", async || r.l1.sends().len() >= 2).await;
        r.l1.hold(false);
    };
    let ((_, posting), ()) = tokio::join!(
        with_post(&r.da, &r.s.chain, &job, std::future::ready(())),
        release
    );
    *h.swap_once.lock().unwrap() = Some(tx.hash);
    let t = tokio::time::Instant::now();
    let out = settle(&r.da, &r.s.chain, &job, &r.snark, posting).await;
    assert!(
        matches!(&out, Settled::Failed(e) if e.contains("other blobs")),
        "{out:?}"
    );
    assert!(
        t.elapsed() >= PROOF_FAIL_FOR,
        "held after {:?}",
        t.elapsed()
    );
    assert_eq!(r.l1.mined_posts(), 2, "re-posted: {:?}", r.l1.sends());
}

/// Blobs of valid field elements with no versioned hash anyone asked for.
fn filler(rng: &mut Rng, n: usize) -> Vec<Blob> {
    (0..n)
        .map(|_| {
            let mut b: Blob = vec![0u8; BLOB_SIZE].into_boxed_slice().try_into().unwrap();
            for cell in b.chunks_mut(32) {
                cell[1..].copy_from_slice(&rng.bytes(31));
            }
            b
        })
        .collect()
}

// A beacon padding its answer with 120 blobs nobody asked for (31 MB, under
// the body and candidate caps): the wanted ones come first, so matching them
// must not cost a KZG commitment per padding blob.
// BUG: match_blobs commits to every candidate an answer offers, however many.
#[tokio::test]
async fn padding_after_the_wanted_blobs_costs_no_commitments() {
    let (blobs, hashes, _) = two_blobs();
    let pad = filler(&mut Rng(5), 120);
    // What one commitment costs on this host now (after loading the setup).
    kzg_commitment(&pad[0]);
    let t = std::time::Instant::now();
    for b in &pad[..4] {
        kzg_commitment(b);
    }
    let one = t.elapsed() / 4;
    let mut answer: Vec<&Blob> = vec![&blobs[0], &blobs[1]];
    answer.extend(pad.iter());
    let (url, _) = beacon(
        reply(StatusCode::OK, blobs_body(&answer)),
        reply(StatusCode::NOT_FOUND, ""),
    )
    .await;
    let src = BeaconBlobs::beacon_only(vec![url]);
    src.slot_for(AT).await.unwrap();
    let t = std::time::Instant::now();
    let got = src.blobs_at(AT, &hashes).await.unwrap();
    let took = t.elapsed();
    assert!(got == blobs);
    // Two commitments plus parsing; 120 more would be about 120 x `one`.
    assert!(
        took < one * 40,
        "{took:?} for 122 candidates, {one:?} a commitment"
    );
}

// The same padding ahead of the wanted blobs: an answer offering far more
// candidates than a block can hold is refused before any commitment, so a
// hostile beacon costs a bounded amount of CPU; a modest one is still read.
// BUG: an answer of any number of candidates is committed to in full.
#[tokio::test]
async fn an_answer_with_too_many_candidates_is_refused_cheaply() {
    use davinci_sequencer::web3::MAX_BLOB_CANDIDATES;
    let (blobs, hashes, _) = two_blobs();
    let pad = filler(&mut Rng(6), 200);
    kzg_commitment(&pad[0]);
    let t = std::time::Instant::now();
    for b in &pad[..4] {
        kzg_commitment(b);
    }
    let one = t.elapsed() / 4;
    let mut answer: Vec<&Blob> = pad.iter().collect();
    answer.extend([&blobs[0], &blobs[1]]);
    let (url, _) = beacon(
        reply(StatusCode::OK, blobs_body(&answer)),
        reply(StatusCode::NOT_FOUND, ""),
    )
    .await;
    let src = BeaconBlobs::beacon_only(vec![url]);
    src.slot_for(AT).await.unwrap();
    let t = std::time::Instant::now();
    let got = src.blobs_at(AT, &hashes).await;
    let took = t.elapsed();
    assert!(got.is_err(), "202 candidates accepted");
    assert!(
        took < one * 40,
        "{took:?} for 202 candidates, {one:?} a commitment"
    );
    // Under the bound, wanted blobs last: read, at a bounded cost.
    let mut answer: Vec<&Blob> = pad[..MAX_BLOB_CANDIDATES - 2].iter().collect();
    answer.extend([&blobs[0], &blobs[1]]);
    let (url, _) = beacon(
        reply(StatusCode::OK, blobs_body(&answer)),
        reply(StatusCode::NOT_FOUND, ""),
    )
    .await;
    let src = BeaconBlobs::beacon_only(vec![url]);
    assert!(src.blobs_at(AT, &hashes).await.unwrap() == blobs);
}

// ------------------------------------------- blob-mode replacements on anvil

// A blob-carrying settlement that is not mined (anvil without mining) is
// replaced at its nonce on a real pool: the call ends `Stuck` after its
// attempts with one transaction pooled. anvil takes a 12.5 % blob
// replacement, so it does not check geth's rule; the scripted chain does
// (`blob_replacements_double_every_fee_in_one_send`).
#[tokio::test(flavor = "multi_thread")]
async fn anvil_blob_replacements_satisfy_the_blob_pool() {
    use alloy::node_bindings::Anvil;
    use alloy::providers::Provider;
    use davinci_zkvm_sdk::limits::VOTE_ID_MIN;
    if std::env::var("ANVIL").as_deref() != Ok("1") {
        eprintln!("skipped: set ANVIL=1");
        return;
    }
    let bin = std::path::PathBuf::from(std::env::var("HOME").unwrap()).join(".foundry/bin/anvil");
    let anvil = Anvil::at(bin)
        .args(["--hardfork", "osaka", "--no-mining"])
        .try_spawn()
        .expect("spawn anvil");
    let url = anvil.endpoint_url();
    let reg = Address::repeat_byte(0x77);
    // A registry that accepts any call: STOP.
    let raw = davinci_sequencer::web3::rpc_provider(std::slice::from_ref(&url));
    let _: () = raw
        .raw_request("anvil_setCode".into(), (reg, "0x00"))
        .await
        .unwrap();
    let key = SecretString::new(ANVIL_KEY);
    let mut c = Contracts::open(std::slice::from_ref(&url), reg, Some(&key), true)
        .await
        .unwrap();
    c.set_receipt_timeout(Duration::from_millis(400));
    let t = TransitionData {
        vote_ids: vec![VOTE_ID_MIN + 1],
        updates: vec![],
        num_fields: 1,
    };
    let blobs = build_blobs(&t, &Fr::from(3u64), &[3u8; 32]).unwrap();
    let e = c
        .submit_transition(&PID, &results_snark(), &blobs)
        .await
        .unwrap_err();
    assert!(matches!(e, Web3Error::Stuck { nonce: 0, .. }), "{e}");
    let n = raw
        .get_transaction_count(c.signer().unwrap())
        .pending()
        .await
        .unwrap();
    assert_eq!(n, 1, "one transaction pooled at nonce 0");
}

// A blob settlement that is not mined, on a pool with geth's blob rule
// (core/txpool/blobpool, `PriceBump` 100: fee cap, tip and blob fee cap each
// doubled): every replacement goes through in one send, each fee at least
// doubled, until the attempts run out.
// BUG: blob replacements bump the fee cap and the tip by 12.5 %, which the blob pool refuses.
#[tokio::test]
async fn blob_replacements_double_every_fee_in_one_send() {
    use davinci_zkvm_sdk::limits::VOTE_ID_MIN;
    let op = calm_op();
    op.lock().unwrap().blob_pool = true;
    let c = op_node(&op, u128::MAX).await;
    let t = TransitionData {
        vote_ids: vec![VOTE_ID_MIN + 1],
        updates: vec![],
        num_fields: 1,
    };
    let blobs = build_blobs(&t, &Fr::from(3u64), &[3u8; 32]).unwrap();
    let e = c
        .submit_transition(&PID, &results_snark(), &blobs)
        .await
        .unwrap_err();
    let sends = op.lock().unwrap().sends.clone();
    assert!(
        matches!(e, Web3Error::Stuck { nonce: 3, .. }),
        "{e}: {sends:?}"
    );
    assert!(sends.len() >= 2, "{sends:?}");
    assert!(sends.iter().all(|s| s.accepted), "{sends:?}");
    for w in sends.windows(2) {
        let (a, b) = (&w[0], &w[1]);
        assert_eq!(a.nonce, b.nonce);
        assert!(
            b.max_fee >= 2 * a.max_fee && b.tip >= 2 * a.tip && b.blob_fee >= 2 * a.blob_fee,
            "{sends:?}"
        );
    }
}

// A plain transaction keeps the gentler bump: +12.5 % (nodes want 10 %).
#[tokio::test]
async fn plain_replacements_keep_the_small_bump() {
    let op = calm_op();
    let c = op_node(&op, u128::MAX).await;
    let snark = results_snark();
    let _ = c.submit_results(&PID, &snark).await.unwrap_err();
    let sends = op.lock().unwrap().sends.clone();
    assert!(sends.len() >= 2, "{sends:?}");
    for w in sends.windows(2) {
        assert!(w[1].max_fee < 2 * w[0].max_fee, "{sends:?}");
        assert!(w[1].max_fee * 8 >= w[0].max_fee * 9, "{sends:?}");
    }
}
