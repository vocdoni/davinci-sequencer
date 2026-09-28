//! The node's redb file, one per deployment
//! (`<datadir>/<chain id>-<registry>/sequencer.redb`, mode 0600): processes,
//! votes, the pending FIFO, transitions and their blobs, the election-key
//! master secret, census trees, meta counters, and one arbo table per process.
//!
//! Keys are big-endian so redb's byte order is the natural order: `pid` is the
//! process id as BE32, followed by BE8 vote ids / sequence numbers / indexes.

use std::collections::HashSet;
use std::fs::OpenOptions;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use davinci_zkvm_sdk::blob::{BLOB_SIZE, Blob};
use davinci_zkvm_sdk::crypto::field::{Fr, fr_to_be};
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition, WriteTransaction};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::web3::OnchainProcess;

/// Bump on any incompatible change to a table layout or record encoding.
pub const SCHEMA_VERSION: u64 = 6;
pub const SCHEMA_VERSION_KEY: &str = "schema_version";
/// The database file inside its directory.
pub const DB_FILE: &str = "sequencer.redb";
/// `meta_bytes` key naming the deployment a database belongs to.
const DEPLOYMENT_KEY: &str = "deployment";

type Bytes = TableDefinition<'static, &'static [u8], &'static [u8]>;

const META: TableDefinition<&str, u64> = TableDefinition::new("meta");
// Small node-level blobs (the monitor's bootstrap retry list).
const META_BYTES: Bytes = TableDefinition::new("meta_bytes");
const PROCESSES: Bytes = TableDefinition::new("processes");
const VOTES: Bytes = TableDefinition::new("votes");
const PENDING: Bytes = TableDefinition::new("pending");
// pid‖vid -> seq: makes pushes idempotent and removals direct.
const PENDING_IDX: Bytes = TableDefinition::new("pending_idx");
const TRANSITIONS: Bytes = TableDefinition::new("transitions");
// pid -> the actor's committed-state record (JSON, actor-owned format).
const COMMITTED: Bytes = TableDefinition::new("committed");
const BLOBS: Bytes = TableDefinition::new("blobs");
// One row: the master secret election keys are derived from (`keys.rs`).
const ENC_KEYS: Bytes = TableDefinition::new("enc_keys");
const MASTER_KEY: &[u8] = b"master";
const CENSUS: Bytes = TableDefinition::new("census");
const CENSUS_ADDR: Bytes = TableDefinition::new("census_addr");
// pid‖addr20 -> slot (u64 BE): O(1) address lookup for CSP ballots.
const VOTE_ADDR: Bytes = TableDefinition::new("vote_addr");
// pid -> `ExposedRecord`: slots a sealed batch exposed, until every
// recorded vote settles or errors.
const EXPOSED: Bytes = TableDefinition::new("exposed");

#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    #[error("storage: {0}")]
    Db(String),
    #[error(
        "schema version {found} in the data file, this build uses {expected}; records are not \
         migrated: start with a fresh DAVINCI_DATADIR (the chain is the source of truth)"
    )]
    Schema { found: u64, expected: u64 },
    #[error("corrupt {table} row: {reason}")]
    Corrupt { table: &'static str, reason: String },
    #[error("not found: {0}")]
    NotFound(String),
    #[error("vote {0} already exists")]
    VoteExists(u64),
    #[error("this database belongs to deployment {found}, not {expected}")]
    Deployment { found: String, expected: String },
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T, E = StorageError> = std::result::Result<T, E>;

fn db_err(e: impl std::fmt::Display) -> StorageError {
    StorageError::Db(e.to_string())
}

/// Vote lifecycle, with davinci-node's wire strings.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VoteStatus {
    Pending,
    /// In a sealed batch.
    Aggregated,
    /// Proven, not yet settled.
    Processed,
    Settled,
    Error,
}

impl VoteStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            VoteStatus::Pending => "pending",
            VoteStatus::Aggregated => "aggregated",
            VoteStatus::Processed => "processed",
            VoteStatus::Settled => "settled",
            VoteStatus::Error => "error",
        }
    }
}

