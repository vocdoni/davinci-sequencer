//! The node's redb file, one per deployment
//! (`<datadir>/<chain id>-<registry>/sequencer.redb`, mode 0600): processes,
//! votes, the pending FIFO, transitions and their blobs, the election-key
//! master secret, census trees, meta counters, and one arbo table per process.
//!
//! Keys are big-endian so redb's byte order is the natural order: `pid` is the
//! process id as BE32, followed by BE8 vote ids / sequence numbers / indexes.

use std::collections::{BTreeMap, BTreeSet, HashSet};
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
pub const SCHEMA_VERSION: u64 = 11;
pub const SCHEMA_VERSION_KEY: &str = "schema_version";
/// The database file inside its directory.
pub const DB_FILE: &str = "sequencer.redb";
/// The redb page cache when the caller names none (`--db-cache-mb`).
pub const DEFAULT_CACHE_MB: usize = 256;
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
// nonce (BE8) -> `L1PostRecord`: blob transactions of the node's Ethereum
// account not yet seen mined (L1-blob mode).
const L1_POSTS: Bytes = TableDefinition::new("l1_posts");
// pid -> []: a rebuild kept votes Settled that its replay may not bring
// back; the check after the replay converges drops it.
const REBUILD_CHECK: Bytes = TableDefinition::new("rebuild_check");
// pid‖root -> `ExposedRecord`: the coverage history. One entry per batch
// carrying votes (our own sealed batches, written in the seal commit before
// anything can publish them; every peer batch we fetch, with all its vote
// ids, before its sync applies): its updated slots and those votes.
// Append-only: a reseal of a vote refreshes the slots of every entry naming
// it. A full peer batch costs one entry (2560 slots and 512 vote ids as
// JSON, about 60 KB) and 512 index keys of 72 bytes (about 37 KB).
const COVERAGE_HISTORY: Bytes = TableDefinition::new("coverage_history");
// pid‖vid‖root -> []: the history's index by vote.
const COVERAGE_BY_VOTE: Bytes = TableDefinition::new("coverage_by_vote");

#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    #[error("storage: {0}")]
    Db(String),
    #[error(
        "schema version {found} in the data file, this build uses {expected}; records are not \
         migrated: move the data file aside and start with an empty DAVINCI_DATADIR (the chain \
         is the source of truth); keep the old file, it holds the master secret"
    )]
    Schema { found: u64, expected: u64 },
    /// No upgrade: the batches it sealed or synced have no coverage history,
    /// so their votes' reseals would leave slots out.
    #[error(
        "this datadir predates the coverage history; start this node with a fresh datadir \
         (move the old one aside and keep it: it holds the master secret)"
    )]
    PredatesHistory,
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

/// A blob transaction the node signed on Ethereum (L1-blob mode), written
/// before it is sent and dropped once mined, so a restart replaces it at its
/// nonce instead of queueing behind it. Holds nothing secret.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct L1PostRecord {
    #[serde(with = "hex::serde")]
    pub pid: [u8; 31],
    #[serde(with = "hex::serde")]
    pub root_before: [u8; 32],
    pub nonce: u64,
    pub tx_hash: alloy::primitives::B256,
    pub fees: crate::web3::L1Fees,
    pub vhs: Vec<alloy::primitives::B256>,
    /// Unix seconds.
    pub posted_at: u64,
    /// The L1 account that sent the post.
    #[serde(with = "hex::serde")]
    pub from: [u8; 20],
}

