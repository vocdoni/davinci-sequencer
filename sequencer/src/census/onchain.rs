//! On-chain census index (origin 3): one lean-IMT per census contract, built
//! from its `CensusMemberAdded` logs, checked against the contract's root and
//! size at the confirmed head, persisted in redb and shared by every process
//! that points at the contract. Synced from the monitor tick.
//!
//! The contract must be append-only with fixed weights (`OnchainCensus` of
//! davinci-onchain-census-contract). A weight
//! change or two members on one ballot slot marks the index unusable for
//! good; logs that do not reproduce the contract's root fail closed and are
//! retried on a backoff.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use alloy::eips::{BlockId, BlockNumberOrTag};
use alloy::network::TransactionBuilder;
use alloy::primitives::{Address, B256, U256};
use alloy::providers::{DynProvider, Provider};
use alloy::rpc::types::{Filter, Log, TransactionRequest};
use alloy::sol_types::{SolCall, SolEvent};
use davinci_zkvm_sdk::census::{CensusProof, LeanImt, census_leaf, slot_key_address};
use davinci_zkvm_sdk::crypto::field::{Fr, fr_from_be, fr_to_be};
use redb::{Database, ReadableDatabase, TableDefinition};
use serde::{Deserialize, Serialize};
use tracing::{error, info, warn};

use super::{CensusError, Result, TreeCache, backoff};
use crate::storage::Db;

alloy::sol!(
    #[allow(missing_docs)]
    OnchainCensus,
    "abi/OnchainCensus.json"
);

type Bytes = TableDefinition<'static, &'static [u8], &'static [u8]>;
// chain8‖contract20 -> Meta (JSON).
const META: Bytes = TableDefinition::new("census_onchain_meta");
// chain8‖contract20‖index8 -> address20‖weight16‖block8‖root32, the root
// being the contract's root right after this insert.
const LEAF: Bytes = TableDefinition::new("census_onchain_leaf");
const ROW: usize = 20 + 16 + 8 + 32;
/// Widest `eth_getLogs` range; halved on provider limits.
const MAX_SPAN: u64 = 5_000;

/// One census member, in insertion order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Member {
    pub address: [u8; 20],
    pub weight: u128,
    pub block: u64,
    /// The contract's root after this insert (BE).
    pub root: [u8; 32],
}

/// A census contract log, as the replay reads it. 32-byte values are the
/// BE uint256 words.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CensusLog {
    Added {
        user: [u8; 20],
        weight: u128,
        leaf: [u8; 32],
        new_root: [u8; 32],
    },
    WeightChanged {
        account: [u8; 20],
        previous: u128,
    },
}

/// A refused [`OnchainIndex::usable`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum UsableError {
    /// `register` never ran (or failed): transient, register and retry.
    #[error("census contract is not indexed")]
    NotIndexed,
    /// The contract can never validate again.
    #[error("census contract unusable: {0}")]
    Unusable(String),
}

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum ReplayError {
    /// The logs do not reproduce the contract: refetch, then fail closed.
    #[error("{0}")]
    Mismatch(String),
    /// The contract did something the protocol cannot follow.
    #[error("{0}")]
    Unusable(String),
}