/// What the node does with a process.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LocalStatus {
    Active,
    /// Not served (bad genesis, census unavailable, ...); see `note`.
    Ignored,
    /// Results settled on-chain.
    Finalized,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessRecord {
    #[serde(with = "fr_hex")]
    pub pid: Fr,
    pub onchain: OnchainProcess,
    pub local: LocalStatus,
    pub note: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredVote {
    #[serde(with = "fr_hex")]
    pub pid: Fr,
    pub vote_id: u64,
    #[serde(with = "hex::serde")]
    pub address: [u8; 20],
    pub slot: u64,
    /// The serialized vote package; the actor owns its format.
    #[serde(with = "hex::serde")]
    pub package: Vec<u8>,
    pub status: VoteStatus,
    pub error: Option<String>,
    /// Unix seconds.
    pub created_at: u64,
    pub updated_at: u64,
}

/// One settled transition of a process (ours or synced from another node).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransitionRecord {
    /// Position in the process's transition sequence, from 0.
    pub index: u64,
    #[serde(with = "hex::serde")]
    pub old_root: [u8; 32],
    #[serde(with = "hex::serde")]
    pub new_root: [u8; 32],
    #[serde(with = "hex::serde")]
    pub tx_hash: [u8; 32],
    pub block: u64,
    #[serde(with = "hex::serde")]
    pub sender: [u8; 20],
    /// Votes and overwrites in this batch.
    pub n_votes: u64,
    pub n_overwrites: u64,
    pub n_blobs: u64,
    pub by_self: bool,
}

/// The exposed set of a process: every ballot slot a sealed batch changed
/// (writes and refreshes as one list — storage must not tell them apart),
/// plus the vote ids of the attempts that exposed them. Kept until every
/// recorded vote is settled or errored.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExposedRecord {
    pub slots: Vec<u64>,
    pub vote_ids: Vec<u64>,
}

/// Shared handle to the node's database. Cloning is cheap.
#[derive(Clone)]
pub struct Db {
    db: Arc<Database>,
    /// Scripted arbo write failures (tests only; always 0 in production).
    arbo_faults: Arc<std::sync::atomic::AtomicU32>,
}

impl std::fmt::Debug for Db {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Db")
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn pid_key(pid: &Fr) -> [u8; 32] {
    fr_to_be(pid)
}

fn key_u64(pid: &Fr, n: u64) -> [u8; 40] {
    let mut k = [0u8; 40];
    k[..32].copy_from_slice(&pid_key(pid));
    k[32..].copy_from_slice(&n.to_be_bytes());
    k
}

fn addr_key(pid: &Fr, addr: &[u8; 20]) -> [u8; 52] {
    let mut k = [0u8; 52];
    k[..32].copy_from_slice(&pid_key(pid));
    k[32..].copy_from_slice(addr);
    k
}

fn blob_key(pid: &Fr, idx: u64, i: u16) -> [u8; 42] {
    let mut k = [0u8; 42];
    k[..40].copy_from_slice(&key_u64(pid, idx));
    k[40..].copy_from_slice(&i.to_be_bytes());
    k
}

/// `[prefix‖0x00.., prefix‖0xff..]` over keys of `len` bytes.
fn prefix_range(prefix: &[u8], len: usize) -> (Vec<u8>, Vec<u8>) {
    let mut lo = prefix.to_vec();
    lo.resize(len, 0);
    let mut hi = prefix.to_vec();
    hi.resize(len, 0xff);
    (lo, hi)
}

fn decode<T: DeserializeOwned>(table: &'static str, v: &[u8]) -> Result<T> {
    serde_json::from_slice(v).map_err(|e| StorageError::Corrupt {
        table,
        reason: e.to_string(),
    })
}

fn encode<T: Serialize>(v: &T) -> Result<Vec<u8>> {
    serde_json::to_vec(v).map_err(db_err)
}

fn u64_value(table: &'static str, v: &[u8]) -> Result<u64> {
    let b: [u8; 8] = v.try_into().map_err(|_| StorageError::Corrupt {
        table,
        reason: format!("{} bytes, want 8", v.len()),
    })?;
    Ok(u64::from_be_bytes(b))
}

impl Db {
    /// Opens or creates the database file, created with mode 0600 (an
    /// existing file is tightened to 0600). Refuses a different schema version.
    pub fn open(path: &Path) -> Result<Self> {
        if !path.exists() {
            OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(path)?;
        }
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        let db = Database::create(path).map_err(db_err)?;
        let this = Db {
            db: Arc::new(db),
            arbo_faults: Arc::new(std::sync::atomic::AtomicU32::new(0)),
        };
        this.init()?;
        Ok(this)
    }

    /// Opens `<dir>/sequencer.redb`, creating the directory (0700).
    pub fn open_in(dir: &Path) -> Result<Self> {
        private_dir(dir)?;
        Self::open(&dir.join(DB_FILE))
    }

