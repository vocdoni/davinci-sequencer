//! Attacks on what the node reads from the settlement chain: hostile
//! `eth_getLogs` answers (through the real `Contracts` decoder against a
//! scripted EL stub), hostile process parameters, hostile event streams and
//! process floods (through the monitor and actors on the shared fakes).

mod common;

use alloy::sol_types::SolEvent;
use common::*;
use davinci_sequencer::web3::Contracts;
use serde_json::{Value, json};

alloy::sol! {
    event ProcessCreated(bytes31 indexed processId, address indexed creator);
    event ProcessMetadataUpdated(bytes31 indexed processId, string metadataURI, bytes32 metadataHash);
    event ProcessStateTransitioned(
        bytes31 indexed processId,
        address indexed sender,
        bytes32 oldStateRoot,
        bytes32 newStateRoot,
        uint256 newVotersCount,
        uint256 newOverwrittenVotesCount,
        uint256 nBlobs
    );
    event ProcessStateTransitionedL1(
        bytes31 indexed processId,
        uint256[] l1BlockNumbers,
        bytes32[] versionedHashes
    );
}

// ------------------------------------------------------------ EL stub

const REGISTRY: [u8; 20] = [0x11; 20];
const STUB_PID: [u8; 31] = [7; 31];

/// An execution-layer stub: `eth_getLogs` answers the scripted logs
/// whatever the filter asks, like a misbehaving or lying endpoint.
#[derive(Default)]
struct El {
    head: AtomicU64,
    logs: Mutex<Vec<Value>>,
}

/// A log of `ev` as an endpoint serializes it.
fn log_of(
    ev: &impl SolEvent,
    address: [u8; 20],
    block: Option<u64>,
    tx: u8,
    index: u64,
    removed: bool,
) -> Value {
    let d = ev.encode_log_data();
    json!({
        "address": Address::from(address),
        "topics": d.topics(),
        "data": d.data,
        "blockNumber": block.map(|b| format!("{b:#x}")),
        "blockHash": B256::repeat_byte(1),
        "transactionHash": B256::repeat_byte(tx),
        "transactionIndex": "0x0",
        "logIndex": format!("{index:#x}"),
        "removed": removed,
    })
}

fn metadata(uri: &str) -> ProcessMetadataUpdated {
    ProcessMetadataUpdated {
        processId: STUB_PID.into(),
        metadataURI: uri.into(),
        metadataHash: B256::ZERO,
    }
}

async fn el(head: u64, logs: Vec<Value>) -> (Contracts, Arc<El>) {
    let n = Arc::new(El {
        head: AtomicU64::new(head),
        logs: Mutex::new(logs),
    });
    let st = n.clone();
    let app = axum::Router::new().route(
        "/",
        axum::routing::post(move |axum::Json(req): axum::Json<Value>| async move {
            let ok = |v: Value| json!({"jsonrpc":"2.0","id":req["id"],"result":v});
            let out = match req["method"].as_str().unwrap_or("") {
                "eth_chainId" => ok("0x64".into()),
                "eth_getCode" => ok("0x6001".into()),
                "eth_blockNumber" => ok(format!("{:#x}", st.head.load(Ordering::SeqCst)).into()),
                "eth_getLogs" => ok(Value::Array(st.logs.lock().unwrap().clone())),
                m => panic!("unexpected method {m}"),
            };
            axum::Json(out)
        }),
    );
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url: url::Url = format!("http://{}/", l.local_addr().unwrap())
        .parse()
        .unwrap();
    tokio::spawn(async move { axum::serve(l, app).await });
    let c = Contracts::open(&[url], Address::from(REGISTRY), None, false)
        .await
        .unwrap();
    (c, n)
}

// BUG: logs outside the asked range are passed on. An event from a block
// past the head raises every actor's `seen_block` above any real head, so
// each later chain read is refused ("endpoint head behind") and the
// close-out and the results wait forever.
#[tokio::test]
async fn logs_outside_the_asked_range_are_dropped() {
    let logs = vec![
        log_of(&metadata("below"), REGISTRY, Some(10), 1, 0, false),
        log_of(&metadata("inside"), REGISTRY, Some(50), 2, 0, false),
        log_of(&metadata("above"), REGISTRY, Some(1_000_000), 3, 0, false),
        log_of(&metadata("far"), REGISTRY, Some(u64::MAX), 4, 0, false),
    ];
    let (c, _) = el(100, logs).await;
    let evs = c.events(40, 60).await.unwrap();
    let blocks: Vec<u64> = evs.iter().map(|e| e.block).collect();
    assert_eq!(blocks, vec![50], "only the log inside 40..=60");
}

// Removed logs, other contracts' logs, unknown and missing topics: skipped,
// the page still delivers the registry's own event.
#[tokio::test]
async fn removed_foreign_and_unknown_logs_are_skipped() {
    let mut unknown = log_of(&metadata("x"), REGISTRY, Some(50), 5, 3, false);
    unknown["topics"][0] = json!(B256::repeat_byte(0x99));
    let mut no_topics = log_of(&metadata("x"), REGISTRY, Some(50), 6, 4, false);
    no_topics["topics"] = json!([]);
    let logs = vec![
        log_of(&metadata("removed"), REGISTRY, Some(50), 1, 0, true),
        log_of(&metadata("other"), [0x22; 20], Some(50), 2, 1, false),
        log_of(&metadata("real"), REGISTRY, Some(50), 3, 2, false),
        unknown,
        no_topics,
    ];
    let (c, _) = el(100, logs).await;
    let evs = c.events(40, 60).await.unwrap();
    assert_eq!(evs.len(), 1);
    assert!(matches!(&evs[0].kind, EventKind::MetadataUpdated { uri, .. } if uri == "real"));
}

// A pending log (no block number) in a confirmed range: the page fails and
// is retried, never routed half-decoded.
#[tokio::test]
async fn a_pending_log_fails_the_page() {
    let logs = vec![
        log_of(&metadata("ok"), REGISTRY, Some(50), 1, 0, false),
        log_of(&metadata("pending"), REGISTRY, None, 2, 0, false),
    ];
    let (c, _) = el(100, logs).await;
    assert!(c.events(40, 60).await.is_err());
}

fn transition_pair(block: u64, tx: u8) -> [Value; 2] {
    let t = ProcessStateTransitioned {
        processId: STUB_PID.into(),
        sender: Address::repeat_byte(0xee),
        oldStateRoot: B256::repeat_byte(1),
        newStateRoot: B256::repeat_byte(2),
        newVotersCount: alloy::primitives::U256::from(1),
        newOverwrittenVotesCount: alloy::primitives::U256::ZERO,
        nBlobs: alloy::primitives::U256::from(1),
    };
    let l1 = ProcessStateTransitionedL1 {
        processId: STUB_PID.into(),
        l1BlockNumbers: vec![alloy::primitives::U256::from(77)],
        versionedHashes: vec![B256::repeat_byte(3)],
    };
    [
        log_of(&t, REGISTRY, Some(block), tx, 0, false),
        log_of(&l1, REGISTRY, Some(block), tx, 1, false),
    ]
}