/// Replays `logs` (in `(block, logIndex)` order) onto `tree`. `members` and
/// `slots` describe the index before the logs. Returns the new members.
pub fn replay(
    tree: &mut LeanImt,
    members: &HashMap<[u8; 20], u64>,
    slots: &HashMap<u64, [u8; 20]>,
    logs: &[(u64, CensusLog)],
    slot_of: fn(&[u8; 20]) -> u64,
) -> Result<Vec<Member>, ReplayError> {
    let mismatch = |m: String| Err(ReplayError::Mismatch(m));
    let mut out = Vec::new();
    let mut new_users = HashMap::new();
    let mut new_slots = HashMap::new();
    for (block, log) in logs {
        let (user, weight, leaf, new_root) = match log {
            // Every add emits WeightChanged(user, 0, w) first.
            CensusLog::WeightChanged { previous: 0, .. } => continue,
            CensusLog::WeightChanged { account, previous } => {
                return Err(ReplayError::Unusable(format!(
                    "weight of 0x{} changed from {previous} at block {block}",
                    hex::encode(account)
                )));
            }
            CensusLog::Added {
                user,
                weight,
                leaf,
                new_root,
            } => (user, *weight, leaf, new_root),
        };
        let who = format!("0x{} at block {block}", hex::encode(user));
        if *user == [0u8; 20] {
            return mismatch(format!("zero address added at block {block}"));
        }
        if members.contains_key(user) || new_users.insert(*user, ()).is_some() {
            return mismatch(format!("{who} added twice"));
        }
        if weight == 0 {
            return mismatch(format!("{who} added with weight 0"));
        }
        let Ok(want) = census_leaf(user, weight) else {
            return mismatch(format!("{who}: weight {weight} above 88 bits"));
        };
        if fr_from_be(leaf).ok() != Some(want) {
            return mismatch(format!("{who}: leaf is not address << 88 | weight"));
        }
        let slot = slot_of(user);
        if let Some(other) = slots.get(&slot).or(new_slots.get(&slot)) {
            return Err(ReplayError::Unusable(format!(
                "ballot slot {slot:#x} is shared by 0x{} and 0x{}",
                hex::encode(other),
                hex::encode(user)
            )));
        }
        new_slots.insert(slot, *user);
        tree.insert(want);
        if fr_to_be(&tree.root()) != *new_root {
            return mismatch(format!("{who}: newRoot differs from the local root"));
        }
        out.push(Member {
            address: *user,
            weight,
            block: *block,
            root: *new_root,
        });
    }
    Ok(out)
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct Meta {
    synced: bool,
    scanned_to: u64,
    #[serde(with = "hex::serde")]
    scanned_hash: [u8; 32],
    unusable: Option<String>,
}

/// Members in insertion order and their lookups.
#[derive(Clone, Debug, Default)]
struct Members {
    list: Vec<Member>,
    by_addr: HashMap<[u8; 20], u64>,
    slots: HashMap<u64, [u8; 20]>,
    /// Root (BE) -> tree size at that root.
    roots: HashMap<[u8; 32], u64>,
}

impl Members {
    fn push(&mut self, m: Member) {
        let i = self.list.len() as u64;
        self.by_addr.insert(m.address, i);
        self.slots.insert(slot_key_address(&m.address), m.address);
        self.roots.insert(m.root, i + 1);
        self.list.push(m);
    }
}

/// One consistent view of a contract's index; replaced whole on each sync.
/// Cloning it is cheap: a sync without new members only swaps `meta`.
#[derive(Clone, Debug, Default)]
struct Snapshot {
    tree: Arc<LeanImt>,
    m: Arc<Members>,
    meta: Meta,
}

/// Consecutive sync failures of one contract.
#[derive(Clone)]
struct Failure {
    at: Instant,
    n: u32,
    why: String,
    /// Already handed to a bootstrap by [`OnchainIndex::take_failure`].
    charged: bool,
}

struct Entry {
    snap: RwLock<Arc<Snapshot>>,
    /// Single-flight sync.
    gate: tokio::sync::Mutex<()>,
    failed: Mutex<Option<Failure>>,
    /// Trees at earlier roots.
    prefix: Mutex<TreeCache>,
}

impl Entry {
    fn snapshot(&self) -> Arc<Snapshot> {
        self.snap.read().map(|s| s.clone()).unwrap_or_default()
    }

    fn set(&self, s: Snapshot) {
        if let Ok(mut g) = self.snap.write() {
            *g = Arc::new(s);
        }
    }
}

/// The confirmed contract state a sync must reach.
#[derive(Clone, Copy)]
struct Target {
    block: u64,
    hash: [u8; 32],
    root: Fr,
    size: u64,
}

/// Every indexed census contract of one chain.
pub struct OnchainIndex {
    db: Arc<Database>,
    chain_id: u64,
    provider: DynProvider,
    entries: Mutex<HashMap<[u8; 20], Arc<Entry>>>,
    span: AtomicU64,
    syncing: AtomicBool,
    /// Retry delay when the target block is not served yet.
    poll: Duration,
}

impl std::fmt::Debug for OnchainIndex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OnchainIndex")
            .field("chain_id", &self.chain_id)
            .finish_non_exhaustive()
    }
}

fn db_err(e: impl std::fmt::Display) -> CensusError {
    CensusError::Storage(crate::storage::StorageError::Db(e.to_string()))
}

fn corrupt(reason: String) -> CensusError {
    CensusError::Storage(crate::storage::StorageError::Corrupt {
        table: "census_onchain",
        reason,
    })
}

fn rpc(e: impl std::fmt::Display) -> CensusError {
    // Node error text quotes contract revert strings verbatim, newlines
    // included: escape before it reaches the logs or the stored failure.
    CensusError::Fetch(super::clean(&e.to_string()))
}

/// Provider errors that mean "ask for a narrower range". Rate limits are
/// transient, not a range problem.
fn too_wide(e: &str) -> bool {
    let e = e.to_ascii_lowercase();
    if e.contains("rate") || e.contains("429") {
        return false;
    }
    ["range", "too many", "limit", "exceed", "too large"]
        .iter()
        .any(|m| e.contains(m))
}

fn decode(log: &Log) -> Result<Option<(u64, u64, CensusLog)>> {
    let bad = |e: &dyn std::fmt::Display| CensusError::Format(format!("census log: {e}"));
    let (Some(block), Some(index)) = (log.block_number, log.log_index) else {
        return Err(bad(&"log without a block position"));
    };
    let ev = match log.topic0() {
        Some(t) if *t == OnchainCensus::CensusMemberAdded::SIGNATURE_HASH => {
            let e = OnchainCensus::CensusMemberAdded::decode_log(&log.inner)
                .map_err(|e| bad(&e))?
                .data;
            CensusLog::Added {
                user: e.user.into_array(),
                weight: e.weight.to::<u128>(),
                leaf: e.leaf.to_be_bytes(),
                new_root: e.newRoot.to_be_bytes(),
            }
        }
        Some(t) if *t == OnchainCensus::WeightChanged::SIGNATURE_HASH => {
            let e = OnchainCensus::WeightChanged::decode_log(&log.inner)
                .map_err(|e| bad(&e))?
                .data;
            CensusLog::WeightChanged {
                account: e.account.into_array(),
                previous: e.previousWeight.to::<u128>(),
            }
        }
        _ => return Ok(None),
    };
    Ok(Some((block, index, ev)))
}

