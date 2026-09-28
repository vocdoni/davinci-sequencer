//! Per-process state: the arbo SMT plus the slot store, occupied list and
//! committed record, all in one `arbo::Storage`.
//!
//! Layout: arbo nodes (32-byte hash keys) plus
//! `b"slot/" || slot_be8 -> compressed active ballot`,
//! `b"occ" -> sorted occupied slot list (u64 LE each)`,
//! `b"committed" -> root || voters || overwrites || compressed accumulator`.

use std::collections::{BTreeSet, HashSet};
use std::sync::Arc;

use arbo::{Sha256, Storage, Tree, WriteBatch};
use davinci_zkvm_sdk::ballot::Ballot;
use davinci_zkvm_sdk::crypto::babyjubjub::Point;
use davinci_zkvm_sdk::crypto::elgamal::Ciphertext;
use davinci_zkvm_sdk::groth16::BallotVerifier;
use davinci_zkvm_sdk::limits::{MAX_BATCH_SIZE, MAX_REFRESH, NUM_FIELDS, SMT_LEVELS, TX_BLOB_CAP};
use davinci_zkvm_sdk::types::SnarkJsVk;
use davinci_zkvm_sdk::{blob, limits, release};

use crate::config::{ProcessConfig, genesis_leaves, key_bytes};
use crate::error::Error;
use crate::transition::PreparedBatch;
use crate::validate::VerifiedVote;

const KEY_OCC: &[u8] = b"occ";
const KEY_COMMITTED: &[u8] = b"committed";

fn slot_db_key(slot: u64) -> Vec<u8> {
    let mut k = b"slot/".to_vec();
    k.extend_from_slice(&slot.to_be_bytes());
    k
}

/// Shared handle so the tree and the slot store use one `S`.
pub(crate) struct Shared<S>(pub(crate) Arc<S>);

impl<S> Clone for Shared<S> {
    fn clone(&self) -> Self {
        Shared(self.0.clone())
    }
}

impl<S: Storage> Storage for Shared<S> {
    fn get(&self, k: &[u8]) -> Result<Option<Vec<u8>>, arbo::Error> {
        self.0.get(k)
    }
    fn write(&self, batch: WriteBatch) -> Result<(), arbo::Error> {
        self.0.write(batch)
    }
}

/// The last proven-and-accepted state.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Committed {
    pub root: [u8; 32],
    pub voters: u64,
    pub overwrites: u64,
    pub accumulator: Ballot,
}

pub(crate) fn ballot_to_bytes(b: &Ballot) -> Vec<u8> {
    let mut out = Vec::with_capacity(NUM_FIELDS * 64);
    for ct in &b.0 {
        out.extend_from_slice(&ct.c1.compress());
        out.extend_from_slice(&ct.c2.compress());
    }
    out
}

pub(crate) fn ballot_from_bytes(b: &[u8]) -> Result<Ballot, Error> {
    if b.len() != NUM_FIELDS * 64 {
        return Err(Error::Invalid(format!("stored ballot: {} bytes", b.len())));
    }
    let mut out = Ballot::identity();
    for (i, ct) in out.0.iter_mut().enumerate() {
        let p = |o: usize| -> Result<Point, Error> {
            let c: [u8; 32] = b[o..o + 32].try_into().expect("length checked");
            Ok(Point::decompress(&c)?)
        };
        *ct = Ciphertext {
            c1: p(i * 64)?,
            c2: p(i * 64 + 32)?,
        };
    }
    Ok(out)
}

fn committed_to_bytes(c: &Committed) -> Vec<u8> {
    let mut out = Vec::with_capacity(48 + NUM_FIELDS * 64);
    out.extend_from_slice(&c.root);
    out.extend_from_slice(&c.voters.to_le_bytes());
    out.extend_from_slice(&c.overwrites.to_le_bytes());
    out.extend_from_slice(&ballot_to_bytes(&c.accumulator));
    out
}