    /// Opens the database of one deployment, [`deployment_dir`] under
    /// `datadir`, created on first use: another registry starts from an
    /// empty store and leaves this one alone. A database of the flat layout
    /// (`<datadir>/sequencer.redb`) moves in once if its processes carry this
    /// deployment's id prefix; otherwise it stays where it is.
    pub fn open_deployment(datadir: &Path, chain_id: u64, registry: &[u8; 20]) -> Result<Self> {
        private_dir(datadir)?;
        let dir = deployment_dir(datadir, chain_id, registry);
        adopt_flat(datadir, &dir, chain_id, registry)?;
        let db = Self::open_in(&dir)?;
        db.bind_deployment(&deployment_name(chain_id, registry))?;
        Ok(db)
    }

    // Records the deployment on first open; refuses a file of another one.
    fn bind_deployment(&self, name: &str) -> Result<()> {
        match self.meta_bytes(DEPLOYMENT_KEY)? {
            None => self.set_meta_bytes(DEPLOYMENT_KEY, name.as_bytes()),
            Some(v) if v == name.as_bytes() => Ok(()),
            Some(v) => Err(StorageError::Deployment {
                found: String::from_utf8_lossy(&v).into_owned(),
                expected: name.to_string(),
            }),
        }
    }

    fn init(&self) -> Result<()> {
        let tx = self.db.begin_write().map_err(db_err)?;
        {
            let mut meta = tx.open_table(META).map_err(db_err)?;
            let found = meta
                .get(SCHEMA_VERSION_KEY)
                .map_err(db_err)?
                .map(|v| v.value());
            match found {
                None => {
                    meta.insert(SCHEMA_VERSION_KEY, SCHEMA_VERSION)
                        .map_err(db_err)?;
                }
                Some(v) if v == SCHEMA_VERSION => {}
                Some(v) => {
                    return Err(StorageError::Schema {
                        found: v,
                        expected: SCHEMA_VERSION,
                    });
                }
            }
            for t in [
                PROCESSES,
                VOTES,
                PENDING,
                PENDING_IDX,
                TRANSITIONS,
                COMMITTED,
                BLOBS,
                ENC_KEYS,
                CENSUS,
                CENSUS_ADDR,
                VOTE_ADDR,
                META_BYTES,
                EXPOSED,
            ] {
                tx.open_table(t).map_err(db_err)?;
            }
        }
        tx.commit().map_err(db_err)
    }

    /// The underlying database, for components that keep their own tables.
    pub fn database(&self) -> Arc<Database> {
        self.db.clone()
    }

    /// Name of the arbo table of a process.
    pub fn arbo_table(pid: &Fr) -> String {
        format!("arbo_{}", hex::encode(pid_key(pid)))
    }

    /// The process's state tree storage, a table of its own in this file.
    pub fn arbo_storage(&self, pid: &Fr) -> Result<ArboStore> {
        let inner =
            arbo::RedbStorage::new(self.db.clone(), &Self::arbo_table(pid)).map_err(db_err)?;
        Ok(ArboStore {
            inner,
            faults: self.arbo_faults.clone(),
        })
    }

    /// Makes the next `n` arbo tree writes fail (any process). Test hook for
    /// commit-failure recovery; one relaxed atomic load per write otherwise.
    /// Compiled only for tests (`test-hooks` covers integration tests).
    #[cfg(any(test, feature = "test-hooks"))]
    pub fn fail_next_arbo_writes(&self, n: u32) {
        self.arbo_faults
            .store(n, std::sync::atomic::Ordering::SeqCst);
    }

    fn get(&self, table: Bytes, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let tx = self.db.begin_read().map_err(db_err)?;
        let t = tx.open_table(table).map_err(db_err)?;
        let v = t.get(key).map_err(db_err)?;
        Ok(v.map(|g| g.value().to_vec()))
    }

    fn put(&self, table: Bytes, key: &[u8], value: &[u8]) -> Result<()> {
        let tx = self.db.begin_write().map_err(db_err)?;
        {
            let mut t = tx.open_table(table).map_err(db_err)?;
            t.insert(key, value).map_err(db_err)?;
        }
        tx.commit().map_err(db_err)
    }

    // Rows whose key starts with `prefix`, all keys `len` bytes long.
    fn scan(&self, table: Bytes, prefix: &[u8], len: usize) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        let (lo, hi) = prefix_range(prefix, len);
        let tx = self.db.begin_read().map_err(db_err)?;
        let t = tx.open_table(table).map_err(db_err)?;
        let mut out = Vec::new();
        for row in t.range(lo.as_slice()..=hi.as_slice()).map_err(db_err)? {
            let (k, v) = row.map_err(db_err)?;
            out.push((k.value().to_vec(), v.value().to_vec()));
        }
        Ok(out)
    }

    pub fn put_process(&self, p: &ProcessRecord) -> Result<()> {
        self.put(PROCESSES, &pid_key(&p.pid), &encode(p)?)
    }