impl OnchainIndex {
    /// The index over `db`; `provider` reads the census contracts. `poll` is
    /// the retry delay when the target block is not served yet.
    pub fn new(db: &Db, chain_id: u64, provider: DynProvider, poll: Duration) -> Result<Self> {
        let db = db.database();
        let tx = db.begin_write().map_err(db_err)?;
        tx.open_table(META).map_err(db_err)?;
        tx.open_table(LEAF).map_err(db_err)?;
        tx.commit().map_err(db_err)?;
        Ok(OnchainIndex {
            db,
            chain_id,
            provider,
            entries: Mutex::new(HashMap::new()),
            span: AtomicU64::new(MAX_SPAN),
            syncing: AtomicBool::new(false),
            poll,
        })
    }

    fn key(&self, contract: &[u8; 20]) -> [u8; 28] {
        let mut k = [0u8; 28];
        k[..8].copy_from_slice(&self.chain_id.to_be_bytes());
        k[8..].copy_from_slice(contract);
        k
    }

    fn entry(&self, contract: &[u8; 20]) -> Option<Arc<Entry>> {
        self.entries.lock().ok()?.get(contract).cloned()
    }

    /// Starts indexing `contract`, loading what a previous run stored.
    pub async fn register(&self, contract: [u8; 20]) -> Result<()> {
        if self.entry(&contract).is_some() {
            return Ok(());
        }
        let (db, key) = (self.db.clone(), self.key(&contract));
        let snap = tokio::task::spawn_blocking(move || match load(&db, &key) {
            Err(CensusError::Storage(crate::storage::StorageError::Corrupt { reason, .. })) => {
                error!(reason, "census index corrupt; rescanning");
                drop_rows(&db, &key).map(|()| Snapshot::default())
            }
            r => r,
        })
        .await
        .map_err(rpc)??;
        let mut m = self
            .entries
            .lock()
            .map_err(|_| CensusError::Config("index lock poisoned".into()))?;
        m.entry(contract).or_insert_with(|| {
            Arc::new(Entry {
                snap: RwLock::new(Arc::new(snap)),
                gate: tokio::sync::Mutex::new(()),
                failed: Mutex::new(None),
                prefix: Mutex::new(TreeCache::default()),
            })
        });
        Ok(())
    }

    /// Root, size and block of the last confirmed sync; `None` before the
    /// first one. Says nothing about [`OnchainIndex::usable`].
    pub fn latest(&self, contract: &[u8; 20]) -> Option<(Fr, u64, u64)> {
        let s = self.entry(contract)?.snapshot();
        s.meta
            .synced
            .then(|| (s.tree.root(), s.m.list.len() as u64, s.meta.scanned_to))
    }

    /// Why votes on this contract must be refused, if they must.
    /// `NotIndexed` is transient (register and retry); `Unusable` is final.
    pub fn usable(&self, contract: &[u8; 20]) -> Result<(), UsableError> {
        let e = self.entry(contract).ok_or(UsableError::NotIndexed)?;
        match &e.snapshot().meta.unusable {
            Some(r) => Err(UsableError::Unusable(r.clone())),
            None => Ok(()),
        }
    }

    /// Test hook: forget a contract, as if `register` had never run.
    #[doc(hidden)]
    pub fn unregister(&self, contract: &[u8; 20]) {
        if let Ok(mut m) = self.entries.lock() {
            m.remove(contract);
        }
    }

    /// The last sync failure, once: a bootstrap charges each failed sync
    /// one attempt, and nothing for polls inside the backoff window.
    pub fn take_failure(&self, contract: &[u8; 20]) -> Option<String> {
        let e = self.entry(contract)?;
        let mut f = e.failed.lock().ok()?;
        let f = f.as_mut().filter(|f| !f.charged)?;
        f.charged = true;
        Some(f.why.clone())
    }

    /// Time left in the contract's sync backoff, if it is backing off.
    pub fn retry_in(&self, contract: &[u8; 20]) -> Option<Duration> {
        let e = self.entry(contract)?;
        let f = e.failed.lock().ok()?.clone()?;
        backoff(f.n)
            .checked_sub(f.at.elapsed())
            .filter(|d| !d.is_zero())
    }

