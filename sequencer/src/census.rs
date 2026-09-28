//! Merkle censuses (origins 1 and 2): downloads the census document, rebuilds
//! the lean-IMT, requires the on-chain root, stores the leaves and serves
//! proofs. Origin 3 lives in [`onchain`].
//!
//! The census URI comes from on-chain data, so any organizer picks it:
//! - `http(s)://`: no redirects, public addresses only (unless
//!   `allow_private`), timeouts and a byte cap;
//! - `file://`: refused unless a census directory is configured, and then only
//!   regular files inside it (after resolving symlinks), read through a cap.
//!
//! Accepted: `{"participants": [{"key": "0x<address>", "weight": "<dec>"}]}`,
//! davinci-node's lean-imt-go `CensusDump` (`address`, numeric `weight` up to
//! 2^88 - 1, `addressIndex`, a `root` that must equal the on-chain one), and
//! JSONL with one participant per line (Content-Type `ndjson`/`jsonl`, or
//! sniffed for files). Two voters on one ballot slot refuse the census.

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::Read;
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use davinci_zkvm_sdk::census::{
    CensusProof, LeanImt, census_leaf, census_leaf_weight, slot_key_address,
};
use davinci_zkvm_sdk::crypto::field::{Fr, fr_from_be, fr_from_dec, fr_to_be};
use serde::Deserialize;
use serde_json::value::RawValue;
use tokio::sync::Semaphore;
use url::{Host, Url};

use crate::config::Config;
use crate::storage::{Db, StorageError};

pub mod onchain;

/// Largest census document read.
pub const MAX_CENSUS_BYTES: u64 = 256 << 20;
/// Census trees kept in memory.
const TREE_CACHE: usize = 8;
/// Concurrent census downloads and builds.
const BUILD_PERMITS: usize = 2;
/// First retry delay after a failed census; doubles per failure up to `MAX_BACKOFF`.
const BASE_BACKOFF: Duration = Duration::from_secs(30);
const MAX_BACKOFF: Duration = Duration::from_secs(3600);
/// Meta key of the origin-2 updates that can never load.
const BAD_KEY: &str = "census_bad_updates";

/// Origin-2 updates that can never load: pid -> (root, why), both BE.
type BadUpdates = HashMap<[u8; 32], ([u8; 32], String)>;
/// Failed (uri, root) pairs remembered.
pub(crate) const MAX_FAILURES: usize = 1024;

#[derive(Debug, thiserror::Error)]
pub enum CensusError {
    #[error("census download: {0}")]
    Fetch(String),
    /// Policy refusal (bad URL, unsupported scheme, non-public address,
    /// 4xx, over the size cap): retrying can never help.
    #[error("census refused: {0}")]
    Refused(String),
    #[error("census format: {0}")]
    Format(String),
    #[error("census root mismatch: the document gives {got}, the chain has {want}")]
    RootMismatch { got: String, want: String },
    #[error("census {0} is not loaded")]
    Unknown(String),
    #[error("census failed before, next try in {0:?}")]
    Backoff(Duration),
    #[error("census config: {0}")]
    Config(String),
    #[error(transparent)]
    Storage(#[from] StorageError),
}

pub type Result<T, E = CensusError> = std::result::Result<T, E>;

/// Where censuses may come from and how big they may be.
#[derive(Clone, Debug)]
pub struct CensusOptions {
    /// `file://` URIs must resolve inside this directory; `None` refuses them.
    pub dir: Option<PathBuf>,
    /// Allow loopback, private and link-local hosts (local dev only).
    pub allow_private: bool,
    pub max_participants: usize,
    pub max_bytes: u64,
}

impl Default for CensusOptions {
    fn default() -> Self {
        CensusOptions {
            dir: None,
            allow_private: false,
            max_participants: 1 << 22,
            max_bytes: MAX_CENSUS_BYTES,
        }
    }
}

impl CensusOptions {
    pub fn from_config(cfg: &Config) -> Self {
        CensusOptions {
            dir: cfg.census_dir.clone(),
            allow_private: cfg.census_allow_private,
            max_participants: cfg.census_max_participants,
            max_bytes: MAX_CENSUS_BYTES,
        }
    }
}

/// A voter address and its leaf index.
type AddressIndex = ([u8; 20], u64);
/// A census that failed: (uri without query or fragment, root).
/// Value: (last failure, failures).
type FailKey = (String, [u8; 32]);

fn fail_key(uri: &str, root: &Fr) -> FailKey {
    let base = match Url::parse(uri) {
        Ok(mut u) => {
            u.set_query(None);
            u.set_fragment(None);
            u.to_string()
        }
        Err(_) => uri.chars().take(256).collect(),
    };
    (base, fr_to_be(root))
}

/// A census document: `{"participants": [...]}`, optionally lean-imt-go's
/// `CensusDump` fields (`root`, and others we ignore).
#[derive(Deserialize)]
struct CensusJson<'a> {
    /// A JSON number (big.Int) or a decimal string; must be the expected root.
    #[serde(default, borrow)]
    root: Option<&'a RawValue>,
    #[serde(borrow)]
    participants: Vec<Participant<'a>>,
}

#[derive(Deserialize)]
struct Participant<'a> {
    #[serde(alias = "address")]
    key: String,
    /// A decimal string, or a JSON number of any size (big.Int dumps).
    #[serde(borrow)]
    weight: &'a RawValue,
    #[serde(rename = "addressIndex")]
    address_index: Option<u64>,
}

/// How to read a census body.
#[derive(Clone, Copy, Debug)]
enum DocFormat {
    /// One JSON object with `participants`.
    Dump,
    /// One participant object per line.
    Jsonl,
    /// Unknown: a dump, else JSONL.
    Sniff,
}

fn parse_doc(body: &[u8], f: DocFormat) -> Result<CensusJson<'_>> {
    let dump = || serde_json::from_slice(body).map_err(|e| CensusError::Format(e.to_string()));
    let jsonl = || -> Result<CensusJson<'_>> {
        let participants = body
            .split(|b| *b == b'\n')
            .enumerate()
            .filter(|(_, l)| !l.trim_ascii().is_empty())
            .map(|(i, l)| {
                serde_json::from_slice(l)
                    .map_err(|e| CensusError::Format(format!("line {}: {e}", i + 1)))
            })
            .collect::<Result<_>>()?;
        Ok(CensusJson {
            root: None,
            participants,
        })
    };
    match f {
        DocFormat::Dump => dump(),
        DocFormat::Jsonl => jsonl(),
        DocFormat::Sniff => dump().or_else(|e| jsonl().map_err(|_| e)),
    }
}

/// The document format a Content-Type names.
fn content_format(ct: Option<&str>) -> DocFormat {
    match ct.map(str::to_ascii_lowercase) {
        Some(t) if t.contains("ndjson") || t.contains("jsonl") => DocFormat::Jsonl,
        Some(t) if t.starts_with("application/json") => DocFormat::Dump,
        _ => DocFormat::Sniff,
    }
}

/// Untrusted text (URIs, document fields) can carry control characters
/// aimed at the logs: escape them before they enter an error message.
fn clean(s: &str) -> String {
    s.chars().flat_map(char::escape_debug).collect()
}

fn hex_fr(root: &Fr) -> String {
    format!("0x{}", hex::encode(fr_to_be(root)))
}

fn parse_address(s: &str) -> Result<[u8; 20]> {
    let mut a = [0u8; 20];
    let h = s.strip_prefix("0x").unwrap_or(s);
    hex::decode_to_slice(h, &mut a)
        .map_err(|_| CensusError::Format(format!("bad address {:.50}", clean(s))))?;
    Ok(a)
}