    pub fn process(&self, pid: &Fr) -> Result<Option<ProcessRecord>> {
        self.get(PROCESSES, &pid_key(pid))?
            .map(|v| decode("processes", &v))
            .transpose()
    }

    pub fn processes(&self) -> Result<Vec<ProcessRecord>> {
        let tx = self.db.begin_read().map_err(db_err)?;
        let t = tx.open_table(PROCESSES).map_err(db_err)?;
        let mut out = Vec::new();
        for row in t.iter().map_err(db_err)? {
            let (_, v) = row.map_err(db_err)?;
            out.push(decode("processes", v.value())?);
        }
        Ok(out)
    }

    pub fn put_vote(&self, v: &StoredVote) -> Result<()> {
        self.put(VOTES, &key_u64(&v.pid, v.vote_id), &encode(v)?)
    }

    pub fn vote(&self, pid: &Fr, vid: u64) -> Result<Option<StoredVote>> {
        self.get(VOTES, &key_u64(pid, vid))?
            .map(|v| decode("votes", &v))
            .transpose()
    }

    /// Every stored vote of a process, in vote-id order.
    pub fn votes(&self, pid: &Fr) -> Result<Vec<StoredVote>> {
        self.scan(VOTES, &pid_key(pid), 40)?
            .iter()
            .map(|(_, v)| decode("votes", v))
            .collect()
    }

    /// Stores the actor's committed-state record for a process.
    pub fn put_committed(&self, pid: &Fr, v: &[u8]) -> Result<()> {
        self.put(COMMITTED, &pid_key(pid), v)
    }

    pub fn committed(&self, pid: &Fr) -> Result<Option<Vec<u8>>> {
        self.get(COMMITTED, &pid_key(pid))
    }

    /// Persists the exposed set before a batch can broadcast. One
    /// slot list, writes and refreshes together — nothing stored may
    /// distinguish a refresh from an overwrite.
    pub fn put_exposed(&self, pid: &Fr, rec: &ExposedRecord) -> Result<()> {
        self.put(EXPOSED, &pid_key(pid), &encode(rec)?)
    }

    pub fn exposed(&self, pid: &Fr) -> Result<Option<ExposedRecord>> {
        self.get(EXPOSED, &pid_key(pid))?
            .map(|v| decode("exposed", &v))
            .transpose()
    }

    pub fn clear_exposed(&self, pid: &Fr) -> Result<()> {
        self.write(|tx| {
            let mut t = tx.open_table(EXPOSED).map_err(db_err)?;
            t.remove(pid_key(pid).as_slice()).map_err(db_err)?;
            Ok(())
        })
    }

    // One write transaction: `f` runs inside it, the commit happens only if
    // it succeeds (dropping an uncommitted transaction aborts it).
    fn write(&self, f: impl FnOnce(&WriteTransaction) -> Result<()>) -> Result<()> {
        let tx = self.db.begin_write().map_err(db_err)?;
        f(&tx)?;
        tx.commit().map_err(db_err)
    }

    /// Appends a vote id to the process's pending FIFO; a vid already
    /// pending keeps its place.
    pub fn push_pending(&self, pid: &Fr, vid: u64) -> Result<()> {
        self.write(|tx| tx_push(tx, pid, vid))
    }

    /// Pending vote ids in arrival order.
    pub fn pending(&self, pid: &Fr) -> Result<Vec<u64>> {
        self.scan(PENDING, &pid_key(pid), 40)?
            .iter()
            .map(|(_, v)| u64_value("pending", v))
            .collect()
    }

    pub fn remove_pending(&self, pid: &Fr, vids: &[u64]) -> Result<()> {
        self.write(|tx| tx_remove(tx, pid, vids))
    }

    /// Moves every listed vote to `s` atomically; any unknown vote aborts.
    pub fn set_status(
        &self,
        pid: &Fr,
        vids: &[u64],
        s: VoteStatus,
        err: Option<&str>,
    ) -> Result<()> {
        self.write(|tx| tx_status(tx, pid, vids, s, err))
    }

    /// Stores a new vote and queues it, in one commit. A vote id the
    /// process already has is refused ([`StorageError::VoteExists`]), so a
    /// settled vote can never be queued again.
    pub fn admit_vote(&self, v: &StoredVote) -> Result<()> {
        self.write(|tx| {
            let key = key_u64(&v.pid, v.vote_id);
            let mut t = tx.open_table(VOTES).map_err(db_err)?;
            if t.get(key.as_slice()).map_err(db_err)?.is_some() {
                return Err(StorageError::VoteExists(v.vote_id));
            }
            t.insert(key.as_slice(), encode(v)?.as_slice())
                .map_err(db_err)?;
            drop(t);
            let mut a = tx.open_table(VOTE_ADDR).map_err(db_err)?;
            a.insert(
                addr_key(&v.pid, &v.address).as_slice(),
                &v.slot.to_be_bytes()[..],
            )
            .map_err(db_err)?;
            drop(a);
            tx_push(tx, &v.pid, v.vote_id)
        })
    }

