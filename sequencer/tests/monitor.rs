//! Monitor loop: census downloads off the loop, the heartbeat task and
//! reorg detection at the event cursor.

mod common;

use common::*;

/// The fake chain with the bad process on a census root nothing else
/// loads, and the blobs of every landing kept for the other node's sync.
#[derive(Clone)]
struct Net {
    chain: FakeChain,
    blobs: Arc<Mutex<HashMap<B256, Vec<Blob>>>>,
    /// `process()` reads of the bad process.
    bad_reads: Arc<AtomicU64>,
    /// Blocks from `.0` on hash with fork tag `.1` (a reorg replaced them).
    fork: Arc<Mutex<(u64, u8)>>,
    /// `(to, from, tag)`: the `events()` call ending at `to` reorgs blocks
    /// from `from` on first (a reorg between the page hash and the page).
    reorg_in_page: Arc<Mutex<Option<(u64, u64, u8)>>>,
    /// While set, every `block_hash` read fails (a rate-limited RPC).
    fail_hashes: Arc<AtomicBool>,
}

impl Net {
    fn new(chain: FakeChain) -> Net {
        Net {
            chain,
            blobs: Arc::default(),
            bad_reads: Arc::default(),
            fork: Arc::new(Mutex::new((u64::MAX, 0))),
            reorg_in_page: Arc::default(),
            fail_hashes: Arc::default(),
        }
    }

    /// A reorg replaces every block from `from` on.
    fn reorg(&self, from: u64, tag: u8) {
        *self.fork.lock().unwrap() = (from, tag);
    }

    fn hash_of(&self, n: u64) -> [u8; 32] {
        let (from, tag) = *self.fork.lock().unwrap();
        let mut h = [0u8; 32];
        h[..8].copy_from_slice(&n.to_be_bytes());
        h[31] = if n >= from { tag } else { 0 };
        h
    }
}

fn bad_census_root() -> Fr {
    LeanImt::from_leaves(vec![census_leaf(&voter_address(7), 1).unwrap()]).root()
}

#[async_trait]
impl Chain for Net {
    fn signer(&self) -> Option<Address> {
        self.chain.signer()
    }
    async fn process(&self, pid: &[u8; 31]) -> Result<OnchainProcess, Web3Error> {
        let mut p = self.chain.process(pid).await?;
        if *pid == bad_pid31() {
            self.bad_reads.fetch_add(1, Ordering::SeqCst);
            p.census.root = fr_to_be(&bad_census_root());
        }
        Ok(p)
    }
    async fn events(&self, from: u64, to: u64) -> Result<Vec<RegistryEvent>, Web3Error> {
        let armed = self.reorg_in_page.lock().unwrap().take_if(|r| r.0 == to);
        if let Some((_, fork, tag)) = armed {
            self.reorg(fork, tag);
        }
        self.chain.events(from, to).await
    }
    async fn head(&self) -> Result<(u64, u64), Web3Error> {
        self.chain.head().await
    }
    async fn confirmed_head(&self, confirmations: u64) -> Result<u64, Web3Error> {
        self.chain.confirmed_head(confirmations).await
    }
    async fn block_hash(&self, n: u64) -> Result<Option<[u8; 32]>, Web3Error> {
        if self.fail_hashes.load(Ordering::SeqCst) {
            return Err(Web3Error::Rpc("scripted block read failure".into()));
        }
        Ok((n <= self.chain.head_block()).then(|| self.hash_of(n)))
    }
    async fn simulate_transition(
        &self,
        pid: &[u8; 31],
        snark: &PlonkSnark,
        blobs: &TransitionBlobs,
    ) -> Result<(), RevertReason> {
        self.chain.simulate_transition(pid, snark, blobs).await
    }
    async fn submit_transition(
        &self,
        pid: &[u8; 31],
        snark: &PlonkSnark,
        blobs: &TransitionBlobs,
    ) -> Result<TxReceipt, Web3Error> {
        let rc = self.chain.submit_transition(pid, snark, blobs).await?;
        self.blobs
            .lock()
            .unwrap()
            .insert(rc.tx_hash, blobs.blobs.clone());
        Ok(rc)
    }
    async fn submit_results(
        &self,
        pid: &[u8; 31],
        snark: &PlonkSnark,
    ) -> Result<TxReceipt, Web3Error> {
        self.chain.submit_results(pid, snark).await
    }
    async fn request_results_decryption(
        &self,
        pid: &[u8; 31],
        accumulator: &[[u8; 32]; 64],
        siblings: &[[u8; 32]],
    ) -> Result<TxReceipt, Web3Error> {
        self.chain
            .request_results_decryption(pid, accumulator, siblings)
            .await
    }
    async fn dkg_results_ready(&self, pid: &[u8; 31]) -> Result<bool, Web3Error> {
        self.chain.dkg_results_ready(pid).await
    }
    async fn council_decryption_open(&self, cid: &[u8; 12]) -> Result<bool, Web3Error> {
        self.chain.council_decryption_open(cid).await
    }
    async fn finalize_results_from_dkg(&self, pid: &[u8; 31]) -> Result<TxReceipt, Web3Error> {
        self.chain.finalize_results_from_dkg(pid).await
    }
}