    /// Proof and weight of `address` at `root`: the latest root or any
    /// earlier one this index has seen. `None` for a non-member at `root`;
    /// an error for an unknown contract or root.
    pub async fn proof(
        &self,
        contract: &[u8; 20],
        root: &Fr,
        address: &[u8; 20],
    ) -> Result<Option<(CensusProof, u128)>> {
        let e = self
            .entry(contract)
            .ok_or_else(|| CensusError::Unknown(format!("contract 0x{}", hex::encode(contract))))?;
        let s = e.snapshot();
        let key = fr_to_be(root);
        // The empty tree's root is 0 and has no member.
        let size = match s.m.roots.get(&key) {
            Some(n) => *n,
            None if key == [0u8; 32] => 0,
            None => return Err(CensusError::Unknown(format!("0x{}", hex::encode(key)))),
        };
        let Some(&i) = s.m.by_addr.get(address).filter(|i| **i < size) else {
            return Ok(None);
        };
        let Some(m) = s.m.list.get(i as usize).copied() else {
            return Ok(None);
        };
        let tree = if size == s.m.list.len() as u64 {
            s.tree.clone()
        } else if let Some(t) = e.prefix.lock().ok().and_then(|mut c| c.get(&key)) {
            t
        } else {
            let leaves =
                s.m.list
                    .get(..size as usize)
                    .ok_or_else(|| corrupt(format!("root at {size} past the members")))?
                    .iter()
                    .map(|m| census_leaf(&m.address, m.weight))
                    .collect::<std::result::Result<Vec<_>, _>>()
                    .map_err(|e| corrupt(e.to_string()))?;
            let t = tokio::task::spawn_blocking(move || Arc::new(LeanImt::from_leaves(leaves)))
                .await
                .map_err(rpc)?;
            if t.root() != *root {
                return Err(corrupt(format!("prefix {size} does not give its root")));
            }
            if let Ok(mut c) = e.prefix.lock() {
                c.put(key, t.clone());
            }
            t
        };
        let proof = tree.proof(i as usize).map_err(|e| corrupt(e.to_string()))?;
        Ok(Some((proof, m.weight)))
    }