fn committed_from_bytes(b: &[u8]) -> Result<Committed, Error> {
    if b.len() != 48 + NUM_FIELDS * 64 {
        return Err(Error::Invalid(format!(
            "stored committed: {} bytes",
            b.len()
        )));
    }
    Ok(Committed {
        root: b[..32].try_into().expect("length checked"),
        voters: u64::from_le_bytes(b[32..40].try_into().expect("length checked")),
        overwrites: u64::from_le_bytes(b[40..48].try_into().expect("length checked")),
        accumulator: ballot_from_bytes(&b[48..])?,
    })
}

fn occ_to_bytes(occ: &[u64]) -> Vec<u8> {
    occ.iter().flat_map(|s| s.to_le_bytes()).collect()
}

fn occ_from_bytes(b: &[u8]) -> Result<Vec<u64>, Error> {
    if !b.len().is_multiple_of(8) {
        return Err(Error::Invalid(format!("stored occ: {} bytes", b.len())));
    }
    Ok(b.chunks_exact(8)
        .map(|c| u64::from_le_bytes(c.try_into().expect("length checked")))
        .collect())
}

/// Per-process election state over an `arbo::Storage`.
pub struct ProcessState<S: Storage> {
    pub(crate) cfg: ProcessConfig,
    pub(crate) db: Shared<S>,
    pub(crate) tree: Tree<Shared<S>, Sha256>,
    pub(crate) committed: Committed,
    /// Sorted occupied ballot slots.
    pub(crate) occupied: Vec<u64>,
    /// The process ballot VK, hash-checked against the 0x07 leaf.
    pub(crate) vk: SnarkJsVk,
    /// Most blobs one transition may use: the chain's per-tx limit.
    pub(crate) blob_cap: usize,
}

fn load_vk(cfg: &ProcessConfig) -> Result<SnarkJsVk, Error> {
    let vk: SnarkJsVk = serde_json::from_str(release::ballot_vk_json())
        .map_err(|e| Error::Invalid(format!("embedded ballot vk: {e}")))?;
    let hash = BallotVerifier::from_snarkjs(&vk)?.vk_hash();
    if hash != cfg.ballot_vk_hash {
        return Err(Error::Invalid(
            "process ballot vk hash differs from the protocol vk".into(),
        ));
    }
    Ok(vk)
}

impl<S: Storage> ProcessState<S> {
    /// Creates the genesis state in an empty `storage`.
    pub fn create(cfg: ProcessConfig, storage: S) -> Result<Self, Error> {
        let db = Shared(Arc::new(storage));
        if db.get(KEY_COMMITTED)?.is_some() {
            return Err(Error::Invalid("storage already holds a process".into()));
        }
        let vk = load_vk(&cfg)?;
        let mut tree = Tree::new(db.clone(), SMT_LEVELS, Sha256)?;
        if tree.root() != arbo::EMPTY_HASH {
            return Err(Error::Invalid("storage already holds a tree".into()));
        }
        for (k, v) in genesis_leaves(&cfg)? {
            tree.add(&key_bytes(k), &v)?;
        }
        let committed = Committed {
            root: tree.root(),
            voters: 0,
            overwrites: 0,
            accumulator: Ballot::identity(),
        };
        let mut batch = WriteBatch::new();
        batch.put(KEY_COMMITTED.to_vec(), committed_to_bytes(&committed));
        batch.put(KEY_OCC.to_vec(), Vec::new());
        db.write(batch)?;
        Ok(ProcessState {
            cfg,
            db,
            tree,
            committed,
            occupied: Vec::new(),
            vk,
            blob_cap: TX_BLOB_CAP,
        })
    }