// BUG: an endpoint that answers a log twice (a load-balanced backend
// merging two answers) fails every page holding an L1-blob transition:
// the second copy of `ProcessStateTransitionedL1` finds its transition
// already paired. The monitor retries the page forever, so routing stops
// for every election.
#[tokio::test]
async fn a_duplicated_log_does_not_wedge_the_page() {
    let [t, l1] = transition_pair(50, 9);
    let logs = vec![t.clone(), l1.clone(), t, l1];
    let (c, _) = el(100, logs).await;
    let evs = c.events(40, 60).await.unwrap();
    assert_eq!(evs.len(), 1, "one transition, once");
    assert!(matches!(
        &evs[0].kind,
        EventKind::StateTransitioned { l1: Some(b), .. } if b.blocks == [77]
    ));
}

// ------------------------------------------------------- hostile chain

/// The fake chain plus processes and events an attacker put next to the
/// honest election (`pid31()`). Hostile processes answer reads from their
/// own records; every write to them fails, so a node's attempt to settle or
/// finalize one cannot touch the honest process's state.
#[derive(Clone)]
struct Hostile {
    chain: FakeChain,
    procs: Arc<Mutex<HashMap<[u8; 31], OnchainProcess>>>,
    events: Arc<Mutex<Vec<RegistryEvent>>>,
    /// Logs the endpoint returns outside the asked range, once each.
    stray: Arc<Mutex<Vec<RegistryEvent>>>,
    /// Calls that would write to a hostile process.
    writes: Arc<AtomicU64>,
    /// `process()`/`process_at()` reads of hostile processes.
    reads: Arc<AtomicU64>,
    /// Blobs the blob source serves for a transaction instead of its own.
    swap: Arc<Mutex<HashMap<B256, Vec<Blob>>>>,
    /// The next `head()` answers this block instead, once.
    bogus_head: Arc<Mutex<Option<u64>>>,
    /// While set, `head()` and `confirmed_head()` answer this many blocks
    /// and seconds behind the chain (a failover onto a lagging endpoint).
    lag: Arc<Mutex<Option<(u64, u64)>>>,
    /// `head()` alone answers this many blocks behind (a lagging backend
    /// behind a balancer; the monitor's pages still move).
    head_lag: Arc<AtomicU64>,
    /// `head()` answers this many blocks ahead of any block the endpoint
    /// has (a bogus `latest`), until reset.
    far_head: Arc<AtomicU64>,
    /// `process()` of the honest election answers this root (a backend on
    /// another state), until reset.
    number_root: Arc<Mutex<Option<[u8; 32]>>>,
}

impl Hostile {
    fn new(chain: FakeChain) -> Self {
        Hostile {
            chain,
            procs: Arc::default(),
            events: Arc::default(),
            stray: Arc::default(),
            writes: Arc::default(),
            reads: Arc::default(),
            swap: Arc::default(),
            bogus_head: Arc::default(),
            lag: Arc::default(),
            head_lag: Arc::default(),
            far_head: Arc::default(),
            number_root: Arc::default(),
        }
    }

    /// Creates `p` under `pid` in the current head block.
    fn create(&self, pid: [u8; 31], p: OnchainProcess) {
        self.procs.lock().unwrap().insert(pid, p);
        self.push(EventKind::ProcessCreated {
            pid,
            creator: Address::repeat_byte(0x66),
        });
    }

    /// Appends an event in the current head block.
    fn push(&self, kind: EventKind) {
        self.push_at(self.chain.head_block(), kind);
    }

    /// Appends an event in `block` (above the head: routed once it is).
    fn push_at(&self, block: u64, kind: EventKind) {
        let mut e = self.events.lock().unwrap();
        let log_index = 1000 + e.len() as u64;
        e.push(RegistryEvent {
            block,
            tx_hash: B256::repeat_byte(0x66),
            log_index,
            kind,
        });
    }

    fn hostile(&self, pid: &[u8; 31]) -> Option<OnchainProcess> {
        self.procs.lock().unwrap().get(pid).cloned()
    }

    fn write(&self, pid: &[u8; 31]) -> Result<(), Web3Error> {
        if *pid == pid31() {
            return Ok(());
        }
        self.writes.fetch_add(1, Ordering::SeqCst);
        Err(Web3Error::Rpc("hostile process: write refused".into()))
    }
}

#[async_trait]
impl Chain for Hostile {
    fn signer(&self) -> Option<Address> {
        self.chain.signer()
    }
    async fn process(&self, pid: &[u8; 31]) -> Result<OnchainProcess, Web3Error> {
        if *pid == pid31() {
            let mut p = self.chain.process(pid).await?;
            if let Some(r) = *self.number_root.lock().unwrap() {
                p.state_root = r;
            }
            return Ok(p);
        }
        self.reads.fetch_add(1, Ordering::SeqCst);
        self.hostile(pid)
            .ok_or_else(|| Web3Error::Data(format!("unknown process 0x{}", hex::encode(pid))))
    }
    async fn process_at(&self, pid: &[u8; 31], block: u64) -> Result<OnchainProcess, Web3Error> {
        // Like a node: no state past its head (a bogus head's block fails).
        if block > self.chain.head_block() {
            return Err(Web3Error::Rpc(format!("header not found: {block}")));
        }
        if *pid == pid31() {
            return self.chain.process_at(pid, block).await;
        }
        self.process(pid).await
    }
    async fn process_at_hash(
        &self,
        pid: &[u8; 31],
        hash: [u8; 32],
    ) -> Result<OnchainProcess, Web3Error> {
        if *pid == pid31() {
            return self.chain.process_at_hash(pid, hash).await;
        }
        self.process(pid).await
    }
    async fn events(&self, from: u64, to: u64) -> Result<Vec<RegistryEvent>, Web3Error> {
        let mut out = self.chain.events(from, to).await?;
        out.extend(
            self.events
                .lock()
                .unwrap()
                .iter()
                .filter(|e| e.block >= from && e.block <= to)
                .cloned(),
        );
        out.append(&mut self.stray.lock().unwrap());
        out.sort_by_key(|e| (e.block, e.log_index));
        Ok(out)
    }
    async fn head(&self) -> Result<(u64, u64), Web3Error> {
        let (block, time) = self.chain.head().await?;
        let (db, dt) = self.lag.lock().unwrap().unwrap_or((0, 0));
        let db = db + self.head_lag.load(Ordering::SeqCst);
        let block = self.bogus_head.lock().unwrap().take().unwrap_or(block);
        let block = block + self.far_head.load(Ordering::SeqCst);
        Ok((block.saturating_sub(db), time.saturating_sub(dt)))
    }
    async fn confirmed_head(&self, confirmations: u64) -> Result<u64, Web3Error> {
        let (db, _) = self.lag.lock().unwrap().unwrap_or((0, 0));
        let b = self.chain.confirmed_head(confirmations).await?;
        Ok(b.saturating_sub(db))
    }
    async fn block_ref(&self, n: u64) -> Result<Option<([u8; 32], u64)>, Web3Error> {
        self.chain.block_ref(n).await
    }
    async fn simulate_transition(
        &self,
        pid: &[u8; 31],
        snark: &PlonkSnark,
        blobs: &TransitionBlobs,
    ) -> Result<(), RevertReason> {
        self.write(pid)
            .map_err(|e| RevertReason::Rpc(e.to_string()))?;
        self.chain.simulate_transition(pid, snark, blobs).await
    }
    async fn submit_transition(
        &self,
        pid: &[u8; 31],
        snark: &PlonkSnark,
        blobs: &TransitionBlobs,
    ) -> Result<TxReceipt, Web3Error> {
        self.write(pid)?;
        self.chain.submit_transition(pid, snark, blobs).await
    }
    async fn submit_results(
        &self,
        pid: &[u8; 31],
        snark: &PlonkSnark,
    ) -> Result<TxReceipt, Web3Error> {
        self.write(pid)?;
        self.chain.submit_results(pid, snark).await
    }
    async fn request_results_decryption(
        &self,
        pid: &[u8; 31],
        accumulator: &[[u8; 32]; 64],
        siblings: &[[u8; 32]],
    ) -> Result<TxReceipt, Web3Error> {
        self.write(pid)?;
        self.chain
            .request_results_decryption(pid, accumulator, siblings)
            .await
    }
    async fn dkg_results_ready(&self, pid: &[u8; 31]) -> Result<bool, Web3Error> {
        if *pid != pid31() {
            return Ok(false);
        }
        self.chain.dkg_results_ready(pid).await
    }
    async fn council_decryption_open(&self, cid: &[u8; 12]) -> Result<bool, Web3Error> {
        self.chain.council_decryption_open(cid).await
    }
    async fn finalize_results_from_dkg(&self, pid: &[u8; 31]) -> Result<TxReceipt, Web3Error> {
        self.write(pid)?;
        self.chain.finalize_results_from_dkg(pid).await
    }
}