    /// Syncs every registered contract to the confirmed head `b`. One run
    /// at a time; a tick that finds one running does nothing.
    pub async fn sync_all(&self, b: u64) {
        if self.syncing.swap(true, Ordering::AcqRel) {
            return;
        }
        struct Reset<'a>(&'a AtomicBool);
        impl Drop for Reset<'_> {
            fn drop(&mut self) {
                self.0.store(false, Ordering::Release);
            }
        }
        let _reset = Reset(&self.syncing);
        let contracts: Vec<[u8; 20]> = match self.entries.lock() {
            Ok(m) => m.keys().copied().collect(),
            Err(_) => return,
        };
        for c in contracts {
            // `sync` logs its own failures.
            let _ = self.sync(c, b).await;
        }
    }

    /// Syncs `contract` to the confirmed head `b`. A failure backs the
    /// contract off; an unusable contract answers `Refused`.
    pub async fn sync(&self, contract: [u8; 20], b: u64) -> Result<()> {
        let e = self
            .entry(&contract)
            .ok_or_else(|| CensusError::Unknown(format!("contract 0x{}", hex::encode(contract))))?;
        let _g = e.gate.lock().await;
        if let Some(r) = &e.snapshot().meta.unusable {
            return Err(CensusError::Refused(r.clone()));
        }
        if let Some(d) = self.retry_in(&contract) {
            return Err(CensusError::Backoff(d));
        }
        let res = self.sync_inner(&e, contract, b).await;
        if let Ok(mut f) = e.failed.lock() {
            match &res {
                Ok(()) => *f = None,
                // Unusable is terminal; Backoff is "not yet", not a failure.
                Err(CensusError::Refused(_) | CensusError::Backoff(_)) => {}
                Err(err) => {
                    let n = f.as_ref().map_or(0, |x| x.n).saturating_add(1);
                    error!(contract = %hex::encode(contract), ?err, failures = n, "census contract sync failed");
                    *f = Some(Failure {
                        at: Instant::now(),
                        n,
                        why: err.to_string(),
                        charged: false,
                    });
                }
            }
        }
        res
    }

    async fn sync_inner(&self, e: &Entry, contract: [u8; 20], b: u64) -> Result<()> {
        let key = self.key(&contract);
        let mut base = e.snapshot();
        if base.meta.synced {
            // A block the node no longer serves counts as reorged away.
            if self.block_hash(base.meta.scanned_to).await? != Some(base.meta.scanned_hash) {
                warn!(contract = %hex::encode(contract), block = base.meta.scanned_to, "census contract reorged; rescanning");
                // Full rescan on a deep reorg; truncate-and-replay if censuses get large.
                let db = self.db.clone();
                tokio::task::spawn_blocking(move || drop_rows(&db, &key))
                    .await
                    .map_err(rpc)??;
                e.set(Snapshot::default());
                if let Ok(mut c) = e.prefix.lock() {
                    *c = TreeCache::default();
                }
                base = e.snapshot();
            } else if b <= base.meta.scanned_to {
                return Ok(());
            }
        }
        let Some(hash) = self.block_hash(b).await? else {
            return Err(CensusError::Backoff(self.poll));
        };
        let (root, size) = self.state_at(contract, hash).await?;
        let target = Target {
            block: b,
            hash,
            root,
            size,
        };
        // First sync: back from B until the adds reach treeSize(B).
        // Incremental: the whole new range, so no weight change is missed.
        let (floor, need) = if base.meta.synced {
            (base.meta.scanned_to + 1, None)
        } else {
            (0, Some(size))
        };
        let mut why = String::new();
        for _ in 0..2 {
            let logs = self.scan(contract, floor, b, need).await?;
            let (db, base2) = (self.db.clone(), base.clone());
            let out = tokio::task::spawn_blocking(move || apply(&db, &key, &base2, &logs, &target))
                .await
                .map_err(rpc)??;
            match out {
                Ok(s) => {
                    info!(contract = %hex::encode(contract), block = b, size = s.m.list.len(), "census contract synced");
                    e.set(s);
                    return Ok(());
                }
                Err((s, r)) if s.meta.unusable.is_some() => {
                    error!(contract = %hex::encode(contract), reason = %r, "census contract unusable");
                    e.set(s);
                    return Err(CensusError::Refused(r.to_string()));
                }
                Err((_, r)) => why = r.to_string(),
            }
        }
        Err(CensusError::Format(format!(
            "census contract 0x{}: {why}",
            hex::encode(contract)
        )))
    }

    async fn block_hash(&self, n: u64) -> Result<Option<[u8; 32]>> {
        let b = self
            .provider
            .get_block_by_number(BlockNumberOrTag::Number(n))
            .await
            .map_err(rpc)?;
        Ok(b.map(|b| b.header.hash.0))
    }

    /// `getCensusRoot()` and `treeSize()` at block `hash`.
    async fn state_at(&self, contract: [u8; 20], hash: [u8; 32]) -> Result<(Fr, u64)> {
        let to = Address::from(contract);
        let at = BlockId::hash(B256::from(hash));
        let call = |data: Vec<u8>| {
            let tx = TransactionRequest::default().with_to(to).with_input(data);
            self.provider.call(tx).block(at)
        };
        let root = call(OnchainCensus::getCensusRootCall {}.abi_encode())
            .await
            .map_err(rpc)?;
        let size = call(OnchainCensus::treeSizeCall {}.abi_encode())
            .await
            .map_err(rpc)?;
        let bad = |e: &dyn std::fmt::Display| CensusError::Format(format!("census contract: {e}"));
        let root: U256 =
            OnchainCensus::getCensusRootCall::abi_decode_returns(&root).map_err(|e| bad(&e))?;
        let size: U256 =
            OnchainCensus::treeSizeCall::abi_decode_returns(&size).map_err(|e| bad(&e))?;
        let root = fr_from_be(&root.to_be_bytes()).map_err(|e| bad(&e))?;
        let size = u64::try_from(size).map_err(|e| bad(&e))?;
        Ok((root, size))
    }

    /// Census logs in `[floor, b]`, scanned backwards in adaptive chunks and
    /// stopping early once `need` adds were seen. Chain order.
    async fn scan(
        &self,
        contract: [u8; 20],
        floor: u64,
        b: u64,
        need: Option<u64>,
    ) -> Result<Vec<(u64, CensusLog)>> {
        let addr = Address::from(contract);
        let sigs = vec![
            OnchainCensus::CensusMemberAdded::SIGNATURE_HASH,
            OnchainCensus::WeightChanged::SIGNATURE_HASH,
        ];
        let mut out = Vec::new();
        let mut added = 0u64;
        let mut hi = b;
        let mut streak = 0;
        while hi >= floor && need.is_none_or(|n| added < n) {
            let span = self.span.load(Ordering::Relaxed).max(1);
            let lo = hi.saturating_sub(span - 1).max(floor);
            let filter = Filter::new()
                .address(addr)
                .event_signature(sigs.clone())
                .from_block(lo)
                .to_block(hi);
            match self.provider.get_logs(&filter).await {
                Ok(logs) => {
                    for log in logs.iter().filter(|l| l.address() == addr) {
                        if let Some(l) = decode(log)? {
                            added += u64::from(matches!(l.2, CensusLog::Added { .. }));
                            out.push(l);
                        }
                    }
                    streak += 1;
                    if streak == 2 {
                        streak = 0;
                        self.span
                            .store(span.saturating_mul(2).min(MAX_SPAN), Ordering::Relaxed);
                    }
                    if lo == floor {
                        break;
                    }
                    hi = lo - 1;
                }
                Err(e) if span > 1 && too_wide(&e.to_string()) => {
                    streak = 0;
                    self.span.store(span / 2, Ordering::Relaxed);
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                Err(e) => return Err(rpc(e)),
            }
        }
        out.sort_by_key(|(b, i, _)| (*b, *i));
        Ok(out.into_iter().map(|(b, _, l)| (b, l)).collect())
    }

    /// Test hook: end the contract's sync backoff now.
    #[doc(hidden)]
    pub fn expire_backoff(&self, contract: &[u8; 20]) {
        if let Some(e) = self.entry(contract)
            && let Ok(mut f) = e.failed.lock()
            && let Some(f) = f.as_mut()
        {
            f.at = Instant::now().checked_sub(backoff(f.n)).unwrap_or(f.at);
        }
    }

    /// Test hook: overwrite the stored `scanned_to` hash (a fake reorg).
    #[doc(hidden)]
    pub fn tamper_scanned_hash(&self, contract: &[u8; 20]) -> Result<()> {
        let e = self
            .entry(contract)
            .ok_or_else(|| CensusError::Unknown("contract".into()))?;
        let mut s = (*e.snapshot()).clone();
        s.meta.scanned_hash = [0xee; 32];
        put_meta(&self.db, &self.key(contract), &s.meta)?;
        e.set(s);
        Ok(())
    }
}