    /// The ballot slot of `addr` in `pid`, if that address ever voted here.
    pub fn vote_slot_by_address(&self, pid: &Fr, addr: &[u8; 20]) -> Result<Option<u64>> {
        self.get(VOTE_ADDR, &addr_key(pid, addr))?
            .map(|v| u64_value("vote_addr", &v))
            .transpose()
    }

    /// Votes enter a batch: `aggregated`, out of the pending queue.
    pub fn seal_batch(&self, pid: &Fr, vids: &[u64]) -> Result<()> {
        self.write(|tx| {
            tx_status(tx, pid, vids, VoteStatus::Aggregated, None)?;
            tx_remove(tx, pid, vids)
        })
    }

    /// Votes go back to `pending` (error cleared) and to the queue tail.
    /// Votes still queued keep their place.
    pub fn requeue(&self, pid: &Fr, vids: &[u64]) -> Result<()> {
        self.write(|tx| {
            tx_status(tx, pid, vids, VoteStatus::Pending, None)?;
            vids.iter().try_for_each(|v| tx_push(tx, pid, *v))
        })
    }

    /// A transition landed: its votes are `settled` and leave the queue, and
    /// the transition and its blobs are stored, all in one commit.
    pub fn settle(
        &self,
        pid: &Fr,
        vids: &[u64],
        t: &TransitionRecord,
        blobs: &[Blob],
    ) -> Result<()> {
        self.write(|tx| {
            tx_status(tx, pid, vids, VoteStatus::Settled, None)?;
            tx_remove(tx, pid, vids)?;
            tx_put_transition(tx, pid, t, blobs)
        })
    }

    /// Stores a transition and its blobs in one commit, replacing any blobs
    /// stored before under the same index.
    pub fn put_transition(&self, pid: &Fr, t: &TransitionRecord, blobs: &[Blob]) -> Result<()> {
        self.write(|tx| tx_put_transition(tx, pid, t, blobs))
    }

    /// Transitions in index order.
    pub fn transitions(&self, pid: &Fr) -> Result<Vec<TransitionRecord>> {
        self.scan(TRANSITIONS, &pid_key(pid), 40)?
            .iter()
            .map(|(_, v)| decode("transitions", v))
            .collect()
    }

    /// Blobs of transition `idx`, in order; empty if none are stored.
    pub fn blobs(&self, pid: &Fr, idx: u64) -> Result<Vec<Blob>> {
        self.scan(BLOBS, &key_u64(pid, idx), 42)?
            .into_iter()
            .map(|(_, v)| {
                let len = v.len();
                v.into_boxed_slice()
                    .try_into()
                    .map_err(|_| StorageError::Corrupt {
                        table: "blobs",
                        reason: format!("{len} bytes, want {BLOB_SIZE}"),
                    })
            })
            .collect()
    }

    pub fn meta_bytes(&self, k: &str) -> Result<Option<Vec<u8>>> {
        self.get(META_BYTES, k.as_bytes())
    }

    pub fn set_meta_bytes(&self, k: &str, v: &[u8]) -> Result<()> {
        self.put(META_BYTES, k.as_bytes(), v)
    }

    pub fn meta_u64(&self, k: &str) -> Result<Option<u64>> {
        let tx = self.db.begin_read().map_err(db_err)?;
        let t = tx.open_table(META).map_err(db_err)?;
        Ok(t.get(k).map_err(db_err)?.map(|v| v.value()))
    }

    pub fn set_meta_u64(&self, k: &str, v: u64) -> Result<()> {
        let tx = self.db.begin_write().map_err(db_err)?;
        {
            let mut t = tx.open_table(META).map_err(db_err)?;
            t.insert(k, v).map_err(db_err)?;
        }
        tx.commit().map_err(db_err)
    }

    /// The node master secret behind every election key, drawn with `draw`
    /// and stored on first use (one write transaction, so it is set once).
    pub(crate) fn master_secret(&self, draw: impl FnOnce() -> [u8; 32]) -> Result<[u8; 32]> {
        let tx = self.db.begin_write().map_err(db_err)?;
        let secret = {
            let mut t = tx.open_table(ENC_KEYS).map_err(db_err)?;
            let found = t
                .get(MASTER_KEY)
                .map_err(db_err)?
                .map(|v| <[u8; 32]>::try_from(v.value()));
            match found {
                Some(Ok(s)) => s,
                Some(Err(_)) => {
                    return Err(StorageError::Corrupt {
                        table: "enc_keys",
                        reason: "master secret is not 32 bytes".into(),
                    });
                }
                None => {
                    let s = draw();
                    t.insert(MASTER_KEY, s.as_slice()).map_err(db_err)?;
                    s
                }
            }
        };
        tx.commit().map_err(db_err)?;
        Ok(secret)
    }