/// Serves the blobs of every batch the fake chain saw published, or the
/// swapped ones while a test set them.
struct SentBlobs(Hostile);

#[async_trait]
impl BlobSource for SentBlobs {
    async fn blobs_for_tx(&self, tx: B256, _block: u64, n: u64) -> Result<Vec<Blob>, Web3Error> {
        let swapped = self.0.swap.lock().unwrap().get(&tx).cloned();
        let mut b = match swapped {
            Some(b) => b,
            None => self
                .0
                .chain
                .sent_batches()
                .into_iter()
                .find(|(t, _, _)| *t == tx)
                .map(|(_, b, _)| b)
                .ok_or_else(|| Web3Error::Blob(format!("no blobs for {tx}")))?,
        };
        if n == 0 || (b.len() as u64) < n {
            return Err(Web3Error::Blob("short blob list".into()));
        }
        b.truncate(n as usize);
        Ok(b)
    }
}

struct ChainClock(FakeChain);

impl Clock for ChainClock {
    fn now(&self) -> u64 {
        self.0.time()
    }
}

async fn start_hostile(
    db: Db,
    dir: &Path,
    s: &TestSetup,
    net: &Hostile,
    prover: Arc<FakeProver>,
    batch_max: usize,
    sd: &CancellationToken,
) -> Node {
    let cfg = test_config(dir, &s.census_dir, batch_max, "0s");
    let deps = Deps {
        db,
        contracts: Arc::new(net.clone()),
        prover,
        blobs: Arc::new(SentBlobs(net.clone())),
        clock: Arc::new(ChainClock(net.chain.clone())),
        grace: net.chain.grace_params(),
        tasks: tokio_util::task::TaskTracker::new(),
        l1: None,
    };
    spawn_node(cfg, deps, sd.clone()).await.unwrap()
}

/// A hostile pid: never the honest one, always in the field.
fn hpid(i: u32) -> [u8; 31] {
    let mut b = [0xc0u8; 31];
    b[27..].copy_from_slice(&i.to_be_bytes());
    b
}

fn hpid_fr(pid: &[u8; 31]) -> Fr {
    let mut be = [0u8; 32];
    be[1..].copy_from_slice(pid);
    fr_from_be(&be).unwrap()
}

/// A CSP process like the honest one, its root the genesis of its own
/// config when that has one (an attacker creates it consistently).
fn hostile_process(
    base: &OnchainProcess,
    pid: &[u8; 31],
    f: impl FnOnce(&mut OnchainProcess),
) -> OnchainProcess {
    let mut p = base.clone();
    p.census = OnchainCensus {
        origin: 4,
        root: {
            let mut r = [0u8; 32];
            r[12..].copy_from_slice(&[0x77; 20]);
            r
        },
        uri: "https://csp.invalid".into(),
        contract_address: [0u8; 20],
    };
    p.state_root = [0xab; 32];
    f(&mut p);
    let origin = match p.census.origin {
        1 => Some(CensusOrigin::MerkleStatic),
        2 => Some(CensusOrigin::MerkleOffchainDynamic),
        3 => Some(CensusOrigin::MerkleOnchainDynamic),
        4 => Some(CensusOrigin::Csp),
        _ => None,
    };
    if p.state_root == [0xab; 32]
        && let Some(origin) = origin
    {
        let root = p.census.root;
        let cfg = ProcessConfig {
            process_id: hpid_fr(pid),
            ballot_mode: p.ballot_mode,
            enc_key: p.enc_key,
            census_origin: origin,
            census_root: root,
            ballot_vk_hash: vk_hash_once(),
        };
        if let Ok(g) = genesis_root(&cfg) {
            p.state_root = g;
        }
    }
    p
}

/// [`vk_hash`], parsed once: floods build thousands of configs.
fn vk_hash_once() -> [u8; 32] {
    static H: std::sync::OnceLock<[u8; 32]> = std::sync::OnceLock::new();
    *H.get_or_init(vk_hash)
}

/// Waits until every hostile pid has an actor or an Ignored record, then
/// returns the ignored ones with their notes.
async fn settle_bootstraps(node: &Node, pids: &[[u8; 31]]) -> HashMap<[u8; 31], String> {
    wait_until("hostile bootstraps", async || {
        let map = node.processes.read().await;
        pids.iter().all(|p| {
            let pid = hpid_fr(p);
            map.contains_key(&pid)
                || node
                    .db
                    .process(&pid)
                    .unwrap()
                    .is_some_and(|r| r.local == LocalStatus::Ignored)
        })
    })
    .await;
    pids.iter()
        .filter_map(|p| {
            let r = node.db.process(&hpid_fr(p)).unwrap()?;
            (r.local == LocalStatus::Ignored).then(|| (*p, r.note.unwrap_or_default()))
        })
        .collect()
}