/// Replays `logs` onto `base` and checks the result against `t`. On success
/// the new snapshot is stored; an unusable contract is stored marked. The
/// error side carries the snapshot to install (marked or unchanged).
#[allow(clippy::type_complexity)]
fn apply(
    db: &Database,
    key: &[u8; 28],
    base: &Snapshot,
    logs: &[(u64, CensusLog)],
    t: &Target,
) -> Result<std::result::Result<Snapshot, (Snapshot, ReplayError)>> {
    // Nothing new: check the target and move the scanned block only.
    if logs.is_empty() {
        let mut s = base.clone();
        if s.tree.len() as u64 != t.size || s.tree.root() != t.root {
            let r = ReplayError::Mismatch(format!(
                "contract has {} members at block {}, the index {} and no new logs",
                t.size,
                t.block,
                s.tree.len()
            ));
            return Ok(Err((s, r)));
        }
        s.meta.synced = true;
        s.meta.scanned_to = t.block;
        s.meta.scanned_hash = t.hash;
        put_meta(db, key, &s.meta)?;
        return Ok(Ok(s));
    }
    let mut tree = (*base.tree).clone();
    let res = replay(
        &mut tree,
        &base.m.by_addr,
        &base.m.slots,
        logs,
        slot_key_address,
    )
    .and_then(|new| {
        if tree.len() as u64 != t.size || tree.root() != t.root {
            return Err(ReplayError::Mismatch(format!(
                "contract has {} members at block {}, the logs give {}",
                t.size,
                t.block,
                tree.len()
            )));
        }
        Ok(new)
    });
    // Copy-on-sync, O(members) per poll with new members; persistent maps
    // if censuses reach millions.
    let mut s = base.clone();
    match res {
        Ok(new) => {
            let first = s.m.list.len();
            let mut m2 = (*s.m).clone();
            for m in new {
                m2.push(m);
            }
            s.m = Arc::new(m2);
            s.tree = Arc::new(tree);
            s.meta.synced = true;
            s.meta.scanned_to = t.block;
            s.meta.scanned_hash = t.hash;
            let tx = db.begin_write().map_err(db_err)?;
            {
                let mut lt = tx.open_table(LEAF).map_err(db_err)?;
                for (i, m) in s.m.list.iter().enumerate().skip(first) {
                    lt.insert(leaf_key(key, i as u64).as_slice(), encode_row(m).as_slice())
                        .map_err(db_err)?;
                }
                let mut mt = tx.open_table(META).map_err(db_err)?;
                let meta = serde_json::to_vec(&s.meta).map_err(db_err)?;
                mt.insert(key.as_slice(), meta.as_slice()).map_err(db_err)?;
            }
            tx.commit().map_err(db_err)?;
            Ok(Ok(s))
        }
        Err(r @ ReplayError::Unusable(_)) => {
            s.meta.unusable = Some(r.to_string());
            put_meta(db, key, &s.meta)?;
            Ok(Err((s, r)))
        }
        Err(r) => Ok(Err((s, r))),
    }
}

fn leaf_key(key: &[u8; 28], i: u64) -> [u8; 36] {
    let mut k = [0u8; 36];
    k[..28].copy_from_slice(key);
    k[28..].copy_from_slice(&i.to_be_bytes());
    k
}

fn encode_row(m: &Member) -> [u8; ROW] {
    let mut r = [0u8; ROW];
    r[..20].copy_from_slice(&m.address);
    r[20..36].copy_from_slice(&m.weight.to_be_bytes());
    r[36..44].copy_from_slice(&m.block.to_be_bytes());
    r[44..].copy_from_slice(&m.root);
    r
}

fn decode_row(v: &[u8]) -> Option<Member> {
    let v: &[u8; ROW] = v.try_into().ok()?;
    let (a, rest) = v.split_first_chunk::<20>()?;
    let (w, rest) = rest.split_first_chunk::<16>()?;
    let (b, root) = rest.split_first_chunk::<8>()?;
    Some(Member {
        address: *a,
        weight: u128::from_be_bytes(*w),
        block: u64::from_be_bytes(*b),
        root: root.try_into().ok()?,
    })
}

fn put_meta(db: &Database, key: &[u8; 28], meta: &Meta) -> Result<()> {
    let tx = db.begin_write().map_err(db_err)?;
    {
        let mut mt = tx.open_table(META).map_err(db_err)?;
        let v = serde_json::to_vec(meta).map_err(db_err)?;
        mt.insert(key.as_slice(), v.as_slice()).map_err(db_err)?;
    }
    tx.commit().map_err(db_err)
}