    /// Rows in `enc_keys`: 1 once the master secret exists, never more.
    pub fn enc_key_count(&self) -> Result<u64> {
        use redb::ReadableTableMetadata;
        let tx = self.db.begin_read().map_err(db_err)?;
        let t = tx.open_table(ENC_KEYS).map_err(db_err)?;
        t.len().map_err(db_err)
    }

    /// Whether a census with this root is stored, without reading its leaves.
    pub(crate) fn census_exists(&self, root: &[u8; 32]) -> Result<bool> {
        let tx = self.db.begin_read().map_err(db_err)?;
        let t = tx.open_table(CENSUS).map_err(db_err)?;
        Ok(t.get(root.as_slice()).map_err(db_err)?.is_some())
    }

    /// Stores a census: its leaves (BE32 each) and the address index.
    pub(crate) fn put_census(
        &self,
        root: &[u8; 32],
        leaves: &[[u8; 32]],
        index: &[([u8; 20], u64)],
    ) -> Result<()> {
        let tx = self.db.begin_write().map_err(db_err)?;
        {
            let mut ct = tx.open_table(CENSUS).map_err(db_err)?;
            ct.insert(root.as_slice(), leaves.concat().as_slice())
                .map_err(db_err)?;
            let mut at = tx.open_table(CENSUS_ADDR).map_err(db_err)?;
            for (addr, i) in index {
                let mut k = [0u8; 52];
                k[..32].copy_from_slice(root);
                k[32..].copy_from_slice(addr);
                at.insert(k.as_slice(), i.to_be_bytes().as_slice())
                    .map_err(db_err)?;
            }
        }
        tx.commit().map_err(db_err)
    }

    pub(crate) fn census_leaves(&self, root: &[u8; 32]) -> Result<Option<Vec<[u8; 32]>>> {
        let Some(v) = self.get(CENSUS, root)? else {
            return Ok(None);
        };
        if v.len() % 32 != 0 {
            return Err(StorageError::Corrupt {
                table: "census",
                reason: format!("{} bytes", v.len()),
            });
        }
        Ok(Some(
            v.chunks_exact(32)
                .map(|c| {
                    let mut l = [0u8; 32];
                    l.copy_from_slice(c);
                    l
                })
                .collect(),
        ))
    }

    pub(crate) fn census_index(&self, root: &[u8; 32], addr: &[u8; 20]) -> Result<Option<u64>> {
        let mut k = [0u8; 52];
        k[..32].copy_from_slice(root);
        k[32..].copy_from_slice(addr);
        self.get(CENSUS_ADDR, &k)?
            .map(|v| u64_value("census_addr", &v))
            .transpose()
    }

    /// Roots of every stored census.
    pub(crate) fn census_roots(&self) -> Result<Vec<[u8; 32]>> {
        let tx = self.db.begin_read().map_err(db_err)?;
        let t = tx.open_table(CENSUS).map_err(db_err)?;
        let mut out = Vec::new();
        for row in t.iter().map_err(db_err)? {
            let (k, _) = row.map_err(db_err)?;
            if let Ok(r) = k.value().try_into() {
                out.push(r);
            }
        }
        Ok(out)
    }

    /// Deletes a stored census and its address index.
    pub(crate) fn drop_census(&self, root: &[u8; 32]) -> Result<()> {
        let (mut lo, mut hi) = ([0u8; 52], [0xffu8; 52]);
        lo[..32].copy_from_slice(root);
        hi[..32].copy_from_slice(root);
        let tx = self.db.begin_write().map_err(db_err)?;
        {
            let mut ct = tx.open_table(CENSUS).map_err(db_err)?;
            ct.remove(root.as_slice()).map_err(db_err)?;
            let mut at = tx.open_table(CENSUS_ADDR).map_err(db_err)?;
            at.retain_in(lo.as_slice()..=hi.as_slice(), |_, _| false)
                .map_err(db_err)?;
        }
        tx.commit().map_err(db_err)
    }
}

/// `<chain id>-<registry>`, the registry in lowercase `0x` hex.
pub fn deployment_name(chain_id: u64, registry: &[u8; 20]) -> String {
    format!("{chain_id}-0x{}", hex::encode(registry))
}

