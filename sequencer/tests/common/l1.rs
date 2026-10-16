//! A scripted Ethereum for the L1-blob mode: real RLP headers chained by
//! parent hash and real type-3 envelopes in real transactions tries, so the
//! node's proofs pass `verify_local`. `head()` mines one block per call while
//! auto-mining is on (time passing); tests drive everything else.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

use alloy::consensus::{Header, SignableTransaction, TxEip4844, TxEip4844Variant, TxEnvelope};
use alloy::eips::eip2718::Encodable2718;
use alloy::primitives::{Address, B256, Bytes, Signature, U256};
use async_trait::async_trait;
use davinci_sequencer::web3::l1proof::{L1BlobProof, L1TxProof, header_rlp, tx_key, tx_trie_proof};
use davinci_sequencer::web3::{
    L1Chain, L1Fees, L1Market, L1Origin, L1Receipt, L1SendError, SignedBlobTx, Web3Error,
};
use davinci_zkvm_sdk::blob::{Blob, TransitionBlobs};

/// A `build_proof` failure scripted to persist: no re-post fixes it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProofFault {
    /// `Web3Error::Data`, as `L1Rpc` reports a header this build cannot
    /// encode.
    Data,
    /// A proof whose header chain does not link (`verify_local` refuses it).
    BrokenChain,
}

/// One send the chain saw, accepted or not.
#[derive(Clone, Debug)]
pub struct L1Send {
    pub from: Address,
    pub nonce: u64,
    pub fees: L1Fees,
    pub hash: B256,
    /// The head when it was sent.
    pub head: u64,
}

#[derive(Clone)]
struct FakeTx {
    hash: B256,
    from: Address,
    nonce: u64,
    fees: L1Fees,
    envelope: Bytes,
}

struct Block {
    header: Header,
    hash: B256,
    txs: Vec<FakeTx>,
}

struct L1Inner {
    blocks: Vec<Block>,
    signed: HashMap<B256, FakeTx>,
    pool: BTreeMap<(Address, u64), FakeTx>,
    blobs: HashMap<B256, Blob>,
    market: L1Market,
    /// OP's origin trails the head by this much, unless pinned.
    lag: u64,
    pinned: Option<u64>,
    /// Pool sends without mining them.
    hold: bool,
    /// Blocks take pooled transactions only while fewer than this many are
    /// mined (any sender); the rest stay pending.
    mine_limit: Option<usize>,
    /// `head()` mines a block per call.
    auto_mine: bool,
    /// The next N accepted sends are lost.
    drop_next: u32,
    /// The next N sends are refused at the door (never reach the chain).
    refuse_next: u32,
    /// `build_proof` fails until then.
    proofs_fail_until: Option<tokio::time::Instant>,
    proof_fault: Option<ProofFault>,
    sends: Vec<L1Send>,
    /// Sets the blocks of a reorg apart from the ones they replace.
    epoch: u8,
}

/// The scripted Ethereum; every clone shares it, each handle posts as its
/// own sender.
#[derive(Clone)]
pub struct FakeL1 {
    inner: Arc<Mutex<L1Inner>>,
    sender: Option<Address>,
}

impl Default for FakeL1 {
    fn default() -> Self {
        Self::new()
    }
}

// A type-3 envelope from `from` to itself; the signature is a placeholder,
// nothing here recovers it.
fn envelope(from: Address, nonce: u64, fees: &L1Fees, vhs: &[B256]) -> (B256, Bytes) {
    let tx = TxEip4844 {
        chain_id: 1,
        nonce,
        gas_limit: 21_000,
        max_fee_per_gas: fees.max_fee,
        max_priority_fee_per_gas: fees.tip,
        to: from,
        value: U256::ZERO,
        access_list: Default::default(),
        blob_versioned_hashes: vhs.to_vec(),
        max_fee_per_blob_gas: fees.blob_fee,
        input: Default::default(),
    };
    let env = TxEnvelope::Eip4844(
        TxEip4844Variant::TxEip4844(tx).into_signed(Signature::test_signature()),
    );
    (*env.tx_hash(), env.encoded_2718().into())
}

