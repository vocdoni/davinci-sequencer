//! Per-process state: the arbo SMT plus the slot store, occupied list and
//! committed record, all in one `arbo::Storage`.
//!
//! Layout: arbo nodes (32-byte hash keys) plus
//! `b"slot/" || slot_be8 -> compressed active ballot`,
//! `b"occ" -> sorted occupied slot list (u64 LE each)`,
//! `b"committed" -> root || voters || overwrites || compressed accumulator`.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::{Arc, LazyLock};

use arbo::{Sha256, Storage, Tree, WriteBatch};
use davinci_zkvm_sdk::ballot::Ballot;
use davinci_zkvm_sdk::crypto::babyjubjub::Point;
use davinci_zkvm_sdk::crypto::elgamal::Ciphertext;
use davinci_zkvm_sdk::groth16::BallotVerifier;
use davinci_zkvm_sdk::limits::{
    MAX_BATCH_SIZE, MAX_BLOBS, MAX_REFRESH, NUM_FIELDS, SMT_LEVELS, TX_BLOB_CAP,
};
use davinci_zkvm_sdk::types::SnarkJsVk;
use davinci_zkvm_sdk::{blob, limits, release};

use crate::config::{ProcessConfig, genesis_leaves, key_bytes};
use crate::error::Error;
use crate::transition::PreparedBatch;
use crate::validate::VerifiedVote;

/// What [`ProcessState::select_covered`] picked.
#[derive(Debug)]
pub struct Selection<'a> {
    pub votes: Vec<&'a VerifiedVote>,
    /// A vote was turned away (it waits for a later batch).
    pub blocked: bool,
    /// The batch is full: no further vote fits for its size alone.
    pub full: bool,
    /// The slots the selection's seal must refresh.
    pub cover: BTreeSet<u64>,
}

/// Per-vote coverage for a selection: the history entries (slot sets) and,
/// per vote id, the entries naming it. A seal of a vote refreshes every
/// slot of every entry naming it.
#[derive(Debug, Default, Clone)]
pub struct Coverage {
    entries: Vec<BTreeSet<u64>>,
    by_vote: HashMap<u64, Vec<usize>>,
    /// The vote ids the histories were read for; `None`: any (no history).
    requested: Option<HashSet<u64>>,
}

impl Coverage {
    /// An empty coverage read for `vids`: a selection takes no other vote,
    /// whose history it does not know.
    pub fn for_votes(vids: &[u64]) -> Self {
        Coverage {
            requested: Some(vids.iter().copied().collect()),
            ..Default::default()
        }
    }

    /// Whether `vid`'s history was read (a selection may charge it).
    pub fn covers(&self, vid: u64) -> bool {
        self.requested.as_ref().is_none_or(|r| r.contains(&vid))
    }

    /// Adds an entry naming `vids`.
    pub fn add(&mut self, slots: BTreeSet<u64>, vids: &[u64]) {
        let i = self.entries.len();
        self.entries.push(slots);
        for v in vids {
            self.by_vote.entry(*v).or_default().push(i);
        }
    }

    fn entries_of(&self, vid: u64) -> &[usize] {
        self.by_vote.get(&vid).map_or(&[], Vec::as_slice)
    }

    /// Every slot the entries naming any of `vids` hold.
    pub fn union(&self, vids: &[u64]) -> BTreeSet<u64> {
        let idx: BTreeSet<usize> = vids
            .iter()
            .flat_map(|v| self.entries_of(*v).iter().copied())
            .collect();
        idx.iter()
            .flat_map(|i| self.entries[*i].iter().copied())
            .collect()
    }
}

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
    fn release_snapshot(&self) {
        self.0.release_snapshot()
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
    /// The process ballot VK, hash-checked against the 0x07 leaf; shared.
    pub(crate) vk: Arc<SnarkJsVk>,
    /// Most blobs one transition may use: the chain's per-tx limit, or the
    /// guest's `MAX_BLOBS` when the blobs go out in several transactions.
    pub(crate) blob_cap: usize,
}

/// The embedded ballot vk and its hash, or why it does not parse.
type EmbeddedVk = Result<(Arc<SnarkJsVk>, [u8; 32]), String>;