/// Runs the honest election to its results through `node`: five votes in one
/// batch, the END, the grace, the results.
async fn honest_election_completes(s: &TestSetup, node: &Node) {
    let h = handle(node).await;
    let votes: Vec<VerifiedVote> = (0..5)
        .map(|i| fake_vote(&s.env, i, &[1, 2], 500 + i as u64))
        .collect();
    let vids: Vec<u64> = votes.iter().map(|v| v.pkg.vote_id).collect();
    for v in &votes {
        h.submit(v.clone()).await.unwrap();
    }
    wait_until("honest votes settled", async || {
        all_settled(&h, &vids).await
    })
    .await;
    assert_eq!(s.chain.voters(), 5);
    s.chain.set_status(ProcessStatus::Ended);
    s.chain.pass_grace();
    wait_until("honest results", async || !s.chain.results().is_empty()).await;
    let mut want = vec![0u64; 16];
    want[0] = 5;
    want[1] = 10;
    assert_eq!(s.chain.results(), want);
    wait_until("honest finalized", async || {
        h.snapshot().await.unwrap().local == LocalStatus::Finalized
    })
    .await;
}

/// Every parameter an organizer chooses, at and past its limits, on
/// processes next to an honest one: each is ignored with a reason or runs
/// its own actor, no actor dies, nothing is written for them that lands,
/// and the honest election settles and publishes its results.
#[tokio::test]
async fn hostile_process_parameters_never_reach_the_honest_election() {
    let dir = TempDir::new().unwrap();
    let db = Db::open_in(dir.path()).unwrap();
    let s = setup(2, 8, Some(node_key(&db)));
    let net = Hostile::new(s.chain.clone());
    let base = s.chain.process(&pid31()).await.unwrap();
    let honest_uri = base.census.uri.clone();
    let fr = |v: u64| Fr::from(v);
    type Tweak = Box<dyn FnOnce(&mut OnchainProcess)>;
    // (tweak, ignored?)
    let cases: Vec<(&str, Tweak, bool)> = vec![
        (
            "identity key",
            Box::new(|p| p.enc_key = Point::IDENTITY),
            true,
        ),
        (
            "low-order key",
            Box::new(move |p| {
                p.enc_key = Point {
                    x: fr(0),
                    y: -fr(1),
                }
            }),
            true,
        ),
        ("origin 0", Box::new(|p| p.census.origin = 0), true),
        ("origin 5", Box::new(|p| p.census.origin = 5), true),
        ("origin 255", Box::new(|p| p.census.origin = 255), true),
        // A 256-bit root above the BN254 modulus is a valid Keccak root: the
        // census document decides, and this one gives another root.
        (
            "census root the document does not give",
            Box::new(move |p| {
                p.census.origin = 1;
                p.census.root = [0xff; 32];
                p.census.uri = honest_uri;
            }),
            true,
        ),
        (
            "group above fields",
            Box::new(|p| p.ballot_mode.group_size = 3),
            true,
        ),
        (
            "max value 2^48",
            Box::new(|p| p.ballot_mode.max_value = 1 << 48),
            true,
        ),
        (
            "min value 2^48",
            Box::new(|p| p.ballot_mode.min_value = 1 << 48),
            true,
        ),
        (
            "max sum 2^63",
            Box::new(|p| p.ballot_mode.max_value_sum = 1 << 63),
            true,
        ),
        (
            "root not genesis",
            Box::new(|p| p.state_root = [0x01; 32]),
            true,
        ),
        (
            "no fields",
            Box::new(|p| {
                p.ballot_mode.num_fields = 0;
                p.ballot_mode.group_size = 0;
            }),
            false,
        ),
        (
            "17 fields",
            Box::new(|p| p.ballot_mode.num_fields = 17),
            false,
        ),
        (
            "255 fields",
            Box::new(|p| p.ballot_mode.num_fields = 255),
            false,
        ),
        (
            "min above max",
            Box::new(|p| {
                p.ballot_mode.min_value = 9;
                p.ballot_mode.max_value = 2;
                p.ballot_mode.min_value_sum = 50;
                p.ballot_mode.max_value_sum = 1;
            }),
            false,
        ),
        (
            "cost exponent 255",
            Box::new(|p| p.ballot_mode.cost_exponent = 255),
            false,
        ),
        (
            "unique without values",
            Box::new(|p| {
                p.ballot_mode.unique_values = true;
                p.ballot_mode.max_value = 0;
            }),
            false,
        ),
        (
            "max values",
            Box::new(|p| {
                p.ballot_mode.max_value = (1 << 48) - 1;
                p.ballot_mode.min_value = (1 << 48) - 1;
                p.ballot_mode.max_value_sum = (1 << 63) - 1;
                p.ballot_mode.min_value_sum = (1 << 63) - 1;
            }),
            false,
        ),
        ("max voters 0", Box::new(|p| p.max_voters = 0), false),
        (
            "max voters huge",
            Box::new(|p| p.max_voters = u64::MAX),
            false,
        ),
        (
            "started long ago",
            Box::new(|p| {
                p.start_time = 1;
                p.duration = 1;
            }),
            false,
        ),
        ("zero duration", Box::new(|p| p.duration = 0), false),
        (
            "end overflows",
            Box::new(|p| {
                p.start_time = u64::MAX;
                p.duration = u64::MAX;
            }),
            false,
        ),
        ("grace huge", Box::new(|p| p.grace = u64::MAX), false),
        (
            "last vote huge",
            Box::new(|p| p.last_vote_at = u64::MAX),
            false,
        ),
        (
            "counts huge",
            Box::new(|p| {
                p.voters_count = u64::MAX;
                p.overwritten_count = u64::MAX;
            }),
            false,
        ),
        (
            "creation block huge",
            Box::new(|p| p.creation_block = u64::MAX),
            false,
        ),
        (
            "dkg automatic",
            Box::new(|p| p.key_mode = KeyMode::DkgAutomatic),
            false,
        ),
        (
            "council",
            Box::new(|p| p.key_mode = KeyMode::Council),
            false,
        ),
        (
            "ended",
            Box::new(|p| p.status = ProcessStatus::Ended),
            false,
        ),
        (
            "canceled",
            Box::new(|p| p.status = ProcessStatus::Canceled),
            false,
        ),
        (
            "results",
            Box::new(|p| {
                p.status = ProcessStatus::Results;
                p.results = vec![u64::MAX; 300];
            }),
            false,
        ),
        (
            "status unknown",
            Box::new(|p| p.status = ProcessStatus::Unknown),
            false,
        ),
        (
            "long strings",
            Box::new(|p| {
                p.metadata_uri = "m".repeat(1 << 20);
                p.census.uri = "c".repeat(1 << 20);
            }),
            false,
        ),
    ];
    let mut pids = Vec::new();
    let mut want_ignored = Vec::new();
    for (i, (name, tweak, ignored)) in cases.into_iter().enumerate() {
        let pid = hpid(i as u32);
        net.create(pid, hostile_process(&base, &pid, tweak));
        pids.push(pid);
        if ignored {
            want_ignored.push((pid, name));
        }
    }
    // An event for a process the registry never created, and one for a pid
    // outside the field.
    net.push(EventKind::StatusChanged {
        pid: hpid(9999),
        old: ProcessStatus::Ready,
        new: ProcessStatus::Ended,
    });
    net.push(EventKind::ProcessCreated {
        pid: [0xff; 31],
        creator: Address::ZERO,
    });
    let sd = CancellationToken::new();
    let node = start_hostile(db, dir.path(), &s, &net, FakeProver::open(), 5, &sd).await;
    let ignored = settle_bootstraps(&node, &pids).await;
    for (pid, name) in &want_ignored {
        assert!(ignored.contains_key(pid), "{name} was served");
    }
    assert_eq!(
        ignored.len(),
        want_ignored.len(),
        "ignored: {:?}",
        ignored.values()
    );

    honest_election_completes(&s, &node).await;
    // Heads keep flowing to every hostile actor through the honest END and
    // grace; none of them died on its parameters.
    s.chain.advance_time(10_000_000);
    tokio::time::sleep(Duration::from_millis(300)).await;
    let map = node.processes.read().await.clone();
    for (pid, h) in &map {
        h.snapshot()
            .await
            .unwrap_or_else(|e| panic!("actor {} died: {e}", hex::encode(fr_to_be(pid))));
    }
    assert_eq!(map.len(), 1 + pids.len() - want_ignored.len());
    sd.cancel();
}