    /// Opens an existing state and rewinds to `committed` (drops any
    /// prepared-but-uncommitted tree nodes).
    pub fn open(cfg: ProcessConfig, storage: S, committed: &Committed) -> Result<Self, Error> {
        let db = Shared(Arc::new(storage));
        let vk = load_vk(&cfg)?;
        let stored = db
            .get(KEY_COMMITTED)?
            .ok_or_else(|| Error::Invalid("storage holds no process".into()))?;
        let stored = committed_from_bytes(&stored)?;
        if stored != *committed {
            return Err(Error::Invalid(
                "stored committed record differs from the given one".into(),
            ));
        }
        let mut tree = Tree::new(db.clone(), SMT_LEVELS, Sha256)?;
        if tree.root() != committed.root {
            tree.set_root(&committed.root)?;
        }
        let occ = db.get(KEY_OCC)?.unwrap_or_default();
        Ok(ProcessState {
            cfg,
            db,
            tree,
            committed: committed.clone(),
            occupied: occ_from_bytes(&occ)?,
            vk,
            blob_cap: TX_BLOB_CAP,
        })
    }

    pub fn config(&self) -> &ProcessConfig {
        &self.cfg
    }

    /// Caps the blobs of every batch `select_batch` and `prepare` size, for
    /// chains that take fewer than `TX_BLOB_CAP` per transaction.
    pub fn set_blob_cap(&mut self, cap: usize) -> Result<(), Error> {
        if cap == 0 || cap > TX_BLOB_CAP {
            return Err(Error::Invalid(format!(
                "blob cap {cap} not in 1..={TX_BLOB_CAP}"
            )));
        }
        self.blob_cap = cap;
        Ok(())
    }

    pub fn blob_cap(&self) -> usize {
        self.blob_cap
    }

    /// Moves the census root the next `prepare` commits to (guest
    /// register 20). Dynamic censuses (origins 2/3) re-root between
    /// batches; the caller re-proves the batch's votes at this root first.
    /// The root is not a genesis leaf, so the tree does not move.
    pub fn set_census_root(&mut self, root: davinci_zkvm_sdk::crypto::field::Fr) {
        self.cfg.census_root = root;
    }

    /// Current tree root (equals `committed().root` outside a prepare).
    pub fn root(&self) -> [u8; 32] {
        self.tree.root()
    }

    pub fn committed(&self) -> Committed {
        self.committed.clone()
    }

    /// Number of occupied ballot slots.
    pub fn occupied(&self) -> usize {
        self.occupied.len()
    }

    pub fn has_vote_id(&self, vid: u64) -> Result<bool, Error> {
        Ok(self.tree.get(&key_bytes(vid))?.is_some())
    }

    /// The active (committed) ballot of `slot`, if occupied.
    pub fn slot_ballot(&self, slot: u64) -> Result<Option<Ballot>, Error> {
        match self.db.get(&slot_db_key(slot))? {
            Some(b) => Ok(Some(ballot_from_bytes(&b)?)),
            None => Ok(None),
        }
    }

    /// Inclusion proof of a vote id under the committed root.
    pub fn vote_id_proof(&self, vid: u64) -> Result<arbo::Proof, Error> {
        let p = self
            .tree
            .gen_proof_at(&self.committed.root, &key_bytes(vid))?;
        if !p.exists {
            return Err(Error::Invalid(format!("vote id {vid:#x} not in the tree")));
        }
        Ok(p)
    }

