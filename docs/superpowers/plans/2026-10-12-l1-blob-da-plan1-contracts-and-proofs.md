# L1-blob DA, Plan 1: L1 proofs and the registry mode

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build and validate the on-chain half of "Ethereum blob DA with OP Stack settlement": a Rust builder for L1 blob-inclusion proofs, a stateless `L1BlobVerifier` contract, and a deploy-time L1-blob mode in `ProcessRegistry`, checked against real Ethereum mainnet data and against OP Mainnet's `L1Block`.

**Architecture:** The node proves that a type-3 transaction is in Ethereum's canonical chain with RLP headers (newest first, anchored at any header whose hash equals OP's `L1Block.hash()`) and a Merkle-Patricia proof of the transaction in its block's transactions trie. `L1BlobVerifier` checks that proof and returns the blob versioned hashes; `ProcessRegistry` opens each commitment with `0x0A` against them. Plan 2 (node, e2e, deployment) builds on the ABI this plan fixes.

**Tech Stack:** Solidity 0.8.28 (Foundry 1.8.3 at `~/.foundry/bin`, `via_ir`, cancun), Optimism's `RLPReader`/`MerkleTrie` (MIT, vendored), Rust 1.95 with alloy 2.5 (`trie`, `rlp` features).

**Spec:** `docs/superpowers/specs/2026-10-12-l1-blob-da-op-design.md` (davinci-sequencer). Read sections 3 and 4 before starting.

## Global Constraints