/// The digits of a JSON number or string holding a plain decimal integer.
fn raw_digits(raw: &RawValue) -> Option<&str> {
    let s = raw.get();
    let d = s
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .unwrap_or(s);
    (!d.is_empty() && d.bytes().all(|c| c.is_ascii_digit())).then_some(d)
}

fn parse_weight(w: &RawValue) -> Result<u128> {
    let bad = || CensusError::Format(format!("bad weight {:.50}", clean(w.get())));
    let d = raw_digits(w).filter(|d| d.len() <= 39).ok_or_else(bad)?;
    d.parse().map_err(|_| bad())
}

fn parse_root(r: &RawValue) -> Result<Fr> {
    raw_digits(r)
        .and_then(|d| fr_from_dec(d).ok())
        .ok_or_else(|| CensusError::Format(format!("bad root {:.80}", clean(r.get()))))
}

/// Leaves in tree order and the address index. The size is the entry count
/// (capped); indexes, when given, must be a permutation of `0..n`. An entry
/// with the zero address is a leaf but not a voter (`0x0`, weight 0 is
/// davinci-node's empty slot). Rejects two voters on one ballot slot,
/// including one address listed twice.
fn leaves_of(doc: &CensusJson, max: usize) -> Result<(Vec<Fr>, Vec<AddressIndex>)> {
    leaves_of_with(doc, max, slot_key_address)
}

fn leaves_of_with(
    doc: &CensusJson,
    max: usize,
    slot_of: fn(&[u8; 20]) -> u64,
) -> Result<(Vec<Fr>, Vec<AddressIndex>)> {
    let n = doc.participants.len();
    if n == 0 {
        return Err(CensusError::Format("no participants".into()));
    }
    if n > max {
        return Err(CensusError::Format(format!(
            "{n} participants, at most {max}"
        )));
    }
    let indexed = doc
        .participants
        .iter()
        .filter(|p| p.address_index.is_some())
        .count();
    if indexed != 0 && indexed != n {
        return Err(CensusError::Format(
            "addressIndex on some entries only".into(),
        ));
    }
    // Check the permutation before any leaf work.
    if indexed == n {
        let mut taken = vec![false; n];
        for p in &doc.participants {
            let i = p.address_index.unwrap_or(u64::MAX);
            match usize::try_from(i).ok().and_then(|i| taken.get_mut(i)) {
                Some(t) if !*t => *t = true,
                _ => {
                    return Err(CensusError::Format(format!(
                        "addressIndex {i} is out of 0..{n} or repeated"
                    )));
                }
            }
        }
    }
    let mut leaves = vec![Fr::default(); n];
    let mut index = Vec::with_capacity(n);
    let mut slots: HashMap<u64, [u8; 20]> = HashMap::with_capacity(n);
    for (pos, p) in doc.participants.iter().enumerate() {
        let i = p.address_index.map_or(pos, |i| i as usize);
        let addr = parse_address(&p.key)?;
        let weight = parse_weight(p.weight)?;
        let leaf = census_leaf(&addr, weight).map_err(|e| CensusError::Format(e.to_string()))?;
        let slot = leaves
            .get_mut(i)
            .ok_or_else(|| CensusError::Format(format!("index {i} out of range")))?;
        *slot = leaf;
        if addr == [0u8; 20] {
            continue;
        }
        let slot = slot_of(&addr);
        match slots.insert(slot, addr) {
            None => {}
            Some(other) if other == addr => {
                return Err(CensusError::Format(format!(
                    "address 0x{} listed twice",
                    hex::encode(addr)
                )));
            }
            Some(other) => {
                return Err(CensusError::Format(format!(
                    "ballot slot {slot:#x} is shared by 0x{} and 0x{}",
                    hex::encode(other),
                    hex::encode(addr)
                )));
            }
        }
        index.push((addr, i as u64));
    }
    Ok((leaves, index))
}

/// Globally routable unicast only.
fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v) => {
            let o = v.octets();
            !(v.is_private()
                || v.is_loopback()
                || v.is_link_local()
                || v.is_unspecified()
                || v.is_broadcast()
                || v.is_documentation()
                || v.is_multicast()
                || o[0] == 0
                || (o[0] == 100 && (o[1] & 0xc0) == 64)
                || o[0] >= 240)
        }
        IpAddr::V6(v) => {
            if let Some(v4) = v.to_ipv4_mapped() {
                return is_public(IpAddr::V4(v4));
            }
            let s = v.segments();
            // IPv4-compatible `::a.b.c.d` (deprecated) carries an IPv4 address too.
            if s[..6] == [0; 6] && !v.is_loopback() && !v.is_unspecified() {
                let o = v.octets();
                return is_public(IpAddr::V4(std::net::Ipv4Addr::new(
                    o[12], o[13], o[14], o[15],
                )));
            }
            !(v.is_loopback()
                || v.is_unspecified()
                || v.is_multicast()
                || (s[0] & 0xfe00) == 0xfc00 // unique local
                || (s[0] & 0xffc0) == 0xfe80 // link local

                || (s[0], s[1]) == (0x64, 0xff9b) // NAT64 (incl. 64:ff9b:1::/48)
                || s[0] == 0x2002 // 6to4
                || (s[0], s[1]) == (0x2001, 0x0db8) // documentation
                || (s[0], s[1]) == (0x2001, 0x0000)) // Teredo
        }
    }
}

// DNS resolver that drops non-public addresses, so a public name cannot
// point the node at its own network.
struct PublicOnly;

impl reqwest::dns::Resolve for PublicOnly {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let host = name.as_str().to_string();
        Box::pin(async move {
            let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host.as_str(), 0))
                .await?
                .filter(|a| is_public(a.ip()))
                .collect();
            if addrs.is_empty() {
                return Err(format!("{host} has no public address").into());
            }
            let it: reqwest::dns::Addrs = Box::new(addrs.into_iter());
            Ok(it)
        })
    }
}

// Kernel and device trees: their "files" can be endless, blocking or secret.
fn special_path(p: &Path) -> bool {
    ["/proc", "/sys", "/dev"].iter().any(|d| p.starts_with(d))
}

// Reads a regular file inside `dir` (canonical), at most `max` bytes.
fn read_census_file(dir: &Path, path: &Path, max: u64) -> Result<Vec<u8>> {
    let refuse = |why: &str| CensusError::Fetch(format!("{}: {why}", path.display()));
    let canon = std::fs::canonicalize(path).map_err(|e| refuse(&e.to_string()))?;
    if !canon.starts_with(dir) {
        return Err(refuse("outside the census directory"));
    }
    if special_path(&canon) {
        return Err(refuse("under /proc, /sys or /dev"));
    }
    if !std::fs::metadata(&canon)
        .map_err(|e| refuse(&e.to_string()))?
        .is_file()
    {
        return Err(refuse("not a regular file"));
    }
    let f = std::fs::File::open(&canon).map_err(|e| refuse(&e.to_string()))?;
    // Checked again on the open handle; sizes are not trusted, the read is capped.
    if !f.metadata().map_err(|e| refuse(&e.to_string()))?.is_file() {
        return Err(refuse("not a regular file"));
    }
    let mut body = Vec::new();
    f.take(max.saturating_add(1))
        .read_to_end(&mut body)
        .map_err(|e| refuse(&e.to_string()))?;
    if body.len() as u64 > max {
        return Err(refuse("larger than the census cap"));
    }
    Ok(body)
}

// Most recently used census trees.
#[derive(Default)]
struct TreeCache {
    map: HashMap<[u8; 32], Arc<LeanImt>>,
    order: VecDeque<[u8; 32]>,
}