/// The directory holding everything the node keeps for one deployment.
pub fn deployment_dir(datadir: &Path, chain_id: u64, registry: &[u8; 20]) -> PathBuf {
    datadir.join(deployment_name(chain_id, registry))
}

/// Bytes 20..24 of every process id the registry creates: the last 4 bytes of
/// `keccak256(uint32 chainId ‖ registry)` (`ProcessIdLib.getPrefix`).
pub fn pid_prefix(chain_id: u64, registry: &[u8; 20]) -> Option<[u8; 4]> {
    let id = u32::try_from(chain_id).ok()?;
    let mut buf = [0u8; 24];
    buf[..4].copy_from_slice(&id.to_be_bytes());
    buf[4..].copy_from_slice(registry);
    let h = alloy::primitives::keccak256(buf);
    let mut out = [0u8; 4];
    out.copy_from_slice(&h[28..]);
    Some(out)
}

fn private_dir(dir: &Path) -> Result<()> {
    if !dir.exists() {
        std::fs::create_dir_all(dir)?;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

// Moves `<datadir>/sequencer.redb` (the flat layout) to `dir` when every
// process it holds is this deployment's. Anything else stays in place.
fn adopt_flat(datadir: &Path, dir: &Path, chain_id: u64, registry: &[u8; 20]) -> Result<()> {
    let flat = datadir.join(DB_FILE);
    if !flat.exists() {
        return Ok(());
    }
    let target = dir.join(DB_FILE);
    if target.exists() {
        tracing::info!(
            path = %flat.display(),
            "database of the flat datadir layout left in place: this deployment has its own"
        );
        return Ok(());
    }
    let (schema, prefixes) = match inspect_flat(&flat) {
        Ok(v) => v,
        Err(redb::Error::DatabaseAlreadyOpen) => {
            return Err(StorageError::Db(format!(
                "{} is open in another process",
                flat.display()
            )));
        }
        Err(e) => {
            tracing::warn!(
                path = %flat.display(),
                error = %e,
                "database of the flat datadir layout is unreadable: left in place"
            );
            return Ok(());
        }
    };
    let ours = pid_prefix(chain_id, registry);
    let why = if prefixes.is_empty() || ours.is_none() {
        "its deployment is unknown (no process to tell it by)"
    } else if prefixes.iter().any(|p| Some(*p) != ours) {
        "it belongs to another deployment"
    } else if schema != Some(SCHEMA_VERSION) {
        "its schema version is not this build's"
    } else {
        ""
    };
    if !why.is_empty() {
        tracing::warn!(
            path = %flat.display(),
            deployment = %dir.display(),
            "database of the flat datadir layout left in place, {why}; this deployment starts empty"
        );
        return Ok(());
    }
    private_dir(dir)?;
    std::fs::rename(&flat, &target)?;
    tracing::info!(
        from = %flat.display(),
        to = %target.display(),
        "moved the flat datadir layout into its deployment directory"
    );
    Ok(())
}

// The schema version of a database file and the distinct id prefixes of its
// processes, read from the keys only, so records of any shape do.
fn inspect_flat(path: &Path) -> Result<(Option<u64>, HashSet<[u8; 4]>), redb::Error> {
    let db = Database::open(path)?;
    let tx = db.begin_read()?;
    let schema = match tx.open_table(META) {
        Ok(t) => t.get(SCHEMA_VERSION_KEY)?.map(|v| v.value()),
        Err(redb::TableError::TableDoesNotExist(_)) => None,
        Err(e) => return Err(e.into()),
    };
    let t = match tx.open_table(PROCESSES) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok((schema, HashSet::new())),
        Err(e) => return Err(e.into()),
    };
    let mut out = HashSet::new();
    for row in t.iter()? {
        let (k, _) = row?;
        if let Some(p) = k
            .value()
            .get(21..25)
            .and_then(|p| <[u8; 4]>::try_from(p).ok())
        {
            out.insert(p);
        }
    }
    Ok((schema, out))
}

fn tx_push(tx: &WriteTransaction, pid: &Fr, vid: u64) -> Result<()> {
    let mut idx = tx.open_table(PENDING_IDX).map_err(db_err)?;
    let ikey = key_u64(pid, vid);
    if idx.get(ikey.as_slice()).map_err(db_err)?.is_some() {
        return Ok(());
    }
    let mut t = tx.open_table(PENDING).map_err(db_err)?;
    let (lo, hi) = prefix_range(&pid_key(pid), 40);
    let last = t
        .range(lo.as_slice()..=hi.as_slice())
        .map_err(db_err)?
        .next_back()
        .transpose()
        .map_err(db_err)?
        .map(|(k, _)| k.value().to_vec());
    let seq = match last {
        None => 0,
        Some(k) => u64_value("pending", k.get(32..).unwrap_or_default())?
            .checked_add(1)
            .ok_or_else(|| StorageError::Db("pending sequence exhausted".into()))?,
    };
    t.insert(key_u64(pid, seq).as_slice(), vid.to_be_bytes().as_slice())
        .map_err(db_err)?;
    idx.insert(ikey.as_slice(), seq.to_be_bytes().as_slice())
        .map_err(db_err)?;
    Ok(())
}

fn tx_remove(tx: &WriteTransaction, pid: &Fr, vids: &[u64]) -> Result<()> {
    let mut idx = tx.open_table(PENDING_IDX).map_err(db_err)?;
    let mut t = tx.open_table(PENDING).map_err(db_err)?;
    let set: HashSet<u64> = vids.iter().copied().collect();
    for vid in set {
        let removed = idx
            .remove(key_u64(pid, vid).as_slice())
            .map_err(db_err)?
            .map(|g| g.value().to_vec());
        if let Some(seq) = removed {
            let seq = u64_value("pending_idx", &seq)?;
            t.remove(key_u64(pid, seq).as_slice()).map_err(db_err)?;
        }
    }
    Ok(())
}

fn tx_status(
    tx: &WriteTransaction,
    pid: &Fr,
    vids: &[u64],
    s: VoteStatus,
    err: Option<&str>,
) -> Result<()> {
    let ts = now();
    let mut t = tx.open_table(VOTES).map_err(db_err)?;
    for vid in vids {
        let key = key_u64(pid, *vid);
        let mut v: StoredVote = match t.get(key.as_slice()).map_err(db_err)? {
            Some(g) => decode("votes", g.value())?,
            None => return Err(StorageError::NotFound(format!("vote {vid}"))),
        };
        v.status = s;
        v.error = err.map(str::to_string);
        v.updated_at = ts.max(v.created_at);
        t.insert(key.as_slice(), encode(&v)?.as_slice())
            .map_err(db_err)?;
    }
    Ok(())
}

fn tx_put_transition(
    tx: &WriteTransaction,
    pid: &Fr,
    t: &TransitionRecord,
    blobs: &[Blob],
) -> Result<()> {
    let n = u16::try_from(blobs.len())
        .map_err(|_| StorageError::Db(format!("{} blobs", blobs.len())))?;
    tx.open_table(TRANSITIONS)
        .map_err(db_err)?
        .insert(key_u64(pid, t.index).as_slice(), encode(t)?.as_slice())
        .map_err(db_err)?;
    let mut bt = tx.open_table(BLOBS).map_err(db_err)?;
    // Drop blobs a previous write left under this index.
    let (lo, hi) = prefix_range(&key_u64(pid, t.index), 42);
    bt.retain_in(lo.as_slice()..=hi.as_slice(), |_, _| false)
        .map_err(db_err)?;
    for (i, b) in (0..n).zip(blobs) {
        bt.insert(blob_key(pid, t.index, i).as_slice(), &b[..])
            .map_err(db_err)?;
    }
    Ok(())
}

/// A process's arbo storage: `RedbStorage` plus the node's scripted
/// write-failure counter, so tests can make one `ProcessState::commit`
/// persist fail and exercise the rollback-and-resync path.
pub struct ArboStore {
    inner: arbo::RedbStorage,
    faults: Arc<std::sync::atomic::AtomicU32>,
}

impl arbo::Storage for ArboStore {
    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, arbo::Error> {
        arbo::Storage::get(&self.inner, key)
    }

    fn write(&self, batch: arbo::WriteBatch) -> Result<(), arbo::Error> {
        use std::sync::atomic::Ordering;
        if self.faults.load(Ordering::SeqCst) > 0
            && self
                .faults
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
                .is_ok()
        {
            return Err(arbo::Error::Storage("scripted write failure".into()));
        }
        arbo::Storage::write(&self.inner, batch)
    }
}

/// Fr as `0x` + BE32 hex; decoding rejects values `>= p`.
pub(crate) mod fr_hex {
    use davinci_zkvm_sdk::crypto::field::{Fr, fr_from_be, fr_to_be};
    use serde::{Deserialize, Deserializer, Serializer, de::Error};

    pub fn serialize<S: Serializer>(x: &Fr, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&format!("0x{}", hex::encode(fr_to_be(x))))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Fr, D::Error> {
        let s = String::deserialize(d)?;
        let mut b = [0u8; 32];
        hex::decode_to_slice(s.trim_start_matches("0x"), &mut b).map_err(D::Error::custom)?;
        fr_from_be(&b).map_err(D::Error::custom)
    }
}
