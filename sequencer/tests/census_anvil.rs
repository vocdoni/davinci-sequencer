//! The on-chain census index against a real `OwnedCensus` on anvil. Gated by
//! `ANVIL=1`; see `common/anvil.rs` for what it needs.

mod common;

use alloy::primitives::U256;
use alloy::providers::Provider;
use common::anvil::{CensusChain, RpcProxy, enabled};
use davinci_sequencer::census::CensusError;
use davinci_sequencer::census::onchain::OnchainIndex;
use davinci_sequencer::storage::Db;
use davinci_zkvm_sdk::census::verify_census_proof;
use davinci_zkvm_sdk::crypto::field::{Fr, fr_to_be};
use std::sync::atomic::Ordering::Relaxed;
use std::time::Duration;

use tempfile::TempDir;

fn member(i: u32) -> ([u8; 20], u128) {
    let mut a = [0x5a; 20];
    a[16..].copy_from_slice(&i.to_be_bytes());
    let w = if i == 3 {
        (1 << 88) - 1
    } else {
        u128::from(i) + 1
    };
    (a, w)
}

fn members(r: std::ops::Range<u32>) -> Vec<([u8; 20], u128)> {
    r.map(member).collect()
}

async fn assert_matches(ix: &OnchainIndex, ch: &CensusChain, c: &[u8; 20], n: u64) -> Fr {
    let (root, size, block) = ix.latest(c).expect("synced");
    assert_eq!((fr_to_be(&root), size), ch.state().await);
    assert_eq!(size, n);
    assert_eq!(block, ch.head().await);
    root
}

async fn assert_proof(ix: &OnchainIndex, c: &[u8; 20], root: &Fr, i: u32) {
    let (a, w) = member(i);
    let (p, weight) = ix.proof(c, root, &a).await.unwrap().expect("member");
    assert_eq!(weight, w);
    assert_eq!(p.root, *root);
    assert!(verify_census_proof(&p), "proof of member {i}");
}

#[tokio::test]
async fn onchain_index_follows_the_contract() {
    if !enabled() {
        return;
    }
    let ch = CensusChain::start().await;
    let c = ch.census.into_array();
    let chain_id = ch.reader.get_chain_id().await.unwrap();

    // 1, 2, 4, 8, 16 members, one block each.
    let mut n = 0;
    for batch in [1, 1, 2, 4, 8] {
        ch.add(&members(n..n + batch)).await;
        n += batch;
    }

    let dir = TempDir::new().unwrap();
    let db = Db::open_in(dir.path()).unwrap();
    let ix =
        OnchainIndex::new(&db, chain_id, ch.reader.clone(), Duration::from_millis(50)).unwrap();
    ix.register(c).await.unwrap();
    assert!(ix.latest(&c).is_none());
    ix.sync(c, ch.head().await).await.unwrap();
    let early = assert_matches(&ix, &ch, &c, 16).await;
    for i in 0..16 {
        assert_proof(&ix, &c, &early, i).await;
    }
    assert!(ix.proof(&c, &early, &member(99).0).await.unwrap().is_none());

    // Incremental: three more blocks.
    ch.add(&members(16..19)).await;
    ch.add(&members(19..20)).await;
    ch.add(&members(20..21)).await;
    ix.sync(c, ch.head().await).await.unwrap();
    let root = assert_matches(&ix, &ch, &c, 21).await;
    assert_proof(&ix, &c, &root, 20).await;
    // The earlier root still serves the members it had, and only those.
    assert_proof(&ix, &c, &early, 2).await;
    assert!(ix.proof(&c, &early, &member(17).0).await.unwrap().is_none());

    // Restart from redb: same state without touching the chain.
    drop(ix);
    let ix =
        OnchainIndex::new(&db, chain_id, ch.reader.clone(), Duration::from_millis(50)).unwrap();
    ix.register(c).await.unwrap();
    assert_eq!(assert_matches(&ix, &ch, &c, 21).await, root);
    assert_proof(&ix, &c, &early, 5).await;

    // A stored hash that no longer matches the chain forces a rescan.
    ix.tamper_scanned_hash(&c).unwrap();
    ix.sync(c, ch.head().await).await.unwrap();
    assert_eq!(assert_matches(&ix, &ch, &c, 21).await, root);

    // A real reorg: the indexed blocks are replaced by others at the same
    // heights. An index that skipped the hash check would keep the old tree.
    let snap: U256 = ch
        .reader
        .raw_request("evm_snapshot".into(), ())
        .await
        .unwrap();
    ch.add(&members(21..22)).await;
    ch.add(&members(22..23)).await;
    ix.sync(c, ch.head().await).await.unwrap();
    assert_matches(&ix, &ch, &c, 23).await;
    let reverted: bool = ch
        .reader
        .raw_request("evm_revert".into(), (snap,))
        .await
        .unwrap();
    assert!(reverted);
    ch.add(&members(30..32)).await;
    ch.add(&members(32..35)).await;
    ix.sync(c, ch.head().await).await.unwrap();
    let root = assert_matches(&ix, &ch, &c, 26).await;
    assert_proof(&ix, &c, &root, 33).await;
    assert!(ix.proof(&c, &root, &member(21).0).await.unwrap().is_none());
}