/// Rebuilds a contract's snapshot from its rows.
fn load(db: &Database, key: &[u8; 28]) -> Result<Snapshot> {
    let tx = db.begin_read().map_err(db_err)?;
    let mt = tx.open_table(META).map_err(db_err)?;
    let meta: Meta = match mt.get(key.as_slice()).map_err(db_err)? {
        Some(v) => serde_json::from_slice(v.value()).map_err(|e| corrupt(e.to_string()))?,
        None => return Ok(Snapshot::default()),
    };
    let lt = tx.open_table(LEAF).map_err(db_err)?;
    let (lo, hi) = (leaf_key(key, 0), leaf_key(key, u64::MAX));
    let mut ms = Members::default();
    let mut leaves = Vec::new();
    for row in lt.range(lo.as_slice()..=hi.as_slice()).map_err(db_err)? {
        let (k, v) = row.map_err(db_err)?;
        let i = ms.list.len();
        if k.value() != leaf_key(key, i as u64).as_slice() {
            return Err(corrupt(format!("leaf {i} missing")));
        }
        let m = decode_row(v.value()).ok_or_else(|| corrupt(format!("leaf {i}")))?;
        leaves.push(census_leaf(&m.address, m.weight).map_err(|e| corrupt(e.to_string()))?);
        ms.push(m);
    }
    let tree = LeanImt::from_leaves(leaves);
    if let Some(last) = ms.list.last()
        && fr_to_be(&tree.root()) != last.root
    {
        return Err(corrupt("stored leaves do not give the stored root".into()));
    }
    Ok(Snapshot {
        tree: Arc::new(tree),
        m: Arc::new(ms),
        meta,
    })
}

fn drop_rows(db: &Database, key: &[u8; 28]) -> Result<()> {
    let tx = db.begin_write().map_err(db_err)?;
    {
        let mut lt = tx.open_table(LEAF).map_err(db_err)?;
        let (lo, hi) = (leaf_key(key, 0), leaf_key(key, u64::MAX));
        lt.retain_in(lo.as_slice()..=hi.as_slice(), |_, _| false)
            .map_err(db_err)?;
        let mut mt = tx.open_table(META).map_err(db_err)?;
        mt.remove(key.as_slice()).map_err(db_err)?;
    }
    tx.commit().map_err(db_err)
}

#[cfg(test)]
mod tests {
    use alloy::providers::ProviderBuilder;

    use super::*;

    fn addr(i: u8) -> [u8; 20] {
        let mut a = [i; 20];
        a[0] = 0xaa;
        a
    }

    // Honest logs for members 1..=n, one per block.
    fn honest(n: u8) -> (Vec<(u64, CensusLog)>, LeanImt) {
        let mut tree = LeanImt::new();
        let mut logs = Vec::new();
        for i in 1..=n {
            let leaf = census_leaf(&addr(i), u128::from(i)).unwrap();
            tree.insert(leaf);
            logs.push((
                u64::from(i),
                CensusLog::WeightChanged {
                    account: addr(i),
                    previous: 0,
                },
            ));
            logs.push((
                u64::from(i),
                CensusLog::Added {
                    user: addr(i),
                    weight: u128::from(i),
                    leaf: fr_to_be(&leaf),
                    new_root: fr_to_be(&tree.root()),
                },
            ));
        }
        (logs, tree)
    }

    fn run(logs: &[(u64, CensusLog)]) -> Result<Vec<Member>, ReplayError> {
        replay(
            &mut LeanImt::new(),
            &HashMap::new(),
            &HashMap::new(),
            logs,
            slot_key_address,
        )
    }

    fn added(logs: &mut [(u64, CensusLog)], i: usize) -> &mut CensusLog {
        &mut logs[2 * i + 1].1
    }

    #[test]
    fn replay_checks_every_add() {
        let (logs, tree) = honest(5);
        let got = run(&logs).unwrap();
        assert_eq!(got.len(), 5);
        assert_eq!(got[4].root, fr_to_be(&tree.root()));

        let mut l = logs.clone();
        if let CensusLog::Added { new_root, .. } = added(&mut l, 2) {
            new_root[31] ^= 1;
        }
        assert!(matches!(run(&l), Err(ReplayError::Mismatch(m)) if m.contains("newRoot")));

        let mut l = logs.clone();
        if let CensusLog::Added { user, .. } = added(&mut l, 3) {
            *user = addr(1);
        }
        assert!(matches!(run(&l), Err(ReplayError::Mismatch(m)) if m.contains("twice")));

        let mut l = logs.clone();
        if let CensusLog::Added { leaf, .. } = added(&mut l, 1) {
            *leaf = fr_to_be(&census_leaf(&addr(2), 3).unwrap());
        }
        assert!(matches!(run(&l), Err(ReplayError::Mismatch(m)) if m.contains("88")));

        let mut l = logs.clone();
        if let CensusLog::Added { weight, .. } = added(&mut l, 0) {
            *weight = 0;
        }
        assert!(matches!(run(&l), Err(ReplayError::Mismatch(m)) if m.contains("weight 0")));

        let mut l = logs.clone();
        if let CensusLog::Added { weight, .. } = added(&mut l, 0) {
            *weight = 1 << 88;
        }
        assert!(matches!(run(&l), Err(ReplayError::Mismatch(m)) if m.contains("88 bits")));

        let mut l = logs.clone();
        if let CensusLog::Added { user, .. } = added(&mut l, 0) {
            *user = [0; 20];
        }
        assert!(matches!(run(&l), Err(ReplayError::Mismatch(m)) if m.contains("zero")));

        // A changed weight or two members on one slot is unusable, not a mismatch.
        let mut l = logs.clone();
        l.push((
            9,
            CensusLog::WeightChanged {
                account: addr(2),
                previous: 2,
            },
        ));
        assert!(matches!(run(&l), Err(ReplayError::Unusable(_))));
        let collide = replay(
            &mut LeanImt::new(),
            &HashMap::new(),
            &HashMap::new(),
            &logs,
            |a| u64::from(a[0]),
        );
        assert!(
            matches!(&collide, Err(ReplayError::Unusable(m)) if m.contains("slot 0xaa")),
            "{collide:?}"
        );
    }