- No change to the zkVM program or any program vk.
- Same Solidity source for both modes; the mode is the registry constructor argument `_l1BlobVerifier` (zero = today's blob mode).
- Blob-mode behaviour, error order and existing tests must not change.
- Header parsing by RLP index without asserting the item count: `parentHash` item 0, `transactionsRoot` item 4, `number` item 8; fewer than 9 items is an error.
- Transactions-trie key is `rlp(txIndex)` (`0x80` for index 0); the value is the EIP-2718 envelope; type-3 `blob_versioned_hashes` is list item 10, non-empty, each 32 bytes starting with `0x01`.
- Anchoring: the chain is accepted if any header's keccak equals `L1Block.hash()`; transactions may only reference that header or older ones.
- Never print, log or commit a private key or a full RPC URL; keys are read from files (`~/optimism-privkey.txt` for OP).
- Commit titles: `type(scope): lowercase title`, no AI attribution or `Co-Authored-By` lines. Commit on branch `feat/l1-blob-da` in both repositories; never push.
- Build output stays in the repositories' `target/`/`out/` or under `~/.cache`, never `/tmp`.

## Review Focus

- **A header chain whose top part is above OP's origin:** headers newer than the anchor must never be accepted as the block of a blob transaction (Task 4 test `test_RevertWhen_TxAboveOrigin`).
- **Transaction indices 0, 128-255 and above 255:** the key encodings `0x80`, `0x81xx`, `0x82xxxx` must all prove (Task 4 tests on the `index0`, `index128plus` and `big_block` fixtures; Task 1 test `tx_key_encodings`).
- **Pre-Prague (Cancun) and post-Prague headers:** different item counts must both decode (Task 4 test on the `cancun` fixture; Task 1 live test over 100 blocks).
- **A proof for a non-blob transaction:** must revert `NotBlobTx`, not return an empty list (Task 4 `test_RevertWhen_NotBlobTx`).
- **A blob-mode registry receiving an L1-mode call and vice versa:** must revert `WrongDAMode` before any state change (Task 6 tests).

---

## File Structure

davinci-sequencer (`/home/p4u/davinci-sequencer`):
- Create `sequencer/src/web3/l1proof.rs`: the proof types, header RLP, transactions-trie proof, local verification, and the async builder.
- Modify `sequencer/src/web3/mod.rs`: `pub mod l1proof;`.
- Modify `sequencer/Cargo.toml`: alloy features `trie`, `rlp`.
- Create `sequencer/examples/l1proof_fixtures.rs`: writes the forge fixtures and the offline Rust fixtures from mainnet.
- Create `sequencer/examples/l1proof_check.rs`: deploys `L1BlobVerifier` on OP and checks a live proof with `eth_call`.
- Create `sequencer/tests/l1proof.rs`: offline tests on stored blocks, plus the live 100-block test gated by `L1PROOF_LIVE=1`.
- Create `sequencer/tests/fixtures/l1proof/*.json`: stored blocks.
- Modify `e2e/src/chain.rs`, `sequencer/tests/web3_anvil.rs`, `client/tests/organizer.rs`: the registry's new constructor argument.
- Modify `sequencer/abi/ProcessRegistry.json`: regenerated ABI.

davinci-contracts (`/home/p4u/davinci-contracts`):
- Create `src/libraries/op/{RLPReader,RLPErrors,Bytes,MerkleTrie}.sol`: vendored, MIT.
- Create `src/interfaces/IL1BlobVerifier.sol`, `src/interfaces/IL1Block.sol`, `src/L1BlobVerifier.sol`.
- Create `test/mocks/MockL1Block.sol`, `test/mocks/MockL1BlobVerifier.sol`, `test/L1BlobVerifier.t.sol`, `test/TransitionL1.t.sol`, `test/vectors/l1blob/*.json`.
- Modify `src/ProcessRegistry.sol`, `src/interfaces/IProcessRegistry.sol`, `test/RegistryTestBase.sol`, `test/Genesis.t.sol`, `test/Grace.t.sol`, `test/NewProcess.t.sol`, `script/DeployAll.s.sol`, `script/verify_deployment.py`, `deploy_all.sh`, `docs/settlement.md`, `docs/errors.md`, `docs/deployment.md`, `golang-types/` (regenerated).

---

### Task 1: L1 proof builder (Rust)

**Files:**
- Create: `sequencer/src/web3/l1proof.rs`
- Modify: `sequencer/src/web3/mod.rs` (add `pub mod l1proof;` next to the other modules)
- Modify: `sequencer/Cargo.toml` (add `"trie", "rlp"` to the alloy feature list at line 39)
- Create: `sequencer/tests/l1proof.rs`, `sequencer/tests/fixtures/l1proof/` (filled by Task 2's example; this task commits the code first with a unit test that needs no fixture)

**Interfaces:**
- Produces (used by Tasks 2, 8 and Plan 2):
  - `pub struct L1TxProof { pub header_index: usize, pub key: Bytes, pub proof: Vec<Bytes> }`
  - `pub struct L1BlobProof { pub headers: Vec<Bytes>, pub txs: Vec<L1TxProof> }`
  - `pub fn tx_key(index: usize) -> Bytes`
  - `pub fn header_rlp(h: &alloy::consensus::Header, hash: B256) -> Result<Bytes, L1ProofError>`
  - `pub fn tx_trie_proof(envelopes: &[Bytes], index: usize) -> (B256, Vec<Bytes>)`
  - `pub fn verify_local(p: &L1BlobProof, origin: B256) -> Result<(Vec<B256>, Vec<u64>), L1ProofError>` (mirrors the contract: returns versioned hashes and the block number per tx)
  - `pub async fn build<P: alloy::providers::Provider>(l1: &P, tx_hashes: &[B256], top: u64) -> Result<L1BlobProof, L1ProofError>`

- [ ] **Step 1: Create the branch and enable the alloy features**

```bash
cd /home/p4u/davinci-sequencer && git checkout feat/l1-blob-da
```
In `sequencer/Cargo.toml`, add `"trie",` and `"rlp",` to the `alloy` feature list (the list that starts with `"std",`).

- [ ] **Step 2: Write the failing unit test for key encodings and a tiny trie**

Append to `sequencer/src/web3/l1proof.rs` (create the file with the module doc and the test module only):

```rust
//! Proof that blob transactions are in Ethereum's canonical chain, for the L1-blob DA mode: RLP
//! headers newest first (from about the L1 head down to each blob block) and a Merkle-Patricia
//! proof of each transaction in its block's transactions trie. `L1BlobVerifier` checks the same
//! thing on OP against `L1Block.hash()`; `verify_local` mirrors it before a send.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tx_key_encodings() {
        assert_eq!(tx_key(0).as_ref(), &[0x80]);
        assert_eq!(tx_key(1).as_ref(), &[0x01]);
        assert_eq!(tx_key(127).as_ref(), &[0x7f]);
        assert_eq!(tx_key(128).as_ref(), &[0x81, 0x80]);
        assert_eq!(tx_key(255).as_ref(), &[0x81, 0xff]);
        assert_eq!(tx_key(256).as_ref(), &[0x82, 0x01, 0x00]);
    }

    #[test]
    fn trie_proofs_verify_for_every_index() {
        // 300 envelopes, so keys of one, two and three bytes all occur.
        let envs: Vec<Bytes> = (0..300u32).map(|i| Bytes::from([0x02u8].into_iter().chain(i.to_be_bytes()).chain([7u8; 100]).collect::<Vec<u8>>())).collect();
        let want = alloy::trie::root::ordered_trie_root_with_encoder(&envs, |e, buf| buf.extend_from_slice(e));
        for i in [0usize, 1, 127, 128, 255, 256, 299] {
            let (root, proof) = tx_trie_proof(&envs, i);
            assert_eq!(root, want, "root for index {i}");
            assert!(verify_tx(root, &tx_key(i), &proof, &envs[i]).is_ok(), "proof for index {i}");
        }
    }
}
```

- [ ] **Step 3: Run it to verify it fails**

Run: `cd /home/p4u/davinci-sequencer && cargo test -p davinci-sequencer --lib l1proof 2>&1 | tail -5`
Expected: FAIL to compile (`tx_key`, `tx_trie_proof`, `verify_tx` not defined).

- [ ] **Step 4: Implement the module**

Insert above the test module in `sequencer/src/web3/l1proof.rs`:

```rust
use alloy::consensus::Header;
use alloy::eips::eip2718::Encodable2718;
use alloy::primitives::{keccak256, Bytes, B256};
use alloy::providers::Provider;
use alloy::rlp::{Decodable, Encodable};
use alloy::trie::proof::{verify_proof, ProofRetainer};
use alloy::trie::{HashBuilder, Nibbles};

/// Merkle-Patricia proof of one transaction in the block of `headers[header_index]`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct L1TxProof {
    pub header_index: usize,
    /// `rlp(txIndex)`.
    pub key: Bytes,
    /// Trie nodes from the root down to the leaf.
    pub proof: Vec<Bytes>,
}

/// Everything `L1BlobVerifier.verify` takes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct L1BlobProof {
    /// RLP headers, newest first, each the parent of the one before.
    pub headers: Vec<Bytes>,
    pub txs: Vec<L1TxProof>,
}

#[derive(Debug, thiserror::Error)]
pub enum L1ProofError {
    #[error("header of block {0} does not hash to its block hash")]
    HeaderHash(u64),
    #[error("transactions root of block {0} does not match its header")]
    TxRoot(u64),
    #[error("header chain broken at position {0}")]
    BrokenChain(usize),
    #[error("no header matches the L1 origin {0}")]
    NoAnchor(B256),
    #[error("transaction {0} references a header above the origin")]
    AboveOrigin(usize),
    #[error("transaction {0} is not a blob transaction")]
    NotBlobTx(usize),
    #[error("bad header: {0}")]
    BadHeader(String),
    #[error("bad transaction proof: {0}")]
    BadProof(String),
    #[error("rpc: {0}")]
    Rpc(String),
}

/// Transactions-trie key of `index`: `rlp(index)` (`0x80` for 0).
pub fn tx_key(index: usize) -> Bytes {
    alloy::rlp::encode(index as u64).into()
}

/// RLP of `h`, checked against `hash`.
pub fn header_rlp(h: &Header, hash: B256) -> Result<Bytes, L1ProofError> {
    let mut out = Vec::new();
    h.encode(&mut out);
    if keccak256(&out) != hash {
        return Err(L1ProofError::HeaderHash(h.number));
    }
    Ok(out.into())
}

/// Root of the transactions trie over `envelopes` (EIP-2718, block order) and the proof of `index`.
pub fn tx_trie_proof(envelopes: &[Bytes], index: usize) -> (B256, Vec<Bytes>) {
    let mut items: Vec<(Nibbles, &Bytes)> =
        envelopes.iter().enumerate().map(|(i, e)| (Nibbles::unpack(tx_key(i)), e)).collect();
    items.sort_by(|a, b| a.0.cmp(&b.0));
    let target = Nibbles::unpack(tx_key(index));
    let mut hb = HashBuilder::default().with_proof_retainer(ProofRetainer::new(vec![target.clone()]));
    for (k, v) in &items {
        hb.add_leaf(k.clone(), v);
    }
    let root = hb.root();
    let proof = hb.take_proof_nodes().matching_nodes_sorted(&target).into_iter().map(|(_, n)| n).collect();
    (root, proof)
}

/// Checks that `value` sits at `key` under `root`.
pub fn verify_tx(root: B256, key: &Bytes, proof: &[Bytes], value: &Bytes) -> Result<(), L1ProofError> {
    verify_proof(root, Nibbles::unpack(key), Some(value.to_vec()), proof.iter())
        .map_err(|e| L1ProofError::BadProof(e.to_string()))
}

/// The fields of a header RLP the verifier reads: (parentHash, transactionsRoot, number).
fn header_fields(rlp: &[u8]) -> Result<(B256, B256, u64), L1ProofError> {
    let h = Header::decode(&mut &rlp[..]).map_err(|e| L1ProofError::BadHeader(e.to_string()))?;
    Ok((h.parent_hash, h.transactions_root, h.number))
}

/// Versioned hashes of a type-3 envelope, or `None` for any other type.
fn blob_hashes(envelope: &[u8]) -> Option<Vec<B256>> {
    let tx = alloy::consensus::TxEnvelope::decode_2718(&mut &envelope[..]).ok()?;
    tx.blob_versioned_hashes().map(|v| v.to_vec()).filter(|v| !v.is_empty())
}

/// Mirrors `L1BlobVerifier.verify`: the versioned hashes in proof order and the block number of
/// each transaction. `origin` is OP's `L1Block.hash()`.
pub fn verify_local(p: &L1BlobProof, origin: B256) -> Result<(Vec<B256>, Vec<u64>), L1ProofError> {
    let mut fields = Vec::with_capacity(p.headers.len());
    let mut anchor = None;
    for (i, h) in p.headers.iter().enumerate() {
        let f = header_fields(h)?;
        let hash = keccak256(h);
        if i > 0 && fields.last().map(|(parent, _, _): &(B256, B256, u64)| *parent) != Some(hash) {
            return Err(L1ProofError::BrokenChain(i));
        }
        if anchor.is_none() && hash == origin {
            anchor = Some(i);
        }
        fields.push(f);
    }
    let anchor = anchor.ok_or(L1ProofError::NoAnchor(origin))?;
    let (mut vhs, mut blocks) = (vec![], vec![]);
    for (t, tx) in p.txs.iter().enumerate() {
        if tx.header_index < anchor || tx.header_index >= p.headers.len() {
            return Err(L1ProofError::AboveOrigin(t));
        }
        let (_, root, number) = fields[tx.header_index];
        let value = proven_value(root, &tx.key, &tx.proof)?;
        vhs.extend(blob_hashes(&value).ok_or(L1ProofError::NotBlobTx(t))?);
        blocks.push(number);
    }
    Ok((vhs, blocks))
}

/// The leaf value of a proof: the last node is the leaf; its value is the envelope.
fn proven_value(root: B256, key: &Bytes, proof: &[Bytes]) -> Result<Bytes, L1ProofError> {
    let leaf = proof.last().ok_or_else(|| L1ProofError::BadProof("empty proof".into()))?;
    let node = alloy::trie::nodes::TrieNode::decode(&mut &leaf[..]).map_err(|e| L1ProofError::BadProof(e.to_string()))?;
    let value = match node {
        alloy::trie::nodes::TrieNode::Leaf(l) => Bytes::from(l.value),
        _ => return Err(L1ProofError::BadProof("last node is not a leaf".into())),
    };
    verify_tx(root, key, proof, &value)?;
    Ok(value)
}

/// Builds the proof for the blob transactions `tx_hashes` (in that order) with headers from block
/// `top` (inclusive, at or above OP's L1 origin; the L1 head is a good choice) down to the oldest
/// blob block.
pub async fn build<P: Provider>(l1: &P, tx_hashes: &[B256], top: u64) -> Result<L1BlobProof, L1ProofError> {
    let rpc = |e: alloy::transports::TransportError| L1ProofError::Rpc(e.to_string());
    let mut located = vec![];
    for h in tx_hashes {
        let r = l1.get_transaction_receipt(*h).await.map_err(rpc)?.ok_or_else(|| L1ProofError::Rpc(format!("no receipt for {h}")))?;
        let n = r.block_number.ok_or_else(|| L1ProofError::Rpc(format!("{h} not mined")))?;
        let i = r.transaction_index.ok_or_else(|| L1ProofError::Rpc(format!("{h} has no index")))? as usize;
        located.push((n, i));
    }
    let oldest = located.iter().map(|(n, _)| *n).min().ok_or_else(|| L1ProofError::Rpc("no transactions".into()))?;
    if top < oldest {
        return Err(L1ProofError::Rpc(format!("top {top} is below block {oldest}")));
    }
    let mut headers = vec![];
    for b in (oldest..=top).rev() {
        let blk = l1.get_block_by_number(b.into()).await.map_err(rpc)?.ok_or_else(|| L1ProofError::Rpc(format!("no block {b}")))?;
        headers.push(header_rlp(&blk.header.inner, blk.header.hash)?);
    }
    let mut txs = vec![];
    for (n, i) in located {
        let blk = l1.get_block_by_number(n.into()).full().await.map_err(rpc)?.ok_or_else(|| L1ProofError::Rpc(format!("no block {n}")))?;
        let envs: Vec<Bytes> = blk.transactions.txns().map(|t| t.inner.encoded_2718().into()).collect();
        let (root, proof) = tx_trie_proof(&envs, i);
        if root != blk.header.transactions_root {
            return Err(L1ProofError::TxRoot(n));
        }
        txs.push(L1TxProof { header_index: (top - n) as usize, key: tx_key(i), proof });
    }
    Ok(L1BlobProof { headers, txs })
}
```

Add `pub mod l1proof;` to `sequencer/src/web3/mod.rs`. If an alloy path differs in 2.5 (for example `TrieNode` or `txns()`), find the equivalent with `grep -rn` in `~/.cargo/registry/src/*/alloy-*/src` and keep the behaviour identical.

- [ ] **Step 5: Run the unit tests**

Run: `cargo test -p davinci-sequencer --lib l1proof 2>&1 | tail -5`
Expected: PASS (`tx_key_encodings`, `trie_proofs_verify_for_every_index`).

- [ ] **Step 6: Write the live test (gated) over 100 mainnet blocks**

Create `sequencer/tests/l1proof.rs`:

```rust
//! L1 proof builder against Ethereum mainnet. The live test runs with `L1PROOF_LIVE=1`
//! (`L1PROOF_RPC` overrides the RPC); the offline tests use blocks stored by
//! `examples/l1proof_fixtures.rs`.

use alloy::eips::eip2718::Encodable2718;
use alloy::primitives::{keccak256, Bytes};
use alloy::providers::{Provider, ProviderBuilder};
use davinci_sequencer::web3::l1proof::{header_rlp, tx_key, tx_trie_proof, verify_tx};

#[tokio::test(flavor = "multi_thread")]
async fn live_hundred_random_blocks() {
    if std::env::var("L1PROOF_LIVE").as_deref() != Ok("1") {
        eprintln!("skip: set L1PROOF_LIVE=1");
        return;
    }
    let url = std::env::var("L1PROOF_RPC").unwrap_or_else(|_| "https://ethereum-rpc.publicnode.com".into());
    let l1 = ProviderBuilder::new().connect_http(url.parse().unwrap());
    let head = l1.get_block_number().await.unwrap();
    // Cancun (19,426,587 .. 22,431,083), Prague and later (22,431,084 .. head): 50 blocks each.
    let mut blocks: Vec<u64> = (0..50).map(|k| 19_426_587 + k * (22_431_083 - 19_426_587) / 50 + k).collect();
    blocks.extend((0..50).map(|k| 22_431_084 + k * (head - 22_431_084) / 50 + 7 * k));
    let (mut big, mut checked) = (false, 0);
    for n in blocks {
        let b = l1.get_block_by_number(n.into()).full().await.unwrap().unwrap();
        let rlp = header_rlp(&b.header.inner, b.header.hash).expect("header hashes to its block hash");
        assert_eq!(keccak256(&rlp), b.header.hash);
        let envs: Vec<Bytes> = b.transactions.txns().map(|t| t.inner.encoded_2718().into()).collect();
        if envs.is_empty() {
            continue;
        }
        big |= envs.len() > 256;
        for i in [0, envs.len() / 2, envs.len() - 1, 128.min(envs.len() - 1)] {
            let (root, proof) = tx_trie_proof(&envs, i);
            assert_eq!(root, b.header.transactions_root, "block {n}");
            verify_tx(root, &tx_key(i), &proof, &envs[i]).unwrap();
        }
        checked += 1;
    }
    assert!(checked >= 95, "only {checked} blocks checked");
    eprintln!("checked {checked} blocks; a block with more than 256 txs seen: {big}");
}
```

- [ ] **Step 7: Run the live test**

Run: `L1PROOF_LIVE=1 cargo test -p davinci-sequencer --test l1proof live_hundred -- --nocapture 2>&1 | tail -5`
Expected: PASS with `checked ≥ 95 blocks`. If public RPC rate-limits, rerun with `L1PROOF_RPC=https://eth.drpc.org`.

- [ ] **Step 8: Clippy and commit**

Run: `cargo clippy -p davinci-sequencer --all-targets -- -D warnings 2>&1 | tail -3` (expected: clean)

```bash
git add sequencer/Cargo.toml sequencer/src/web3/mod.rs sequencer/src/web3/l1proof.rs sequencer/tests/l1proof.rs Cargo.lock
git commit -m "feat(web3): proofs that blob transactions are in Ethereum's canonical chain"
```

---

### Task 2: Fixtures from mainnet (Rust example)

**Files:**
- Create: `sequencer/examples/l1proof_fixtures.rs`
- Create: `/home/p4u/davinci-contracts/test/vectors/l1blob/{recent,cancun,index0,index128plus,big_block,not_blob}.json`
- Create: `sequencer/tests/fixtures/l1proof/recent.json`
- Modify: `sequencer/tests/l1proof.rs` (offline test on the stored block)

**Interfaces:**
- Consumes: Task 1's `build`, `header_rlp`, `tx_trie_proof`, `tx_key`, `verify_local`.
- Produces: JSON fixtures with this exact shape (used by Task 4):

```json
{
  "name": "recent",
  "headers": ["0x…rlp", "…"],
  "header_hashes": ["0x…", "…"],
  "header_numbers": [123, 122],
  "txs": [{ "header_index": 1, "key": "0x80", "proof": ["0x…", "…"] }],
  "tx_type": 3,
  "expected_vhs": ["0x01…"],
  "expected_tx_blocks": [122]
}
```

- [ ] **Step 1: Write the example**

Create `sequencer/examples/l1proof_fixtures.rs`. It takes `--rpc URL` (default publicnode) and `--out-contracts DIR` (default `../davinci-contracts/test/vectors/l1blob`) and `--out-rust DIR` (default `sequencer/tests/fixtures/l1proof`), then:

```rust
//! Writes L1 proof fixtures from Ethereum mainnet for the forge tests of L1BlobVerifier and the
//! offline Rust tests. Run: cargo run -p davinci-sequencer --example l1proof_fixtures -- [--rpc URL]

use alloy::eips::eip2718::Encodable2718;
use alloy::primitives::{keccak256, Bytes, B256};
use alloy::providers::{Provider, ProviderBuilder};
use davinci_sequencer::web3::l1proof::{build, header_rlp, tx_key, tx_trie_proof, verify_local, L1BlobProof};
use serde_json::json;

/// A blob tx (type 3) in `range`, scanning upward: (block, index, hash).
async fn find_blob_tx<P: Provider>(l1: &P, from: u64, to: u64, want_index: impl Fn(usize) -> bool) -> (u64, usize, B256) {
    for n in from..to {
        let b = l1.get_block_by_number(n.into()).full().await.unwrap().unwrap();
        for (i, t) in b.transactions.txns().enumerate() {
            if t.inner.ty() == 3 && want_index(i) {
                return (n, i, *t.inner.tx_hash());
            }
        }
    }
    panic!("no blob tx found in {from}..{to}");
}

fn fixture(name: &str, p: &L1BlobProof, numbers: &[u64], tx_type: u8, vhs: &[B256], blocks: &[u64]) -> serde_json::Value {
    json!({
        "name": name,
        "headers": p.headers,
        "header_hashes": p.headers.iter().map(|h| keccak256(h)).collect::<Vec<_>>(),
        "header_numbers": numbers,
        "txs": p.txs.iter().map(|t| json!({"header_index": t.header_index, "key": t.key, "proof": t.proof})).collect::<Vec<_>>(),
        "tx_type": tx_type,
        "expected_vhs": vhs,
        "expected_tx_blocks": blocks,
    })
}
```

and in `main` produces:
1. `recent`: `find_blob_tx(head-600, head-1, |_| true)`; `build(&l1, &[hash], n+3)`; `verify_local(&p, keccak(p.headers[0]))` → `expected_vhs`, blocks; header numbers `n+3..=n` descending.
2. `cancun`: same with `find_blob_tx(20_000_000, 20_001_000, |_| true)`, top = n+2.
3. `index128plus`: `find_blob_tx(head-20_000, head-1, |i| i >= 128)` (blob txs often sit late in a block); top = n.
4. `index0` and `big_block` and `not_blob`: built by hand from `tx_trie_proof` because they need not be blob txs: `index0` uses tx index 0 of the `recent` blob block (type of that tx goes in `tx_type`, expected_vhs empty unless it is type 3); `big_block` scans `head-5000..head` for a block with more than 256 txs and proves index 257; `not_blob` proves the first non-type-3 tx of the `recent` block. For these, `headers = [header_rlp(block)]`, `txs = [{header_index: 0, key: tx_key(i), proof}]`.
5. `sequencer/tests/fixtures/l1proof/recent.json`: `{ "number", "hash", "header_rlp", "transactions_root", "envelopes": [...] }` of the `recent` blob block.

Each fixture is checked before writing: `verify_local` succeeds for blob fixtures, `verify_tx` for the others; print one summary line per fixture (name, block, index, header count, proof nodes).

- [ ] **Step 2: Run the example**

Run: `cd /home/p4u/davinci-sequencer && cargo run -p davinci-sequencer --example l1proof_fixtures 2>&1 | tail -8`
Expected: six fixture lines and seven files written.

- [ ] **Step 3: Offline test on the stored block**

Append to `sequencer/tests/l1proof.rs`:

```rust
#[test]
fn stored_block_root_and_proofs() {
    let v: serde_json::Value = serde_json::from_str(include_str!("fixtures/l1proof/recent.json")).unwrap();
    let envs: Vec<Bytes> = v["envelopes"].as_array().unwrap().iter().map(|e| e.as_str().unwrap().parse().unwrap()).collect();
    let want: alloy::primitives::B256 = v["transactions_root"].as_str().unwrap().parse().unwrap();
    let hdr: Bytes = v["header_rlp"].as_str().unwrap().parse().unwrap();
    assert_eq!(keccak256(&hdr).to_string(), v["hash"].as_str().unwrap());
    for i in [0, envs.len() / 2, envs.len() - 1] {
        let (root, proof) = tx_trie_proof(&envs, i);
        assert_eq!(root, want);
        verify_tx(root, &tx_key(i), &proof, &envs[i]).unwrap();
    }
}
```

Run: `cargo test -p davinci-sequencer --test l1proof stored 2>&1 | tail -3` → PASS.

- [ ] **Step 4: Commit (both repositories)**

```bash
cd /home/p4u/davinci-contracts && git checkout -b feat/l1-blob-da && git add test/vectors/l1blob && git commit -m "test(l1blob): mainnet fixtures for L1 blob inclusion proofs"
cd /home/p4u/davinci-sequencer && git add sequencer/examples/l1proof_fixtures.rs sequencer/tests/l1proof.rs sequencer/tests/fixtures/l1proof && git commit -m "test(web3): mainnet fixtures for the L1 proof builder"
```

---

### Task 3: Vendored Optimism libraries (davinci-contracts)

**Files:**
- Create: `src/libraries/op/RLPReader.sol`, `src/libraries/op/RLPErrors.sol`, `src/libraries/op/Bytes.sol`, `src/libraries/op/MerkleTrie.sol`
- Create: `src/libraries/op/README.md`

**Interfaces:**
- Produces: `RLPReader.toRLPItem(bytes)`, `RLPReader.readList(RLPItem|bytes) -> RLPItem[]`, `RLPReader.readBytes(RLPItem) -> bytes`, `MerkleTrie.get(bytes key, bytes[] proof, bytes32 root) -> bytes` (reverts on a bad proof), `Bytes.slice`.

- [ ] **Step 1: Fetch the four files at a pinned commit**

```bash
cd /home/p4u/davinci-contracts && mkdir -p src/libraries/op
C=$(git ls-remote https://github.com/ethereum-optimism/optimism refs/heads/develop | cut -c1-40)
for f in rlp/RLPReader.sol rlp/RLPErrors.sol trie/MerkleTrie.sol Bytes.sol; do
  curl -sf -o src/libraries/op/$(basename $f) "https://raw.githubusercontent.com/ethereum-optimism/optimism/$C/packages/contracts-bedrock/src/libraries/$f"
done
echo "$C"
```

- [ ] **Step 2: Fix imports to the vendored layout**

In `RLPReader.sol`, `MerkleTrie.sol`: replace `src/libraries/rlp/RLPErrors.sol`, `src/libraries/rlp/RLPReader.sol` and `src/libraries/Bytes.sol` with `./RLPErrors.sol`, `./RLPReader.sol`, `./Bytes.sol`. Keep the MIT SPDX headers. Add at the top of each file, under the SPDX line: `// Vendored from ethereum-optimism/optimism packages/contracts-bedrock at commit <C>; unmodified except for import paths.`

- [ ] **Step 3: Write `src/libraries/op/README.md`**

```markdown
# Vendored Optimism libraries

`RLPReader`, `RLPErrors`, `Bytes` and `MerkleTrie` from ethereum-optimism/optimism
(`packages/contracts-bedrock/src/libraries`), commit <C>, MIT licensed. Only the import paths
changed. Used by `L1BlobVerifier` to read L1 headers and transaction-trie proofs.
```

- [ ] **Step 4: Build**

Run: `~/.foundry/bin/forge build 2>&1 | tail -3`
Expected: compiles (pragmas `^0.8.0`/`^0.8.8` accept 0.8.28).

- [ ] **Step 5: Commit**

```bash
git add src/libraries/op && git commit -m "chore(libraries): vendor Optimism RLPReader and MerkleTrie"
```

---

### Task 4: `L1BlobVerifier` (davinci-contracts)

**Files:**
- Create: `src/interfaces/IL1Block.sol`, `src/interfaces/IL1BlobVerifier.sol`, `src/L1BlobVerifier.sol`
- Create: `test/mocks/MockL1Block.sol`, `test/L1BlobVerifier.t.sol`

**Interfaces:**
- Consumes: Task 3 libraries; Task 2 fixtures.
- Produces (used by Task 6 and Plan 2):

```solidity
// src/interfaces/IL1BlobVerifier.sol
interface IL1BlobVerifier {
    struct L1TxProof { uint256 headerIndex; bytes key; bytes[] proof; }
    struct L1BlobProof { bytes[] headers; L1TxProof[] txs; }
    error L1OriginMismatch();
    error BrokenHeaderChain(uint256 index);
    error BadHeader(uint256 index);
    error TxAboveOrigin(uint256 tx);
    error NotBlobTx(uint256 tx);
    error BadVersionedHash(uint256 tx);
    function l1Block() external view returns (address);
    function verify(L1BlobProof calldata p) external view returns (bytes32[] memory vhs, uint256[] memory txBlocks);
}
// src/interfaces/IL1Block.sol
interface IL1Block { function hash() external view returns (bytes32); function number() external view returns (uint64); }
```

- [ ] **Step 1: Write the mock and the failing tests**

`test/mocks/MockL1Block.sol`:

```solidity
// SPDX-License-Identifier: AGPL-3.0-only
pragma solidity ^0.8.28;

import {IL1Block} from "../../src/interfaces/IL1Block.sol";

/// Stands in for OP's L1Block predeploy: the test or the e2e harness sets the origin.
contract MockL1Block is IL1Block {
    bytes32 public hash;
    uint64 public number;

    function set(uint64 n, bytes32 h) external {
        number = n;
        hash = h;
    }
}
```

`test/L1BlobVerifier.t.sol` (forge-std `Test`, `stdJson`): a helper loads a fixture into `IL1BlobVerifier.L1BlobProof` (`headers` from `.headers`, each tx from `.txs[i]`), and a harness `L1BlobVerifierHarness is L1BlobVerifier` exposes `txAt(bytes header, bytes key, bytes[] proof) external pure returns (bytes memory)` (decodes `transactionsRoot` and returns `MerkleTrie.get`). Tests:

```solidity
function test_Recent_AnchorAtTop() public { /* origin = header_hashes[0]; verify == expected_vhs, expected_tx_blocks */ }
function test_Recent_AnchorInMiddle() public { /* origin = header_hashes[1] */ }
function test_Recent_AnchorAtBlobBlock() public { /* origin = header_hashes[last]; the tx sits at the last header */ }
function test_Cancun_DecodesShorterHeader() public { /* cancun fixture, anchor at top */ }
function test_Index128Plus() public { /* index128plus fixture */ }
function test_KeyIndex0() public { /* harness.txAt on index0 fixture returns an envelope whose keccak is a tx hash in that block: assert its first byte == tx_type when tx_type > 0 */ }
function test_BigBlockIndex257() public { /* harness.txAt on big_block fixture, key 0x820101 */ }
function test_RevertWhen_NoAnchor() public { /* origin = keccak("x") -> L1OriginMismatch */ }
function test_RevertWhen_BrokenChain() public { /* swap headers[1] and headers[2] -> BrokenHeaderChain(1) */ }
function test_RevertWhen_TxAboveOrigin() public { /* origin = last header, set txs[0].headerIndex = 0 -> TxAboveOrigin(0) */ }
function test_RevertWhen_NotBlobTx() public { /* not_blob fixture, origin = its header -> NotBlobTx(0) */ }
function test_RevertWhen_BadProof() public { /* flip one byte in txs[0].proof[0] -> reverts */ }
function test_RevertWhen_ShortHeader() public { /* headers[0] = hex"c30102" (list of 3), origin = keccak of it -> BadHeader(0) */ }
function test_RevertWhen_EmptyHeaders() public { /* no headers -> L1OriginMismatch */ }
```

Write each body in full (load fixture, `l1Block.set(...)`, `vm.expectRevert(abi.encodeWithSelector(IL1BlobVerifier.X.selector, arg))` where the error has an argument). Add `fs_permissions = [{ access = "read", path = "./test/vectors" }]` to `foundry.toml` if not present.

- [ ] **Step 2: Run to verify failure**

Run: `~/.foundry/bin/forge test --match-path test/L1BlobVerifier.t.sol 2>&1 | tail -5`
Expected: FAIL to compile (`L1BlobVerifier` missing).

- [ ] **Step 3: Implement**

`src/L1BlobVerifier.sol`:

```solidity
// SPDX-License-Identifier: AGPL-3.0-only
pragma solidity ^0.8.28;

import {IL1Block} from "./interfaces/IL1Block.sol";
import {IL1BlobVerifier} from "./interfaces/IL1BlobVerifier.sol";
import {RLPReader} from "./libraries/op/RLPReader.sol";
import {MerkleTrie} from "./libraries/op/MerkleTrie.sol";
import {Bytes} from "./libraries/op/Bytes.sol";

/// @title L1BlobVerifier
/// @notice Proves on an OP Stack chain that blob transactions are in Ethereum's canonical chain.
///         Headers come newest first; the chain is anchored at any header whose hash is OP's
///         current L1 origin, and transactions may only sit in that header or older ones.
///         Stateless and ownerless.
contract L1BlobVerifier is IL1BlobVerifier {
    /// The L1Block predeploy (0x4200…0015 on OP Stack chains).
    address public immutable override l1Block;

    constructor(address _l1Block) {
        require(_l1Block != address(0), "L1BlobVerifier: zero L1Block");
        l1Block = _l1Block;
    }

    /// @inheritdoc IL1BlobVerifier
    function verify(L1BlobProof calldata p)
        external
        view
        override
        returns (bytes32[] memory vhs, uint256[] memory txBlocks)
    {
        uint256 nh = p.headers.length;
        bytes32 origin = IL1Block(l1Block).hash();
        bytes32[] memory txRoots = new bytes32[](nh);
        uint256[] memory numbers = new uint256[](nh);
        uint256 anchor = type(uint256).max;
        bytes32 expected; // parentHash of the previous header
        for (uint256 i = 0; i < nh; ++i) {
            bytes memory h = p.headers[i];
            bytes32 hash = keccak256(h);
            if (i > 0 && hash != expected) revert BrokenHeaderChain(i);
            if (anchor == type(uint256).max && hash == origin) anchor = i;
            RLPReader.RLPItem[] memory f = RLPReader.readList(h);
            if (f.length < 9) revert BadHeader(i);
            expected = _bytes32(RLPReader.readBytes(f[0]), i);
            txRoots[i] = _bytes32(RLPReader.readBytes(f[4]), i);
            numbers[i] = _uint(RLPReader.readBytes(f[8]));
        }
        if (anchor == type(uint256).max) revert L1OriginMismatch();

        uint256 nt = p.txs.length;
        bytes32[][] memory perTx = new bytes32[][](nt);
        txBlocks = new uint256[](nt);
        uint256 total;
        for (uint256 t = 0; t < nt; ++t) {
            L1TxProof calldata tx_ = p.txs[t];
            if (tx_.headerIndex < anchor || tx_.headerIndex >= nh) revert TxAboveOrigin(t);
            bytes memory env = MerkleTrie.get(tx_.key, tx_.proof, txRoots[tx_.headerIndex]);
            perTx[t] = _blobHashes(env, t);
            txBlocks[t] = numbers[tx_.headerIndex];
            total += perTx[t].length;
        }
        vhs = new bytes32[](total);
        uint256 k;
        for (uint256 t = 0; t < nt; ++t) {
            for (uint256 j = 0; j < perTx[t].length; ++j) vhs[k++] = perTx[t][j];
        }
    }

    /// Type-3 envelope -> blob_versioned_hashes (list item 10), non-empty, each 0x01-prefixed.
    function _blobHashes(bytes memory env, uint256 t) private pure returns (bytes32[] memory out) {
        if (env.length < 2 || env[0] != 0x03) revert NotBlobTx(t);
        RLPReader.RLPItem[] memory f = RLPReader.readList(Bytes.slice(env, 1));
        if (f.length < 11) revert NotBlobTx(t);
        RLPReader.RLPItem[] memory hs = RLPReader.readList(f[10]);
        if (hs.length == 0) revert NotBlobTx(t);
        out = new bytes32[](hs.length);
        for (uint256 j = 0; j < hs.length; ++j) {
            bytes memory b = RLPReader.readBytes(hs[j]);
            if (b.length != 32 || b[0] != 0x01) revert BadVersionedHash(t);
            out[j] = bytes32(b);
        }
    }

    function _bytes32(bytes memory b, uint256 i) private pure returns (bytes32) {
        if (b.length != 32) revert BadHeader(i);
        return bytes32(b);
    }

    function _uint(bytes memory b) private pure returns (uint256 v) {
        for (uint256 j = 0; j < b.length; ++j) v = (v << 8) | uint8(b[j]);
    }
}
```

- [ ] **Step 4: Run the tests**

Run: `~/.foundry/bin/forge test --match-path test/L1BlobVerifier.t.sol -vv 2>&1 | tail -20`
Expected: all PASS. Then `~/.foundry/bin/forge test --match-path test/L1BlobVerifier.t.sol --gas-report 2>&1 | grep -A8 'L1BlobVerifier'` and record the `verify` gas in the commit message body.

- [ ] **Step 5: Commit**

```bash
git add src/L1BlobVerifier.sol src/interfaces/IL1BlobVerifier.sol src/interfaces/IL1Block.sol test/mocks/MockL1Block.sol test/L1BlobVerifier.t.sol foundry.toml
git commit -m "feat(l1blob): stateless verifier of L1 blob transaction inclusion"
```

---

### Task 5: Registry refactor and the constructor argument (pure refactor)

**Files:**
- Modify: `src/ProcessRegistry.sol` (constructor ~L153, immutables ~L85-133, `submitStateTransition` L475-522, `_checkBlobsDigest` L910-935, `_verifyOpenings` L939-959)
- Modify: `src/interfaces/IProcessRegistry.sol` (errors, `l1BlobVerifier()`)
- Modify: `test/RegistryTestBase.sol` L62-76, `test/Genesis.t.sol` ~L43, `test/Grace.t.sol` ~L59, `test/NewProcess.t.sol` ~L220, `script/DeployAll.s.sol`

**Interfaces:**
- Produces: `IL1BlobVerifier public immutable l1BlobVerifier;` and constructor parameter `address _l1BlobVerifier` appended after `_noticeMin`; error `WrongDAMode()`; private helpers used by Task 6:

```solidity
function _prepareTransition(bytes31 processId, bytes calldata publicValues)
    private view returns (DAVINCITypes.Process storage p, bytes32 rootBefore, uint256 votersCount, uint256 newVoters, uint256 overwrites);
function _checkBlobArrays(bytes calldata publicValues, bytes[] calldata commitments, bytes32[] calldata ys, bytes[] calldata kzgProofs) private pure returns (uint256 n);
function _checkDigest(bytes calldata publicValues, bytes[] calldata commitments, bytes32[] calldata ys) private pure;
function _verifyOpenings(bytes31 processId, bytes32 rootBefore, bytes[] calldata commitments, bytes32[] calldata ys, bytes[] calldata kzgProofs, bytes32[] memory vhs) private view;
function _applyTransition(DAVINCITypes.Process storage p, bytes31 processId, bytes calldata publicValues, bytes32 rootBefore, uint256 votersCount, uint256 newVoters, uint256 overwrites, uint256 nBlobs) private;
```

- [ ] **Step 1: Run the full suite as the baseline**

Run: `cd /home/p4u/davinci-contracts && ~/.foundry/bin/forge test 2>&1 | tail -3` → note the pass count.

- [ ] **Step 2: Refactor**

Split `submitStateTransition` exactly along the existing lines: L483-504 into `_prepareTransition`; L512-521 into `_applyTransition`; `_checkBlobsDigest` into `_checkBlobArrays` (`n` from `REG_N_BLOBS`, `NoBlobs`, the three length checks, `InvalidBlobCommitmentLength`, `InvalidKZGProofLength`) plus `_checkDigest` (pairs and `InvalidBlobsDigest`). The blob-mode entry point becomes, preserving the original check order:

```solidity
function submitStateTransition(
    bytes31 processId,
    bytes calldata publicValues,
    bytes calldata proofBytes,
    bytes[] calldata commitments,
    bytes32[] calldata ys,
    bytes[] calldata kzgProofs
) external override nonReentrant {
    if (address(l1BlobVerifier) != address(0)) revert WrongDAMode();
    (DAVINCITypes.Process storage p, bytes32 rootBefore, uint256 votersCount, uint256 newVoters, uint256 overwrites) =
        _prepareTransition(processId, publicValues);
    uint256 nBlobs = _checkBlobArrays(publicValues, commitments, ys, kzgProofs);
    // No blob may follow the n the guest committed to: nodes decode every blob of the tx.
    if (BlobsLib.blobHash(nBlobs) != bytes32(0)) revert BlobCountMismatch();
    _checkDigest(publicValues, commitments, ys);
    ziskVerifier.verifySnarkProof(batchProgramVK, rootCVadcopFinal, publicValues, proofBytes);
    bytes32[] memory vhs = new bytes32[](nBlobs);
    for (uint256 i = 0; i < nBlobs; ++i) {
        vhs[i] = BlobsLib.blobHash(i);
        if (vhs[i] == bytes32(0)) revert MissingBlob(i);
    }
    _verifyOpenings(processId, rootBefore, commitments, ys, kzgProofs, vhs);
    _applyTransition(p, processId, publicValues, rootBefore, votersCount, newVoters, overwrites, nBlobs);
}
```

`_verifyOpenings` takes `vhs` instead of reading `BlobsLib.blobHash(i)`. Add the immutable, the constructor parameter (`l1BlobVerifier = IL1BlobVerifier(_l1BlobVerifier);`, zero allowed), `WrongDAMode` in `IProcessRegistry`, and `function l1BlobVerifier() external view returns (IL1BlobVerifier);` there. Append `address(0)` to the four test constructor calls and pass `cfg.l1BlobVerifier` (default zero, from env `L1_BLOB_VERIFIER`) in `DeployAll.s.sol`.

- [ ] **Step 3: Run the full suite**

Run: `~/.foundry/bin/forge test 2>&1 | tail -3`
Expected: the same pass count as Step 1, nothing failing (pure refactor). Then `~/.foundry/bin/forge build --sizes 2>&1 | grep ProcessRegistry` and note the runtime size.

- [ ] **Step 4: Commit**

```bash
git add src test script && git commit -m "refactor(registry): split settlement checks and add the L1 blob verifier argument"
```

---

### Task 6: Registry L1-blob entry point (davinci-contracts)

**Files:**
- Modify: `src/ProcessRegistry.sol`, `src/interfaces/IProcessRegistry.sol`
- Create: `test/mocks/MockL1BlobVerifier.sol`, `test/TransitionL1.t.sol`

**Interfaces:**
- Consumes: Task 5 helpers, Task 4 `IL1BlobVerifier`.
- Produces (ABI used by Plan 2):

```solidity
event ProcessStateTransitionedL1(bytes31 indexed processId, uint256[] l1BlockNumbers, bytes32[] versionedHashes);
function submitStateTransitionL1(
    bytes31 processId,
    bytes calldata publicValues,
    bytes calldata proofBytes,
    bytes[] calldata commitments,
    bytes32[] calldata ys,
    bytes[] calldata kzgProofs,
    IL1BlobVerifier.L1BlobProof calldata l1
) external;
```

- [ ] **Step 1: Write the mock and failing tests**

`test/mocks/MockL1BlobVerifier.sol`: implements `IL1BlobVerifier`, stores `bytes32[] vhs`, `uint256[] blocks`, `bool fail`; `set(...)`; `verify` returns them or reverts `L1OriginMismatch()` when `fail`.

`test/TransitionL1.t.sol`: a variant of `RegistryTestBase` whose `setUp` deploys the mock from `address(0xBEEF)` (so the registry stays at `CREATE(DEPLOYER, 0)`), then the registry with the mock's address. The fixture transition from `test/vectors/transition.json` (as used by `Transition.t.sol`) provides commitments, ys, KZG proofs and publics; the expected versioned hashes are `0x01 ‖ sha256(commitment)[1:]` computed in the test. Tests:

```solidity
function test_SubmitL1_OneBlob() public           // state updated as in Transition.t.sol test_OneBlob; emits ProcessStateTransitioned and ProcessStateTransitionedL1(pid, [blockNo], vhs)
function test_RevertWhen_L1_WrongMode() public    // blob-mode registry (RegistryTestBase) calling submitStateTransitionL1 -> WrongDAMode
function test_RevertWhen_Blob_WrongMode() public  // L1-mode registry calling submitStateTransition -> WrongDAMode
function test_RevertWhen_L1_CountMismatch() public // mock returns n+1 hashes -> BlobCountMismatch
function test_RevertWhen_L1_WrongHash() public    // mock returns a different 0x01 hash -> InvalidBlobOpening(0)
function test_RevertWhen_L1_Digest() public       // a flipped y -> InvalidBlobsDigest
function test_RevertWhen_L1_VerifierFails() public // mock fail=true -> L1OriginMismatch bubbles up
function test_RevertWhen_L1_StaleRoot() public    // second submit of the same transition -> InvalidStateRoot
```

- [ ] **Step 2: Run to verify failure**

Run: `~/.foundry/bin/forge test --match-path test/TransitionL1.t.sol 2>&1 | tail -5` → FAIL to compile.

- [ ] **Step 3: Implement the entry point**

```solidity
/// @inheritdoc IProcessRegistry
/// @dev L1-blob mode: the blobs live in Ethereum blob transactions proven through `l1`.
function submitStateTransitionL1(
    bytes31 processId,
    bytes calldata publicValues,
    bytes calldata proofBytes,
    bytes[] calldata commitments,
    bytes32[] calldata ys,
    bytes[] calldata kzgProofs,
    IL1BlobVerifier.L1BlobProof calldata l1
) external override nonReentrant {
    if (address(l1BlobVerifier) == address(0)) revert WrongDAMode();
    (DAVINCITypes.Process storage p, bytes32 rootBefore, uint256 votersCount, uint256 newVoters, uint256 overwrites) =
        _prepareTransition(processId, publicValues);
    uint256 nBlobs = _checkBlobArrays(publicValues, commitments, ys, kzgProofs);
    _checkDigest(publicValues, commitments, ys);
    (bytes32[] memory vhs, uint256[] memory l1Blocks) = l1BlobVerifier.verify(l1);
    if (vhs.length != nBlobs) revert BlobCountMismatch();
    ziskVerifier.verifySnarkProof(batchProgramVK, rootCVadcopFinal, publicValues, proofBytes);
    _verifyOpenings(processId, rootBefore, commitments, ys, kzgProofs, vhs);
    _applyTransition(p, processId, publicValues, rootBefore, votersCount, newVoters, overwrites, nBlobs);
    emit ProcessStateTransitionedL1(processId, l1Blocks, vhs);
}
```

Declare the function and event in `IProcessRegistry` with NatSpec.

- [ ] **Step 4: Run all tests and check the size**

Run: `~/.foundry/bin/forge test 2>&1 | tail -3` → all PASS.
Run: `~/.foundry/bin/forge build --sizes 2>&1 | grep -E 'ProcessRegistry|L1BlobVerifier'` → `ProcessRegistry` runtime below 24,576 bytes. If it is over, move `_verifyOpenings` into a new `library BlobOpenings` with an `external` function (linked library, deployed by `DeployAll.s.sol`) and rerun.

- [ ] **Step 5: Commit**

```bash
git add src test && git commit -m "feat(registry): settle transitions whose blobs are on Ethereum L1"
```

---

### Task 7: Deploy scripts, bindings, docs and the sequencer's copies

**Files:**
- Modify: `script/DeployAll.s.sol`, `script/verify_deployment.py`, `deploy_all.sh`, `.env.example` (davinci-contracts), `docs/settlement.md`, `docs/errors.md`, `docs/deployment.md`; regenerate `golang-types/`.
- Modify (davinci-sequencer): `sequencer/abi/ProcessRegistry.json`, `e2e/src/chain.rs` (constructor encoding ~L220-238), `sequencer/tests/web3_anvil.rs` (`ctor_tail` ~L41), `client/tests/organizer.rs` (deploy ~L60-102), any other `GRACE_ARGS` encoder found by `grep -rn GRACE_ARGS`.

- [ ] **Step 1: Deploy script**

In `DeployAll.s.sol`: `Config` gains `address l1BlobVerifier` and `address l1Block`. If env `L1_BLOB_VERIFIER` is set use it; else if `L1_BLOCK` is set, deploy `new L1BlobVerifier(L1_BLOCK)` before the registry and use its address; else zero. Log both. `verify_deployment.py`: read `l1BlobVerifier()`; if non-zero read its `l1Block()` and compare with `--l1-block` (new optional argument, default `0x4200000000000000000000000000000000000015`). `deploy_all.sh` passes `L1_BLOB_VERIFIER`/`L1_BLOCK` through and the new argument to the verifier. Document both variables in `.env.example` and `docs/deployment.md`.

- [ ] **Step 2: Local dry run of the deploy**

Run an anvil (`~/.foundry/bin/anvil --hardfork prague --port 8551 &`), then `L1_BLOCK=<MockL1Block deployed address> … forge script script/DeployAll.s.sol --rpc-url http://127.0.0.1:8551 --broadcast` with the anvil dev key in `PRIVATE_KEY`; confirm `l1BlobVerifier()` is non-zero and `verify_deployment.py` passes; stop anvil.

- [ ] **Step 3: Bindings and docs**

Run `./go_bind.sh` if `abigen` is installed (else note it in the commit body). In `docs/settlement.md` add the L1-blob mode section (flow, the `L1BlobProof` layout, the anchoring rule, the event); in `docs/errors.md` add `WrongDAMode`, `L1OriginMismatch`, `BrokenHeaderChain`, `BadHeader`, `TxAboveOrigin`, `NotBlobTx`, `BadVersionedHash`.

- [ ] **Step 4: Sequencer copies**

Regenerate `sequencer/abi/ProcessRegistry.json` with the CONTRIBUTING.md `jq` transform from `../davinci-contracts/out/ProcessRegistry.sol/ProcessRegistry.json`; append a zero address word to every hand-encoded registry constructor in the sequencer repository. Run:
`cargo build --workspace && cargo test --workspace 2>&1 | tail -5` (expected: PASS) and
`ANVIL=1 cargo test -p davinci-sequencer --test web3_anvil 2>&1 | tail -5` (expected: PASS; needs `forge build` in davinci-contracts).

- [ ] **Step 5: Commit both repositories**

```bash
cd /home/p4u/davinci-contracts && git add -A script docs golang-types deploy_all.sh .env.example && git commit -m "feat(deploy): deploy and verify registries in L1-blob mode"
cd /home/p4u/davinci-sequencer && git add sequencer/abi e2e client sequencer/tests && git commit -m "chore(abi): registry ABI with the L1-blob mode"
```

---

### Task 8: Live check on OP Mainnet

**Files:**
- Create: `sequencer/examples/l1proof_check.rs`
- Create: `docs/superpowers/reports/2026-10-12-l1blob-verifier-op-check.md`

**Interfaces:**
- Consumes: Task 1 `build`, `verify_local`; Task 4 artifact `../davinci-contracts/out/L1BlobVerifier.sol/L1BlobVerifier.json`.

- [ ] **Step 1: Write the example**

`l1proof_check` subcommands, reading `--op-rpc-file` (first line) and `--key-file` (0x hex), never printing either:
- `deploy`: deploy `L1BlobVerifier(0x4200000000000000000000000000000000000015)` on OP from the artifact's creation bytecode (EIP-1559, tip 10,000 wei, `max_fee = 2·base + tip`), print the address and cost.
- `check --verifier ADDR [--txs N]`: find the latest N (default 3) type-3 transactions on Ethereum (scan back from head), read OP's `L1Block.number()`, `build(&l1, &[tx], head)` with `head` = the current Ethereum head, `verify_local` against OP's `L1Block.hash()`, then `eth_call` `verify` on OP and require the returned hashes and blocks to match; print `eth_estimateGas` of the call, the number of headers and the calldata size. Repeat each check twice, 15 s apart, to show the proof survives an origin advance.

- [ ] **Step 2: Run it**

```bash
cargo run -p davinci-sequencer --example l1proof_check -- --op-rpc-file ~/optimism-rpcs.txt --key-file ~/optimism-privkey.txt deploy
cargo run -p davinci-sequencer --example l1proof_check -- --op-rpc-file ~/optimism-rpcs.txt --key-file ~/optimism-privkey.txt check --verifier <ADDR>
```
Expected: every check returns the expected versioned hashes on OP Mainnet, including the repeat after the origin advanced.

- [ ] **Step 3: Write the short report and commit**

Report: verifier address, three transactions checked, gas of `verify`, header count and calldata size, the origin-advance repeat. Then:

```bash
git add sequencer/examples/l1proof_check.rs docs/superpowers/reports/2026-10-12-l1blob-verifier-op-check.md
git commit -m "test(web3): L1 blob proofs checked against OP Mainnet's L1Block"
```

---

## Gate after Plan 1

Fable 5.1 reviews the diff of Tasks 1-8 (both repositories) before Plan 2 starts. Plan 2 covers the node (configuration, L1 blob sender with write-ahead, actor flow, sync, presets), the dual-anvil e2e harness, the OP Mainnet deployment, the live e2e and the benchmarks.