async fn mine(ch: &CensusChain, n: u64) {
    let () = ch
        .reader
        .raw_request("anvil_mine".into(), (U256::from(n),))
        .await
        .unwrap();
}

// A provider that caps getLogs at 4 blocks: the backward scan halves down,
// walks the range in chunks and stops at the chunk that completes the tree.
#[tokio::test]
async fn onchain_scan_walks_a_capped_range_in_chunks() {
    if !enabled() {
        return;
    }
    let ch = CensusChain::start().await;
    let c = ch.census.into_array();
    let chain_id = ch.reader.get_chain_id().await.unwrap();
    mine(&ch, 20).await;
    ch.add(&members(0..2)).await;
    let first = ch.head().await;
    mine(&ch, 12).await;
    ch.add(&members(2..4)).await;

    let proxy = RpcProxy::start(ch.anvil.endpoint()).await;
    proxy.knobs.max_range.store(4, Relaxed);
    let dir = TempDir::new().unwrap();
    let db = Db::open_in(dir.path()).unwrap();
    let ix = OnchainIndex::new(&db, chain_id, proxy.provider(), Duration::from_millis(50)).unwrap();
    ix.register(c).await.unwrap();
    ix.sync(c, ch.head().await).await.unwrap();
    let root = assert_matches(&ix, &ch, &c, 4).await;
    assert_proof(&ix, &c, &root, 0).await;
    assert_proof(&ix, &c, &root, 3).await;
    // Four 4-block chunks from the head to the first add, plus the refusals.
    assert!(proxy.knobs.get_logs.load(Relaxed) > 4);
    // The scan stopped once it had every member, not at block 0.
    let lowest = proxy.knobs.min_from.load(Relaxed);
    assert!(lowest > 0 && lowest <= first, "lowest fromBlock {lowest}");

    // Incremental: only the new blocks, ending at the last scanned one.
    mine(&ch, 9).await;
    ch.add(&members(4..5)).await;
    let from = ch.head().await;
    proxy.knobs.min_from.store(u64::MAX, Relaxed);
    ix.sync(c, from).await.unwrap();
    let root = assert_matches(&ix, &ch, &c, 5).await;
    assert_proof(&ix, &c, &root, 4).await;
    assert!(proxy.knobs.min_from.load(Relaxed) > first);
}

// A contract with no members is a valid, empty census: nobody is in it.
#[tokio::test]
async fn onchain_index_serves_an_empty_contract() {
    if !enabled() {
        return;
    }
    let ch = CensusChain::start().await;
    let c = ch.census.into_array();
    let chain_id = ch.reader.get_chain_id().await.unwrap();
    let dir = TempDir::new().unwrap();
    let db = Db::open_in(dir.path()).unwrap();
    let ix =
        OnchainIndex::new(&db, chain_id, ch.reader.clone(), Duration::from_millis(50)).unwrap();
    ix.register(c).await.unwrap();
    ix.sync(c, ch.head().await).await.unwrap();
    let root = assert_matches(&ix, &ch, &c, 0).await;
    assert_eq!(root, Fr::from(0u64));
    assert!(ix.proof(&c, &root, &member(0).0).await.unwrap().is_none());

    // A block past the head is not an error, just not yet.
    let far = ch.head().await + 1000;
    ch.add(&members(0..1)).await;
    let e = ix.sync(c, far).await.unwrap_err();
    assert!(matches!(e, CensusError::Backoff(_)), "{e}");
    assert!(ix.retry_in(&c).is_none());
}