/// [`EmbeddedVk`], parsed once: every process shares the one protocol key,
/// and a parse per actor cost ~5 ms and a copy each.
static EMBEDDED_VK: LazyLock<EmbeddedVk> = LazyLock::new(|| {
    let vk: SnarkJsVk = serde_json::from_str(release::ballot_vk_json())
        .map_err(|e| format!("embedded ballot vk: {e}"))?;
    let hash = BallotVerifier::from_snarkjs(&vk)
        .map_err(|e| format!("embedded ballot vk: {e}"))?
        .vk_hash();
    Ok((Arc::new(vk), hash))
});

fn load_vk(cfg: &ProcessConfig) -> Result<Arc<SnarkJsVk>, Error> {
    let (vk, hash) = EMBEDDED_VK
        .as_ref()
        .map_err(|e| Error::Invalid(e.clone()))?;
    if *hash != cfg.ballot_vk_hash {
        return Err(Error::Invalid(
            "process ballot vk hash differs from the protocol vk".into(),
        ));
    }
    Ok(vk.clone())
}

impl<S: Storage> ProcessState<S> {
    /// Creates the genesis state in a `storage` holding no process. The
    /// committed record is written last, so a tree without one is a genesis
    /// a crash cut short: it starts over from the empty root (no slot was
    /// ever written without the record).
    pub fn create(cfg: ProcessConfig, storage: S) -> Result<Self, Error> {
        let db = Shared(Arc::new(storage));
        if db.get(KEY_COMMITTED)?.is_some() {
            return Err(Error::Invalid("storage already holds a process".into()));
        }
        let vk = load_vk(&cfg)?;
        let mut tree = Tree::new(db.clone(), SMT_LEVELS, Sha256)?;
        if tree.root() != arbo::EMPTY_HASH {
            tree.set_root(&arbo::EMPTY_HASH)?;
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
        Self::open_at(cfg, storage, Some(committed))
    }

    /// Opens an existing state at the committed record it stores, which is
    /// written in one batch with the slot store it describes, and rewinds
    /// to it. Fails with "storage holds no process" before `create`.
    pub fn open_stored(cfg: ProcessConfig, storage: S) -> Result<Self, Error> {
        Self::open_at(cfg, storage, None)
    }

    // `want`, when given, must equal the stored record.
    fn open_at(cfg: ProcessConfig, storage: S, want: Option<&Committed>) -> Result<Self, Error> {
        let db = Shared(Arc::new(storage));
        let vk = load_vk(&cfg)?;
        let stored = db
            .get(KEY_COMMITTED)?
            .ok_or_else(|| Error::Invalid("storage holds no process".into()))?;
        let committed = committed_from_bytes(&stored)?;
        if want.is_some_and(|w| *w != committed) {
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
            committed,
            occupied: occ_from_bytes(&occ)?,
            vk,
            blob_cap: TX_BLOB_CAP,
        })
    }

    pub fn config(&self) -> &ProcessConfig {
        &self.cfg
    }