    fn index(dir: &tempfile::TempDir) -> OnchainIndex {
        let db = Db::open(&dir.path().join("sequencer.redb")).unwrap();
        let p = ProviderBuilder::new()
            .disable_recommended_fillers()
            .connect_http("http://127.0.0.1:1".parse().unwrap())
            .erased();
        OnchainIndex::new(&db, 1, p, Duration::from_millis(50)).unwrap()
    }

    // Applying logs stores the index; a weight change marks it unusable,
    // and both survive a restart.
    #[tokio::test]
    async fn apply_persists_members_and_unusable() {
        let dir = tempfile::tempdir().unwrap();
        let c = [7u8; 20];
        let (logs, tree) = honest(3);
        {
            let ix = index(&dir);
            ix.register(c).await.unwrap();
            let e = ix.entry(&c).unwrap();
            let t = Target {
                block: 3,
                hash: [3; 32],
                root: tree.root(),
                size: 3,
            };
            let s = apply(&ix.db, &ix.key(&c), &e.snapshot(), &logs, &t)
                .unwrap()
                .unwrap();
            e.set(s);
            assert_eq!(ix.latest(&c), Some((tree.root(), 3, 3)));
            // Wrong size: a mismatch, nothing stored.
            let bad = Target { size: 4, ..t };
            assert!(matches!(
                apply(&ix.db, &ix.key(&c), &e.snapshot(), &[], &bad).unwrap(),
                Err((_, ReplayError::Mismatch(_)))
            ));
        }
        let ix = index(&dir);
        ix.register(c).await.unwrap();
        assert_eq!(ix.latest(&c), Some((tree.root(), 3, 3)));
        assert!(ix.usable(&c).is_ok());
        let (p, w) = ix.proof(&c, &tree.root(), &addr(2)).await.unwrap().unwrap();
        assert_eq!(w, 2);
        assert!(davinci_zkvm_sdk::census::verify_census_proof(&p));
        // An earlier root proves the members it had.
        let r1 = fr_from_be(&ix.entry(&c).unwrap().snapshot().m.list[0].root).unwrap();
        assert!(ix.proof(&c, &r1, &addr(1)).await.unwrap().is_some());
        assert!(ix.proof(&c, &r1, &addr(2)).await.unwrap().is_none());

        let e = ix.entry(&c).unwrap();
        let t = Target {
            block: 4,
            hash: [4; 32],
            root: tree.root(),
            size: 3,
        };
        let tampered = [(
            4,
            CensusLog::WeightChanged {
                account: addr(1),
                previous: 1,
            },
        )];
        let (s, r) = apply(&ix.db, &ix.key(&c), &e.snapshot(), &tampered, &t)
            .unwrap()
            .unwrap_err();
        assert!(matches!(r, ReplayError::Unusable(_)));
        e.set(s);
        assert!(ix.usable(&c).is_err());
        drop(ix);
        let ix = index(&dir);
        ix.register(c).await.unwrap();
        let why = ix.usable(&c).unwrap_err().to_string();
        assert!(why.contains("weight"), "{why}");
        assert_eq!(ix.usable(&[9u8; 20]), Err(UsableError::NotIndexed));
        assert!(matches!(ix.sync(c, 10).await, Err(CensusError::Refused(_))));
    }

    #[test]
    fn rate_limits_are_not_range_errors() {
        assert!(too_wide("block range too large"));
        assert!(too_wide("query exceeds max results 10000"));
        assert!(!too_wide("429 Too Many Requests: rate limit exceeded"));
        assert!(!too_wide("Rate limited, retry later"));
    }

    // A reverting census contract puts its revert string, newlines and
    // all, in the node's error text. `rpc` must escape it before it lands
    // in a log line or `Failure.why`.
    #[test]
    fn rpc_error_text_is_escaped() {
        let why = rpc("execution reverted: Error(\"x\nERROR forged line\")").to_string();
        assert!(!why.contains('\n'), "{why}");
        assert!(why.contains("x\\nERROR"), "{why}");
    }
}