/// Shared handle to the node's database. Cloning is cheap.
#[derive(Clone)]
pub struct Db {
    db: Arc<Database>,
    /// Scripted arbo write failures (tests only; always 0 in production),
    /// after skipping `arbo_skip` writes.
    arbo_faults: Arc<std::sync::atomic::AtomicU32>,
    arbo_skip: Arc<std::sync::atomic::AtomicU32>,
    /// Arbo reads (any process), counted in test builds only.
    arbo_reads: Arc<std::sync::atomic::AtomicU64>,
    /// Every arbo handle handed out, so tests can see open snapshots.
    #[cfg(any(test, feature = "test-hooks"))]
    arbo_handles: Arc<std::sync::Mutex<Vec<arbo::RedbStorage>>>,
    /// Scripted failures of the node's own write transactions (tests only).
    #[cfg(any(test, feature = "test-hooks"))]
    write_faults: Arc<std::sync::atomic::AtomicU32>,
    /// Scripted failures of process-record reads (tests only).
    #[cfg(any(test, feature = "test-hooks"))]
    process_read_faults: Arc<std::sync::atomic::AtomicU32>,
    /// While set, reads of this `meta_bytes` key fail (tests only).
    #[cfg(any(test, feature = "test-hooks"))]
    meta_read_fault: Arc<std::sync::Mutex<Option<String>>>,
    /// While set, transition-record writes fail after `transition_skip`
    /// more go through (tests only).
    #[cfg(any(test, feature = "test-hooks"))]
    transition_faults: Arc<std::sync::atomic::AtomicBool>,
    #[cfg(any(test, feature = "test-hooks"))]
    transition_skip: Arc<std::sync::atomic::AtomicU32>,
    /// While set, the write after a rebuild's check fails (tests only).
    #[cfg(any(test, feature = "test-hooks"))]
    check_faults: Arc<std::sync::atomic::AtomicBool>,
    /// Armed: the next history write (a seal commit or a synced batch's
    /// entry) is followed by a crash (tests only).
    #[cfg(any(test, feature = "test-hooks"))]
    history_crash: Arc<std::sync::atomic::AtomicBool>,
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
        Self::open_with(path, DEFAULT_CACHE_MB)
    }

    /// [`Db::open`] with a redb page cache of `cache_mb` MiB.
    pub fn open_with(path: &Path, cache_mb: usize) -> Result<Self> {
        if !path.exists() {
            OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(path)?;
        }
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        let db = redb::Builder::new()
            .set_cache_size(cache_mb.saturating_mul(1 << 20))
            .create(path)
            .map_err(db_err)?;
        let this = Db {
            db: Arc::new(db),
            arbo_faults: Arc::new(std::sync::atomic::AtomicU32::new(0)),
            arbo_skip: Arc::new(std::sync::atomic::AtomicU32::new(0)),
            arbo_reads: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            #[cfg(any(test, feature = "test-hooks"))]
            arbo_handles: Default::default(),
            #[cfg(any(test, feature = "test-hooks"))]
            write_faults: Default::default(),
            #[cfg(any(test, feature = "test-hooks"))]
            process_read_faults: Default::default(),
            #[cfg(any(test, feature = "test-hooks"))]
            meta_read_fault: Default::default(),
            #[cfg(any(test, feature = "test-hooks"))]
            transition_faults: Default::default(),
            #[cfg(any(test, feature = "test-hooks"))]
            transition_skip: Default::default(),
            #[cfg(any(test, feature = "test-hooks"))]
            check_faults: Default::default(),
            #[cfg(any(test, feature = "test-hooks"))]
            history_crash: Default::default(),
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
    /// (`<datadir>/sequencer.redb`) predates the coverage history: refused.
    pub fn open_deployment(
        datadir: &Path,
        chain_id: u64,
        registry: &[u8; 20],
        cache_mb: usize,
    ) -> Result<Self> {
        private_dir(datadir)?;
        let dir = deployment_dir(datadir, chain_id, registry);
        if datadir.join(DB_FILE).exists() {
            return Err(StorageError::PredatesHistory);
        }
        private_dir(&dir)?;
        let db = Self::open_with(&dir.join(DB_FILE), cache_mb)?;
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
                Some(v) if v < SCHEMA_VERSION => return Err(StorageError::PredatesHistory),
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
                BLOBS,
                ENC_KEYS,
                CENSUS,
                CENSUS_ADDR,
                VOTE_ADDR,
                META_BYTES,
                EXPOSED,
                L1_POSTS,
                REBUILD_CHECK,
                COVERAGE_HISTORY,
                COVERAGE_BY_VOTE,
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
        #[cfg(any(test, feature = "test-hooks"))]
        self.arbo_handles
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(inner.clone());
        Ok(ArboStore {
            inner,
            faults: self.arbo_faults.clone(),
            skip: self.arbo_skip.clone(),
            reads: self.arbo_reads.clone(),
        })
    }

    /// Makes the next `n` arbo tree writes fail (any process). Test hook for
    /// commit-failure recovery; one relaxed atomic load per write otherwise.
    /// Compiled only for tests (`test-hooks` covers integration tests).
    #[cfg(any(test, feature = "test-hooks"))]
    pub fn fail_next_arbo_writes(&self, n: u32) {
        self.fail_arbo_writes_after(0, n);
    }

    /// Lets the next `skip` arbo writes through, then fails `n` (a crash
    /// part way through a run of writes). Test hook.
    #[cfg(any(test, feature = "test-hooks"))]
    pub fn fail_arbo_writes_after(&self, skip: u32, n: u32) {
        use std::sync::atomic::Ordering;
        self.arbo_skip.store(skip, Ordering::SeqCst);
        self.arbo_faults.store(n, Ordering::SeqCst);
    }

    /// Makes the next `n` record writes (processes, votes, queue,
    /// transitions) fail while arbo writes go through; `u32::MAX` stands for
    /// a crash right after a tree commit. Test hook.
    #[cfg(any(test, feature = "test-hooks"))]
    pub fn fail_next_writes(&self, n: u32) {
        self.write_faults
            .store(n, std::sync::atomic::Ordering::SeqCst);
    }

    /// While `on`, every transition-record write fails and every other write
    /// goes through (a crash right before the record). Test hook.
    #[cfg(any(test, feature = "test-hooks"))]
    pub fn fail_transition_writes(&self, on: bool) {
        self.fail_transition_writes_after(0);
        self.transition_faults
            .store(on, std::sync::atomic::Ordering::SeqCst);
    }

    /// While `on`, [`Db::finish_rebuild_check`] fails. Test hook.
    #[cfg(any(test, feature = "test-hooks"))]
    pub fn fail_rebuild_check_writes(&self, on: bool) {
        self.check_faults
            .store(on, std::sync::atomic::Ordering::SeqCst);
    }

    /// Every record write after the next history write (a seal commit or
    /// a synced batch's entry) fails, from the very next one on (a crash
    /// right after it). Test hook.
    #[cfg(any(test, feature = "test-hooks"))]
    pub fn fail_writes_after_history(&self) {
        self.history_crash
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    /// Lets the next `skip` transition-record writes through, then fails
    /// every one. Test hook.
    #[cfg(any(test, feature = "test-hooks"))]
    pub fn fail_transition_writes_after(&self, skip: u32) {
        use std::sync::atomic::Ordering;
        self.transition_skip.store(skip, Ordering::SeqCst);
        self.transition_faults.store(true, Ordering::SeqCst);
    }

    /// Makes the next `n` [`Db::process`] reads fail. Test hook.
    #[cfg(any(test, feature = "test-hooks"))]
    pub fn fail_next_process_reads(&self, n: u32) {
        self.process_read_faults
            .store(n, std::sync::atomic::Ordering::SeqCst);
    }

    /// While `Some(k)`, every [`Db::meta_bytes`] read of `k` fails. Test hook.
    #[cfg(any(test, feature = "test-hooks"))]
    pub fn fail_meta_reads(&self, k: Option<&str>) {
        *self
            .meta_read_fault
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = k.map(str::to_string);
    }

    /// Arbo handles (any process) holding an open read snapshot. Test hook.
    #[cfg(any(test, feature = "test-hooks"))]
    pub fn arbo_snapshots_open(&self) -> usize {
        self.arbo_handles
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter(|h| h.snapshot_open())
            .count()
    }

    /// Scripted record-write failures not yet used. Test hook.
    #[cfg(any(test, feature = "test-hooks"))]
    pub fn write_faults_left(&self) -> u32 {
        self.write_faults.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Arbo tree and slot-store reads so far (any process). Test hook.
    #[cfg(any(test, feature = "test-hooks"))]
    pub fn arbo_reads(&self) -> u64 {
        self.arbo_reads.load(std::sync::atomic::Ordering::SeqCst)
    }

    fn get(&self, table: Bytes, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let tx = self.db.begin_read().map_err(db_err)?;
        let t = tx.open_table(table).map_err(db_err)?;
        let v = t.get(key).map_err(db_err)?;
        Ok(v.map(|g| g.value().to_vec()))
    }

    fn put(&self, table: Bytes, key: &[u8], value: &[u8]) -> Result<()> {
        self.write(|tx| {
            let mut t = tx.open_table(table).map_err(db_err)?;
            t.insert(key, value).map_err(db_err)?;
            Ok(())
        })
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
        self.write(|tx| tx_put_process(tx, p))
    }

    pub fn process(&self, pid: &Fr) -> Result<Option<ProcessRecord>> {
        #[cfg(any(test, feature = "test-hooks"))]
        if self
            .process_read_faults
            .fetch_update(
                std::sync::atomic::Ordering::SeqCst,
                std::sync::atomic::Ordering::SeqCst,
                |n| n.checked_sub(1),
            )
            .is_ok()
        {
            return Err(StorageError::Db("scripted read failure".into()));
        }
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

    /// Ids of a process's votes in one of `statuses`, without loading the
    /// rest: only the id and status of each record are decoded.
    pub fn vote_ids_with_status(&self, pid: &Fr, statuses: &[VoteStatus]) -> Result<Vec<u64>> {
        #[derive(Deserialize)]
        struct Head {
            vote_id: u64,
            status: VoteStatus,
        }
        let (lo, hi) = prefix_range(&pid_key(pid), 40);
        let tx = self.db.begin_read().map_err(db_err)?;
        let t = tx.open_table(VOTES).map_err(db_err)?;
        let mut out = Vec::new();
        for row in t.range(lo.as_slice()..=hi.as_slice()).map_err(db_err)? {
            let (_, v) = row.map_err(db_err)?;
            let h: Head = decode("votes", v.value())?;
            if statuses.contains(&h.status) {
                out.push(h.vote_id);
            }
        }
        Ok(out)
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

    /// Whether a rebuild's kept-vote check is pending.
    pub fn rebuild_check(&self, pid: &Fr) -> Result<bool> {
        Ok(self.get(REBUILD_CHECK, &pid_key(pid))?.is_some())
    }

    /// The check after a rebuild's replay: `vids` (settled, but not on the
    /// chain the replay reached) go back to pending at the queue head and
    /// the marker is dropped, in one commit.
    pub fn finish_rebuild_check(&self, pid: &Fr, vids: &[u64]) -> Result<()> {
        #[cfg(any(test, feature = "test-hooks"))]
        if self.check_faults.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(StorageError::Db("scripted rebuild check failure".into()));
        }
        self.write(|tx| {
            tx_status(tx, pid, vids, VoteStatus::Pending, None)?;
            tx_prepend(tx, pid, vids)?;
            tx.open_table(REBUILD_CHECK)
                .map_err(db_err)?
                .remove(pid_key(pid).as_slice())
                .map_err(db_err)?;
            Ok(())
        })
    }

    pub fn clear_exposed(&self, pid: &Fr) -> Result<()> {
        self.write(|tx| {
            tx.open_table(EXPOSED)
                .map_err(db_err)?
                .remove(pid_key(pid).as_slice())
                .map_err(db_err)?;
            Ok(())
        })
    }

    /// Records a blob transaction before it is sent; a replacement at the
    /// same nonce overwrites the record.
    pub fn put_l1_post(&self, r: &L1PostRecord) -> Result<()> {
        self.put(L1_POSTS, &r.nonce.to_be_bytes(), &encode(r)?)
    }

    /// Every recorded blob transaction, by nonce.
    pub fn l1_posts(&self) -> Result<Vec<L1PostRecord>> {
        self.scan(L1_POSTS, &[], 8)?
            .iter()
            .map(|(_, v)| decode("l1_posts", v))
            .collect()
    }

    /// Drops the record at `nonce`.
    pub fn remove_l1_post(&self, nonce: u64) -> Result<()> {
        self.write(|tx| {
            let mut t = tx.open_table(L1_POSTS).map_err(db_err)?;
            t.remove(nonce.to_be_bytes().as_slice()).map_err(db_err)?;
            Ok(())
        })
    }

    /// Drops the records at or below `nonce`: mined.
    pub fn remove_l1_posts_upto(&self, nonce: u64) -> Result<()> {
        self.write(|tx| {
            let mut t = tx.open_table(L1_POSTS).map_err(db_err)?;
            let (lo, hi) = ([0u8; 8], nonce.to_be_bytes());
            let keys: Vec<Vec<u8>> = t
                .range(lo.as_slice()..=hi.as_slice())
                .map_err(db_err)?
                .map(|r| r.map(|(k, _)| k.value().to_vec()))
                .collect::<std::result::Result<_, _>>()
                .map_err(db_err)?;
            for k in keys {
                t.remove(k.as_slice()).map_err(db_err)?;
            }
            Ok(())
        })
    }

    // One write transaction: `f` runs inside it, the commit happens only if
    // it succeeds (dropping an uncommitted transaction aborts it).
    fn write(&self, f: impl FnOnce(&WriteTransaction) -> Result<()>) -> Result<()> {
        #[cfg(any(test, feature = "test-hooks"))]
        if self
            .write_faults
            .fetch_update(
                std::sync::atomic::Ordering::SeqCst,
                std::sync::atomic::Ordering::SeqCst,
                |n| n.checked_sub(1),
            )
            .is_ok()
        {
            return Err(StorageError::Db("scripted write failure".into()));
        }
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

    /// Records the coverage of the batch reaching `root` (idempotent: the
    /// same root overwrites itself), in its own commit.
    pub fn put_coverage(&self, pid: &Fr, root: &[u8; 32], entry: &ExposedRecord) -> Result<()> {
        let r = self.write(|tx| tx_put_coverage(tx, pid, root, entry));
        #[cfg(any(test, feature = "test-hooks"))]
        self.crash_after_history(&r);
        r
    }

    /// Every slot of every history entry naming one of `vids`.
    pub fn coverage_for(&self, pid: &Fr, vids: &[u64]) -> Result<BTreeSet<u64>> {
        Ok(self.coverage(pid, vids)?.union(vids))
    }

    /// The history entries naming any of `vids`, each read once, in one
    /// read transaction: one index scan per vote, one fetch per entry.
    pub fn coverage(&self, pid: &Fr, vids: &[u64]) -> Result<davinci_state::Coverage> {
        let tx = self.db.begin_read().map_err(db_err)?;
        let index = tx.open_table(COVERAGE_BY_VOTE).map_err(db_err)?;
        let mut roots: BTreeMap<[u8; 32], Vec<u64>> = BTreeMap::new();
        for vid in vids {
            let (lo, hi) = prefix_range(&key_u64(pid, *vid), 72);
            for row in index.range(lo.as_slice()..=hi.as_slice()).map_err(db_err)? {
                let (k, _) = row.map_err(db_err)?;
                if let Ok(r) = <[u8; 32]>::try_from(&k.value()[40..]) {
                    roots.entry(r).or_default().push(*vid);
                }
            }
        }
        let history = tx.open_table(COVERAGE_HISTORY).map_err(db_err)?;
        let mut out = davinci_state::Coverage::for_votes(vids);
        for (r, named) in roots {
            if let Some(v) = history.get(root_key(pid, &r).as_slice()).map_err(db_err)? {
                let e: ExposedRecord = decode("coverage_history", v.value())?;
                out.add(e.slots.into_iter().collect(), &named);
            }
        }
        Ok(out)
    }

    /// Every history entry of the process, by root. Test hook and diagnostics.
    pub fn coverage_history(&self, pid: &Fr) -> Result<Vec<([u8; 32], ExposedRecord)>> {
        self.scan(COVERAGE_HISTORY, &pid_key(pid), 64)?
            .iter()
            .map(|(k, v)| {
                let mut r = [0u8; 32];
                r.copy_from_slice(&k[32..]);
                Ok((r, decode("coverage_history", v)?))
            })
            .collect()
    }

    /// A seal, in one commit before anything can publish the batch: its
    /// votes go `aggregated` and leave the queue, the exposed set is stored,
    /// and the batch (reaching `root`) joins the coverage history.
    pub fn seal_batch_covered(
        &self,
        pid: &Fr,
        vids: &[u64],
        exposed: &ExposedRecord,
        root: &[u8; 32],
        entry: &ExposedRecord,
    ) -> Result<()> {
        let r = self.write(|tx| {
            tx_status(tx, pid, vids, VoteStatus::Aggregated, None)?;
            tx_remove(tx, pid, vids)?;
            tx.open_table(EXPOSED)
                .map_err(db_err)?
                .insert(pid_key(pid).as_slice(), encode(exposed)?.as_slice())
                .map_err(db_err)?;
            tx_put_coverage(tx, pid, root, entry)
        });
        #[cfg(any(test, feature = "test-hooks"))]
        self.crash_after_history(&r);
        r
    }

    #[cfg(any(test, feature = "test-hooks"))]
    fn crash_after_history(&self, r: &Result<()>) {
        if r.is_ok()
            && self
                .history_crash
                .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            self.fail_next_writes(u32::MAX);
        }
    }

    /// Votes enter a batch: `aggregated`, out of the pending queue.
    pub fn seal_batch(&self, pid: &Fr, vids: &[u64]) -> Result<()> {
        self.write(|tx| {
            tx_status(tx, pid, vids, VoteStatus::Aggregated, None)?;
            tx_remove(tx, pid, vids)
        })
    }

    /// Votes go back to `pending` (error cleared) at the queue head: they
    /// were sealed ahead of every queued vote of their slot, so the head
    /// keeps each slot's cast order. Votes still queued keep their place.
    pub fn requeue(&self, pid: &Fr, vids: &[u64]) -> Result<()> {
        self.write(|tx| {
            tx_status(tx, pid, vids, VoteStatus::Pending, None)?;
            tx_prepend(tx, pid, vids)
        })
    }

    /// A transition landed (ours or synced): its votes, if any, are `settled`
    /// and leave the queue, and the transition, its blobs and the process
    /// record (pin and counts) are stored, all in one commit.
    pub fn settle(
        &self,
        pid: &Fr,
        vids: &[u64],
        t: &TransitionRecord,
        blobs: &[Blob],
        process: &ProcessRecord,
    ) -> Result<()> {
        #[cfg(any(test, feature = "test-hooks"))]
        {
            use std::sync::atomic::Ordering;
            if self.transition_faults.load(Ordering::SeqCst)
                && self
                    .transition_skip
                    .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
                    .is_err()
            {
                return Err(StorageError::Db("scripted transition write failure".into()));
            }
        }
        self.write(|tx| {
            tx_status(tx, pid, vids, VoteStatus::Settled, None)?;
            tx_remove(tx, pid, vids)?;
            tx_put_transition(tx, pid, t, blobs)?;
            tx_put_process(tx, process)
        })
    }

    /// A reorg took transitions the process committed: drops its tree
    /// table, every transition and blob, puts `vids` back in the queue
    /// (pending, at its head in this order), and stores the exposed set, the
    /// kept-vote check marker (`check`) and the process record, in one
    /// commit. The coverage history stays. The tree is re-created at genesis
    /// after it; a crash in between reopens at genesis (no stored tree).
    pub fn reset_process(
        &self,
        pid: &Fr,
        vids: &[u64],
        exposed: &ExposedRecord,
        check: bool,
        process: &ProcessRecord,
    ) -> Result<()> {
        self.write(|tx| {
            let mut marker = tx.open_table(REBUILD_CHECK).map_err(db_err)?;
            match check {
                true => drop(
                    marker
                        .insert(pid_key(pid).as_slice(), &[][..])
                        .map_err(db_err)?,
                ),
                false => drop(marker.remove(pid_key(pid).as_slice()).map_err(db_err)?),
            }
            drop(marker);
            let name = Self::arbo_table(pid);
            tx.delete_table(TableDefinition::<&[u8], &[u8]>::new(&name))
                .map_err(db_err)?;
            let (lo, hi) = prefix_range(&pid_key(pid), 40);
            tx.open_table(TRANSITIONS)
                .map_err(db_err)?
                .retain_in(lo.as_slice()..=hi.as_slice(), |_, _| false)
                .map_err(db_err)?;
            let (lo, hi) = prefix_range(&pid_key(pid), 42);
            tx.open_table(BLOBS)
                .map_err(db_err)?
                .retain_in(lo.as_slice()..=hi.as_slice(), |_, _| false)
                .map_err(db_err)?;
            tx_status(tx, pid, vids, VoteStatus::Pending, None)?;
            tx_prepend(tx, pid, vids)?;
            tx.open_table(EXPOSED)
                .map_err(db_err)?
                .insert(pid_key(pid).as_slice(), encode(exposed)?.as_slice())
                .map_err(db_err)?;
            tx_put_process(tx, process)
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
        #[cfg(any(test, feature = "test-hooks"))]
        if self
            .meta_read_fault
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_deref()
            == Some(k)
        {
            return Err(StorageError::Db("scripted read failure".into()));
        }
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

    /// Sets `k` and, in the same transaction, the bytes under `kb` (`None`
    /// removes them), so the two never disagree after a crash.
    pub fn set_meta_u64_and_bytes(
        &self,
        k: &str,
        v: u64,
        kb: &str,
        vb: Option<&[u8]>,
    ) -> Result<()> {
        let tx = self.db.begin_write().map_err(db_err)?;
        {
            let mut t = tx.open_table(META).map_err(db_err)?;
            t.insert(k, v).map_err(db_err)?;
            let mut b = tx.open_table(META_BYTES).map_err(db_err)?;
            match vb {
                Some(vb) => drop(b.insert(kb.as_bytes(), vb).map_err(db_err)?),
                None => drop(b.remove(kb.as_bytes()).map_err(db_err)?),
            }
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

// A history entry and its index rows.
fn tx_put_coverage(
    tx: &WriteTransaction,
    pid: &Fr,
    root: &[u8; 32],
    entry: &ExposedRecord,
) -> Result<()> {
    tx.open_table(COVERAGE_HISTORY)
        .map_err(db_err)?
        .insert(root_key(pid, root).as_slice(), encode(entry)?.as_slice())
        .map_err(db_err)?;
    let mut index = tx.open_table(COVERAGE_BY_VOTE).map_err(db_err)?;
    for vid in &entry.vote_ids {
        let mut k = [0u8; 72];
        k[..40].copy_from_slice(&key_u64(pid, *vid));
        k[40..].copy_from_slice(root);
        index.insert(k.as_slice(), &[][..]).map_err(db_err)?;
    }
    Ok(())
}

fn root_key(pid: &Fr, root: &[u8; 32]) -> [u8; 64] {
    let mut k = [0u8; 64];
    k[..32].copy_from_slice(&pid_key(pid));
    k[32..].copy_from_slice(root);
    k
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

// Renumbers the process's queue as `vids` (those not queued yet) followed
// by the current queue. An O(queue) rewrite, fine for the rare requeue.
fn tx_prepend(tx: &WriteTransaction, pid: &Fr, vids: &[u64]) -> Result<()> {
    let mut idx = tx.open_table(PENDING_IDX).map_err(db_err)?;
    let mut t = tx.open_table(PENDING).map_err(db_err)?;
    let (lo, hi) = prefix_range(&pid_key(pid), 40);
    let mut queue = Vec::new();
    for row in t.range(lo.as_slice()..=hi.as_slice()).map_err(db_err)? {
        let (_, v) = row.map_err(db_err)?;
        queue.push(u64_value("pending", v.value())?);
    }
    let mut seen: HashSet<u64> = queue.iter().copied().collect();
    let mut order: Vec<u64> = vids.iter().copied().filter(|v| seen.insert(*v)).collect();
    if order.is_empty() {
        return Ok(());
    }
    order.extend(queue);
    t.retain_in(lo.as_slice()..=hi.as_slice(), |_, _| false)
        .map_err(db_err)?;
    for (seq, vid) in order.iter().enumerate() {
        let seq = seq as u64;
        t.insert(key_u64(pid, seq).as_slice(), vid.to_be_bytes().as_slice())
            .map_err(db_err)?;
        idx.insert(key_u64(pid, *vid).as_slice(), seq.to_be_bytes().as_slice())
            .map_err(db_err)?;
    }
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

fn tx_put_process(tx: &WriteTransaction, p: &ProcessRecord) -> Result<()> {
    tx.open_table(PROCESSES)
        .map_err(db_err)?
        .insert(pid_key(&p.pid).as_slice(), encode(p)?.as_slice())
        .map_err(db_err)?;
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
    skip: Arc<std::sync::atomic::AtomicU32>,
    #[cfg_attr(not(any(test, feature = "test-hooks")), allow(dead_code))]
    reads: Arc<std::sync::atomic::AtomicU64>,
}

impl arbo::Storage for ArboStore {
    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, arbo::Error> {
        #[cfg(any(test, feature = "test-hooks"))]
        self.reads
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        arbo::Storage::get(&self.inner, key)
    }

    fn write(&self, batch: arbo::WriteBatch) -> Result<(), arbo::Error> {
        use std::sync::atomic::Ordering;
        let take = |c: &std::sync::atomic::AtomicU32| {
            c.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
                .is_ok()
        };
        if self.faults.load(Ordering::SeqCst) > 0 && !take(&self.skip) && take(&self.faults) {
            return Err(arbo::Error::Storage("scripted write failure".into()));
        }
        arbo::Storage::write(&self.inner, batch)
    }

    fn release_snapshot(&self) {
        arbo::Storage::release_snapshot(&self.inner)
    }
}

// The snapshot is shared with the handle's clones (the test-hooks registry
// keeps one): an actor dropped mid-message must not leave it pinned.
impl Drop for ArboStore {
    fn drop(&mut self) {
        arbo::Storage::release_snapshot(&self.inner)
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