fn mine(i: &mut L1Inner) {
    let number = i.blocks.len() as u64;
    let parent_hash = i.blocks.last().map_or(B256::ZERO, |b| b.hash);
    let room = i.mine_limit.map_or(usize::MAX, |m| {
        m.saturating_sub(i.blocks.iter().map(|b| b.txs.len()).sum())
    });
    let txs: Vec<FakeTx> = if i.hold {
        Vec::new()
    } else {
        let keys: Vec<(Address, u64)> = i.pool.keys().take(room).copied().collect();
        keys.iter().filter_map(|k| i.pool.remove(k)).collect()
    };
    let envs: Vec<Bytes> = txs.iter().map(|t| t.envelope.clone()).collect();
    let header = Header {
        parent_hash,
        number,
        timestamp: 1_700_000_000 + 12 * number,
        transactions_root: alloy::trie::root::ordered_trie_root_with_encoder(&envs, |e, buf| {
            buf.extend_from_slice(e)
        }),
        extra_data: Bytes::from(vec![i.epoch]),
        ..Default::default()
    };
    let hash = header.hash_slow();
    i.blocks.push(Block { header, hash, txs });
}

fn head_of(i: &L1Inner) -> u64 {
    i.blocks.len() as u64 - 1
}

fn mined_count(i: &L1Inner, from: Address) -> u64 {
    i.blocks
        .iter()
        .flat_map(|b| &b.txs)
        .filter(|t| t.from == from)
        .count() as u64
}

fn origin_of(i: &L1Inner) -> L1Origin {
    let head = head_of(i);
    let n = i.pinned.unwrap_or(head.saturating_sub(i.lag)).min(head);
    L1Origin {
        number: n,
        hash: i.blocks[n as usize].hash,
    }
}

fn locate(i: &L1Inner, h: &B256) -> Option<(u64, usize)> {
    i.blocks.iter().enumerate().find_map(|(n, b)| {
        b.txs
            .iter()
            .position(|t| t.hash == *h)
            .map(|idx| (n as u64, idx))
    })
}

impl FakeL1 {
    /// Ten empty blocks, a 1 gwei base fee and a 1 wei blob base fee; OP's
    /// origin at the head.
    pub fn new() -> Self {
        let mut i = L1Inner {
            blocks: Vec::new(),
            signed: HashMap::new(),
            pool: BTreeMap::new(),
            blobs: HashMap::new(),
            market: L1Market {
                base_fee: 1_000_000_000,
                tip: 0,
                blob_base_fee: 1,
            },
            lag: 0,
            pinned: None,
            hold: false,
            mine_limit: None,
            auto_mine: true,
            drop_next: 0,
            refuse_next: 0,
            proofs_fail_until: None,
            proof_fault: None,
            sends: Vec::new(),
            epoch: 0,
        };
        for _ in 0..10 {
            mine(&mut i);
        }
        FakeL1 {
            inner: Arc::new(Mutex::new(i)),
            sender: Some(Address::repeat_byte(0x1a)),
        }
    }

    /// Another sender on the same chain.
    pub fn with_sender(&self, byte: u8) -> FakeL1 {
        FakeL1 {
            inner: self.inner.clone(),
            sender: Some(Address::repeat_byte(byte)),
        }
    }