impl TreeCache {
    fn get(&mut self, root: &[u8; 32]) -> Option<Arc<LeanImt>> {
        let t = self.map.get(root)?.clone();
        self.order.retain(|r| r != root);
        self.order.push_back(*root);
        Some(t)
    }

    fn remove(&mut self, root: &[u8; 32]) {
        self.order.retain(|r| r != root);
        self.map.remove(root);
    }

    fn put(&mut self, root: [u8; 32], t: Arc<LeanImt>) {
        self.order.retain(|r| *r != root);
        self.order.push_back(root);
        self.map.insert(root, t);
        while self.order.len() > TREE_CACHE {
            if let Some(old) = self.order.pop_front() {
                self.map.remove(&old);
            }
        }
    }
}

/// Census trees, stored in the node's database and cached in memory.
pub struct CensusStore {
    db: Db,
    opts: CensusOptions,
    http: reqwest::Client,
    cache: Mutex<TreeCache>,
    builds: Semaphore,
    failures: Mutex<HashMap<FailKey, (Instant, u32)>>,
    /// Per-root single-flight gates for cache-miss rebuilds.
    rebuilds: tokio::sync::Mutex<HashMap<[u8; 32], Arc<tokio::sync::Mutex<()>>>>,
    bad: Mutex<BadUpdates>,
}

impl std::fmt::Debug for CensusStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CensusStore")
            .field("opts", &self.opts)
            .finish_non_exhaustive()
    }
}

fn backoff(failures: u32) -> Duration {
    BASE_BACKOFF
        .saturating_mul(1u32 << failures.saturating_sub(1).min(16))
        .min(MAX_BACKOFF)
}

impl CensusStore {
    /// Defaults: no `file://`, public hosts only.
    pub fn new(db: Db) -> Result<Self> {
        Self::build(db, CensusOptions::default())
    }

    /// Canonicalises the census directory and refuses `/` and anything
    /// under /proc, /sys or /dev.
    pub fn with_options(db: Db, mut opts: CensusOptions) -> Result<Self> {
        if let Some(d) = &opts.dir {
            let canon = std::fs::canonicalize(d)
                .map_err(|e| CensusError::Config(format!("{}: {e}", d.display())))?;
            if !canon.is_dir() || canon == Path::new("/") || special_path(&canon) {
                return Err(CensusError::Config(format!(
                    "{} is not a usable census directory",
                    canon.display()
                )));
            }
            opts.dir = Some(canon);
        }
        Self::build(db, opts)
    }