    /// Caps the blobs of every batch `select_batch` and `prepare` size
    /// (`TX_BLOB_CAP` by default): lower for chains that take fewer per
    /// transaction, up to the guest's `MAX_BLOBS` for a node that posts one
    /// transition in several transactions.
    pub fn set_blob_cap(&mut self, cap: usize) -> Result<(), Error> {
        if cap == 0 || cap > MAX_BLOBS {
            return Err(Error::Invalid(format!(
                "blob cap {cap} not in 1..={MAX_BLOBS}"
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
    pub fn set_census_root(&mut self, root: [u8; 32]) {
        self.cfg.census_root = root;
    }

    /// Current tree root (equals `committed().root` outside a prepare).
    pub fn root(&self) -> [u8; 32] {
        self.tree.root()
    }

    pub fn committed(&self) -> Committed {
        self.committed.clone()
    }

    /// Drops the storage's cached read snapshot. Call when idle: an open
    /// snapshot keeps the database from reusing pages freed after it.
    pub fn release_reads(&self) {
        self.db.release_snapshot();
    }

    /// Number of occupied ballot slots.
    pub fn occupied(&self) -> usize {
        self.occupied.len()
    }

    /// Whether `vid` is in the committed tree. Read at the committed root,
    /// never the tree's current one, which a prepare (or one whose reset
    /// failed) has moved ahead.
    pub fn has_vote_id(&self, vid: u64) -> Result<bool, Error> {
        Ok(self
            .tree
            .gen_proof_at(&self.committed.root, &key_bytes(vid))?
            .exists)
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

    /// FIFO batch selection: at most one vote per slot and per address, no
    /// reused vote id, the on-chain `max_voters` cap on distinct voters, at
    /// most `max_votes` (and `MAX_BATCH_SIZE`) votes, and the whole transition (refreshes
    /// included) within the blob cap ([`Self::set_blob_cap`]). Votes that
    /// don't fit are left for a later batch; the flag is true when the blob
    /// or refresh cap turned one away (the allowed blobs are full).
    ///
    /// The refresh count is at least the live `must_include` slots (the
    /// exposed set `prepare` will keep), so a large exposed set shrinks the
    /// batch. If a vote does not fit even alone next to it (more than
    /// `MAX_REFRESH` live slots, or the blob cap), this returns
    /// `RefreshOverflow`.
    pub fn select_batch<'a>(
        &self,
        pending: &'a [VerifiedVote],
        max_voters: u64,
        must_include: &BTreeSet<u64>,
        max_votes: usize,
    ) -> Result<(Vec<&'a VerifiedVote>, bool), Error> {
        self.select_batch_within(pending, max_voters, must_include, max_votes, self.blob_cap)
    }

    /// [`Self::select_batch`] within at most `max_blobs` blobs (never above
    /// the blob cap): a node whose landing time fits fewer blob
    /// transactions sizes the batch down to them. A vote that fits the cap
    /// but not `max_blobs` is skipped (`Ok` with the flag set: wait for a
    /// wider window); `RefreshOverflow` means the blob cap or `MAX_REFRESH`
    /// binds (permanent). `prepare` keeps checking the blob cap, which a
    /// selection made here never passes.
    pub fn select_batch_within<'a>(
        &self,
        pending: &'a [VerifiedVote],
        max_voters: u64,
        must_include: &BTreeSet<u64>,
        max_votes: usize,
        max_blobs: usize,
    ) -> Result<(Vec<&'a VerifiedVote>, bool), Error> {
        let none = Coverage::default();
        let sel = self.select_covered(
            pending,
            max_voters,
            must_include,
            &none,
            max_votes,
            max_blobs,
        )?;
        Ok((sel.votes, sel.blocked))
    }

    /// [`Self::select_batch_within`] with per-vote coverage: a vote is
    /// charged `base` plus the histories (`cov`) of the votes selected so
    /// far and its own, never those of votes left out. A vote that does not
    /// fit is skipped (its slot too, keeping cast order), the rest go on.
    /// `CoverageOverflow` names a vote whose own history does not fit even
    /// alone at the blob cap; `RefreshOverflow` means `base` alone does not.
    /// Returns the selection, whether a vote was turned away (`blocked`) and
    /// whether that was the batch being full (`full`: `max_votes` reached, or
    /// a vote bringing no history of its own did not fit), and the slots its
    /// seal must refresh (`base` and the selected votes' histories).
    pub fn select_covered<'a>(
        &self,
        pending: &'a [VerifiedVote],
        max_voters: u64,
        base: &BTreeSet<u64>,
        cov: &Coverage,
        max_votes: usize,
        max_blobs: usize,
    ) -> Result<Selection<'a>, Error> {
        if max_blobs == 0 {
            return Err(Error::Invalid("max_blobs must be at least 1".into()));
        }
        let cap = max_blobs.min(self.blob_cap);
        let nf = self.cfg.ballot_mode.num_fields;
        let occupied_before = self.occupied.len();
        let live = |s: &u64| self.occupied.binary_search(s).is_ok();
        // Whether a batch of `n` votes, `w` of them overwrites, refreshing
        // at least `e` must slots fits `cap` blobs.
        let fits = |n: usize, w: usize, e: usize, cap: usize| {
            let r = limits::required_refresh(n, w, occupied_before).max(e);
            r <= MAX_REFRESH && blob::blob_count(n, n + r, nf) <= cap
        };
        let mut out: Vec<&VerifiedVote> = Vec::new();
        let mut slots: HashSet<u64> = HashSet::new();
        // Slots of votes turned away: later votes of the slot wait too.
        let mut held: HashSet<u64> = HashSet::new();
        let mut vids: HashSet<u64> = HashSet::new();
        // The guest refuses one voter address twice in a batch (3B.3). Only
        // CSP can trip it: there the slot is the index, not the address.
        let mut addrs: HashSet<[u8; 20]> = HashSet::new();
        let mut cover: BTreeSet<u64> = base.clone();
        let mut used: HashSet<usize> = HashSet::new();
        let mut new_distinct = 0u64;
        let mut w = 0usize;
        // Live cover slots that stay refresh candidates (not written).
        let mut exposed = cover.iter().filter(|s| live(s)).count();
        let mut blocked = false;
        let mut full = false;
        for v in pending {
            if out.len() >= max_votes.min(MAX_BATCH_SIZE) {
                full = true;
                break;
            }
            if slots.contains(&v.slot) || held.contains(&v.slot) || vids.contains(&v.pkg.vote_id) {
                continue;
            }
            // Waits for a later batch; later votes of its slot wait too.
            if addrs.contains(&v.pkg.address) {
                held.insert(v.slot);
                continue;
            }
            // A vote whose history was not read would be charged nothing.
            if !cov.covers(v.pkg.vote_id) {
                continue;
            }
            // A reused vote id would fail the guest's INSERT; a storage
            // error is the caller's problem, not a skip.
            if self.has_vote_id(v.pkg.vote_id)? {
                continue;
            }
            let overwrite = live(&v.slot);
            if !overwrite && self.occupied.len() as u64 + new_distinct + 1 > max_voters {
                continue;
            }
            // Its history beyond the cover so far.
            let entries: Vec<usize> = cov
                .entries_of(v.pkg.vote_id)
                .iter()
                .copied()
                .filter(|i| !used.contains(i))
                .collect();
            let added: BTreeSet<u64> = entries
                .iter()
                .flat_map(|i| cov.entries[*i].iter().copied())
                .filter(|s| !cover.contains(s))
                .collect();
            let add_live = added
                .iter()
                .filter(|s| live(s) && !slots.contains(s))
                .count();
            // An overwritten cover slot is written, not refreshed.
            let own = overwrite && (cover.contains(&v.slot) || added.contains(&v.slot));
            let e1 = (exposed + add_live).saturating_sub(own as usize);
            let w1 = w + overwrite as usize;
            if !fits(out.len() + 1, w1, e1, cap) {
                blocked = true;
                full |= added.is_empty();
                held.insert(v.slot);
                // Alone, at the blob cap: does it ever fit?
                let mut alone: BTreeSet<u64> = base.clone();
                for i in cov.entries_of(v.pkg.vote_id) {
                    alone.extend(cov.entries[*i].iter().copied());
                }
                let mine = alone.iter().filter(|s| live(s)).count();
                let e = mine.saturating_sub((overwrite && alone.contains(&v.slot)) as usize);
                if !fits(1, overwrite as usize, e, self.blob_cap) {
                    let base_live = base.iter().filter(|s| live(s)).count();
                    if fits(1, overwrite as usize, base_live, self.blob_cap) {
                        return Err(Error::CoverageOverflow {
                            vote_id: v.pkg.vote_id,
                            slots: e,
                        });
                    }
                    return Err(Error::RefreshOverflow {
                        exposed: base_live,
                        votes: 1,
                    });
                }
                continue;
            }
            cover.extend(added);
            used.extend(entries);
            w = w1;
            exposed = e1;
            if !overwrite {
                new_distinct += 1;
            }
            slots.insert(v.slot);
            vids.insert(v.pkg.vote_id);
            addrs.insert(v.pkg.address);
            out.push(v);
        }
        Ok(Selection {
            votes: out,
            blocked,
            full,
            cover,
        })
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

    /// Whether this storage holds tree root `root`: one our tree reached at
    /// some point (nodes are keyed by hash and never deleted).
    pub fn has_root(&self, root: &[u8; 32]) -> Result<bool, Error> {
        Ok(*root == arbo::EMPTY_HASH || self.db.get(root)?.is_some())
    }

    /// Points the tree back at the committed root, after a failed prepare
    /// whose own reset failed. The committed record is authoritative.
    pub fn reset_to_committed(&mut self) -> Result<(), Error> {
        Ok(self.tree.set_root(&self.committed.root)?)
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