/// CPU time this process has used, in milliseconds (`/proc/self/stat`).
fn cpu_ms() -> u64 {
    let s = std::fs::read_to_string("/proc/self/stat").unwrap();
    let f: Vec<&str> = s.rsplit(')').next().unwrap().split_whitespace().collect();
    let ticks: u64 = f[11].parse::<u64>().unwrap() + f[12].parse::<u64>().unwrap();
    ticks * 10
}

/// `n` processes next to the honest one, created in its block: CSP, keyed
/// to an election key this node does not hold, ending at `T0 + 10`.
fn flood(s: &TestSetup, net: &Hostile, base: &OnchainProcess, n: u32) -> Vec<[u8; 31]> {
    let _ = s;
    let (_, foreign) = keygen(&mut StdRng::seed_from_u64(99));
    (0..n)
        .map(|i| {
            let pid = hpid(i);
            net.create(
                pid,
                hostile_process(base, &pid, |p| {
                    p.enc_key = foreign;
                    p.duration = 10;
                }),
            );
            pid
        })
        .collect()
}

// BUG: a process keyed to another node's (or nobody's) election key derives
// this node's key for it, a scalar multiplication, on every message once its
// window closed, and nobody ever finalizes such a process. A flood of them
// burns the node's CPU on every heartbeat, forever.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn closed_foreign_key_processes_cost_no_key_derivation_per_tick() {
    let dir = TempDir::new().unwrap();
    let db = Db::open_in(dir.path()).unwrap();
    let s = setup(2, 8, Some(node_key(&db)));
    let net = Hostile::new(s.chain.clone());
    let base = s.chain.process(&pid31()).await.unwrap();
    let pids = flood(&s, &net, &base, 400);
    let sd = CancellationToken::new();
    let node = start_hostile(db, dir.path(), &s, &net, FakeProver::open(), 5, &sd).await;
    assert!(settle_bootstraps(&node, &pids).await.is_empty());
    let idle = |secs| async move {
        tokio::time::sleep(Duration::from_millis(500)).await;
        let c0 = cpu_ms();
        tokio::time::sleep(Duration::from_secs(secs)).await;
        cpu_ms() - c0
    };
    let open = idle(2).await;
    // Past every window: each tick of each process reaches the finalize
    // check, which only the key holder passes.
    s.chain.advance_time(DURATION * 2);
    let closed = idle(2).await;
    assert!(
        closed < open * 3 + 500,
        "{} ms of CPU in 2 s for closed processes, {} ms while open",
        closed,
        open
    );
    sd.cancel();
}

/// Another sequencer's transition whose blobs first never appear, then come
/// from a lying source and do not replay to its `newRoot`: never applied,
/// nothing seals on the root the chain left, the node's own vote stays
/// pending; once the real blobs come it applies and the vote settles on top.
#[tokio::test]
async fn foreign_transition_without_replayable_blobs_is_never_applied() {
    let s = setup(2, 8, None);
    let net = Hostile::new(s.chain.clone());
    let dir = TempDir::new().unwrap();
    let db = Db::open_in(dir.path()).unwrap();
    let prover = FakeProver::open();
    let sd = CancellationToken::new();
    let node = start_hostile(db, dir.path(), &s, &net, prover.clone(), 1, &sd).await;
    let h = handle(&node).await;
    let genesis = s.chain.root();
    let peer = [
        fake_vote(&s.env, 0, &[1, 1], 10),
        fake_vote(&s.env, 1, &[2, 2], 11),
    ];
    let real = foreign_batch_of(&s, &peer);
    let lie = foreign_batch_of(&s, &[fake_vote(&s.env, 2, &[3, 3], 12)]);
    // Withheld first: no blob at all, from before the event is routed (the
    // fake names its first foreign transaction so).
    let mut h1 = [0x5e; 32];
    h1[24..].copy_from_slice(&1u64.to_be_bytes());
    let tx = B256::from(h1);
    net.swap.lock().unwrap().insert(tx, Vec::new());
    s.chain.settle_externally(&real);
    assert_eq!(s.chain.last_transition_tx(), tx);
    let mine = fake_vote(&s.env, 3, &[1, 0], 13);
    let vid = mine.pkg.vote_id;
    h.submit(mine).await.unwrap();
    let held = async |what: &str| {
        for _ in 0..20 {
            s.chain.advance_time(1);
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
        let snap = h.snapshot().await.unwrap();
        assert_eq!(snap.root, genesis, "{what}: applied");
        assert!(!snap.synced, "{what}");
        assert_eq!(prover.calls(), 0, "{what}: sealed on a root the chain left");
        assert_eq!(
            vote_status(&h, vid).await,
            Some(VoteStatus::Pending),
            "{what}"
        );
        assert_eq!(node.metrics.synced_from_others.load(Ordering::SeqCst), 0);
    };
    held("withheld").await;
    // A lying source: blobs of another batch on the same root.
    net.swap.lock().unwrap().insert(tx, lie.blobs.blobs.clone());
    held("lying blobs").await;
    // The real blobs: it applies, and the vote settles on top.
    net.swap.lock().unwrap().clear();
    wait_until("synced and settled", async || {
        vote_status(&h, vid).await == Some(VoteStatus::Settled)
    })
    .await;
    assert_eq!(h.snapshot().await.unwrap().root, s.chain.root());
    assert_eq!(node.metrics.synced_from_others.load(Ordering::SeqCst), 1);
    for v in &peer {
        h.vote_id_proof(v.pkg.vote_id).await.unwrap();
    }
    sd.cancel();
}

// BUG: one head answer far ahead of the chain (a glitching endpoint, a
// load balancer briefly on another chain) raised every actor's
// `seen_block` above any real head: every later chain read was refused as
// "behind", so no process closed out or published results until a restart.
#[tokio::test]
async fn a_single_bogus_head_does_not_wedge_every_process() {
    let dir = TempDir::new().unwrap();
    let db = Db::open_in(dir.path()).unwrap();
    let s = setup(2, 8, Some(node_key(&db)));
    let net = Hostile::new(s.chain.clone());
    let sd = CancellationToken::new();
    let node = start_hostile(db, dir.path(), &s, &net, FakeProver::open(), 5, &sd).await;
    let _ = handle(&node).await;
    *net.bogus_head.lock().unwrap() = Some(u64::MAX / 2);
    wait_until("bogus head taken", async || {
        net.bogus_head.lock().unwrap().is_none()
    })
    .await;
    honest_election_completes(&s, &node).await;
    sd.cancel();
}

async fn wait_long(what: &str, mut f: impl AsyncFnMut() -> bool) {
    for _ in 0..12000 {
        if f().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("timeout waiting for {what}");
}

/// Accepts connections and never answers: a census that never loads.
async fn silent_census_server() -> String {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let uri = format!("http://{}/census.json", l.local_addr().unwrap());
    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((c, _)) = l.accept().await {
            held.push(c);
        }
    });
    uri
}