    fn build(db: Db, opts: CensusOptions) -> Result<Self> {
        let mut b = reqwest::Client::builder()
            // A proxy would resolve the host itself, past `PublicOnly`.
            .no_proxy()
            .user_agent(crate::web3::USER_AGENT)
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(10))
            .read_timeout(Duration::from_secs(30))
            .timeout(Duration::from_secs(300));
        if !opts.allow_private {
            b = b.dns_resolver(Arc::new(PublicOnly));
        }
        // No fallback client: it would follow redirects and skip the filter.
        let http = b
            .build()
            .map_err(|e| CensusError::Config(format!("census http client: {e}")))?;
        let bad: Vec<([u8; 32], [u8; 32], String)> = match db.meta_bytes(BAD_KEY)? {
            Some(b) => serde_json::from_slice(&b).map_err(|e| StorageError::Corrupt {
                table: "meta_bytes",
                reason: format!("{BAD_KEY}: {e}"),
            })?,
            None => Vec::new(),
        };
        Ok(CensusStore {
            db,
            opts,
            http,
            cache: Mutex::new(TreeCache::default()),
            builds: Semaphore::new(BUILD_PERMITS),
            failures: Mutex::new(HashMap::new()),
            rebuilds: tokio::sync::Mutex::new(HashMap::new()),
            bad: Mutex::new(bad.into_iter().map(|(p, r, w)| (p, (r, w))).collect()),
        })
    }

    /// Records that `pid`'s census update to `root` can never load; votes
    /// answer 412 with `why` until the next update.
    pub fn mark_bad(&self, pid: &Fr, root: &Fr, why: &str) -> Result<()> {
        self.edit_bad(|m| {
            m.insert(fr_to_be(pid), (fr_to_be(root), why.to_string()))
                .is_none_or(|(r, w)| r != fr_to_be(root) || w != why)
        })
    }

    /// Forgets `pid`'s broken update (a newer one arrived).
    pub fn clear_bad(&self, pid: &Fr) -> Result<()> {
        self.edit_bad(|m| m.remove(&fr_to_be(pid)).is_some())
    }

    /// Why `pid`'s update to `root` can never load, if it can't.
    pub fn bad_reason(&self, pid: &Fr, root: &Fr) -> Option<String> {
        let m = self.bad.lock().ok()?;
        m.get(&fr_to_be(pid))
            .filter(|(r, _)| *r == fr_to_be(root))
            .map(|(_, w)| w.clone())
    }

    // Applies `f` and, if it reports a change, persists the map under the
    // lock so concurrent edits land in order.
    fn edit_bad(&self, f: impl FnOnce(&mut BadUpdates) -> bool) -> Result<()> {
        let mut m = self
            .bad
            .lock()
            .map_err(|_| CensusError::Fetch("census state poisoned".into()))?;
        if !f(&mut m) {
            return Ok(());
        }
        let list: Vec<_> = m.iter().map(|(p, (r, w))| (p, r, w)).collect();
        let bytes = serde_json::to_vec(&list).map_err(|e| CensusError::Format(e.to_string()))?;
        Ok(self.db.set_meta_bytes(BAD_KEY, &bytes)?)
    }

    /// Deletes every stored census whose root (BE) is not in `keep`.
    /// Returns how many it deleted.
    pub async fn prune(&self, keep: HashSet<[u8; 32]>) -> Result<usize> {
        let db = self.db.clone();
        let gone = tokio::task::spawn_blocking(move || -> Result<Vec<[u8; 32]>> {
            let mut gone = Vec::new();
            for r in db.census_roots()? {
                if !keep.contains(&r) {
                    db.drop_census(&r)?;
                    gone.push(r);
                }
            }
            Ok(gone)
        })
        .await
        .map_err(|e| CensusError::Fetch(e.to_string()))??;
        if let Ok(mut c) = self.cache.lock() {
            for r in &gone {
                c.remove(r);
            }
        }
        Ok(gone.len())
    }

    /// Whether the census with this root is stored.
    pub fn has(&self, root: &Fr) -> Result<bool> {
        Ok(self.db.census_exists(&fr_to_be(root))?)
    }

    /// Downloads the census at `uri`, and stores it if its lean-IMT root is
    /// `expected_root`. A census already stored is not downloaded again; a
    /// (uri, root) that failed is not retried before its backoff expires.
    pub async fn fetch(&self, uri: &str, expected_root: &Fr) -> Result<()> {
        if self.has(expected_root)? {
            return Ok(());
        }
        let key = fail_key(uri, expected_root);
        if let Some((at, n)) = self.failures.lock().ok().and_then(|f| f.get(&key).copied()) {
            let wait = backoff(n);
            let since = at.elapsed();
            if since < wait {
                return Err(CensusError::Backoff(wait - since));
            }
        }
        let res = async {
            let _permit = self
                .builds
                .acquire()
                .await
                .map_err(|e| CensusError::Fetch(e.to_string()))?;
            self.fetch_and_store(uri, expected_root).await
        }
        .await;
        if let Ok(mut f) = self.failures.lock() {
            match &res {
                Ok(()) => {
                    f.remove(&key);
                }
                Err(_) => {
                    let n = f.get(&key).map_or(0, |e| e.1).saturating_add(1);
                    if !f.contains_key(&key) && f.len() >= MAX_FAILURES {
                        f.retain(|_, (at, n)| at.elapsed() < backoff(*n));
                        // Still full: drop the oldest failures, in backoff or not.
                        while f.len() >= MAX_FAILURES {
                            let Some(oldest) = f
                                .iter()
                                .min_by_key(|(_, (at, _))| *at)
                                .map(|(k, _)| k.clone())
                            else {
                                break;
                            };
                            f.remove(&oldest);
                        }
                    }
                    f.insert(key, (Instant::now(), n));
                }
            }
        }
        res
    }

    async fn fetch_and_store(&self, uri: &str, expected_root: &Fr) -> Result<()> {
        let (body, format) = self.download(uri).await?;
        let want = *expected_root;
        let max = self.opts.max_participants;
        let (leaves, index, tree) = tokio::task::spawn_blocking(move || -> Result<_> {
            let doc = parse_doc(&body, format)?;
            if let Some(r) = doc.root.map(parse_root).transpose()?
                && r != want
            {
                return Err(CensusError::RootMismatch {
                    got: hex_fr(&r),
                    want: hex_fr(&want),
                });
            }
            let (leaves, index) = leaves_of(&doc, max)?;
            drop(doc);
            drop(body);
            let tree = LeanImt::from_leaves(leaves.clone());
            if tree.root() != want {
                return Err(CensusError::RootMismatch {
                    got: hex_fr(&tree.root()),
                    want: hex_fr(&want),
                });
            }
            Ok((leaves, index, tree))
        })
        .await
        .map_err(|e| CensusError::Format(format!("census build task: {e}")))??;
        let root = fr_to_be(expected_root);
        let be: Vec<[u8; 32]> = leaves.iter().map(fr_to_be).collect();
        self.db.put_census(&root, &be, &index)?;
        if let Ok(mut c) = self.cache.lock() {
            c.put(root, Arc::new(tree));
        }
        tracing::info!(root = %hex_fr(expected_root), leaves = leaves.len(), "census stored");
        Ok(())
    }

    async fn download(&self, uri: &str) -> Result<(Vec<u8>, DocFormat)> {
        let url = Url::parse(uri)
            .map_err(|e| CensusError::Refused(format!("{:.200}: {e}", clean(uri))))?;
        let max = self.opts.max_bytes;
        match url.scheme() {
            "http" | "https" => {
                let ip = match url.host() {
                    Some(Host::Ipv4(v)) => Some(IpAddr::V4(v)),
                    Some(Host::Ipv6(v)) => Some(IpAddr::V6(v)),
                    _ => None,
                };
                if let Some(ip) = ip.filter(|ip| !self.opts.allow_private && !is_public(*ip)) {
                    return Err(CensusError::Refused(format!(
                        "{ip} is not a public address"
                    )));
                }
                let mut resp = self
                    .http
                    .get(url)
                    .send()
                    .await
                    .map_err(|e| CensusError::Fetch(e.to_string()))?;
                // 4xx can never turn into the right document — except 408
                // and 429, which are the server asking to retry; redirects
                // are not followed, so a 3xx lands here too and stays
                // transient.
                if resp.status() != reqwest::StatusCode::OK {
                    let s = resp.status();
                    let retry_later = matches!(s.as_u16(), 408 | 429);
                    return Err(if s.is_client_error() && !retry_later {
                        CensusError::Refused(format!("status {s}"))
                    } else {
                        CensusError::Fetch(format!("status {s}"))
                    });
                }
                let format = content_format(
                    resp.headers()
                        .get(reqwest::header::CONTENT_TYPE)
                        .and_then(|v| v.to_str().ok()),
                );
                let mut body = Vec::new();
                while let Some(chunk) = resp
                    .chunk()
                    .await
                    .map_err(|e| CensusError::Fetch(e.to_string()))?
                {
                    if (body.len() + chunk.len()) as u64 > max {
                        return Err(CensusError::Refused("census document too large".into()));
                    }
                    body.extend_from_slice(&chunk);
                }
                Ok((body, format))
            }
            "file" => {
                let dir = self.opts.dir.clone().ok_or_else(|| {
                    CensusError::Refused(
                        "file:// censuses are disabled (no census directory)".into(),
                    )
                })?;
                let path = url
                    .to_file_path()
                    .map_err(|_| CensusError::Refused(format!("{uri:.200}: not a local path")))?;
                // File errors stay transient (Fetch): the organizer may still
                // be placing the document; the bootstrap attempt cap ends them.
                let body = tokio::task::spawn_blocking(move || read_census_file(&dir, &path, max))
                    .await
                    .map_err(|e| CensusError::Fetch(e.to_string()))??;
                Ok((body, DocFormat::Sniff))
            }
            s => Err(CensusError::Refused(format!("unsupported scheme {s}"))),
        }
    }

    fn tree(&self, root: &[u8; 32]) -> Result<Arc<LeanImt>> {
        if let Some(t) = self.cache.lock().ok().and_then(|mut c| c.get(root)) {
            return Ok(t);
        }
        let leaves = self
            .db
            .census_leaves(root)?
            .ok_or_else(|| CensusError::Unknown(format!("0x{}", hex::encode(root))))?;
        let tree = Arc::new(rebuild_tree(root, &leaves)?);
        if let Ok(mut c) = self.cache.lock() {
            c.put(*root, tree.clone());
        }
        Ok(tree)
    }

    /// [`CensusStore::tree`] for the async workers: the O(n) rebuild runs
    /// in `spawn_blocking`, single-flight per root.
    async fn tree_async(&self, key: [u8; 32]) -> Result<Arc<LeanImt>> {
        if let Some(t) = self.cache.lock().ok().and_then(|mut c| c.get(&key)) {
            return Ok(t);
        }
        let gate = {
            let mut m = self.rebuilds.lock().await;
            m.retain(|_, g| Arc::strong_count(g) > 1); // drop idle gates
            m.entry(key).or_default().clone()
        };
        let _g = gate.lock().await;
        // A concurrent miss may have built it while we waited.
        if let Some(t) = self.cache.lock().ok().and_then(|mut c| c.get(&key)) {
            return Ok(t);
        }
        let leaves = self
            .db
            .census_leaves(&key)?
            .ok_or_else(|| CensusError::Unknown(format!("0x{}", hex::encode(key))))?;
        let tree = tokio::task::spawn_blocking(move || rebuild_tree(&key, &leaves))
            .await
            .map_err(|e| CensusError::Fetch(e.to_string()))??;
        let tree = Arc::new(tree);
        if let Ok(mut c) = self.cache.lock() {
            c.put(key, tree.clone());
        }
        Ok(tree)
    }

    /// Census proof and weight of `address`; `None` if it is not in the
    /// census, an error if the census is not loaded. A cache miss rebuilds
    /// the tree (O(n)) on the calling thread; async handlers must use
    /// [`CensusStore::proof_async`] instead.
    pub fn proof(&self, root: &Fr, address: &[u8; 20]) -> Result<Option<(CensusProof, u128)>> {
        let key = fr_to_be(root);
        let tree = self.tree(&key)?;
        let Some(i) = self.db.census_index(&key, address)? else {
            return Ok(None);
        };
        proof_at(&tree, i, address).map(Some)
    }

    /// [`CensusStore::proof`] for the async workers: a non-member never
    /// triggers a rebuild (index point-lookup first) and a rebuild runs
    /// in `spawn_blocking`, single-flight per root.
    pub async fn proof_async(
        &self,
        root: &Fr,
        address: &[u8; 20],
    ) -> Result<Option<(CensusProof, u128)>> {
        let key = fr_to_be(root);
        let Some(i) = self.db.census_index(&key, address)? else {
            if self.db.census_exists(&key)? {
                return Ok(None);
            }
            return Err(CensusError::Unknown(format!("0x{}", hex::encode(key))));
        };
        let tree = self.tree_async(key).await?;
        proof_at(&tree, i, address).map(Some)
    }
}

