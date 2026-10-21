//! Origin 3, what one census scan may hold: a hostile contract chooses its
//! own logs, so the index must not keep its whole event history in memory
//! nor walk it when the counts cannot be a census. Gated by `ANVIL=1`.

mod common;

use std::sync::atomic::Ordering::SeqCst;
use std::time::Duration;

use alloy::network::{EthereumWallet, TransactionBuilder};
use alloy::node_bindings::{Anvil, AnvilInstance};
use alloy::primitives::{Address, B256, Bytes, U256};
use alloy::providers::{DynProvider, Provider, ProviderBuilder};
use alloy::rpc::types::TransactionRequest;
use alloy::signers::local::PrivateKeySigner;
use alloy::sol_types::{SolCall, SolEvent};
use common::anvil::{RpcProxy, enabled};
use davinci_sequencer::census::CensusError;
use davinci_sequencer::census::onchain::{OnchainIndex, UsableError};
use davinci_sequencer::storage::Db;
use davinci_zkvm_sdk::census::{LeanImt, census_leaf};
use tempfile::TempDir;

alloy::sol!(
    #[allow(missing_docs)]
    Census,
    "abi/OnchainCensus.json"
);

struct Chain {
    anvil: AnvilInstance,
    owner: DynProvider,
    reader: DynProvider,
}

impl Chain {
    async fn start() -> Self {
        let bin = std::path::PathBuf::from(std::env::var("HOME").unwrap_or_default())
            .join(".foundry/bin/anvil");
        let anvil = if bin.exists() {
            Anvil::at(bin).spawn()
        } else {
            Anvil::new().spawn()
        };
        let signer: PrivateKeySigner = anvil.keys()[0].clone().into();
        let owner = ProviderBuilder::new()
            .with_simple_nonce_management()
            .wallet(EthereumWallet::from(signer))
            .connect_http(anvil.endpoint_url())
            .erased();
        let reader = ProviderBuilder::new()
            .disable_recommended_fillers()
            .connect_http(anvil.endpoint_url())
            .erased();
        Chain {
            anvil,
            owner,
            reader,
        }
    }

    async fn set_code(&self, at: Address, code: Vec<u8>) {
        let () = self
            .reader
            .raw_request("anvil_setCode".into(), (at, Bytes::from(code)))
            .await
            .unwrap();
    }

    async fn set_slot(&self, at: Address, slot: U256, v: U256) {
        let _: serde_json::Value = self
            .reader
            .raw_request(
                "anvil_setStorageAt".into(),
                (at, B256::from(slot), B256::from(v)),
            )
            .await
            .unwrap();
    }

    async fn mine(&self, n: u64) {
        let () = self
            .reader
            .raw_request("anvil_mine".into(), (U256::from(n),))
            .await
            .unwrap();
    }

    async fn head(&self) -> u64 {
        self.reader.get_block_number().await.unwrap()
    }

    async fn send(&self, to: Address, input: Vec<u8>) {
        let tx = TransactionRequest::default()
            .with_to(to)
            .with_input(Bytes::from(input));
        let r = self
            .owner
            .send_transaction(tx)
            .await
            .unwrap()
            .get_receipt()
            .await
            .unwrap();
        assert!(r.status());
    }
}

/// Runtime code answering any 4-byte call with `SLOAD(selector)` and, for
/// calldata of 64 bytes or more, emitting `LOG2(word0, word1, rest)`: a
/// census contract whose state and logs are whatever the test says.
fn puppet() -> Vec<u8> {
    let mut c = vec![
        0x36, 0x60, 0x40, 0x11, 0x60, 0x00, 0x57, // size < 0x40: jump to read
        0x60, 0x40, 0x36, 0x03, 0x60, 0x40, 0x60, 0x00, 0x37, // copy the data words
        0x60, 0x20, 0x35, 0x60, 0x00, 0x35, // topic1, topic0
        0x60, 0x40, 0x36, 0x03, 0x60, 0x00, 0xa2, 0x00, // LOG2, STOP
    ];
    let read = c.len() as u8;
    c[5] = read;
    c.extend_from_slice(&[
        0x5b, 0x60, 0x00, 0x35, 0x60, 0xe0, 0x1c, 0x54, // SLOAD(selector)
        0x60, 0x00, 0x52, 0x60, 0x20, 0x60, 0x00, 0xf3, // RETURN(0, 32)
    ]);
    c
}