struct SharedBlobs(Arc<Mutex<HashMap<B256, Vec<Blob>>>>);

#[async_trait]
impl BlobSource for SharedBlobs {
    async fn blobs_for_tx(&self, tx: B256, _block: u64, n: u64) -> Result<Vec<Blob>, Web3Error> {
        let mut b = self
            .0
            .lock()
            .unwrap()
            .get(&tx)
            .cloned()
            .ok_or_else(|| Web3Error::Blob(format!("no blobs for {tx}")))?;
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

async fn start(
    db: Db,
    net: &Net,
    prover: Arc<FakeProver>,
    cfg: Config,
    sd: &CancellationToken,
) -> Node {
    let deps = Deps {
        db,
        contracts: Arc::new(net.clone()),
        prover,
        blobs: Arc::new(SharedBlobs(net.blobs.clone())),
        clock: Arc::new(ChainClock(net.chain.clone())),
        grace: net.chain.grace_params(),
        tasks: tokio_util::task::TaskTracker::new(),
        l1: None,
    };
    spawn_node(cfg, deps, sd.clone()).await.unwrap()
}

/// Serves `body` at `/census.json` once `gate` gives a permit, or never.
async fn census_server(body: Vec<u8>, gate: Arc<Semaphore>, hits: Arc<AtomicU64>) -> String {
    let app = axum::Router::new().route(
        "/census.json",
        axum::routing::get(move || {
            let (body, gate, hits) = (body.clone(), gate.clone(), hits.clone());
            async move {
                hits.fetch_add(1, Ordering::SeqCst);
                let _p = gate.acquire().await.unwrap();
                body
            }
        }),
    );
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    format!("http://{addr}/census.json")
}

/// A server that answers 503 to everything.
async fn failing_server() -> String {
    let app = axum::Router::new().route(
        "/census.json",
        axum::routing::get(|| async { axum::http::StatusCode::SERVICE_UNAVAILABLE }),
    );
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    format!("http://{addr}/census.json")
}

fn cfg(dir: &Path, census_dir: &Path, batch_max: usize) -> Config {
    test_config_with(
        dir,
        census_dir,
        batch_max,
        "0s",
        &["--census-allow-private"],
    )
}

/// Bootstrap attempts recorded for `pid31`.
fn boot_attempts(db: &Db, pid31: &[u8; 31]) -> Option<u32> {
    let b = db.meta_bytes("monitor_boot_retries").unwrap()?;
    let list: Vec<(String, u32)> = serde_json::from_slice(&b).unwrap();
    list.into_iter()
        .find(|(p, _)| *p == hex::encode(pid31))
        .map(|(_, n)| n)
}

/// A new process whose census server never answers must not stall the
/// running election: it still gets the heads, another sequencer's
/// transition and the END, flushes inside its grace and publishes results.
#[tokio::test]
async fn slow_census_does_not_stall_the_heartbeat() {
    let dir_a = TempDir::new().unwrap();
    let dir_b = TempDir::new().unwrap();
    let db_a = Db::open_in(dir_a.path()).unwrap();
    let s = setup(2, 8, Some(node_key(&db_a)));
    let sd = CancellationToken::new();
    let blobs = Arc::new(Mutex::new(HashMap::new()));
    let net_a = Net {
        blobs: blobs.clone(),
        ..Net::new(s.chain.with_signer(0xa1))
    };
    let net_b = Net {
        blobs,
        ..Net::new(s.chain.with_signer(0xb2))
    };
    let (prover_a, prover_b) = (FakeProver::open(), FakeProver::open());
    let cfg_a = cfg(dir_a.path(), &s.census_dir, 10);
    let cfg_b = cfg(dir_b.path(), &s.census_dir, 1);
    let node_a = start(db_a.clone(), &net_a, prover_a.clone(), cfg_a, &sd).await;
    let db_b = Db::open_in(dir_b.path()).unwrap();
    let node_b = start(db_b, &net_b, prover_b.clone(), cfg_b, &sd).await;
    let ha = handle(&node_a).await;
    let hb = handle(&node_b).await;

    // A new process whose census server accepts and never answers.
    let hits = Arc::new(AtomicU64::new(0));
    let uri = census_server(Vec::new(), Arc::new(Semaphore::new(0)), hits.clone()).await;
    let mut bad = s.env.cfg.clone();
    bad.process_id = bad_pid_fr();
    bad.census_root = bad_census_root();
    s.chain.add_bad_process(BadKind::CensusUri {
        uri,
        state_root: genesis_root(&bad).unwrap(),
    });
    s.chain.advance_time(0);
    s.chain.push_event(EventKind::ProcessCreated {
        pid: bad_pid31(),
        creator: Address::ZERO,
    });
    wait_until("both downloads stalled", async || {
        hits.load(Ordering::SeqCst) >= 2
    })
    .await;

    // A holds a vote for the flush; B settles one, which A must sync.
    let v0 = fake_vote(&s.env, 0, &[1, 2], 10);
    let v1 = fake_vote(&s.env, 1, &[2, 1], 11);
    let (vid0, vid1) = (v0.pkg.vote_id, v1.pkg.vote_id);
    ha.submit(v0).await.unwrap();
    hb.submit(v1).await.unwrap();
    wait_until("B settled", async || all_settled(&hb, &[vid1]).await).await;
    wait_until("A synced B's transition", async || {
        let snap = ha.snapshot().await.unwrap();
        snap.root == s.chain.root() && snap.voters == 1
    })
    .await;
    assert_eq!(prover_a.calls(), 0, "A only seals in the flush");

    // END: A flushes its vote inside the grace.
    s.chain.set_status(ProcessStatus::Ended);
    wait_until("A saw the END", async || {
        ha.snapshot().await.unwrap().status == ProcessStatus::Ended
    })
    .await;
    wait_until("A flushed", async || all_settled(&ha, &[vid0]).await).await;
    assert!(s.chain.time() < s.chain.grace_end(), "inside the grace");
    assert_eq!(s.chain.voters(), 2);

    // The grace ends on a head: A publishes the results.
    s.chain.pass_grace();
    wait_until("results on chain", async || !s.chain.results().is_empty()).await;

    // The stalled process waits, unspawned and not ignored, at no attempt
    // cost and with no second download.
    for (node, db) in [(&node_a, &db_a), (&node_b, &node_b.db)] {
        assert!(!node.processes.read().await.contains_key(&bad_pid_fr()));
        assert!(db.process(&bad_pid_fr()).unwrap().is_none());
        assert_eq!(boot_attempts(db, &bad_pid31()), Some(0));
    }
    assert_eq!(hits.load(Ordering::SeqCst), 2);
    // Nor a chain read per poll while it downloads: one per node.
    for net in [&net_a, &net_b] {
        let reads = net.bad_reads.load(Ordering::SeqCst);
        assert_eq!(reads, 1, "re-read while waiting");
    }
    sd.cancel();
}

/// The bootstrap census downloads off the loop: routing moves past the
/// creation while it runs, and the process spawns once it loads.
#[tokio::test]
async fn bootstrap_with_a_queued_census_spawns_when_it_loads() {
    let env = env(2, 8, None);
    let census_dir = TempDir::new().unwrap();
    write_census(census_dir.path(), 8);
    let body = std::fs::read(census_dir.path().join("census.json")).unwrap();
    let gate = Arc::new(Semaphore::new(0));
    let hits = Arc::new(AtomicU64::new(0));
    let uri = census_server(body, gate.clone(), hits.clone()).await;
    let chain = FakeChain::new(&env, uri);
    let dir = TempDir::new().unwrap();
    let db = Db::open_in(dir.path()).unwrap();
    let sd = CancellationToken::new();
    let net = Net::new(chain.clone());
    let node = start(
        db.clone(),
        &net,
        FakeProver::open(),
        cfg(dir.path(), census_dir.path(), 1),
        &sd,
    )
    .await;
    wait_until("download running", async || {
        hits.load(Ordering::SeqCst) >= 1
    })
    .await;
    wait_until("cursor past the creation", async || {
        db.meta_u64("monitor_last_block").unwrap() >= Some(2)
    })
    .await;
    // Later events keep routing while the download runs.
    chain.advance_time(0);
    let head = chain.head_block();
    wait_until("cursor at head", async || {
        db.meta_u64("monitor_last_block").unwrap() == Some(head)
    })
    .await;
    assert!(node.processes.read().await.is_empty());
    assert!(db.process(&pid_fr()).unwrap().is_none(), "not ignored");
    assert_eq!(
        boot_attempts(&db, &pid31()),
        Some(0),
        "a wait, not a failure"
    );

    gate.add_permits(16);
    let h = handle(&node).await;
    assert_eq!(
        db.process(&pid_fr()).unwrap().unwrap().local,
        LocalStatus::Active
    );
    assert_eq!(boot_attempts(&db, &pid31()), None, "retry entry dropped");
    let v = fake_vote(&env, 0, &[1, 2], 5);
    let vid = v.pkg.vote_id;
    h.submit(v).await.unwrap();
    wait_until("settled", async || all_settled(&h, &[vid]).await).await;
    assert_eq!(hits.load(Ordering::SeqCst), 1, "one download");
    sd.cancel();
}

/// A failed bootstrap download still costs one attempt (the cap still
/// ends a census that never loads); waiting out the store's backoff costs
/// none.
#[tokio::test]
async fn failed_bootstrap_download_costs_one_attempt() {
    let env = env(2, 8, None);
    let census_dir = TempDir::new().unwrap();
    let chain = FakeChain::new(&env, failing_server().await);
    let dir = TempDir::new().unwrap();
    let db = Db::open_in(dir.path()).unwrap();
    let sd = CancellationToken::new();
    let net = Net::new(chain);
    let node = start(
        db.clone(),
        &net,
        FakeProver::open(),
        cfg(dir.path(), census_dir.path(), 1),
        &sd,
    )
    .await;
    wait_until("one attempt", async || {
        boot_attempts(&db, &pid31()) == Some(1)
    })
    .await;
    // 50 polls at 20 ms inside the store's 30 s backoff.
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(boot_attempts(&db, &pid31()), Some(1));
    assert!(node.processes.read().await.is_empty());
    assert!(db.process(&pid_fr()).unwrap().is_none(), "not ignored");
    sd.cancel();
}

/// A stalled `eth_getLogs` page holds routing, not the heartbeat: the head
/// time still reaches the actor, which flushes past the end.
#[tokio::test]
async fn slow_event_page_does_not_stall_the_heartbeat() {
    let s = setup(2, 8, None);
    let dir = TempDir::new().unwrap();
    let sd = CancellationToken::new();
    let prover = FakeProver::open();
    let node = start_node(
        Db::open_in(dir.path()).unwrap(),
        dir.path(),
        &s,
        s.chain.clone(),
        prover.clone(),
        10,
        "0s",
        sd.clone(),
    )
    .await;
    let h = handle(&node).await;
    let v = fake_vote(&s.env, 0, &[1, 2], 5);
    let vid = v.pkg.vote_id;
    h.submit(v).await.unwrap();
    s.chain.events_delay(Duration::from_secs(3600));
    s.chain.advance_time(DURATION + 1);
    // Watch the chain, not the actor: every message to it runs a tick.
    wait_until("flushed past the end", async || s.chain.voters() == 1).await;
    assert!(all_settled(&h, &[vid]).await);
    assert_eq!(prover.calls(), 1);
    sd.cancel();
}

/// A census that can never load (a policy refusal) ends its process
/// ignored after the queued download, and drops its failure mark: an
/// attacker's processes must not grow the persisted map.
#[tokio::test]
async fn refused_bootstrap_census_is_ignored_and_forgotten() {
    let s = setup(2, 8, None);
    let mut bad = s.env.cfg.clone();
    bad.process_id = bad_pid_fr();
    bad.census_root = bad_census_root();
    s.chain.add_bad_process(BadKind::CensusUri {
        uri: "ftp://x/census.json".into(),
        state_root: genesis_root(&bad).unwrap(),
    });
    let dir = TempDir::new().unwrap();
    let db = Db::open_in(dir.path()).unwrap();
    let sd = CancellationToken::new();
    let net = Net::new(s.chain.clone());
    let node = start(
        db.clone(),
        &net,
        FakeProver::open(),
        cfg(dir.path(), &s.census_dir, 1),
        &sd,
    )
    .await;
    let _h = handle(&node).await;
    wait_until("bad process ignored", async || {
        db.process(&bad_pid_fr())
            .unwrap()
            .is_some_and(|r| r.local == LocalStatus::Ignored)
    })
    .await;
    let note = db.process(&bad_pid_fr()).unwrap().unwrap().note.unwrap();
    assert!(note.contains("refused"), "{note}");
    assert!(
        node.census
            .bad_reason(&bad_pid_fr(), &bad_census_root())
            .is_none()
    );
    assert_eq!(boot_attempts(&db, &bad_pid31()), None);
    sd.cancel();
}

/// Two processes share a census root. The good URI stores it while the
/// bad one is still downloading, so the bad process spawns on the stored
/// root; its download then fails for good. That failure must not mark
/// the running process, or every one of its votes would be refused.
#[tokio::test]
async fn shared_root_stored_mid_download_leaves_no_failure_mark() {
    let s = setup(2, 8, None);
    let gate = Arc::new(Semaphore::new(0));
    let hits = Arc::new(AtomicU64::new(0));
    let uri = census_server(b"not a census".to_vec(), gate.clone(), hits.clone()).await;
    let mut bad = s.env.cfg.clone();
    bad.process_id = bad_pid_fr();
    s.chain.add_bad_process(BadKind::CensusUri {
        uri,
        state_root: genesis_root(&bad).unwrap(),
    });
    let dir = TempDir::new().unwrap();
    let sd = CancellationToken::new();
    let node = start_node_cfg(
        Db::open_in(dir.path()).unwrap(),
        s.chain.clone(),
        FakeProver::open(),
        cfg(dir.path(), &s.census_dir, 1),
        sd.clone(),
    )
    .await;
    wait_until("bad download running", async || {
        hits.load(Ordering::SeqCst) >= 1
    })
    .await;
    let _h = handle(&node).await;
    wait_until("bad process spawned on the stored root", async || {
        node.processes.read().await.contains_key(&bad_pid_fr())
    })
    .await;
    // The download now ends in a terminal format error.
    gate.add_permits(1);
    wait_until("download over", async || gate.available_permits() == 0).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let root = s.env.cfg.census_root;
    assert_eq!(node.census.bad_reason(&bad_pid_fr(), &root), None);
    let rec = node.db.process(&bad_pid_fr()).unwrap().unwrap();
    assert_eq!(rec.local, LocalStatus::Active);
    sd.cancel();
}

// ------------------------------------------------- reorgs at the cursor

const HASH_KEY: &str = "monitor_last_hash";

fn cursor(db: &Db) -> Option<u64> {
    db.meta_u64("monitor_last_block").unwrap()
}

/// The bad process on its own census (voter 7) in `census_dir/bad`.
fn bad_process_on_file(s: &TestSetup) {
    let d = s.census_dir.join("bad");
    std::fs::create_dir_all(&d).unwrap();
    let uri = write_census_of(&d, [7]);
    let mut bad = s.env.cfg.clone();
    bad.process_id = bad_pid_fr();
    bad.census_root = bad_census_root();
    s.chain.add_bad_process(BadKind::CensusUri {
        uri,
        state_root: genesis_root(&bad).unwrap(),
    });
}

/// Waits until the cursor is at the chain head with that block's hash.
async fn settled_at_head(db: &Db, net: &Net) {
    let head = net.chain.head_block();
    wait_until("cursor at head with its hash", async || {
        cursor(db) == Some(head)
            && db.meta_bytes(HASH_KEY).unwrap() == Some(net.hash_of(head).to_vec())
    })
    .await;
}

/// A reorg below the cursor brings in an event the old branch did not
/// have: the monitor sees the cursor block's hash change, rewinds and
/// routes the range again (the event bootstraps a process). Stable hashes
/// never rewind.
#[tokio::test]
async fn reorg_at_the_cursor_reroutes_the_replaced_range() {
    let s = setup(2, 8, None);
    let dir = TempDir::new().unwrap();
    let db = Db::open_in(dir.path()).unwrap();
    let sd = CancellationToken::new();
    let net = Net::new(s.chain.clone());
    let node = start(
        db.clone(),
        &net,
        FakeProver::open(),
        cfg(dir.path(), &s.census_dir, 1),
        &sd,
    )
    .await;
    let _h = handle(&node).await;
    for _ in 0..3 {
        s.chain.advance_time(0);
    }
    settled_at_head(&db, &net).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(node.metrics.reorgs.load(Ordering::SeqCst), 0);

    // The new branch from block 2 on carries a process creation.
    bad_process_on_file(&s);
    let calls = s.chain.event_calls().len();
    net.reorg(2, 1);
    wait_until("process of the new branch spawned", async || {
        node.processes.read().await.contains_key(&bad_pid_fr())
    })
    .await;
    assert_eq!(node.metrics.reorgs.load(Ordering::SeqCst), 1);
    // The rewind goes back 128 blocks, here to block 0.
    assert_eq!(s.chain.event_calls()[calls].0, 1);
    settled_at_head(&db, &net).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(node.metrics.reorgs.load(Ordering::SeqCst), 1);
    sd.cancel();
}

/// The rewind goes back 128 blocks, wherever the fork is.
#[tokio::test]
async fn reorg_rewinds_128_blocks() {
    let s = setup(2, 8, None);
    let dir = TempDir::new().unwrap();
    let db = Db::open_in(dir.path()).unwrap();
    let sd = CancellationToken::new();
    let net = Net::new(s.chain.clone());
    let node = start(
        db.clone(),
        &net,
        FakeProver::open(),
        cfg(dir.path(), &s.census_dir, 1),
        &sd,
    )
    .await;
    let _h = handle(&node).await;
    s.chain.set_head_block(500);
    settled_at_head(&db, &net).await;
    // (fork block, first block scanned again)
    let cases = [(499, 373), (400, 373)];
    for (i, (from, rescan)) in cases.into_iter().enumerate() {
        let calls = s.chain.event_calls().len();
        net.reorg(from, i as u8 + 1);
        let n = i as u64 + 1;
        wait_until("reorg seen", async || {
            node.metrics.reorgs.load(Ordering::SeqCst) == n
        })
        .await;
        settled_at_head(&db, &net).await;
        let after = &s.chain.event_calls()[calls..];
        assert_eq!(
            after.first().map(|c| c.0),
            Some(rescan),
            "case {i}: {after:?}"
        );
    }
    sd.cancel();
}

/// The cursor's hash is persisted with it: a reorg while the node is down
/// is caught at the first poll after the restart.
#[tokio::test]
async fn reorg_while_down_is_caught_after_a_restart() {
    let s = setup(2, 8, None);
    let dir = TempDir::new().unwrap();
    let sd = CancellationToken::new();
    let net = Net::new(s.chain.clone());
    let db = Db::open_in(dir.path()).unwrap();
    let node = start(
        db.clone(),
        &net,
        FakeProver::open(),
        cfg(dir.path(), &s.census_dir, 1),
        &sd,
    )
    .await;
    let _h = handle(&node).await;
    s.chain.advance_time(0);
    settled_at_head(&db, &net).await;
    sd.cancel();
    drop((_h, node, db));

    bad_process_on_file(&s);
    net.reorg(2, 1);
    let db = reopen_db(dir.path()).await;
    let sd = CancellationToken::new();
    let node = start(
        db,
        &net,
        FakeProver::open(),
        cfg(dir.path(), &s.census_dir, 1),
        &sd,
    )
    .await;
    wait_until("process of the new branch spawned", async || {
        node.processes.read().await.contains_key(&bad_pid_fr())
    })
    .await;
    assert_eq!(node.metrics.reorgs.load(Ordering::SeqCst), 1);
    sd.cancel();
}

/// On OP the derivation behind an unsafe-head reorg makes the new branch
/// safe at once, so the safe head read after it can sit above the fork
/// (490 here, written RED against a safe-head floor): the rewind goes the
/// full 128 blocks.
#[tokio::test]
async fn reorg_rewind_reaches_below_a_fork_the_safe_head_passed() {
    let s = setup(2, 8, None);
    let dir = TempDir::new().unwrap();
    let db = Db::open_in(dir.path()).unwrap();
    let sd = CancellationToken::new();
    let net = Net::new(s.chain.clone());
    let node = start(
        db.clone(),
        &net,
        FakeProver::open(),
        cfg(dir.path(), &s.census_dir, 1),
        &sd,
    )
    .await;
    let _h = handle(&node).await;
    s.chain.set_head_block(500);
    settled_at_head(&db, &net).await;

    // The new branch from 480 on carries a process creation at 485.
    bad_process_on_file(&s);
    s.chain.set_head_block(485);
    s.chain.push_event(EventKind::ProcessCreated {
        pid: bad_pid31(),
        creator: Address::ZERO,
    });
    s.chain.set_head_block(500);
    net.reorg(480, 1);
    wait_until("creation at 485 routed", async || {
        node.processes.read().await.contains_key(&bad_pid_fr())
    })
    .await;
    assert_eq!(node.metrics.reorgs.load(Ordering::SeqCst), 1);
    sd.cancel();
}

/// The page's end hash is read before its events: a reorg between the two
/// leaves the old branch's hash, which the next poll catches.
#[tokio::test]
async fn reorg_between_page_hash_and_events_is_caught() {
    let s = setup(2, 8, None);
    let dir = TempDir::new().unwrap();
    let db = Db::open_in(dir.path()).unwrap();
    let sd = CancellationToken::new();
    let net = Net::new(s.chain.clone());
    let node = start(
        db.clone(),
        &net,
        FakeProver::open(),
        cfg(dir.path(), &s.census_dir, 1),
        &sd,
    )
    .await;
    let _h = handle(&node).await;
    settled_at_head(&db, &net).await;
    let next = s.chain.head_block() + 1;
    *net.reorg_in_page.lock().unwrap() = Some((next, next, 1));
    s.chain.advance_time(0);
    wait_until("reorg seen at the next poll", async || {
        node.metrics.reorgs.load(Ordering::SeqCst) == 1
    })
    .await;
    settled_at_head(&db, &net).await;
    sd.cancel();
}

/// Block reads failing while the rest of the RPC answers (rate limits)
/// must not stall routing: after a few failed polls the monitor routes
/// without the check, and resumes it once the reads answer.
#[tokio::test]
async fn failing_block_hash_reads_do_not_stall_routing() {
    let s = setup(2, 8, None);
    let dir = TempDir::new().unwrap();
    let db = Db::open_in(dir.path()).unwrap();
    let sd = CancellationToken::new();
    let net = Net::new(s.chain.clone());
    let node = start(
        db.clone(),
        &net,
        FakeProver::open(),
        cfg(dir.path(), &s.census_dir, 1),
        &sd,
    )
    .await;
    let _h = handle(&node).await;
    settled_at_head(&db, &net).await;

    net.fail_hashes.store(true, Ordering::SeqCst);
    bad_process_on_file(&s);
    s.chain.advance_time(0);
    s.chain.push_event(EventKind::ProcessCreated {
        pid: bad_pid31(),
        creator: Address::ZERO,
    });
    wait_until("creation routed without hashes", async || {
        node.processes.read().await.contains_key(&bad_pid_fr())
    })
    .await;
    net.fail_hashes.store(false, Ordering::SeqCst);
    s.chain.advance_time(0);
    settled_at_head(&db, &net).await;
    assert_eq!(node.metrics.reorgs.load(Ordering::SeqCst), 0);
    sd.cancel();
}

/// A cursor routed without a hash (block reads failing) is armed at the
/// first poll whose read answers, with no new page: a reorg at the cursor
/// height on an idle chain is still caught.
#[tokio::test]
async fn cursor_without_a_hash_is_armed_at_the_next_poll() {
    let s = setup(2, 8, None);
    let dir = TempDir::new().unwrap();
    let db = Db::open_in(dir.path()).unwrap();
    let sd = CancellationToken::new();
    let net = Net::new(s.chain.clone());
    let node = start(
        db.clone(),
        &net,
        FakeProver::open(),
        cfg(dir.path(), &s.census_dir, 1),
        &sd,
    )
    .await;
    let _h = handle(&node).await;
    settled_at_head(&db, &net).await;

    // Five failed reads, then routing goes on without a hash.
    net.fail_hashes.store(true, Ordering::SeqCst);
    bad_process_on_file(&s);
    s.chain.advance_time(0);
    s.chain.push_event(EventKind::ProcessCreated {
        pid: bad_pid31(),
        creator: Address::ZERO,
    });
    wait_until("creation routed without hashes", async || {
        node.processes.read().await.contains_key(&bad_pid_fr())
    })
    .await;
    assert_eq!(cursor(&db), Some(s.chain.head_block()));
    assert_eq!(db.meta_bytes(HASH_KEY).unwrap(), None);

    // Reads recover; no new block, so no page stores a hash.
    net.fail_hashes.store(false, Ordering::SeqCst);
    settled_at_head(&db, &net).await;
    net.reorg(s.chain.head_block(), 1);
    wait_until("same-height reorg seen", async || {
        node.metrics.reorgs.load(Ordering::SeqCst) == 1
    })
    .await;
    sd.cancel();
}