    /// The same chain for a node without an Ethereum key.
    pub fn observer(&self) -> FakeL1 {
        FakeL1 {
            inner: self.inner.clone(),
            sender: None,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, L1Inner> {
        self.inner.lock().unwrap()
    }

    /// OP's view of Ethereum: the head minus the lag, or the pinned block.
    pub fn origin(&self) -> L1Origin {
        origin_of(&self.lock())
    }

    pub fn mine(&self, n: u64) {
        let mut i = self.lock();
        for _ in 0..n {
            mine(&mut i);
        }
    }

    pub fn head_number(&self) -> u64 {
        head_of(&self.lock())
    }

    pub fn set_lag(&self, lag: u64) {
        self.lock().lag = lag;
    }

    pub fn pin_origin(&self, n: Option<u64>) {
        self.lock().pinned = n;
    }

    pub fn hold(&self, on: bool) {
        self.lock().hold = on;
    }

    pub fn set_auto_mine(&self, on: bool) {
        self.lock().auto_mine = on;
    }

    /// Mines pooled transactions only up to `n` mined in all; `None` lifts it.
    pub fn limit_mined(&self, n: Option<usize>) {
        self.lock().mine_limit = n;
    }

    pub fn drop_next(&self, n: u32) {
        self.lock().drop_next = n;
    }

    /// The L1 node drops pending `tx` from its pool (an eviction).
    pub fn forget(&self, tx: B256) {
        self.lock().pool.retain(|_, t| t.hash != tx);
    }

    pub fn refuse_next(&self, n: u32) {
        self.lock().refuse_next = n;
    }

    /// `build_proof` fails for `d` from now (a lagging or pruning endpoint).
    pub fn fail_proofs_for(&self, d: std::time::Duration) {
        self.lock().proofs_fail_until = Some(tokio::time::Instant::now() + d);
    }

    /// Every `build_proof` fails with `f` until set to `None`.
    pub fn fault_proofs(&self, f: Option<ProofFault>) {
        self.lock().proof_fault = f;
    }

    pub fn market(&self) -> L1Market {
        self.lock().market
    }

    pub fn set_market(&self, m: L1Market) {
        self.lock().market = m;
    }

    /// Drops the newest `depth` blocks with their transactions (they do not
    /// return to the pool) and mines as many empty ones.
    pub fn reorg(&self, depth: u64) {
        let mut i = self.lock();
        let keep = i.blocks.len().saturating_sub(depth as usize).max(1);
        i.blocks.truncate(keep);
        i.epoch = i.epoch.wrapping_add(1);
        let hold = std::mem::replace(&mut i.hold, true);
        for _ in 0..depth {
            mine(&mut i);
        }
        i.hold = hold;
    }

    pub fn sends(&self) -> Vec<L1Send> {
        self.lock().sends.clone()
    }

    /// Blob transactions mined, any sender.
    pub fn mined_posts(&self) -> usize {
        self.lock().blocks.iter().map(|b| b.txs.len()).sum()
    }

    pub fn block_of(&self, tx: &B256) -> Option<u64> {
        locate(&self.lock(), tx).map(|(n, _)| n)
    }

    pub fn blob(&self, vh: &B256) -> Option<Blob> {
        self.lock().blobs.get(vh).cloned()
    }

    /// A pending transaction of this sender at `nonce` that no record knows
    /// (a lost database).
    pub fn inject_orphan(&self, nonce: u64, fees: L1Fees) -> B256 {
        let from = self.sender.expect("a sender");
        let (hash, envelope) = envelope(from, nonce, &fees, &[B256::repeat_byte(1)]);
        self.lock().pool.insert(
            (from, nonce),
            FakeTx {
                hash,
                from,
                nonce,
                fees,
                envelope,
            },
        );
        hash
    }

    /// Another sequencer posts `blobs`, mined at once; returns the block.
    pub fn post_foreign(&self, blobs: &TransitionBlobs) -> u64 {
        let from = Address::repeat_byte(0xee);
        let vhs: Vec<B256> = blobs
            .versioned_hashes
            .iter()
            .map(|h| B256::from(*h))
            .collect();
        let mut i = self.lock();
        let nonce = mined_count(&i, from);
        let fees = L1Fees {
            max_fee: 1,
            tip: 1,
            blob_fee: 1,
        };
        let (hash, envelope) = envelope(from, nonce, &fees, &vhs);
        for (vh, b) in vhs.iter().zip(&blobs.blobs) {
            i.blobs.insert(*vh, b.clone());
        }
        i.pool.insert(
            (from, nonce),
            FakeTx {
                hash,
                from,
                nonce,
                fees,
                envelope,
            },
        );
        let hold = std::mem::replace(&mut i.hold, false);
        mine(&mut i);
        i.hold = hold;
        head_of(&i)
    }
}

#[async_trait]
impl L1Chain for FakeL1 {
    fn sender(&self) -> Option<Address> {
        self.sender
    }

    async fn head(&self) -> Result<u64, Web3Error> {
        let mut i = self.lock();
        if i.auto_mine {
            mine(&mut i);
        }
        Ok(head_of(&i))
    }

    async fn nonce(&self) -> Result<u64, Web3Error> {
        let from = self.sender.ok_or(Web3Error::NoSigner)?;
        Ok(mined_count(&self.lock(), from))
    }

    async fn fees(&self) -> Result<L1Market, Web3Error> {
        Ok(self.lock().market)
    }

    async fn sign_blob_tx(
        &self,
        nonce: u64,
        fees: &L1Fees,
        blobs: &TransitionBlobs,
    ) -> Result<SignedBlobTx, Web3Error> {
        let from = self.sender.ok_or(Web3Error::NoSigner)?;
        let vhs: Vec<B256> = blobs
            .versioned_hashes
            .iter()
            .map(|h| B256::from(*h))
            .collect();
        let (hash, envelope) = envelope(from, nonce, fees, &vhs);
        let mut i = self.lock();
        for (vh, b) in vhs.iter().zip(&blobs.blobs) {
            i.blobs.insert(*vh, b.clone());
        }
        i.signed.insert(
            hash,
            FakeTx {
                hash,
                from,
                nonce,
                fees: *fees,
                envelope,
            },
        );
        Ok(SignedBlobTx {
            hash,
            nonce,
            fees: *fees,
            envelope: None,
        })
    }

    // The blob pool's rules: the next nonce only, a replacement must double
    // every fee.
    async fn send_blob_tx(&self, tx: &SignedBlobTx) -> Result<(), L1SendError> {
        let mut i = self.lock();
        if i.refuse_next > 0 {
            i.refuse_next -= 1;
            return Err(L1SendError::Refused("413 payload too large".into()));
        }
        let t = i
            .signed
            .get(&tx.hash)
            .cloned()
            .ok_or_else(|| L1SendError::Refused("unknown transaction".into()))?;
        let head = head_of(&i);
        i.sends.push(L1Send {
            from: t.from,
            nonce: t.nonce,
            fees: t.fees,
            hash: t.hash,
            head,
        });
        let count = mined_count(&i, t.from);
        if t.nonce < count {
            return Err(L1SendError::NonceTaken(format!(
                "nonce too low: next nonce {count}, tx nonce {}",
                t.nonce
            )));
        }
        if t.nonce > count {
            return Err(L1SendError::Gap("nonce too high".into()));
        }
        if let Some(old) = i.pool.get(&(t.from, t.nonce)) {
            if old.hash == t.hash {
                return Ok(());
            }
            let (o, n) = (old.fees, t.fees);
            if n.max_fee < 2 * o.max_fee || n.tip < 2 * o.tip || n.blob_fee < 2 * o.blob_fee {
                return Err(L1SendError::Underpriced(
                    "replacement transaction underpriced".into(),
                ));
            }
        }
        if i.drop_next > 0 {
            i.drop_next -= 1;
            return Ok(());
        }
        i.pool.insert((t.from, t.nonce), t);
        Ok(())
    }

    async fn receipt(&self, tx: B256) -> Result<Option<L1Receipt>, Web3Error> {
        Ok(locate(&self.lock(), &tx).map(|(block, index)| L1Receipt {
            block,
            index: index as u64,
        }))
    }

    async fn tx_known(&self, tx: B256) -> Result<bool, Web3Error> {
        let i = self.lock();
        Ok(i.pool.values().any(|t| t.hash == tx) || locate(&i, &tx).is_some())
    }

    // `l1proof::build` on this chain's blocks.
    async fn build_proof(&self, txs: &[B256], top: u64) -> Result<L1BlobProof, Web3Error> {
        let i = self.lock();
        if i.proofs_fail_until
            .is_some_and(|t| tokio::time::Instant::now() < t)
        {
            return Err(Web3Error::Rpc("scripted proof failure".into()));
        }
        if i.proof_fault == Some(ProofFault::Data) {
            return Err(Web3Error::Data(
                "header of block 1 does not hash to its block hash".into(),
            ));
        }
        let located = txs
            .iter()
            .map(|h| locate(&i, h).ok_or_else(|| Web3Error::Rpc(format!("no receipt for {h}"))))
            .collect::<Result<Vec<_>, _>>()?;
        let oldest = located
            .iter()
            .map(|(n, _)| *n)
            .min()
            .ok_or_else(|| Web3Error::Data("no transactions".into()))?;
        if top < oldest || top > head_of(&i) {
            return Err(Web3Error::Rpc(format!("top {top} out of range")));
        }
        let mut headers = (oldest..=top)
            .rev()
            .map(|n| {
                let b = &i.blocks[n as usize];
                header_rlp(&b.header, b.hash).map_err(|e| Web3Error::Data(e.to_string()))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let txs = located
            .iter()
            .map(|(n, idx)| {
                let envs: Vec<Bytes> = i.blocks[*n as usize]
                    .txs
                    .iter()
                    .map(|t| t.envelope.clone())
                    .collect();
                let (_, proof) = tx_trie_proof(&envs, *idx);
                let above = top
                    .checked_sub(*n)
                    .ok_or_else(|| Web3Error::Rpc(format!("block {n} is above top {top}")))?;
                Ok(L1TxProof {
                    header_index: above as usize,
                    key: tx_key(*idx),
                    proof,
                })
            })
            .collect::<Result<Vec<_>, Web3Error>>()?;
        if i.proof_fault == Some(ProofFault::BrokenChain)
            && let Some(last) = headers.last().cloned()
        {
            headers.push(last);
        }
        Ok(L1BlobProof { headers, txs })
    }
}