// BUG: every bootstrap that waits for its census (the first sight of every
// Merkle-census process) rewrites the whole persisted retry list, so a page
// of n creations writes O(n^2): 2000 of them held routing for 2 s, 4000 for
// about 9 s on disk, while every other election's events wait behind them.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_flood_of_waiting_bootstraps_routes_in_linear_time() {
    let n = 8000u32;
    let dir = TempDir::new().unwrap();
    let db = Db::open_in(dir.path()).unwrap();
    let s = setup(2, 8, None);
    let net = Hostile::new(s.chain.clone());
    let base = s.chain.process(&pid31()).await.unwrap();
    let uri = silent_census_server().await;
    let sd = CancellationToken::new();
    let cfg = test_config_with(
        dir.path(),
        &s.census_dir,
        5,
        "0s",
        &["--census-allow-private"],
    );
    let deps = Deps {
        db,
        contracts: Arc::new(net.clone()),
        prover: FakeProver::open(),
        blobs: Arc::new(SentBlobs(net.clone())),
        clock: Arc::new(ChainClock(net.chain.clone())),
        grace: net.chain.grace_params(),
        tasks: tokio_util::task::TaskTracker::new(),
        l1: None,
    };
    let node = spawn_node(cfg, deps, sd.clone()).await.unwrap();
    handle(&node).await;
    // n Merkle-census processes in one block, each on its own root.
    let b = s.chain.head_block() + 1;
    for i in 0..n {
        let pid = hpid(i);
        let p = hostile_process(&base, &pid, |p| {
            p.census.origin = 1;
            p.census.root = fr_to_be(&Fr::from(1000 + u64::from(i)));
            p.census.uri = uri.clone();
        });
        net.procs.lock().unwrap().insert(pid, p);
        net.push_at(
            b,
            EventKind::ProcessCreated {
                pid,
                creator: Address::ZERO,
            },
        );
    }
    // The honest election's event right behind them.
    let t = std::time::Instant::now();
    s.chain.set_head_block(b);
    s.chain.set_grace(333);
    wait_long("the honest event", async || {
        node.db.process(&pid_fr()).unwrap().unwrap().onchain.grace == 333
    })
    .await;
    let took = t.elapsed();
    assert!(
        took < Duration::from_secs(3),
        "{n} waiting bootstraps held routing for {took:?}"
    );
    // Every one of them is on the persisted retry list.
    let list: Vec<(String, u32)> =
        serde_json::from_slice(&node.db.meta_bytes("monitor_boot_retries").unwrap().unwrap())
            .unwrap();
    assert_eq!(list.len(), n as usize);
    sd.cancel();
}