/// Rebuilds the lean-IMT from stored leaves and checks it gives `root`.
fn rebuild_tree(root: &[u8; 32], leaves: &[[u8; 32]]) -> Result<LeanImt> {
    let corrupt = |reason: String| StorageError::Corrupt {
        table: "census",
        reason,
    };
    let leaves = leaves
        .iter()
        .map(|l| fr_from_be(l).map_err(|e| corrupt(e.to_string())))
        .collect::<Result<Vec<_>, _>>()?;
    let tree = LeanImt::from_leaves(leaves);
    if fr_to_be(&tree.root()) != *root {
        return Err(corrupt("stored leaves do not give the root".into()).into());
    }
    Ok(tree)
}

/// Proof at a known index, with the leaf checked against the address.
fn proof_at(tree: &LeanImt, i: u64, address: &[u8; 20]) -> Result<(CensusProof, u128)> {
    let corrupt = |reason: &str| StorageError::Corrupt {
        table: "census_addr",
        reason: reason.into(),
    };
    let proof = tree
        .proof(usize::try_from(i).map_err(|_| corrupt("index"))?)
        .map_err(|_| corrupt("index out of range"))?;
    let weight = census_leaf_weight(&proof.leaf);
    // The leaf must be this address's, or the index row is wrong.
    if census_leaf(address, weight).ok() != Some(proof.leaf) {
        return Err(corrupt("leaf belongs to another address").into());
    }
    Ok((proof, weight))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use davinci_zkvm_sdk::census::verify_census_proof;
    use davinci_zkvm_sdk::crypto::field::fr_from_dec;

    use super::*;

    // lean-imt-go vector from the SDK: leaves are `address << 88 | weight`.
    fn vector() -> (Vec<([u8; 20], u128)>, Fr) {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../davinci-zkvm/rust-sdk/testdata/leanimt.json");
        let v: serde_json::Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        let mut out = Vec::new();
        for l in v["leaves"].as_array().unwrap().iter().take(10) {
            let be = fr_to_be(&fr_from_dec(l.as_str().unwrap()).unwrap());
            let mut addr = [0u8; 20];
            addr.copy_from_slice(&be[1..21]);
            let mut w = [0u8; 16];
            w[5..].copy_from_slice(&be[21..]);
            out.push((addr, u128::from_be_bytes(w)));
        }
        let tree = &v["trees"][9];
        assert_eq!(tree["size"], 10);
        (out, fr_from_dec(tree["root"].as_str().unwrap()).unwrap())
    }

    fn census_json(parts: &[([u8; 20], u128)]) -> String {
        let ps: Vec<_> = parts
            .iter()
            .map(|(a, w)| serde_json::json!({"key": format!("0x{}", hex::encode(a)), "weight": w.to_string()}))
            .collect();
        serde_json::json!({ "participants": ps }).to_string()
    }

    fn write(dir: &tempfile::TempDir, name: &str, body: &str) -> String {
        let p = dir.path().join(name);
        std::fs::write(&p, body).unwrap();
        format!("file://{}", p.display())
    }

    fn db(dir: &tempfile::TempDir) -> Db {
        Db::open(&dir.path().join("sequencer.redb")).unwrap()
    }

    // `file://` allowed under `dir`.
    fn store(dir: &tempfile::TempDir) -> CensusStore {
        let opts = CensusOptions {
            dir: Some(dir.path().to_path_buf()),
            ..CensusOptions::default()
        };
        CensusStore::with_options(db(dir), opts).unwrap()
    }

    #[tokio::test]
    async fn file_census_matches_lean_imt_go() {
        let dir = tempfile::tempdir().unwrap();
        let (parts, root) = vector();
        let uri = write(&dir, "census.json", &census_json(&parts));
        let cs = store(&dir);
        cs.fetch(&uri, &root).await.unwrap();
        for (i, (addr, weight)) in parts.iter().enumerate() {
            let (proof, w) = cs.proof(&root, addr).unwrap().expect("in census");
            assert_eq!(w, *weight);
            assert_eq!(proof.root, root);
            assert_eq!(proof.leaf, census_leaf(addr, *weight).unwrap());
            assert!(verify_census_proof(&proof), "proof {i}");
        }
        assert!(cs.proof(&root, &[0xee; 20]).unwrap().is_none());
        // Fetching again is a no-op, and a reopened store still serves proofs.
        cs.fetch(&uri, &root).await.unwrap();
        drop(cs);
        let cs = store(&dir);
        let (proof, _) = cs.proof(&root, &parts[3].0).unwrap().unwrap();
        assert!(verify_census_proof(&proof));
    }

    // A non-member is answered from the index alone (no O(n) rebuild),
    // and a member's async proof matches the sync path.
    #[tokio::test]
    async fn proof_async_skips_rebuild_for_non_members() {
        let dir = tempfile::tempdir().unwrap();
        let (parts, root) = vector();
        let uri = write(&dir, "census.json", &census_json(&parts));
        let cs = store(&dir);
        cs.fetch(&uri, &root).await.unwrap();
        // A reopened store has only the db: the cache starts empty.
        drop(cs);
        let cs = store(&dir);
        assert!(cs.proof_async(&root, &[0xee; 20]).await.unwrap().is_none());
        let key = fr_to_be(&root);
        assert!(
            cs.cache.lock().unwrap().get(&key).is_none(),
            "a non-member lookup must not rebuild the tree"
        );
        let (pa, wa) = cs.proof_async(&root, &parts[2].0).await.unwrap().unwrap();
        assert!(cs.cache.lock().unwrap().get(&key).is_some());
        let (ps, ws) = cs.proof(&root, &parts[2].0).unwrap().unwrap();
        assert_eq!(
            (pa.root, pa.leaf, pa.path_bits),
            (ps.root, ps.leaf, ps.path_bits)
        );
        assert_eq!((&pa.siblings, wa), (&ps.siblings, ws));
        assert!(verify_census_proof(&pa));
        // An unloaded census is an error, not a silent None.
        assert!(
            cs.proof_async(&(root + Fr::from(1u64)), &parts[0].0)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn wrong_root_is_rejected_and_backed_off() {
        let dir = tempfile::tempdir().unwrap();
        let (parts, root) = vector();
        let uri = write(&dir, "census.json", &census_json(&parts));
        let cs = store(&dir);
        let wrong = root + Fr::from(1u64);
        assert!(matches!(
            cs.fetch(&uri, &wrong).await,
            Err(CensusError::RootMismatch { .. })
        ));
        // Nothing was stored under either root.
        assert!(cs.proof(&wrong, &parts[0].0).is_err());
        assert!(cs.proof(&root, &parts[0].0).is_err());
        // The same bad (uri, root) is not rebuilt right away.
        assert!(matches!(
            cs.fetch(&uri, &wrong).await,
            Err(CensusError::Backoff(_))
        ));
        // Another root for the same uri is its own entry.
        cs.fetch(&uri, &root).await.unwrap();
    }

    #[tokio::test]
    async fn malformed_censuses_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let (parts, _) = vector();
        let cs = store(&dir);
        // A duplicate address would give one voter two slots.
        let mut dup = parts.clone();
        dup.push(parts[2]);
        let leaves: Vec<Fr> = dup
            .iter()
            .map(|(a, w)| census_leaf(a, *w).unwrap())
            .collect();
        let uri = write(&dir, "dup.json", &census_json(&dup));
        let e = cs
            .fetch(&uri, &LeanImt::from_leaves(leaves).root())
            .await
            .err()
            .unwrap();
        assert!(e.to_string().contains("listed twice"), "{e}");

        let any = Fr::from(5u64);
        let a = hex::encode([1u8; 20]);
        let b = hex::encode([2u8; 20]);
        for (name, body) in [
            ("empty.json", r#"{"participants":[]}"#.to_string()),
            ("notjson.json", "{participants".to_string()),
            (
                "badaddr.json",
                r#"{"participants":[{"key":"0x12","weight":"1"}]}"#.to_string(),
            ),
            (
                "heavy.json",
                format!(
                    r#"{{"participants":[{{"key":"0x{a}","weight":"{}"}}]}}"#,
                    1u128 << 88
                ),
            ),
            (
                "negweight.json",
                format!(r#"{{"participants":[{{"key":"0x{a}","weight":"-1"}}]}}"#),
            ),
            (
                "dupindex.json",
                format!(
                    r#"{{"participants":[{{"addressIndex":0,"address":"0x{a}","weight":1}},{{"addressIndex":0,"address":"0x{b}","weight":1}}]}}"#
                ),
            ),
            (
                "someindex.json",
                format!(
                    r#"{{"participants":[{{"addressIndex":0,"address":"0x{a}","weight":1}},{{"address":"0x{b}","weight":1}}]}}"#
                ),
            ),
        ] {
            let uri = write(&dir, name, &body);
            let e = cs.fetch(&uri, &any).await.err().unwrap();
            assert!(matches!(e, CensusError::Format(_)), "{name}: {e}");
        }
        for uri in [
            "ftp://x/c.json",
            "file:///nonexistent/c.json",
            "file:///tmp",
        ] {
            assert!(cs.fetch(uri, &any).await.is_err(), "{uri}");
        }
    }

    // Dump (lean-imt-go `CensusDump`) and JSONL give the same leaves: a
    // big.Int weight up to 2^88 - 1 as a bare number, an explicit empty entry
    // as a zero leaf at its index, blank lines and CRLF in JSONL.
    #[test]
    fn dump_and_jsonl_formats() {
        let (a, b, z) = ([1u8; 20], [2u8; 20], [0u8; 20]);
        let big = (1u128 << 88) - 1;
        let want = vec![
            census_leaf(&a, big).unwrap(),
            Fr::default(),
            census_leaf(&b, 1).unwrap(),
        ];
        let (ha, hb, hz) = (hex::encode(a), hex::encode(b), hex::encode(z));
        let dump = format!(
            r#"{{"root":"7","tree":{{}},"participants":[{{"addressIndex":2,"address":"0x{hb}","weight":1}},{{"addressIndex":1,"address":"0x{hz}","weight":0}},{{"addressIndex":0,"address":"0x{ha}","weight":{big}}}]}}"#
        );
        let jsonl = format!(
            "{{\"key\":\"0x{ha}\",\"weight\":\"{big}\"}}\r\n\n{{\"address\":\"0x{hz}\",\"weight\":0}}\n  \n{{\"key\":\"0x{hb}\",\"weight\":1}}\n\n"
        );
        for (body, f) in [
            (&dump, DocFormat::Dump),
            (&dump, DocFormat::Sniff),
            (&jsonl, DocFormat::Jsonl),
            (&jsonl, DocFormat::Sniff),
        ] {
            let doc = parse_doc(body.as_bytes(), f).unwrap();
            let (leaves, mut index) = leaves_of(&doc, 10).unwrap();
            assert_eq!(leaves, want, "{f:?}");
            index.sort();
            assert_eq!(index, vec![(a, 0), (b, 2)], "{f:?}");
        }
        let doc = parse_doc(dump.as_bytes(), DocFormat::Dump).unwrap();
        assert_eq!(parse_root(doc.root.unwrap()).unwrap(), Fr::from(7u64));
        // A bad JSONL line is named.
        let e = parse_doc(
            b"{\"key\":\"0x00\",\"weight\":1}\n\n{oops",
            DocFormat::Jsonl,
        )
        .err()
        .unwrap();
        assert!(e.to_string().contains("line 3"), "{e}");
        // Weights: 2^88 and beyond (as numbers or strings), signs, fractions.
        for w in [
            (1u128 << 88).to_string(),
            format!("\"{}\"", 1u128 << 88),
            u128::MAX.to_string(),
            "1".repeat(40),
            "-1".into(),
            "1.0".into(),
            "1e3".into(),
            "\"\"".into(),
            "null".into(),
        ] {
            let body = format!(r#"{{"participants":[{{"key":"0x{ha}","weight":{w}}}]}}"#);
            let doc = parse_doc(body.as_bytes(), DocFormat::Dump);
            let r = doc.and_then(|d| leaves_of(&d, 10).map(|_| ()));
            assert!(matches!(r, Err(CensusError::Format(_))), "{w}: {r:?}");
        }
    }

    // A dump whose `root` is not the on-chain root is refused as a mismatch,
    // and one whose `root` matches is accepted.
    #[tokio::test]
    async fn dump_root_must_match() {
        let dir = tempfile::tempdir().unwrap();
        let (parts, root) = vector();
        let ps: Vec<_> = parts
            .iter()
            .map(|(a, w)| serde_json::json!({"address": format!("0x{}", hex::encode(a)), "weight": w.to_string()}))
            .collect();
        let dec = davinci_zkvm_sdk::crypto::field::fr_to_dec(&root);
        let cs = store(&dir);
        let bad = write(
            &dir,
            "bad.json",
            &serde_json::json!({"root": "5", "participants": ps}).to_string(),
        );
        assert!(matches!(
            cs.fetch(&bad, &root).await,
            Err(CensusError::RootMismatch { .. })
        ));
        let good = write(
            &dir,
            "good.json",
            &serde_json::json!({"root": dec, "participants": ps}).to_string(),
        );
        cs.fetch(&good, &root).await.unwrap();
    }

    // Two voters on one ballot slot refuse the census, naming the slot.
    #[test]
    fn slot_collision_is_refused() {
        let a = [1u8; 20];
        let mut b = [1u8; 20];
        b[19] = 2;
        let body = format!(
            r#"{{"participants":[{{"key":"0x{}","weight":1}},{{"key":"0x{}","weight":2}}]}}"#,
            hex::encode(a),
            hex::encode(b)
        );
        let doc = parse_doc(body.as_bytes(), DocFormat::Dump).unwrap();
        // Distinct under the real slot function, colliding under the first byte.
        assert!(leaves_of(&doc, 10).is_ok());
        let e = leaves_of_with(&doc, 10, |a| u64::from(a[0]) + 0x10)
            .err()
            .unwrap();
        assert!(matches!(e, CensusError::Format(_)));
        assert!(e.to_string().contains("ballot slot 0x11"), "{e}");
    }

    #[tokio::test]
    async fn sparse_index_is_refused_before_building() {
        let dir = tempfile::tempdir().unwrap();
        let cs = store(&dir);
        // 120 bytes that would otherwise force a 2^24-leaf tree.
        let body = format!(
            r#"{{"participants":[{{"addressIndex":16777215,"address":"0x{}","weight":1}}]}}"#,
            hex::encode([1u8; 20])
        );
        let uri = write(&dir, "sparse.json", &body);
        let t = Instant::now();
        let err = cs.fetch(&uri, &Fr::from(1u64)).await.err().unwrap();
        assert!(matches!(err, CensusError::Format(_)), "{err}");
        assert!(
            t.elapsed() < Duration::from_millis(500),
            "{:?}",
            t.elapsed()
        );
        // Too many participants are refused before any leaf is built.
        let (parts, root) = vector();
        let opts = CensusOptions {
            dir: Some(dir.path().to_path_buf()),
            max_participants: 9,
            ..CensusOptions::default()
        };
        let small = CensusStore::with_options(db(&tempfile::tempdir().unwrap()), opts).unwrap();
        let uri = write(&dir, "ten.json", &census_json(&parts));
        assert!(matches!(
            small.fetch(&uri, &root).await,
            Err(CensusError::Format(_))
        ));
    }

    #[tokio::test]
    async fn file_uris_are_confined() {
        let dir = tempfile::tempdir().unwrap();
        let (parts, root) = vector();
        let good = write(&dir, "census.json", &census_json(&parts));
        let any = Fr::from(3u64);
        let procs = [
            "file:///proc/self/pagemap",
            "file:///proc/self/environ",
            "file:///dev/zero",
        ];

        // No census directory: file:// is off, even for a good file.
        let off = CensusStore::new(db(&dir)).unwrap();
        for uri in procs.iter().copied().chain([good.as_str()]) {
            let e = off.fetch(uri, &any).await.err().unwrap();
            assert!(e.to_string().contains("disabled"), "{uri}: {e}");
        }

        drop(off);
        // With a directory: only regular files inside it.
        let cs = store(&dir);
        for uri in procs {
            let e = cs.fetch(uri, &any).await.err().unwrap();
            assert!(matches!(e, CensusError::Fetch(_)), "{uri}: {e}");
        }
        // A symlink inside the directory that points out of it.
        std::os::unix::fs::symlink("/proc/self/environ", dir.path().join("env.json")).unwrap();
        std::os::unix::fs::symlink("/etc/hostname", dir.path().join("host.json")).unwrap();
        for name in ["env.json", "host.json"] {
            let uri = format!("file://{}", dir.path().join(name).display());
            let e = cs.fetch(&uri, &any).await.err().unwrap();
            assert!(e.to_string().contains("outside"), "{name}: {e}");
        }
        let uri = format!("file://{}/sub/../../etc/passwd", dir.path().display());
        assert!(cs.fetch(&uri, &any).await.is_err());
        cs.fetch(&good, &root).await.unwrap();

        // The read is capped, whatever the metadata says.
        let opts = CensusOptions {
            dir: Some(dir.path().to_path_buf()),
            max_bytes: 100,
            ..CensusOptions::default()
        };
        let tiny = CensusStore::with_options(db(&tempfile::tempdir().unwrap()), opts).unwrap();
        let e = tiny.fetch(&good, &root).await.err().unwrap();
        assert!(e.to_string().contains("cap"), "{e}");
        let canon = std::fs::canonicalize(dir.path()).unwrap();
        let e = read_census_file(&canon, &canon.join("census.json"), 10)
            .err()
            .unwrap();
        assert!(e.to_string().contains("cap"), "{e}");

        // A census directory under /proc, /sys or /dev is refused.
        for d in ["/proc/self", "/dev", "/sys"] {
            let opts = CensusOptions {
                dir: Some(PathBuf::from(d)),
                ..CensusOptions::default()
            };
            assert!(
                CensusStore::with_options(db(&tempfile::tempdir().unwrap()), opts).is_err(),
                "{d}"
            );
        }
    }

    async fn serve(app: axum::Router) -> SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        addr
    }

    #[tokio::test]
    async fn http_census_and_number_weights() {
        use axum::{Router, routing::get};
        let (parts, root) = vector();
        // davinci-node dumps: `address` and numeric weights are accepted too.
        let ps: Vec<_> = parts
            .iter()
            .enumerate()
            .map(|(i, (a, w))| {
                serde_json::json!({"addressIndex": 9 - i, "address": format!("0x{}", hex::encode(a)), "weight": *w as u64})
            })
            .rev()
            .collect();
        // Reversed indexes: the tree order follows addressIndex, not the array.
        let reversed: Vec<Fr> = parts
            .iter()
            .rev()
            .map(|(a, w)| census_leaf(a, *w).unwrap())
            .collect();
        let rroot = LeanImt::from_leaves(reversed).root();
        assert_ne!(rroot, root);
        let dec = davinci_zkvm_sdk::crypto::field::fr_to_dec(&rroot);
        // big.Int root as a bare JSON number.
        let body = format!(
            r#"{{"root":{dec},"participants":{}}}"#,
            serde_json::Value::from(ps)
        );
        // The same census as JSONL, found by its Content-Type.
        let jsonl: String = parts
            .iter()
            .rev()
            .map(|(a, w)| format!("{{\"key\":\"0x{}\",\"weight\":{w}}}\n", hex::encode(a)))
            .collect();
        let app = Router::new()
            .route("/census.json", get(move || async move { body }))
            .route(
                "/census",
                get(move || async move {
                    (
                        [(axum::http::header::CONTENT_TYPE, "application/x-ndjson")],
                        jsonl,
                    )
                }),
            );
        let addr = serve(app).await;

        let dir = tempfile::tempdir().unwrap();
        let opts = CensusOptions {
            allow_private: true,
            ..CensusOptions::default()
        };
        let cs = CensusStore::with_options(db(&dir), opts.clone()).unwrap();
        cs.fetch(&format!("http://{addr}/census.json"), &rroot)
            .await
            .unwrap();
        let (proof, w) = cs.proof(&rroot, &parts[9].0).unwrap().unwrap();
        assert_eq!(w, parts[9].1);
        assert_eq!(proof.path_bits, 0, "parts[9] is leaf 0");
        assert!(verify_census_proof(&proof));
        let dir2 = tempfile::tempdir().unwrap();
        let cs2 = CensusStore::with_options(db(&dir2), opts).unwrap();
        assert!(
            cs2.fetch(&format!("http://{addr}/missing.json"), &rroot)
                .await
                .is_err()
        );
        cs2.fetch(&format!("http://{addr}/census"), &rroot)
            .await
            .unwrap();
        assert!(cs2.proof(&rroot, &parts[0].0).unwrap().is_some());
    }

    #[tokio::test]
    async fn private_hosts_and_redirects_are_refused() {
        use axum::response::Redirect;
        use axum::{Router, routing::get};
        let (parts, root) = vector();
        let body = census_json(&parts);
        let app = Router::new()
            .route("/census.json", get(move || async move { body }))
            .route(
                "/moved",
                get(|| async { Redirect::temporary("/census.json") }),
            );
        let addr = serve(app).await;
        let port = addr.port();

        // Default: loopback by literal or by name is refused.
        let dir = tempfile::tempdir().unwrap();
        let cs = CensusStore::new(db(&dir)).unwrap();
        for host in ["127.0.0.1", "localhost", "[::1]", "0.0.0.0"] {
            let e = cs
                .fetch(&format!("http://{host}:{port}/census.json"), &root)
                .await
                .err()
                .unwrap();
            // A literal private IP is refused for good; a name that resolves
            // privately fails at DNS time and stays transient.
            assert!(
                matches!(e, CensusError::Fetch(_) | CensusError::Refused(_)),
                "{host}: {e}"
            );
        }
        // Even with private hosts allowed, a redirect is not followed.
        let dir = tempfile::tempdir().unwrap();
        let opts = CensusOptions {
            allow_private: true,
            ..CensusOptions::default()
        };
        let cs = CensusStore::with_options(db(&dir), opts).unwrap();
        let e = cs
            .fetch(&format!("http://{addr}/moved"), &root)
            .await
            .err()
            .unwrap();
        assert!(e.to_string().contains("307"), "{e}");
        cs.fetch(&format!("http://{addr}/census.json"), &root)
            .await
            .unwrap();

        for ip in [
            "10.0.0.1",
            "192.168.1.1",
            "169.254.169.254",
            "100.64.0.1",
            "fd00::1",
            "fe80::1",
            "::ffff:127.0.0.1",
        ] {
            assert!(!is_public(ip.parse().unwrap()), "{ip}");
        }
        for ip in ["1.1.1.1", "2606:4700::1111"] {
            assert!(is_public(ip.parse().unwrap()), "{ip}");
        }
    }

    // Runs only inside `proxy_env_is_ignored`'s child process, which has the
    // proxy variables set (the test cannot set env vars: no `unsafe`).
    #[tokio::test]
    async fn proxy_env_child() {
        if std::env::var("DAVINCI_PROXY_CHILD").is_err() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let cs = CensusStore::new(db(&dir)).unwrap();
        assert!(
            cs.fetch("http://census.invalid/c.json", &Fr::from(1u64))
                .await
                .is_err()
        );
        println!("proxy child ran");
    }

    #[test]
    fn proxy_env_is_ignored() {
        let proxy = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", proxy.local_addr().unwrap());
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["census::tests::proxy_env_child", "--exact", "--nocapture"])
            .env("DAVINCI_PROXY_CHILD", "1")
            .env("HTTP_PROXY", &url)
            .env("http_proxy", &url)
            .env("HTTPS_PROXY", &url)
            .env("https_proxy", &url)
            .env("ALL_PROXY", &url)
            .env("all_proxy", &url)
            .env_remove("NO_PROXY")
            .env_remove("no_proxy")
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(out.status.success(), "{stdout}");
        assert!(stdout.contains("proxy child ran"), "{stdout}");
        // The proxy never saw a connection.
        proxy.set_nonblocking(true).unwrap();
        match proxy.accept() {
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            other => panic!("census request went through the proxy: {other:?}"),
        }
    }

    #[test]
    fn root_dir_and_special_files_are_refused() {
        let opts = CensusOptions {
            dir: Some(PathBuf::from("/")),
            ..CensusOptions::default()
        };
        assert!(CensusStore::with_options(db(&tempfile::tempdir().unwrap()), opts).is_err());
        // Even with a census directory that contains them.
        for f in [
            "/proc/self/environ",
            "/proc/self/pagemap",
            "/sys/kernel/uevent_seqnum",
            "/dev/null",
        ] {
            let e = read_census_file(Path::new("/"), Path::new(f), 1 << 20)
                .err()
                .unwrap();
            assert!(e.to_string().contains("/proc, /sys or /dev"), "{f}: {e}");
        }
        assert!(read_census_file(Path::new("/"), Path::new("/etc/hostname"), 1 << 20).is_ok());
    }

    #[tokio::test]
    async fn failure_map_is_bounded_and_ignores_queries() {
        let dir = tempfile::tempdir().unwrap();
        let cs = CensusStore::new(db(&dir)).unwrap();
        let root = Fr::from(9u64);
        // file:// is off here, so every fetch fails fast and is remembered.
        assert!(matches!(
            cs.fetch("file:///a.json?x=1", &root).await,
            Err(CensusError::Refused(_))
        ));
        assert!(matches!(
            cs.fetch("file:///a.json?x=2#frag", &root).await,
            Err(CensusError::Backoff(_))
        ));
        for i in 0..MAX_FAILURES + 10 {
            let _ = cs.fetch(&format!("file:///n{i}.json"), &root).await;
        }
        assert!(cs.failures.lock().unwrap().len() <= MAX_FAILURES);
        // The oldest entries went first, the newest are still backing off.
        assert!(matches!(
            cs.fetch("file:///a.json", &root).await,
            Err(CensusError::Refused(_))
        ));
        let last = format!("file:///n{}.json", MAX_FAILURES + 9);
        assert!(matches!(
            cs.fetch(&last, &root).await,
            Err(CensusError::Backoff(_))
        ));
    }

    #[test]
    fn is_public_table() {
        for ip in [
            "10.0.0.1",
            "192.168.1.1",
            "169.254.169.254",
            "100.64.0.1",
            "0.1.2.3",
            "240.0.0.1",
            "fd00::1",
            "fe80::1",
            "::ffff:127.0.0.1",
            "::ffff:10.0.0.1",
            "::127.0.0.1",
            "::10.1.2.3",
            "64:ff9b::1.1.1.1",
            "64:ff9b:1::1",
            "2002:c0a8:0101::1",
            "2001:db8::1",
            "2001:0:4136:e378::1",
            "::",
            "::1",
        ] {
            assert!(!is_public(ip.parse().unwrap()), "{ip}");
        }
        for ip in ["1.1.1.1", "8.8.8.8", "2606:4700::1111", "::ffff:1.1.1.1"] {
            assert!(is_public(ip.parse().unwrap()), "{ip}");
        }
    }

    // 408/429 classify transient (bootstrap retries them); a 404 refuses.
    #[tokio::test]
    async fn rate_limited_census_is_transient_not_refused() {
        use axum::http::StatusCode;
        use axum::{Router, routing::get};
        let (parts, root) = vector();
        let body = census_json(&parts);
        let app = Router::new()
            .route("/rl", get(|| async { StatusCode::TOO_MANY_REQUESTS }))
            .route("/slow", get(|| async { StatusCode::REQUEST_TIMEOUT }))
            .route("/census.json", get(move || async move { body }));
        let addr = serve(app).await;
        let dir = tempfile::tempdir().unwrap();
        let opts = CensusOptions {
            allow_private: true,
            ..CensusOptions::default()
        };
        let cs = CensusStore::with_options(db(&dir), opts).unwrap();
        for path in ["rl", "slow"] {
            let e = cs
                .fetch(&format!("http://{addr}/{path}"), &root)
                .await
                .err()
                .unwrap();
            assert!(matches!(e, CensusError::Fetch(_)), "{path}: {e}");
        }
        let e = cs
            .fetch(&format!("http://{addr}/missing"), &root)
            .await
            .err()
            .unwrap();
        assert!(matches!(e, CensusError::Refused(_)), "{e}");
        // The same document still loads once the rate limit lifts.
        cs.fetch(&format!("http://{addr}/census.json"), &root)
            .await
            .unwrap();
    }

    #[test]
    fn tree_cache_is_bounded() {
        let mut c = TreeCache::default();
        let t = Arc::new(LeanImt::new());
        for i in 0..20u8 {
            c.put([i; 32], t.clone());
            if i == 10 {
                // Touch an old entry so it survives.
                assert!(c.get(&[5; 32]).is_some());
            }
        }
        assert_eq!(c.map.len(), TREE_CACHE);
        assert_eq!(c.order.len(), TREE_CACHE);
        assert!(c.get(&[19; 32]).is_some());
        assert!(c.get(&[0; 32]).is_none());
    }

    /// Organizer text (census URI, document fields) cannot forge log
    /// lines: control characters are escaped before they enter an error.
    #[tokio::test]
    async fn untrusted_error_text_is_escaped() {
        let dir = tempfile::tempdir().unwrap();
        let cs = store(&dir);
        let e = cs
            .fetch("no scheme\nERROR forged line", &Fr::default())
            .await
            .unwrap_err();
        assert!(!e.to_string().contains('\n'), "{e}");
        assert!(e.to_string().contains("no scheme\\nERROR"), "{e}");
        let e = parse_address("0x00\nERROR forged line").unwrap_err();
        assert!(!e.to_string().contains('\n'), "{e}");
    }
}