fn slot_of(selector: [u8; 4]) -> U256 {
    U256::from(u32::from_be_bytes(selector))
}

fn word(v: U256) -> [u8; 32] {
    v.to_be_bytes()
}

fn user(n: u32) -> [u8; 20] {
    let mut a = [0x77; 20];
    a[16..].copy_from_slice(&n.to_be_bytes());
    a
}

fn topic(a: &[u8; 20]) -> [u8; 32] {
    let mut t = [0u8; 32];
    t[12..].copy_from_slice(a);
    t
}

struct Puppet {
    at: Address,
    tree: LeanImt,
}

impl Puppet {
    async fn deploy(ch: &Chain, at: Address) -> Self {
        ch.set_code(at, puppet()).await;
        Puppet {
            at,
            tree: LeanImt::new(),
        }
    }

    async fn set_state(&self, ch: &Chain, root: U256, size: u64) {
        let root_slot = slot_of(Census::getCensusRootCall::SELECTOR);
        let size_slot = slot_of(Census::treeSizeCall::SELECTOR);
        ch.set_slot(self.at, root_slot, root).await;
        ch.set_slot(self.at, size_slot, U256::from(size)).await;
    }

    async fn weight_changed(&self, ch: &Chain, who: &[u8; 20], prev: u128, new: u128) {
        let mut input = Census::WeightChanged::SIGNATURE_HASH.to_vec();
        input.extend_from_slice(&topic(who));
        input.extend_from_slice(&word(U256::from(prev)));
        input.extend_from_slice(&word(U256::from(new)));
        ch.send(self.at, input).await;
    }

    /// An honest add: the notice, the addition, then the state.
    async fn add(&mut self, ch: &Chain, who: &[u8; 20], weight: u128) {
        let leaf = census_leaf(who, weight).unwrap();
        self.tree.insert(leaf);
        self.weight_changed(ch, who, 0, weight).await;
        let mut input = Census::CensusMemberAdded::SIGNATURE_HASH.to_vec();
        input.extend_from_slice(&topic(who));
        let root = U256::from_be_bytes(self.tree.root());
        for w in [
            U256::from(weight),
            U256::from_be_bytes(leaf),
            root,
            U256::ZERO,
        ] {
            input.extend_from_slice(&word(w));
        }
        ch.send(self.at, input).await;
        self.set_state(ch, root, self.tree.len() as u64).await;
    }
}

fn index(db: &Db, provider: DynProvider) -> OnchainIndex {
    OnchainIndex::new(
        db,
        31337,
        provider,
        Duration::from_millis(50),
        Duration::ZERO,
        0,
    )
    .unwrap()
}

// A contract that claims one member and logs a long history of zero-previous
// weight notices (which an honest census emits only next to an addition):
// the first scan walks back to genesis, page after page, keeping every one
// of them, and then does it all again. The second notice already shows the
// logs cannot be a one-member census.
#[tokio::test]
async fn weight_notices_without_members_are_refused_early() {
    if !enabled() {
        return;
    }
    let ch = Chain::start().await;
    let p = Puppet::deploy(&ch, Address::from([0xe1; 20])).await;
    p.set_state(&ch, U256::from(1), 1).await;
    // One notice per 5000-block page (the widest getLogs range).
    let notices = 120u32;
    for i in 0..notices {
        p.weight_changed(&ch, &user(i), 0, 1).await;
        ch.mine(5_000).await;
    }
    let proxy = RpcProxy::start(ch.anvil.endpoint()).await;
    let dir = TempDir::new().unwrap();
    let db = Db::open_in(dir.path()).unwrap();
    let ix = index(&db, proxy.provider());
    let c = p.at.into_array();
    ix.register(c).await.unwrap();
    let e = ix.sync(c, ch.head().await).await.unwrap_err();
    let pages = proxy.knobs.get_logs.load(SeqCst);
    eprintln!("refused after {pages} getLogs pages: {e}");
    assert!(matches!(e, CensusError::Format(_)), "{e}");
    assert!(ix.latest(&c).is_none());
    // BUG: the scan keeps every notice and walks the whole history twice
    assert!(
        pages <= 4,
        "{pages} getLogs pages for a one-member contract"
    );
    assert!(e.to_string().contains("notices"), "{e}");
}