    /// FIFO batch selection: at most one vote per slot, no reused vote id,
    /// the on-chain `max_voters` cap on distinct voters, `MAX_BATCH_SIZE`,
    /// and the whole transition (refreshes included) within the blob cap
    /// (`TX_BLOB_CAP` unless [`Self::set_blob_cap`] lowered it). Votes that
    /// don't fit are left for a later batch.
    ///
    /// The refresh count is at least the live `must_include` slots (the
    /// exposed set `prepare` will keep), so a large exposed set shrinks the
    /// batch. If not even one vote fits next to it (more than `MAX_REFRESH`
    /// live slots, or the blob cap), this returns `RefreshOverflow`.
    pub fn select_batch<'a>(
        &self,
        pending: &'a [VerifiedVote],
        max_voters: u64,
        must_include: &BTreeSet<u64>,
    ) -> Result<Vec<&'a VerifiedVote>, Error> {
        let nf = self.cfg.ballot_mode.num_fields;
        let occupied_before = self.occupied.len();
        let mut out: Vec<&VerifiedVote> = Vec::new();
        let mut slots: HashSet<u64> = HashSet::new();
        let mut vids: HashSet<u64> = HashSet::new();
        let mut new_distinct = 0u64;
        let mut w = 0usize;
        // Live exposed slots that stay refresh candidates.
        let mut exposed = must_include
            .iter()
            .filter(|s| self.occupied.binary_search(s).is_ok())
            .count();
        let mut blocked = false;
        for v in pending {
            if out.len() == MAX_BATCH_SIZE {
                break;
            }
            if slots.contains(&v.slot) || vids.contains(&v.pkg.vote_id) {
                continue;
            }
            // A reused vote id would fail the guest's INSERT; a storage
            // error is the caller's problem, not a skip.
            if self.has_vote_id(v.pkg.vote_id)? {
                continue;
            }
            let overwrite = self.occupied.binary_search(&v.slot).is_ok();
            if !overwrite && self.occupied.len() as u64 + new_distinct + 1 > max_voters {
                continue;
            }
            // Blob cap: n votes plus the refreshes the guest will demand or
            // the exposed set forces, whichever is larger. An overwritten
            // exposed slot is written, not refreshed.
            let n = out.len() + 1;
            let w1 = w + overwrite as usize;
            let e1 = exposed.saturating_sub((overwrite && must_include.contains(&v.slot)) as usize);
            let r = limits::required_refresh(n, w1, occupied_before).max(e1);
            if r > MAX_REFRESH || blob::blob_count(n, n + r, nf) > self.blob_cap {
                blocked = true;
                continue;
            }
            w = w1;
            exposed = e1;
            if !overwrite {
                new_distinct += 1;
            }
            slots.insert(v.slot);
            vids.insert(v.pkg.vote_id);
            out.push(v);
        }
        // One vote with its own refreshes always fits, so an empty batch
        // here means the exposed set alone is too large.
        if out.is_empty() && blocked {
            return Err(Error::RefreshOverflow { exposed, votes: 1 });
        }
        Ok(out)
    }

    /// Applies a prepared batch after its proof was accepted: slot store,
    /// occupied list and the committed record move to the new root.
    pub fn commit(&mut self, b: &PreparedBatch) -> Result<(), Error> {
        if b.old_root != self.committed.root {
            return Err(Error::Invalid("batch was prepared on another root".into()));
        }
        if self.tree.root() != b.new_root {
            return Err(Error::Invalid("tree is not at the prepared root".into()));
        }
        let mut occ = self.occupied.clone();
        occ.extend_from_slice(&b.new_slots);
        occ.sort_unstable();
        let committed = Committed {
            root: b.new_root,
            voters: self.committed.voters + b.vote_ids.len() as u64,
            overwrites: self.committed.overwrites + b.overwrites as u64,
            accumulator: b.accumulator,
        };
        self.persist(&b.slot_writes, &occ, &committed)?;
        self.occupied = occ;
        self.committed = committed;
        Ok(())
    }

    /// Drops a prepared batch: the tree goes back to the committed root.
    /// Slot writes were never applied, so nothing else moves.
    pub fn rollback(&mut self, b: &PreparedBatch) -> Result<(), Error> {
        if b.old_root != self.committed.root {
            return Err(Error::Invalid("batch was prepared on another root".into()));
        }
        self.tree.set_root(&b.old_root)?;
        Ok(())
    }

    /// One atomic write of slot ballots, occupied list and committed record.
    /// Callers assign the in-memory copies only after this succeeds.
    pub(crate) fn persist(
        &self,
        slot_writes: &[(u64, Ballot)],
        occ: &[u64],
        committed: &Committed,
    ) -> Result<(), Error> {
        let mut batch = WriteBatch::new();
        for (slot, ballot) in slot_writes {
            batch.put(slot_db_key(*slot), ballot_to_bytes(ballot));
        }
        batch.put(KEY_OCC.to_vec(), occ_to_bytes(occ));
        batch.put(KEY_COMMITTED.to_vec(), committed_to_bytes(committed));
        self.db.write(batch)?;
        Ok(())
    }
}