/// The endpoint fails over onto one lagging far behind (its head block and
/// time step back below everything the node saw) while batches are in
/// flight and queued: nothing is errored or closed, the landings commit,
/// and once the head is back the election ends and the results go out.
#[tokio::test]
async fn a_head_stepping_back_neither_closes_nor_loses_votes() {
    let dir = TempDir::new().unwrap();
    let db = Db::open_in(dir.path()).unwrap();
    let s = setup(2, 8, Some(node_key(&db)));
    let net = Hostile::new(s.chain.clone());
    let sd = CancellationToken::new();
    let prover = FakeProver::gated();
    let node = start_hostile(db, dir.path(), &s, &net, prover.clone(), 2, &sd).await;
    let h = handle(&node).await;
    for _ in 0..5 {
        s.chain.advance_time(10);
    }
    let votes: Vec<VerifiedVote> = (0..4)
        .map(|i| fake_vote(&s.env, i, &[1, 1], 700 + i as u64))
        .collect();
    let vids: Vec<u64> = votes.iter().map(|v| v.pkg.vote_id).collect();
    for v in &votes {
        h.submit(v.clone()).await.unwrap();
    }
    wait_until("first batch proving", async || prover.calls() >= 1).await;
    // A lagging endpoint, behind everything the node saw, by more than the
    // whole election so far.
    *net.lag.lock().unwrap() = Some((4, 1_000));
    prover.release(100);
    for _ in 0..20 {
        s.chain.advance_time(1);
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    for v in &vids {
        let sv = h.status(*v).await.unwrap().unwrap();
        assert_ne!(sv.status, VoteStatus::Error, "{:?}", sv.error);
    }
    *net.lag.lock().unwrap() = None;
    wait_until("settled", async || all_settled(&h, &vids).await).await;
    s.chain.set_status(ProcessStatus::Ended);
    s.chain.pass_grace();
    wait_until("results", async || !s.chain.results().is_empty()).await;
    let mut want = vec![0u64; 16];
    want[0] = 4;
    want[1] = 4;
    assert_eq!(s.chain.results(), want);
    sd.cancel();
}

/// Events the chain never had (a lying endpoint, or a reorg that took
/// them): a `ResultsSet`, then a cancel after it (a jump the registry
/// refuses), on the running election with votes queued. The node errors nothing
/// on their word: the fresh read at the close-out shows the election open,
/// so the votes settle and the real results go out at the end.
#[tokio::test]
async fn close_events_the_chain_does_not_back_error_nothing() {
    let dir = TempDir::new().unwrap();
    let db = Db::open_in(dir.path()).unwrap();
    let s = setup(2, 8, Some(node_key(&db)));
    let net = Hostile::new(s.chain.clone());
    let sd = CancellationToken::new();
    let prover = FakeProver::gated();
    let node = start_hostile(db, dir.path(), &s, &net, prover.clone(), 3, &sd).await;
    let h = handle(&node).await;
    s.chain.advance_time(5);
    let votes: Vec<VerifiedVote> = (0..3)
        .map(|i| fake_vote(&s.env, i, &[2, 1], 900 + i as u64))
        .collect();
    let vids: Vec<u64> = votes.iter().map(|v| v.pkg.vote_id).collect();
    for v in &votes[..2] {
        h.submit(v.clone()).await.unwrap();
    }
    let lies = [
        EventKind::ResultsSet {
            pid: pid31(),
            sender: Address::repeat_byte(0xee),
            results: vec![7; 16],
        },
        EventKind::StatusChanged {
            pid: pid31(),
            old: ProcessStatus::Results,
            new: ProcessStatus::Canceled,
        },
    ];
    for lie in lies {
        let b = s.chain.head_block() + 1;
        net.push_at(b, lie);
        s.chain.advance_time(1);
        for _ in 0..10 {
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
        for v in &vids[..2] {
            let sv = h.status(*v).await.unwrap().unwrap();
            assert_ne!(sv.status, VoteStatus::Error, "{:?}", sv.error);
        }
    }
    assert_eq!(prover.calls(), 0);
    // The chain is still open: once read, a third vote is admitted, and
    // the batch seals and settles.
    s.chain.advance_time(1);
    let mut admitted = false;
    for _ in 0..100 {
        if h.submit(votes[2].clone()).await.is_ok() {
            admitted = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(admitted, "admission never reopened");
    wait_until("proving", async || prover.calls() >= 1).await;
    prover.release(100);
    wait_until("settled", async || all_settled(&h, &vids).await).await;
    assert!(s.chain.results().is_empty());
    s.chain.set_status(ProcessStatus::Ended);
    s.chain.pass_grace();
    prover.release_results(100);
    wait_until("real results", async || !s.chain.results().is_empty()).await;
    let mut want = vec![0u64; 16];
    want[0] = 6;
    want[1] = 3;
    assert_eq!(s.chain.results(), want);
    sd.cancel();
}

/// Merkle-census processes whose census URI points anywhere an attacker
/// likes: files outside `--census-dir`, the metadata service, loopback,
/// other schemes, garbage, a megabyte of URI. None is served, the policy
/// refusals end ignored at once (file errors stay transient by design and
/// wait on the retry list), and the honest election completes.
#[tokio::test]
async fn absurd_census_uris_are_refused_without_stalling_anyone() {
    let dir = TempDir::new().unwrap();
    let db = Db::open_in(dir.path()).unwrap();
    let s = setup(2, 8, Some(node_key(&db)));
    let net = Hostile::new(s.chain.clone());
    let base = s.chain.process(&pid31()).await.unwrap();
    let up = format!("file://{}/../../../../etc/hostname", s.census_dir.display());
    let uris = [
        "file:///etc/passwd".to_string(),
        up,
        "file:///proc/self/environ".into(),
        "http://169.254.169.254/latest/meta-data/".into(),
        "http://127.0.0.1:1/census.json".into(),
        "http://[::1]:1/census.json".into(),
        "http://0.0.0.0:1/census.json".into(),
        "gopher://127.0.0.1:70/".into(),
        "data:application/json,{\"participants\":[]}".into(),
        "not a uri".into(),
        String::new(),
        format!("file://{}", "a/".repeat(500_000)),
    ];
    let mut pids = Vec::new();
    for (i, uri) in uris.iter().enumerate() {
        let pid = hpid(i as u32);
        net.create(
            pid,
            hostile_process(&base, &pid, |p| {
                p.census.origin = 1;
                p.census.root = fr_to_be(&Fr::from(77 + i as u64));
                p.census.uri = uri.clone();
            }),
        );
        pids.push(pid);
    }
    let sd = CancellationToken::new();
    let node = start_hostile(db, dir.path(), &s, &net, FakeProver::open(), 5, &sd).await;
    honest_election_completes(&s, &node).await;
    let file = |i: usize| uris[i].starts_with("file:///") && i < 3;
    wait_until("policy refusals ignored", async || {
        (0..uris.len()).filter(|i| !file(*i)).all(|i| {
            node.db
                .process(&hpid_fr(&pids[i]))
                .unwrap()
                .is_some_and(|r| r.local == LocalStatus::Ignored)
        })
    })
    .await;
    let map = node.processes.read().await;
    for (i, p) in pids.iter().enumerate() {
        assert!(
            !map.contains_key(&hpid_fr(p)),
            "{} served",
            &uris[i][..uris[i].len().min(60)]
        );
    }
    drop(map);
    let list: Vec<(String, u32)> =
        serde_json::from_slice(&node.db.meta_bytes("monitor_boot_retries").unwrap().unwrap())
            .unwrap();
    for i in (0..uris.len()).filter(|i| file(*i)) {
        assert!(
            list.iter().any(|(p, _)| *p == hex::encode(pids[i])),
            "{} neither ignored nor retried",
            uris[i]
        );
    }
    sd.cancel();
}

// Census downloads once shared 4 permits node-wide, so four silent servers
// held every new Merkle-census election for the 30 s read timeout per
// attempt. Downloads now hold a socket and charge bytes as they arrive: four
// silent processes created first do not delay an honest one's actor.
#[tokio::test]
async fn silent_census_servers_do_not_starve_another_bootstrap() {
    let dir = TempDir::new().unwrap();
    let db = Db::open_in(dir.path()).unwrap();
    let s = setup(2, 8, None);
    let net = Hostile::new(s.chain.clone());
    let base = s.chain.process(&pid31()).await.unwrap();
    let uri = silent_census_server().await;
    // Created one block before the honest election.
    for i in 0..4u32 {
        let pid = hpid(i);
        let p = hostile_process(&base, &pid, |p| {
            p.census.origin = 1;
            p.census.root = fr_to_be(&Fr::from(1000 + u64::from(i)));
            p.census.uri = format!("{uri}?{i}");
        });
        net.procs.lock().unwrap().insert(pid, p);
        net.push_at(
            1,
            EventKind::ProcessCreated {
                pid,
                creator: Address::ZERO,
            },
        );
    }
    let sd = CancellationToken::new();
    let cfg = test_config_with(
        dir.path(),
        &s.census_dir,
        5,
        "0s",
        &["--census-allow-private"],
    );
    let deps = Deps {
        db,
        contracts: Arc::new(net.clone()),
        prover: FakeProver::open(),
        blobs: Arc::new(SentBlobs(net.clone())),
        clock: Arc::new(ChainClock(net.chain.clone())),
        grace: net.chain.grace_params(),
        tasks: tokio_util::task::TaskTracker::new(),
        l1: None,
    };
    // The node sees block 1 first: the silent downloads take the permits,
    // then the honest election is created.
    s.chain.set_head_block(1);
    let node = spawn_node(cfg, deps, sd.clone()).await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    let t = std::time::Instant::now();
    s.chain.set_head_block(2);
    wait_long("the honest actor", async || {
        node.processes.read().await.contains_key(&pid_fr())
    })
    .await;
    let took = t.elapsed();
    assert!(
        took < Duration::from_secs(10),
        "the honest census waited {took:?} behind silent servers"
    );
    sd.cancel();
}

/// An event from a block above the chain's head (a peer endpoint ahead of
/// ours) does hold reads back, unlike a Head: an endpoint behind it could
/// rewind what the event set. Reads resume once the chain gets there.
#[tokio::test]
async fn an_event_above_the_head_holds_reads_until_the_chain_gets_there() {
    let dir = TempDir::new().unwrap();
    let db = Db::open_in(dir.path()).unwrap();
    let s = setup(2, 8, Some(node_key(&db)));
    let net = Hostile::new(s.chain.clone());
    let sd = CancellationToken::new();
    let node = start_hostile(db, dir.path(), &s, &net, FakeProver::open(), 5, &sd).await;
    let h = handle(&node).await;
    let ahead = s.chain.head_block() + 50;
    net.stray.lock().unwrap().push(RegistryEvent {
        block: ahead,
        tx_hash: B256::repeat_byte(0x55),
        log_index: 0,
        kind: EventKind::MetadataUpdated {
            pid: pid31(),
            uri: "ahead".into(),
            hash: [0; 32],
        },
    });
    s.chain.advance_time(1);
    wait_until("stray routed", async || {
        net.stray.lock().unwrap().is_empty()
    })
    .await;
    // A landing asks for a fresh read of the landing time.
    let votes: Vec<VerifiedVote> = (0..5)
        .map(|i| fake_vote(&s.env, i, &[1, 2], 960 + i as u64))
        .collect();
    let vids: Vec<u64> = votes.iter().map(|v| v.pkg.vote_id).collect();
    for v in &votes {
        h.submit(v.clone()).await.unwrap();
    }
    wait_until("settled", async || all_settled(&h, &vids).await).await;
    s.chain.set_status(ProcessStatus::Ended);
    s.chain.pass_grace();
    for _ in 0..30 {
        s.chain.advance_time(0);
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(s.chain.head_block() < ahead);
    assert!(
        s.chain.results().is_empty(),
        "read from an endpoint behind an event"
    );
    s.chain.set_head_block(ahead);
    wait_until("results", async || !s.chain.results().is_empty()).await;
    wait_until("finalized", async || {
        h.snapshot().await.unwrap().local == LocalStatus::Finalized
    })
    .await;
    sd.cancel();
}

/// After a read served at block R, a backend behind R (a balancer sending
/// the next read elsewhere) is refused even with no event or commit at R:
/// its older state would rewind what the read merged, here an end the chain
/// moved out again, which would close admission.
#[tokio::test]
async fn a_lagging_read_after_a_successful_one_is_refused() {
    let s = setup(2, 8, None);
    let net = Hostile::new(s.chain.clone());
    let dir = TempDir::new().unwrap();
    let db = Db::open_in(dir.path()).unwrap();
    let sd = CancellationToken::new();
    let node = start_hostile(db, dir.path(), &s, &net, FakeProver::open(), 1, &sd).await;
    let h = handle(&node).await;
    s.chain.advance_time(5);
    let v1 = fake_vote(&s.env, 0, &[1, 1], 950);
    let vid1 = v1.pkg.vote_id;
    h.submit(v1).await.unwrap();
    wait_until("first landed", async || {
        vote_status(&h, vid1).await == Some(VoteStatus::Settled)
    })
    .await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let landing = s.chain.last_transition_tx();
    let replay = s
        .chain
        .events(0, u64::MAX)
        .await
        .unwrap()
        .into_iter()
        .find(|e| e.tx_hash == landing)
        .unwrap();
    // A block whose state a backend behind the head would serve: the
    // election already over. The chain then moves the end out again.
    let duration = s.chain.process(&pid31()).await.unwrap().duration;
    let now = s.chain.time();
    s.chain.mutate_process(|p| p.duration = now - p.start_time);
    s.chain.checkpoint();
    s.chain.mutate_process(|p| p.duration = duration);
    s.chain.advance_time(1);
    // A replayed landing (its old block) makes the actor check the chain,
    // at the anchor, which shows the election open.
    net.stray.lock().unwrap().push(replay.clone());
    s.chain.advance_time(1);
    wait_until("first replay routed", async || {
        net.stray.lock().unwrap().is_empty()
    })
    .await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(h.snapshot().await.unwrap().accepting);
    // `head()` now answers from a backend behind that read, at the block
    // that still shows the old end, and another replay asks for a read.
    net.head_lag.store(3, Ordering::SeqCst);
    net.stray.lock().unwrap().push(replay);
    s.chain.advance_time(1);
    wait_until("replay routed", async || {
        net.stray.lock().unwrap().is_empty()
    })
    .await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(
        h.snapshot().await.unwrap().accepting,
        "a lagging read closed admission"
    );
    h.submit(fake_vote(&s.env, 1, &[1, 1], 951)).await.unwrap();
    sd.cancel();
}

// BUG: a lost race raised the anchored-read floor to the block an
// unverified `head()` answered: a bogus `latest` far ahead left every
// anchored read below it, so the sync latch never cleared and a lone
// sequencer sealed, closed out and finalized nothing until a restart.
#[tokio::test]
async fn a_bogus_head_after_a_lost_race_does_not_wedge_the_process() {
    let s = setup(2, 8, None);
    let net = Hostile::new(s.chain.clone());
    let dir = TempDir::new().unwrap();
    let db = Db::open_in(dir.path()).unwrap();
    let sd = CancellationToken::new();
    let node = start_hostile(db, dir.path(), &s, &net, FakeProver::open(), 1, &sd).await;
    let h = handle(&node).await;
    s.chain.advance_time(5);
    // Every head answer is far beyond any block the endpoint has, and
    // reads by number show another root: the settlement's refusal reads as
    // a lost race.
    net.far_head.store(1_000_000, Ordering::SeqCst);
    *net.number_root.lock().unwrap() = Some([0xAB; 32]);
    s.chain.revert_simulates("InvalidStateRoot", 1);
    let v = fake_vote(&s.env, 0, &[1, 1], 960);
    let vid = v.pkg.vote_id;
    h.submit(v).await.unwrap();
    wait_until("settled after the stale race", async || {
        s.chain.advance_time(1);
        vote_status(&h, vid).await == Some(VoteStatus::Settled)
    })
    .await;
    // And the window still closes out at its grace end.
    let late = fake_vote(&s.env, 1, &[1, 1], 961);
    let late_id = late.pkg.vote_id;
    s.chain.revert_simulates("InvalidStatus", u32::MAX);
    h.submit(late).await.unwrap();
    s.chain.pass_grace();
    wait_until("closed out", async || {
        s.chain.advance_time(1);
        vote_status(&h, late_id).await == Some(VoteStatus::Error)
    })
    .await;
    sd.cancel();
}