// The same flood on a contract that is already indexed: the incremental
// range holds notices and no addition, so it is refused instead of kept,
// and the snapshot the index had stays served.
#[tokio::test]
async fn incremental_notices_without_additions_are_refused() {
    if !enabled() {
        return;
    }
    let ch = Chain::start().await;
    let mut p = Puppet::deploy(&ch, Address::from([0xe2; 20])).await;
    p.add(&ch, &user(1), 3).await;
    p.add(&ch, &user(2), 4).await;
    let dir = TempDir::new().unwrap();
    let db = Db::open_in(dir.path()).unwrap();
    let proxy = RpcProxy::start(ch.anvil.endpoint()).await;
    let ix = index(&db, proxy.provider());
    let c = p.at.into_array();
    ix.register(c).await.unwrap();
    ix.sync(c, ch.head().await).await.unwrap();
    let root = ix.latest(&c).unwrap().0;
    for i in 10..40 {
        p.weight_changed(&ch, &user(i), 0, 1).await;
        ch.mine(5_000).await;
    }
    let before = proxy.knobs.get_logs.load(SeqCst);
    // BUG: notices without additions are accepted as an up-to-date census
    let e = ix.sync(c, ch.head().await).await.unwrap_err();
    let pages = proxy.knobs.get_logs.load(SeqCst) - before;
    assert!(e.to_string().contains("notices"), "{e}");
    assert!(pages <= 4, "{pages} getLogs pages");
    // Not terminal, and the last good snapshot still answers.
    assert!(ix.usable(&c).is_ok());
    assert_eq!(ix.latest(&c).unwrap().0, root);
    let (proof, w) = ix.proof(&c, &root, &user(2)).await.unwrap().unwrap();
    assert_eq!(w, 4);
    assert!(davinci_zkvm_sdk::census::verify_census_proof(&proof));
}

// Many weight changes on real members still make the contract unusable for
// good: keeping only the earliest one changes nothing in the verdict.
#[tokio::test]
async fn repeated_weight_changes_still_condemn_the_contract() {
    if !enabled() {
        return;
    }
    let ch = Chain::start().await;
    let mut p = Puppet::deploy(&ch, Address::from([0xe3; 20])).await;
    p.add(&ch, &user(1), 3).await;
    p.add(&ch, &user(2), 4).await;
    for i in 0..20u128 {
        p.weight_changed(&ch, &user(1), 3 + i, 4 + i).await;
        ch.mine(5_000).await;
    }
    let dir = TempDir::new().unwrap();
    let db = Db::open_in(dir.path()).unwrap();
    let ix = index(&db, ch.reader.clone());
    let c = p.at.into_array();
    ix.register(c).await.unwrap();
    let e = ix.sync(c, ch.head().await).await.unwrap_err();
    assert!(matches!(e, CensusError::Refused(_)), "{e}");
    match ix.usable(&c) {
        Err(UsableError::Unusable(why)) => {
            assert!(why.contains("changed from 3"), "the earliest change: {why}")
        }
        other => panic!("{other:?}"),
    }
}

// The control: honest members, each add with its notice, spread over pages,
// first scan and incremental, are indexed exactly.
#[tokio::test]
async fn honest_members_over_many_pages_are_indexed() {
    if !enabled() {
        return;
    }
    let ch = Chain::start().await;
    let mut p = Puppet::deploy(&ch, Address::from([0xe4; 20])).await;
    for i in 0..6 {
        p.add(&ch, &user(i), u128::from(i) + 1).await;
        ch.mine(5_000).await;
    }
    let dir = TempDir::new().unwrap();
    let db = Db::open_in(dir.path()).unwrap();
    let ix = index(&db, ch.reader.clone());
    let c = p.at.into_array();
    ix.register(c).await.unwrap();
    ix.sync(c, ch.head().await).await.unwrap();
    assert_eq!(ix.latest(&c).unwrap().1, 6);
    for i in 6..9 {
        p.add(&ch, &user(i), 1).await;
        ch.mine(5_000).await;
    }
    ix.sync(c, ch.head().await).await.unwrap();
    let (root, size, _) = ix.latest(&c).unwrap();
    assert_eq!((root, size), (p.tree.root(), 9));
    for i in 0..9 {
        let (proof, _) = ix.proof(&c, &root, &user(i)).await.unwrap().unwrap();
        assert!(davinci_zkvm_sdk::census::verify_census_proof(&proof));
    }
}
