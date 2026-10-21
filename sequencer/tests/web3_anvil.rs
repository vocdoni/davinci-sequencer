//! web3 layer against a real anvil (Osaka) with the zkvm ProcessRegistry,
//! the accept-all mock verifier and, for the DKG and Council modes, the
//! contracts' `MockDKG` and `MockCouncilManager`. Gated by `ANVIL=1`; needs `anvil` on PATH (or in
//! ~/.foundry/bin) and `forge build` run in `DAVINCI_CONTRACTS_DIR` (default
//! `../../davinci-contracts` from this crate).

mod common;

use std::path::PathBuf;
use std::time::Duration;

use alloy::network::{EthereumWallet, TransactionBuilder};
use alloy::node_bindings::Anvil;
use alloy::primitives::{Address, B256, Bytes, FixedBytes};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::rpc::types::TransactionRequest;
use alloy::signers::local::PrivateKeySigner;
use alloy::sol_types::{SolCall, SolValue};
use davinci_client::organizer::metadata_hash;
use davinci_sequencer::config::SecretString;
use davinci_sequencer::web3::{
    AnvilBlobs, BlobSource, Contracts, EventKind, GraceParams, NewProcess, OnchainCensus,
    ProcessStatus, RevertReason, Web3Error,
};
use davinci_zkvm_sdk::ballot::{Ballot, BallotMode};
use davinci_zkvm_sdk::blob::{TransitionBlobs, TransitionData, build_blobs, decode_blobs};
use davinci_zkvm_sdk::client::PlonkSnark;
use davinci_zkvm_sdk::crypto::elgamal::{Ciphertext, encrypt, keygen};
use davinci_zkvm_sdk::crypto::field::{Fr, U256, fr_from_be_mod_order};
use davinci_zkvm_sdk::limits::VOTE_ID_MIN;

const BATCH_VK: [u8; 32] = [0x11; 32];
const RESULTS_VK: [u8; 32] = [0x22; 32];
const ROOT_C: [u8; 32] = [0x33; 32];
const BALLOT_VK_HASH: [u8; 32] = [0x44; 32];
const NF: u8 = 4;
/// Registry grace constructor args: default, floor, ceil, max total, notice.
const GRACE_ARGS: (u32, u32, u32, u32, u32) = (10, 2, 60, 60, 5);
/// Registry constructor args after `_dkgManager`: `_councilManager`, the grace
/// window, then `_l1BlobVerifier` (zero: blob mode).
fn ctor_tail(council_manager: Address) -> Vec<u8> {
    ctor_tail_l1(council_manager, Address::ZERO)
}

/// [`ctor_tail`] with an L1 blob verifier: the registry in L1-blob mode.
fn ctor_tail_l1(council_manager: Address, l1_blob_verifier: Address) -> Vec<u8> {
    let mut v = council_manager.abi_encode();
    v.extend(GRACE_ARGS.abi_encode_params());
    v.extend(l1_blob_verifier.abi_encode());
    v
}

/// The metadata document behind the `ipfs://` URIs, which nothing fetches.
const META_DOC: &[u8] = br#"{"title":{"default":"web3 test"}}"#;

alloy::sol! {
    function setProcessGrace(bytes31 processId, uint32 grace) external;
}

fn enabled() -> bool {
    std::env::var("ANVIL").is_ok_and(|v| v == "1")
}

fn contracts_dir() -> PathBuf {
    std::env::var("DAVINCI_CONTRACTS_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../davinci-contracts")
        })
}

fn anvil_bin() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_default();
    let p = PathBuf::from(home).join(".foundry/bin/anvil");
    if p.exists() {
        p
    } else {
        PathBuf::from("anvil")
    }
}

fn bytecode(artifact: &str) -> Vec<u8> {
    let path = contracts_dir().join("out").join(artifact);
    let json: serde_json::Value = serde_json::from_slice(
        &std::fs::read(&path)
            .unwrap_or_else(|e| panic!("{}: {e}; run forge build", path.display())),
    )
    .unwrap();
    let hex_code = json["bytecode"]["object"].as_str().unwrap();
    hex::decode(hex_code.trim_start_matches("0x")).unwrap()
}

/// Moves anvil's clock `secs` forward and mines a block.
async fn advance<P: Provider>(rpc: &P, secs: u64) {
    let _: serde_json::Value = rpc
        .raw_request("evm_increaseTime".into(), (secs,))
        .await
        .unwrap();
    let _: serde_json::Value = rpc.raw_request("evm_mine".into(), ()).await.unwrap();
}

async fn deploy<P: Provider>(p: &P, code: Vec<u8>) -> Address {
    let tx = TransactionRequest::default().with_deploy_code(Bytes::from(code));
    let r = p
        .send_transaction(tx)
        .await
        .unwrap()
        .get_receipt()
        .await
        .unwrap();
    assert!(r.status(), "deploy reverted");
    r.contract_address.unwrap()
}

// 512-byte publicValues: register k is the u64 LE word k.
struct Publics([u8; 512]);

impl Publics {
    fn new() -> Self {
        let mut p = Publics([0u8; 512]);
        p.word(0, 1);
        p
    }
    fn word(&mut self, k: usize, v: u32) {
        self.0[8 * k..8 * k + 4].copy_from_slice(&v.to_le_bytes());
    }
    fn reg32(&mut self, k: usize, v: &[u8; 32]) {
        for j in 0..8 {
            self.0[8 * (k + j)..8 * (k + j) + 4].copy_from_slice(&v[4 * j..4 * j + 4]);
        }
    }
}

struct Batch {
    before: [u8; 32],
    after: [u8; 32],
    voters: u32,
    overwrites: u32,
    occupied_before: u32,
}

fn batch_snark(b: &Batch, census_root_be: &[u8; 32], blobs: &TransitionBlobs) -> PlonkSnark {
    let mut p = Publics::new();
    p.reg32(2, &b.before);
    p.reg32(10, &b.after);
    p.word(18, b.voters);
    p.word(19, b.overwrites);
    let mut census_le = *census_root_be;
    census_le.reverse();
    p.reg32(20, &census_le);
    p.reg32(28, &blobs.digest);
    p.word(36, blobs.blobs.len() as u32);
    p.word(42, b.occupied_before);
    p.reg32(46, &blobs.cells_digest);
    PlonkSnark {
        program_vk: BATCH_VK,
        root_c_vadcop_final: ROOT_C,
        public_values: p.0.to_vec(),
        proof_bytes: vec![0u8; 768],
    }
}

fn results_snark(root: &[u8; 32], values: &[u64]) -> PlonkSnark {
    let mut p = Publics::new();
    p.reg32(2, root);
    for (i, v) in values.iter().enumerate() {
        p.word(10 + 2 * i, *v as u32);
        p.word(11 + 2 * i, (*v >> 32) as u32);
    }
    PlonkSnark {
        program_vk: RESULTS_VK,
        root_c_vadcop_final: ROOT_C,
        public_values: p.0.to_vec(),
        proof_bytes: vec![0u8; 768],
    }
}

fn some_ballot(pk: &davinci_zkvm_sdk::crypto::babyjubjub::Point, seed: u64) -> Ballot {
    let mut b = Ballot::identity();
    for (i, ct) in b.0.iter_mut().take(NF as usize).enumerate() {
        *ct = encrypt(pk, i as u64, &U256::from(seed * 31 + i as u64 + 1));
    }
    b
}

fn pid_fr(pid: &[u8; 31]) -> Fr {
    fr_from_be_mod_order(pid)
}

#[tokio::test(flavor = "multi_thread")]
async fn registry_on_anvil() {
    if !enabled() {
        eprintln!("skipped: set ANVIL=1");
        return;
    }
    let anvil = Anvil::at(anvil_bin())
        .args(["--hardfork", "osaka"])
        .try_spawn()
        .expect("spawn anvil");
    let url = anvil.endpoint_url();
    let key: PrivateKeySigner = anvil.keys()[0].clone().into();
    let me = key.address();
    let secret = SecretString::new(hex::encode(key.to_bytes()));
    let deployer = ProviderBuilder::new()
        .wallet(EthereumWallet::from(key))
        .connect_http(url.clone());

    let verifier = deploy(
        &deployer,
        bytecode("MockZiskVerifier.sol/MockZiskVerifier.json"),
    )
    .await;
    let mut code = bytecode("ProcessRegistry.sol/ProcessRegistry.json");
    code.extend(
        (
            anvil.chain_id() as u32,
            verifier,
            B256::from(BATCH_VK),
            B256::from(RESULTS_VK),
            B256::from(ROOT_C),
            B256::from(BALLOT_VK_HASH),
            Address::ZERO,
        )
            .abi_encode_params(),
    );
    code.extend(ctor_tail(Address::ZERO));
    let registry = deploy(&deployer, code).await;

    // An observer cannot send.
    let observer = Contracts::new(std::slice::from_ref(&url), registry, None)
        .await
        .unwrap();
    assert!(observer.signer().is_none());

    let c = Contracts::new(std::slice::from_ref(&url), registry, Some(&secret))
        .await
        .unwrap();
    assert_eq!(c.signer(), Some(me));
    assert!(
        c.cell_proofs(),
        "anvil --hardfork osaka must select v1 sidecars"
    );

    // Organizer: create a process with a fresh election key.
    let (_sk, pk) = keygen(&mut rand::rngs::OsRng);
    let mut census_root = [0u8; 32];
    census_root[31] = 0x2a;
    census_root[0] = 0x01;
    let mode = BallotMode {
        num_fields: NF,
        group_size: 1,
        unique_values: false,
        cost_exponent: 1,
        max_value: 5,
        min_value: 0,
        max_value_sum: 20,
        min_value_sum: 0,
    };
    let np = NewProcess {
        status: ProcessStatus::Ready,
        start_time: 0,
        duration: 3600,
        max_voters: 1000,
        ballot_mode: mode,
        census: OnchainCensus {
            origin: 1,
            root: census_root,
            uri: "file:///tmp/census.json".into(),
            contract_address: [0u8; 20],
        },
        metadata: "ipfs://meta".into(),
        metadata_hash: metadata_hash(META_DOC),
        enc_key: pk,
    };
    let err = observer.create_process(&np).await.err().unwrap();
    assert!(matches!(err, Web3Error::NoSigner), "{err}");
    // The registry's cost rule, refused by name (the vendored ABI has it).
    for (cost_exponent, max_value) in [(0u8, 5u64), (2, 1 << 31)] {
        let mut bad = np.clone();
        bad.ballot_mode.num_fields = 2;
        bad.ballot_mode.cost_exponent = cost_exponent;
        bad.ballot_mode.max_value = max_value;
        match c.create_process(&bad).await {
            Err(Web3Error::Revert(r)) => assert_eq!(r.name(), Some("InvalidBallotCost")),
            other => panic!("cost rule: {other:?}"),
        }
    }
    let (pid, created) = c.create_process(&np).await.unwrap();
    // The id prefix the datadir migration matches processes on.
    assert_eq!(
        pid[20..24],
        davinci_sequencer::storage::pid_prefix(c.chain_id(), &registry.into_array()).unwrap()
    );

    let (head, ts) = c.head().await.unwrap();
    assert!(head >= created.block && ts > 0);
    let events = c.events(0, head).await.unwrap();
    let ev = events
        .iter()
        .find(|e| matches!(e.kind, EventKind::ProcessCreated { .. }))
        .expect("ProcessCreated");
    assert_eq!(ev.tx_hash, created.tx_hash);
    assert_eq!(ev.block, created.block);
    assert_eq!(ev.kind, EventKind::ProcessCreated { pid, creator: me });
    // The same transaction logs the initial metadata right after.
    let meta: Vec<_> = events
        .iter()
        .filter(|e| matches!(e.kind, EventKind::MetadataUpdated { .. }))
        .collect();
    assert_eq!(meta.len(), 1);
    assert_eq!(
        (meta[0].tx_hash, meta[0].log_index),
        (ev.tx_hash, ev.log_index + 1)
    );
    assert_eq!(
        meta[0].kind,
        EventKind::MetadataUpdated {
            pid,
            uri: "ipfs://meta".into(),
            hash: metadata_hash(META_DOC),
        }
    );

    let p = c.process(&pid).await.unwrap();
    assert_eq!(p.status, ProcessStatus::Ready);
    assert_eq!(p.metadata_uri, "ipfs://meta");
    assert_eq!(p.metadata_hash, metadata_hash(META_DOC));
    assert_eq!(p.organizer, me.0.0);
    assert_eq!(p.enc_key, pk);
    assert_eq!(p.ballot_mode, mode);
    assert_eq!(p.census.root, census_root);
    assert_eq!(p.census.origin, 1);
    assert_eq!(p.max_voters, 1000);
    assert_eq!(p.voters_count, 0);
    assert_ne!(p.state_root, [0u8; 32], "genesis root computed on-chain");
    // The grace immutables and the process's window.
    let gp = c.grace_params().await.unwrap();
    assert_eq!(
        gp,
        GraceParams {
            default_grace: 10,
            grace_floor: 2,
            grace_ceil: 60,
            grace_max_total: 60,
            notice_min: 5,
        }
    );
    assert_eq!((p.grace, p.last_vote_at), (10, 0));
    assert_eq!(c.grace_end(&pid).await.unwrap(), p.end_time() + 10);

    // First transition: one blob, real KZG data over real ciphertexts.
    let t1 = TransitionData {
        vote_ids: vec![VOTE_ID_MIN + 42, VOTE_ID_MIN + 7],
        updates: vec![(0x12, some_ballot(&pk, 1)), (0x11, some_ballot(&pk, 2))],
        num_fields: NF,
    };
    let root1 = [0x91u8; 32];
    let blobs1 = build_blobs(&t1, &pid_fr(&pid), &p.state_root).unwrap();
    assert_eq!(blobs1.blobs.len(), 1);
    let b1 = Batch {
        before: p.state_root,
        after: root1,
        voters: 2,
        overwrites: 0,
        occupied_before: 0,
    };
    let snark1 = batch_snark(&b1, &census_root, &blobs1);

    // Pre-flight: the honest fixture passes, a stale root is named.
    c.simulate_transition(&pid, &snark1, &blobs1).await.unwrap();
    let stale = batch_snark(
        &Batch {
            before: [0x55; 32],
            ..b1
        },
        &census_root,
        &blobs1,
    );
    let reason = c
        .simulate_transition(&pid, &stale, &blobs1)
        .await
        .err()
        .unwrap();
    assert_eq!(reason.name(), Some("InvalidStateRoot"), "{reason}");
    // A wrong census root is named too.
    let reason = c
        .simulate_transition(&pid, &batch_snark(&b1, &[7u8; 32], &blobs1), &blobs1)
        .await
        .err()
        .unwrap();
    assert_eq!(reason.name(), Some("InvalidCensusRoot"), "{reason}");
    // A transition without a vote would only move the grace end.
    let empty = batch_snark(&Batch { voters: 0, ..b1 }, &census_root, &blobs1);
    let reason = c
        .simulate_transition(&pid, &empty, &blobs1)
        .await
        .err()
        .unwrap();
    assert_eq!(reason.name(), Some("EmptyTransition"), "{reason}");

    let r1 = c.submit_transition(&pid, &snark1, &blobs1).await.unwrap();
    let blobs = AnvilBlobs::new(std::slice::from_ref(&url));
    let got = blobs
        .blobs_for_tx(r1.tx_hash, r1.block, blobs1.blobs.len() as u64)
        .await
        .unwrap();
    assert_eq!(got, blobs1.blobs);
    let mut sorted = t1.clone();
    sorted.vote_ids.sort();
    sorted.updates.sort_by_key(|(k, _)| *k);
    assert_eq!(decode_blobs(&got, NF).unwrap(), sorted);

    // Replaying the settled batch loses the race: submit surfaces the name.
    let err = c
        .submit_transition(&pid, &snark1, &blobs1)
        .await
        .err()
        .unwrap();
    match &err {
        Web3Error::Revert(r) => assert_eq!(r.name(), Some("InvalidStateRoot")),
        e => panic!("want a revert, got {e}"),
    }

    // Second transition: two blobs, occupied_before = the first batch's voters.
    let n = 500u64;
    let t2 = TransitionData {
        vote_ids: (0..n).map(|i| VOTE_ID_MIN + 1000 + i).collect(),
        updates: (0..n)
            .map(|i| (0x20 + i, Ballot([Ciphertext::IDENTITY; 16])))
            .collect(),
        num_fields: NF,
    };
    let blobs2 = build_blobs(&t2, &pid_fr(&pid), &root1).unwrap();
    assert_eq!(blobs2.blobs.len(), 2);
    let root2 = [0x92u8; 32];
    let snark2 = batch_snark(
        &Batch {
            before: root1,
            after: root2,
            voters: n as u32,
            overwrites: 0,
            occupied_before: 2,
        },
        &census_root,
        &blobs2,
    );
    c.simulate_transition(&pid, &snark2, &blobs2).await.unwrap();
    // A blob tx and a plain tx from the same key at once: the sender
    // serialises them, both land.
    let np2 = NewProcess {
        metadata: "ipfs://second".into(),
        ..np.clone()
    };
    let (r2, second) = tokio::join!(
        c.submit_transition(&pid, &snark2, &blobs2),
        c.create_process(&np2)
    );
    let r2 = r2.unwrap();
    let (pid2, second) = second.unwrap();
    assert_ne!(pid2, pid);
    assert_ne!(second.tx_hash, r2.tx_hash);
    assert_eq!((r2.replacements, second.replacements), (0, 0));
    let got2 = blobs
        .blobs_for_tx(r2.tx_hash, r2.block, blobs2.blobs.len() as u64)
        .await
        .unwrap();
    assert_eq!(got2, blobs2.blobs);

    let p = c.process(&pid).await.unwrap();
    assert_eq!(p.state_root, root2);
    assert_eq!(p.voters_count, 2 + n);
    assert_eq!(p.batch_number, 2);
    // Each landing stamps its block time; the node's formula matches.
    let t2 = deployer
        .get_block_by_number(r2.block.into())
        .await
        .unwrap()
        .unwrap()
        .header
        .timestamp;
    assert_eq!(p.last_vote_at, t2);
    assert_eq!(
        c.grace_end(&pid).await.unwrap(),
        p.grace_end(gp.grace_max_total)
    );

    // Both transitions show up as typed events with their tx and block.
    let (head, _) = c.head().await.unwrap();
    let evs = c.events(created.block, head).await.unwrap();
    let st: Vec<_> = evs
        .iter()
        .filter(|e| matches!(e.kind, EventKind::StateTransitioned { .. }))
        .collect();
    assert_eq!(st.len(), 2);
    assert_eq!(st[0].tx_hash, r1.tx_hash);
    assert_eq!(st[1].block, r2.block);
    assert_eq!(
        st[1].kind,
        EventKind::StateTransitioned {
            pid,
            sender: me,
            old_root: root1,
            new_root: root2,
            voters: 2 + n,
            overwrites: 0,
            n_blobs: 2,
            l1: None,
        }
    );

    // The organizer widens the grace while open: a typed event and the
    // stored value; below the floor is refused.
    let set_grace = |grace: u32| {
        TransactionRequest::default().with_to(registry).with_input(
            setProcessGraceCall {
                processId: FixedBytes(pid),
                grace,
            }
            .abi_encode(),
        )
    };
    // A fresh provider: `c` sent from this key since the deployer cached its nonce.
    let organizer = ProviderBuilder::new()
        .wallet(EthereumWallet::from(PrivateKeySigner::from(
            anvil.keys()[0].clone(),
        )))
        .connect_http(url.clone());
    let g = organizer
        .send_transaction(set_grace(30))
        .await
        .unwrap()
        .get_receipt()
        .await
        .unwrap();
    assert!(g.status());
    let b = g.block_number.unwrap();
    let evs = c.events(b, b).await.unwrap();
    assert!(
        evs.iter()
            .any(|e| e.kind == EventKind::GraceChanged { pid, grace: 30 })
    );
    assert_eq!(c.process(&pid).await.unwrap().grace, 30);
    assert!(organizer.send_transaction(set_grace(1)).await.is_err());

    // Results: end the process, wait out the grace, settle the tally.
    c.end_process(&pid).await.unwrap();
    let values = [7u64, (1 << 32) + 5, 0, 3];
    let snark = results_snark(&root2, &values);
    match c.submit_results(&pid, &snark).await.err().unwrap() {
        Web3Error::Revert(r) => assert_eq!(r.name(), Some("GraceOpen"), "{r}"),
        e => panic!("want a revert, got {e}"),
    }
    let p = c.process(&pid).await.unwrap();
    assert_eq!(p.grace_end(gp.grace_max_total), p.end_time() + 30);
    advance(&ProviderBuilder::new().connect_http(url.clone()), 30).await;
    let r = c.submit_results(&pid, &snark).await.unwrap();
    let evs = c.events(r.block, r.block).await.unwrap();
    assert!(evs.iter().any(|e| e.kind
        == EventKind::ResultsSet {
            pid,
            sender: me,
            results: values.to_vec()
        }));
    assert!(evs.iter().any(|e| matches!(
        e.kind,
        EventKind::StatusChanged {
            new: ProcessStatus::Results,
            ..
        }
    )));
    let p = c.process(&pid).await.unwrap();
    assert_eq!(p.status, ProcessStatus::Results);
    assert_eq!(p.results, values.to_vec());
    // Observers can pre-flight too; a finished process is closed.
    let r = observer
        .simulate_transition(&pid, &snark2, &blobs2)
        .await
        .err()
        .unwrap();
    assert!(
        matches!(&r, RevertReason::Revert { name, .. } if name == "InvalidStatus"),
        "{r}"
    );

    // A tx that does not get mined in time is replaced at the same nonce with
    // higher fees; the replacement is what lands.
    let rpc = ProviderBuilder::new().connect_http(url.clone());
    let _: () = rpc
        .raw_request("evm_setAutomine".into(), (false,))
        .await
        .unwrap();
    let mut slow = c.clone();
    slow.set_receipt_timeout(Duration::from_millis(700));
    let miner = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(1800)).await;
        let _: serde_json::Value = rpc.raw_request("evm_mine".into(), ()).await.unwrap();
        let _: () = rpc
            .raw_request("evm_setAutomine".into(), (true,))
            .await
            .unwrap();
    });
    let (pid3, r3) = slow
        .create_process(&NewProcess {
            metadata: "ipfs://third".into(),
            ..np.clone()
        })
        .await
        .unwrap();
    miner.await.unwrap();
    assert!(r3.replacements >= 1, "{r3:?}");
    assert_eq!(c.process(&pid3).await.unwrap().metadata_uri, "ipfs://third");
    // The node keeps sending after it.
    c.end_process(&pid2).await.unwrap();
}

// `--op-max-tx-fee` below a settlement's price: the node's transition is
// withheld before signing, nothing reaches the chain, and the same call
// lands under the default cap.
#[tokio::test(flavor = "multi_thread")]
async fn op_max_tx_fee_withholds_a_settlement() {
    use davinci_sequencer::config::Config;
    use davinci_sequencer::web3::DEFAULT_MAX_TX_FEE;

    if !enabled() {
        eprintln!("skipped: set ANVIL=1");
        return;
    }
    let anvil = Anvil::at(anvil_bin())
        .args(["--hardfork", "osaka"])
        .try_spawn()
        .expect("spawn anvil");
    let url = anvil.endpoint_url();
    let key: PrivateKeySigner = anvil.keys()[0].clone().into();
    let me = key.address();
    let secret = SecretString::new(hex::encode(key.to_bytes()));
    let deployer = ProviderBuilder::new()
        .wallet(EthereumWallet::from(key))
        .connect_http(url.clone());
    let verifier = deploy(
        &deployer,
        bytecode("MockZiskVerifier.sol/MockZiskVerifier.json"),
    )
    .await;
    let mut code = bytecode("ProcessRegistry.sol/ProcessRegistry.json");
    code.extend(
        (
            anvil.chain_id() as u32,
            verifier,
            B256::from(BATCH_VK),
            B256::from(RESULTS_VK),
            B256::from(ROOT_C),
            B256::from(BALLOT_VK_HASH),
            Address::ZERO,
        )
            .abi_encode_params(),
    );
    code.extend(ctor_tail(Address::ZERO));
    let registry = deploy(&deployer, code).await;

    // The node, from its configuration, capped at one wei.
    let mut cfg = Config::parse_args([
        "davinci-sequencer",
        "--network",
        "custom",
        "--registry",
        &registry.to_string(),
        "--rpc-url",
        url.as_str(),
        "--blob-source",
        "anvil",
        "--op-max-tx-fee",
        "1",
    ])
    .unwrap();
    cfg.privkey = Some(secret);
    let mut c = Contracts::connect(&cfg).await.unwrap();
    assert_eq!(c.signer(), Some(me));

    let (_sk, pk) = keygen(&mut rand::rngs::OsRng);
    let mut census_root = [0u8; 32];
    census_root[0] = 0x01;
    let np = NewProcess {
        status: ProcessStatus::Ready,
        start_time: 0,
        duration: 3600,
        max_voters: 1000,
        ballot_mode: BallotMode {
            num_fields: NF,
            group_size: 1,
            unique_values: false,
            cost_exponent: 1,
            max_value: 5,
            min_value: 0,
            max_value_sum: 20,
            min_value_sum: 0,
        },
        census: OnchainCensus {
            origin: 1,
            root: census_root,
            uri: "file:///tmp/census.json".into(),
            contract_address: [0u8; 20],
        },
        metadata: "ipfs://meta".into(),
        metadata_hash: metadata_hash(META_DOC),
        enc_key: pk,
    };
    let count = async || {
        let latest = deployer.get_transaction_count(me).await.unwrap();
        let pending = deployer.get_transaction_count(me).pending().await.unwrap();
        (latest, pending)
    };

    // Any send: the process creation is withheld too.
    let before = count().await;
    let err = c.create_process(&np).await.err().unwrap();
    assert!(matches!(err, Web3Error::FeeCap { cap: 1, .. }), "{err}");
    assert_eq!(count().await, before, "nothing broadcast");
    c.set_max_tx_fee(DEFAULT_MAX_TX_FEE);
    let (pid, _) = c.create_process(&np).await.unwrap();
    c.set_max_tx_fee(1);

    let p = c.process(&pid).await.unwrap();
    let t = TransitionData {
        vote_ids: vec![VOTE_ID_MIN + 1],
        updates: vec![(0x11, some_ballot(&pk, 1))],
        num_fields: NF,
    };
    let blobs = build_blobs(&t, &pid_fr(&pid), &p.state_root).unwrap();
    let snark = batch_snark(
        &Batch {
            before: p.state_root,
            after: [0x91; 32],
            voters: 1,
            overwrites: 0,
            occupied_before: 0,
        },
        &census_root,
        &blobs,
    );
    c.simulate_transition(&pid, &snark, &blobs).await.unwrap();
    let before = count().await;
    let err = c
        .submit_transition(&pid, &snark, &blobs)
        .await
        .err()
        .unwrap();
    let Web3Error::FeeCap { want, cap: 1 } = err else {
        panic!("want FeeCap, got {err}")
    };
    assert_eq!(count().await, before, "nothing broadcast");
    assert_eq!(c.process(&pid).await.unwrap().voters_count, 0);

    // Under the default cap the same transition lands.
    assert!(want <= DEFAULT_MAX_TX_FEE, "{want}");
    c.set_max_tx_fee(DEFAULT_MAX_TX_FEE);
    let r = c.submit_transition(&pid, &snark, &blobs).await.unwrap();
    assert_eq!(r.replacements, 0);
    assert_eq!(count().await, (before.0 + 1, before.1 + 1));
    assert_eq!(c.process(&pid).await.unwrap().voters_count, 1);
}

// An organizer replaces the metadata: the node reads each value from
// getProcess and as a typed event, creation first, in order.
#[tokio::test(flavor = "multi_thread")]
async fn metadata_updates_on_anvil() {
    use davinci_client::organizer::{KeyMode as OrgKeyMode, NewProcess as OrgProcess, Organizer};

    if !enabled() {
        eprintln!("skipped: set ANVIL=1");
        return;
    }
    let anvil = Anvil::at(anvil_bin())
        .args(["--hardfork", "osaka"])
        .try_spawn()
        .expect("spawn anvil");
    let url = anvil.endpoint_url();
    let key: PrivateKeySigner = anvil.keys()[0].clone().into();
    let deployer = ProviderBuilder::new()
        .wallet(EthereumWallet::from(key))
        .connect_http(url.clone());
    let verifier = deploy(
        &deployer,
        bytecode("MockZiskVerifier.sol/MockZiskVerifier.json"),
    )
    .await;
    let mut code = bytecode("ProcessRegistry.sol/ProcessRegistry.json");
    code.extend(
        (
            anvil.chain_id() as u32,
            verifier,
            B256::from(BATCH_VK),
            B256::from(RESULTS_VK),
            B256::from(ROOT_C),
            B256::from(BALLOT_VK_HASH),
            Address::ZERO,
        )
            .abi_encode_params(),
    );
    code.extend(ctor_tail(Address::ZERO));
    let registry = deploy(&deployer, code).await;
    let node = Contracts::new(std::slice::from_ref(&url), registry, None)
        .await
        .unwrap();

    // Account 1 organizes through the client. The metadata changes only
    // before the start.
    let org = Organizer::connect(url.as_str(), anvil.keys()[1].clone().into(), registry).unwrap();
    let (_sk, pk) = keygen(&mut rand::rngs::OsRng);
    let (_, now) = node.head().await.unwrap();
    let pid = org
        .create_process(&OrgProcess {
            process_id: org.next_process_id().await.unwrap(),
            start_time: now + 120,
            duration: 3600,
            max_voters: 100,
            ballot_mode: BallotMode {
                num_fields: NF,
                group_size: 1,
                unique_values: false,
                cost_exponent: 1,
                max_value: 5,
                min_value: 0,
                max_value_sum: 20,
                min_value_sum: 0,
            },
            census_origin: 1,
            census_root: [0xc5; 32],
            census_contract: [0; 20],
            census_uri: "file:///tmp/census.json".into(),
            metadata: "ipfs://meta".into(),
            metadata_hash: metadata_hash(META_DOC),
            key_mode: OrgKeyMode::Sequencer(pk),
        })
        .await
        .unwrap()
        .pid;
    let doc2: &[u8] = br#"{"title":{"default":"web3 test, v2"}}"#;
    org.set_process_metadata(&pid, "ipfs://meta-2", metadata_hash(doc2))
        .await
        .unwrap();

    let p = node.process(&pid).await.unwrap();
    assert_eq!(p.metadata_uri, "ipfs://meta-2");
    assert_eq!(p.metadata_hash, metadata_hash(doc2));
    let (head, _) = node.head().await.unwrap();
    let got: Vec<_> = node
        .events(0, head)
        .await
        .unwrap()
        .into_iter()
        .filter_map(|e| match e.kind {
            EventKind::MetadataUpdated { pid: at, uri, hash } if at == pid => Some((uri, hash)),
            _ => None,
        })
        .collect();
    assert_eq!(
        got,
        [
            ("ipfs://meta".to_string(), metadata_hash(META_DOC)),
            ("ipfs://meta-2".to_string(), metadata_hash(doc2)),
        ]
    );
}

// The boot check: only a registry carrying the release pins, on this chain,
// settling through the pinned ZiskVerifier passes.
#[tokio::test(flavor = "multi_thread")]
async fn registry_must_be_the_pinned_release() {
    if !enabled() {
        eprintln!("skipped: set ANVIL=1");
        return;
    }
    use davinci_zkvm_sdk::release;
    let anvil = Anvil::at(anvil_bin())
        .args(["--hardfork", "osaka"])
        .try_spawn()
        .expect("spawn anvil");
    let url = anvil.endpoint_url();
    let key: PrivateKeySigner = anvil.keys()[0].clone().into();
    let deployer = ProviderBuilder::new()
        .wallet(EthereumWallet::from(key))
        .connect_http(url.clone());
    let zisk = deploy(&deployer, bytecode("ZiskVerifier.sol/ZiskVerifier.json")).await;
    let mock = deploy(
        &deployer,
        bytecode("MockZiskVerifier.sol/MockZiskVerifier.json"),
    )
    .await;
    let vk_hash =
        davinci_zkvm_sdk::groth16::BallotVerifier::from_snarkjs_json(release::ballot_vk_json())
            .unwrap()
            .vk_hash();
    let chain = anvil.chain_id() as u32;
    let registry = async |chain: u32, verifier: Address, batch_vk: [u8; 32]| {
        let mut code = bytecode("ProcessRegistry.sol/ProcessRegistry.json");
        code.extend(
            (
                chain,
                verifier,
                B256::from(batch_vk),
                B256::from(release::RESULTS_PROGRAM_VK),
                B256::from(release::ROOT_C_VADCOP_FINAL),
                B256::from(vk_hash),
                Address::ZERO,
            )
                .abi_encode_params(),
        );
        code.extend(ctor_tail(Address::ZERO));
        let r = deploy(&deployer, code).await;
        Contracts::new(std::slice::from_ref(&url), r, None)
            .await
            .unwrap()
    };
    let fields = |e: Web3Error| match e {
        Web3Error::Release(m) => m.iter().map(|m| m.field).collect::<Vec<_>>(),
        e => panic!("not a release error: {e}"),
    };

    let good = registry(chain, zisk, release::BATCH_PROGRAM_VK).await;
    good.check_release(&vk_hash).await.unwrap();
    // A node running another ballot VK than the registry pins.
    let mut other = vk_hash;
    other[0] ^= 1;
    let e = good.check_release(&other).await.unwrap_err();
    assert_eq!(fields(e), ["registry.ballotVKHash"]);

    let c = registry(chain, zisk, [0x11; 32]).await;
    let e = c.check_release(&vk_hash).await.unwrap_err();
    let msg = e.to_string();
    assert!(
        msg.contains(&format!(
            "registry.batchProgramVK: expected 0x{}, got 0x{}",
            hex::encode(release::BATCH_PROGRAM_VK),
            "11".repeat(32)
        )),
        "{msg}"
    );
    assert_eq!(fields(e), ["registry.batchProgramVK"]);

    let c = registry(chain + 1, zisk, release::BATCH_PROGRAM_VK).await;
    let e = c.check_release(&vk_hash).await.unwrap_err();
    assert_eq!(fields(e), ["registry.chainID"]);

    let c = registry(chain, mock, release::BATCH_PROGRAM_VK).await;
    let e = c.check_release(&vk_hash).await.unwrap_err();
    assert_eq!(
        fields(e),
        ["verifier.getRootCVadcopFinal", "verifier.codehash"]
    );
}

// The P256VERIFY probe tells Osaka apart without eth_config.
#[tokio::test(flavor = "multi_thread")]
async fn p256_probe_detects_osaka() {
    if !enabled() {
        eprintln!("skipped: set ANVIL=1");
        return;
    }
    for (fork, osaka) in [("osaka", true), ("prague", false)] {
        let anvil = Anvil::at(anvil_bin())
            .args(["--hardfork", fork])
            .try_spawn()
            .expect("spawn anvil");
        let p = ProviderBuilder::new().connect_http(anvil.endpoint_url());
        assert_eq!(
            davinci_sequencer::web3::probe_osaka(&p).await.unwrap(),
            osaka,
            "{fork}"
        );
    }
}

// An RPC that forwards `eth_sendRawTransaction` to anvil, then answers 503:
// the send fails over to anvil itself, which has the tx already. The node
// must take it as sent and wait for its receipt. Then a replacement answered
// "nonce too low" (not ours: the first send got mined) keeps waiting on the
// earlier hash instead of failing.
#[tokio::test(flavor = "multi_thread")]
async fn send_that_failed_over_is_maybe_sent() {
    if !enabled() {
        eprintln!("skipped: set ANVIL=1");
        return;
    }
    let anvil = Anvil::at(anvil_bin())
        .args(["--hardfork", "osaka"])
        .try_spawn()
        .expect("spawn anvil");
    let url = anvil.endpoint_url();
    let key: PrivateKeySigner = anvil.keys()[0].clone().into();
    let secret = SecretString::new(hex::encode(key.to_bytes()));
    let deployer = ProviderBuilder::new()
        .wallet(EthereumWallet::from(key))
        .connect_http(url.clone());
    let verifier = deploy(
        &deployer,
        bytecode("MockZiskVerifier.sol/MockZiskVerifier.json"),
    )
    .await;
    let mut code = bytecode("ProcessRegistry.sol/ProcessRegistry.json");
    code.extend(
        (
            anvil.chain_id() as u32,
            verifier,
            B256::from(BATCH_VK),
            B256::from(RESULTS_VK),
            B256::from(ROOT_C),
            B256::from(BALLOT_VK_HASH),
            Address::ZERO,
        )
            .abi_encode_params(),
    );
    code.extend(ctor_tail(Address::ZERO));
    let registry = deploy(&deployer, code).await;

    let sends = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let (up, n) = (url.clone(), sends.clone());
    let app = axum::Router::new().route(
        "/",
        axum::routing::post(move |body: axum::body::Bytes| async move {
            let req: serde_json::Value = serde_json::from_slice(&body).unwrap();
            let fwd = async |body: Vec<u8>| {
                let resp = reqwest::Client::new()
                    .post(up.clone())
                    .header("content-type", "application/json")
                    .body(body)
                    .send()
                    .await
                    .unwrap();
                resp.bytes().await.unwrap().to_vec()
            };
            let ok = axum::http::StatusCode::OK;
            if req["method"] != "eth_sendRawTransaction" {
                return (ok, fwd(body.to_vec()).await);
            }
            match n.fetch_add(1, std::sync::atomic::Ordering::SeqCst) {
                // Pooled, but the answer is lost.
                0 => {
                    fwd(body.to_vec()).await;
                    (axum::http::StatusCode::SERVICE_UNAVAILABLE, Vec::new())
                }
                1 => (ok, fwd(body.to_vec()).await),
                // The replacement: the first send gets mined instead.
                _ => {
                    let mine =
                        serde_json::json!({"jsonrpc":"2.0","id":1,"method":"evm_mine","params":[]});
                    fwd(mine.to_string().into_bytes()).await;
                    let e = serde_json::json!({"code":-32000,"message":"nonce too low"});
                    let v = serde_json::json!({"jsonrpc":"2.0","id":req["id"],"error":e});
                    (ok, v.to_string().into_bytes())
                }
            }
        }),
    );
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy: url::Url = format!("http://{}/", l.local_addr().unwrap())
        .parse()
        .unwrap();
    tokio::spawn(async move { axum::serve(l, app).await });

    let c = Contracts::new(&[proxy.clone(), url.clone()], registry, Some(&secret))
        .await
        .unwrap();
    let (_sk, pk) = keygen(&mut rand::rngs::OsRng);
    let np = NewProcess {
        status: ProcessStatus::Ready,
        start_time: 0,
        duration: 3600,
        max_voters: 10,
        ballot_mode: BallotMode {
            num_fields: NF,
            group_size: 1,
            unique_values: false,
            cost_exponent: 1,
            max_value: 5,
            min_value: 0,
            max_value_sum: 20,
            min_value_sum: 0,
        },
        census: OnchainCensus {
            origin: 1,
            root: [1u8; 32],
            uri: "file:///tmp/census.json".into(),
            contract_address: [0u8; 20],
        },
        metadata: "ipfs://meta".into(),
        metadata_hash: metadata_hash(META_DOC),
        enc_key: pk,
    };
    let (pid, r) = c.create_process(&np).await.unwrap();
    assert_eq!(sends.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(r.replacements, 0);
    assert_eq!(c.process(&pid).await.unwrap().max_voters, 10);

    let rpc = ProviderBuilder::new().connect_http(url);
    let _: () = rpc
        .raw_request("evm_setAutomine".into(), (false,))
        .await
        .unwrap();
    let mut c = Contracts::new(&[proxy], registry, Some(&secret))
        .await
        .unwrap();
    c.set_receipt_timeout(Duration::from_millis(500));
    let (pid, r) = c
        .create_process(&NewProcess {
            metadata: "ipfs://second".into(),
            ..np
        })
        .await
        .unwrap();
    assert_eq!(sends.load(std::sync::atomic::Ordering::SeqCst), 3);
    assert_eq!(r.replacements, 0);
    assert_eq!(c.process(&pid).await.unwrap().metadata_uri, "ipfs://second");
}

/// How a [`lagging_proxy`] lags; 0 turns a knob off.
#[derive(Default)]
struct Lag {
    /// `eth_call` at "latest" runs at this block: a backend behind the head.
    latest: std::sync::atomic::AtomicU64,
    /// `eth_call` at a block above this answers "header not found".
    known: std::sync::atomic::AtomicU64,
}

// A JSON-RPC proxy in front of anvil whose `eth_call` lags per `lag`, like
// a load-balanced public RPC whose backends trail each other.
async fn lagging_proxy(upstream: url::Url, lag: std::sync::Arc<Lag>) -> url::Url {
    use std::sync::atomic::Ordering::Relaxed;
    let app = axum::Router::new().route(
        "/",
        axum::routing::post(move |body: axum::body::Bytes| {
            let (lag, upstream) = (lag.clone(), upstream.clone());
            async move {
                let mut req: serde_json::Value = serde_json::from_slice(&body).unwrap();
                if req["method"] == "eth_call" {
                    let tag = req["params"][1].as_str().unwrap_or("latest").to_string();
                    let (latest, known) = (lag.latest.load(Relaxed), lag.known.load(Relaxed));
                    if tag == "latest" && latest != 0 {
                        req["params"][1] = format!("{latest:#x}").into();
                    } else if let Some(n) = tag.strip_prefix("0x")
                        && known != 0
                        && u64::from_str_radix(n, 16).unwrap() > known
                    {
                        let e = serde_json::json!({"code": -32000, "message": "header not found"});
                        return serde_json::json!({"jsonrpc": "2.0", "id": req["id"], "error": e})
                            .to_string();
                    }
                }
                reqwest::Client::new()
                    .post(upstream)
                    .header("content-type", "application/json")
                    .body(req.to_string())
                    .send()
                    .await
                    .unwrap()
                    .text()
                    .await
                    .unwrap()
            }
        }),
    );
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/", l.local_addr().unwrap())
        .parse()
        .unwrap();
    tokio::spawn(async move { axum::serve(l, app).await });
    url
}

// Two sequencers settle the same root in one block. The loser's RPC lags
// behind that block, so a replay at `latest` passes and names nothing.
// Replayed at the receipt's block it names InvalidStateRoot; when the RPC
// lacks that block too, the loser gets `Lost`, never a named failure.
#[tokio::test(flavor = "multi_thread")]
async fn same_block_race_names_the_loser() {
    use std::sync::atomic::Ordering::Relaxed;
    if !enabled() {
        eprintln!("skipped: set ANVIL=1");
        return;
    }
    let anvil = Anvil::at(anvil_bin())
        .args(["--hardfork", "osaka"])
        .try_spawn()
        .expect("spawn anvil");
    let url = anvil.endpoint_url();
    let secret = |k: usize| {
        let key: PrivateKeySigner = anvil.keys()[k].clone().into();
        SecretString::new(hex::encode(key.to_bytes()))
    };
    let deployer = ProviderBuilder::new()
        .wallet(EthereumWallet::from(PrivateKeySigner::from(
            anvil.keys()[0].clone(),
        )))
        .connect_http(url.clone());
    let verifier = deploy(
        &deployer,
        bytecode("MockZiskVerifier.sol/MockZiskVerifier.json"),
    )
    .await;
    let mut code = bytecode("ProcessRegistry.sol/ProcessRegistry.json");
    code.extend(
        (
            anvil.chain_id() as u32,
            verifier,
            B256::from(BATCH_VK),
            B256::from(RESULTS_VK),
            B256::from(ROOT_C),
            B256::from(BALLOT_VK_HASH),
            Address::ZERO,
        )
            .abi_encode_params(),
    );
    code.extend(ctor_tail(Address::ZERO));
    let registry = deploy(&deployer, code).await;
    let lag = std::sync::Arc::new(Lag::default());
    let proxy = lagging_proxy(url.clone(), lag.clone()).await;
    let a = Contracts::new(std::slice::from_ref(&proxy), registry, Some(&secret(0)))
        .await
        .unwrap();
    let b = Contracts::new(std::slice::from_ref(&proxy), registry, Some(&secret(1)))
        .await
        .unwrap();

    let (_sk, pk) = keygen(&mut rand::rngs::OsRng);
    let mut census_root = [0u8; 32];
    census_root[31] = 0x2a;
    let (pid, _) = a
        .create_process(&NewProcess {
            status: ProcessStatus::Ready,
            start_time: 0,
            duration: 3600,
            max_voters: 1000,
            ballot_mode: BallotMode {
                num_fields: NF,
                group_size: 1,
                unique_values: false,
                cost_exponent: 1,
                max_value: 5,
                min_value: 0,
                max_value_sum: 20,
                min_value_sum: 0,
            },
            census: OnchainCensus {
                origin: 1,
                root: census_root,
                uri: "file:///tmp/census.json".into(),
                contract_address: [0u8; 20],
            },
            metadata: "ipfs://meta".into(),
            metadata_hash: metadata_hash(META_DOC),
            enc_key: pk,
        })
        .await
        .unwrap();

    let rpc = ProviderBuilder::new().connect_http(url.clone());
    // Both nodes settle a 2-vote transition from the current root, mined
    // together; returns the winner's new root and the loser's error.
    let race = async |round: u64| {
        let p = a.process(&pid).await.unwrap();
        let transition = |tag: u8| {
            let t = TransitionData {
                vote_ids: vec![
                    VOTE_ID_MIN + 100 * round + tag as u64,
                    VOTE_ID_MIN + 100 * round + 50 + tag as u64,
                ],
                updates: vec![
                    (0x10 + 2 * tag as u64, some_ballot(&pk, tag as u64)),
                    (0x11 + 2 * tag as u64, some_ballot(&pk, 9 + tag as u64)),
                ],
                num_fields: NF,
            };
            let blobs = build_blobs(&t, &pid_fr(&pid), &p.state_root).unwrap();
            let batch = Batch {
                before: p.state_root,
                after: [tag + 0x10 * round as u8; 32],
                voters: 2,
                overwrites: 0,
                occupied_before: p.voters_count as u32,
            };
            (batch_snark(&batch, &census_root, &blobs), blobs)
        };
        let ((sa, ba), (sb, bb)) = (transition(1), transition(2));
        let _: () = rpc
            .raw_request("evm_setAutomine".into(), (false,))
            .await
            .unwrap();
        let miner = async {
            // Mine once both transactions are pooled.
            for _ in 0..600 {
                let st: serde_json::Value =
                    rpc.raw_request("txpool_status".into(), ()).await.unwrap();
                let pending = match &st["pending"] {
                    serde_json::Value::String(s) => {
                        u64::from_str_radix(s.trim_start_matches("0x"), 16).unwrap()
                    }
                    v => v.as_u64().unwrap(),
                };
                if pending == 2 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            let _: serde_json::Value = rpc.raw_request("evm_mine".into(), ()).await.unwrap();
            let _: () = rpc
                .raw_request("evm_setAutomine".into(), (true,))
                .await
                .unwrap();
        };
        let (ra, rb, ()) = tokio::join!(
            a.submit_transition(&pid, &sa, &ba),
            b.submit_transition(&pid, &sb, &bb),
            miner
        );
        let (won, lost) = match (ra, rb) {
            (Ok(r), Err(e)) | (Err(e), Ok(r)) => (r, e),
            (ra, rb) => panic!("want one winner: {ra:?} / {rb:?}"),
        };
        (won, lost)
    };

    // The RPC answers `latest` from the block before the race.
    let (head, _) = a.head().await.unwrap();
    lag.latest.store(head, Relaxed);
    let (won, lost) = race(1).await;
    assert_eq!(won.block, head + 1);
    match &lost {
        Web3Error::Revert(r) => assert_eq!(r.name(), Some("InvalidStateRoot"), "{r}"),
        e => panic!("want a named revert, got {e}"),
    }

    // Nor does it serve the race block: the replay falls back to `latest`,
    // which passes. A lost race, not a failure.
    let (head, _) = a.head().await.unwrap();
    lag.latest.store(head, Relaxed);
    lag.known.store(head, Relaxed);
    let (won, lost) = race(2).await;
    assert_eq!(won.block, head + 1);
    assert!(
        matches!(lost, Web3Error::Lost { block, .. } if block == won.block),
        "{lost}"
    );

    lag.latest.store(0, Relaxed);
    lag.known.store(0, Relaxed);
    let p = a.process(&pid).await.unwrap();
    assert_eq!((p.batch_number, p.voters_count), (2, 4));
}

// The only RPC rate-limits the receipt read of a tx already sent for longer
// than the transport waits on a resting endpoint, so the read fails: the
// node keeps waiting on the tx instead of giving the settlement up as failed
// (and later taking its own transition for another sequencer's).
#[tokio::test(flavor = "multi_thread")]
async fn receipt_wait_rides_out_a_rate_limit() {
    use axum::response::IntoResponse;
    use std::sync::atomic::Ordering::SeqCst;
    if !enabled() {
        eprintln!("skipped: set ANVIL=1");
        return;
    }
    let anvil = Anvil::at(anvil_bin())
        .args(["--hardfork", "osaka"])
        .try_spawn()
        .expect("spawn anvil");
    let url = anvil.endpoint_url();
    let key: PrivateKeySigner = anvil.keys()[0].clone().into();
    let secret = SecretString::new(hex::encode(key.to_bytes()));
    let deployer = ProviderBuilder::new()
        .wallet(EthereumWallet::from(key))
        .connect_http(url.clone());
    let verifier = deploy(
        &deployer,
        bytecode("MockZiskVerifier.sol/MockZiskVerifier.json"),
    )
    .await;
    let mut code = bytecode("ProcessRegistry.sol/ProcessRegistry.json");
    code.extend(
        (
            anvil.chain_id() as u32,
            verifier,
            B256::from(BATCH_VK),
            B256::from(RESULTS_VK),
            B256::from(ROOT_C),
            B256::from(BALLOT_VK_HASH),
            Address::ZERO,
        )
            .abi_encode_params(),
    );
    code.extend(ctor_tail(Address::ZERO));
    let registry = deploy(&deployer, code).await;

    // Forwards everything to anvil but answers the first receipt read 429,
    // with a Retry-After past the 10 s a request waits for a resting
    // endpoint: the transport rests it and fails the read at once.
    const RETRY_AFTER: u64 = 12;
    let reads = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let (up, n) = (url.clone(), reads.clone());
    let app = axum::Router::new().route(
        "/",
        axum::routing::post(move |body: axum::body::Bytes| async move {
            let req: serde_json::Value = serde_json::from_slice(&body).unwrap();
            if req["method"] == "eth_getTransactionReceipt" && n.fetch_add(1, SeqCst) == 0 {
                return (
                    axum::http::StatusCode::TOO_MANY_REQUESTS,
                    [(axum::http::header::RETRY_AFTER, RETRY_AFTER.to_string())],
                    b"429 Too Many Requests".to_vec(),
                )
                    .into_response();
            }
            let resp = reqwest::Client::new()
                .post(up.clone())
                .header("content-type", "application/json")
                .body(body.to_vec())
                .send()
                .await
                .unwrap();
            (
                axum::http::StatusCode::OK,
                resp.bytes().await.unwrap().to_vec(),
            )
                .into_response()
        }),
    );
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy: url::Url = format!("http://{}/", l.local_addr().unwrap())
        .parse()
        .unwrap();
    tokio::spawn(async move { axum::serve(l, app).await });

    let c = Contracts::new(&[proxy], registry, Some(&secret))
        .await
        .unwrap();
    let (_sk, pk) = keygen(&mut rand::rngs::OsRng);
    let t = std::time::Instant::now();
    let (pid, r) = c
        .create_process(&NewProcess {
            status: ProcessStatus::Ready,
            start_time: 0,
            duration: 3600,
            max_voters: 10,
            ballot_mode: BallotMode {
                num_fields: NF,
                group_size: 1,
                unique_values: false,
                cost_exponent: 1,
                max_value: 5,
                min_value: 0,
                max_value_sum: 20,
                min_value_sum: 0,
            },
            census: OnchainCensus {
                origin: 1,
                root: [1u8; 32],
                uri: "file:///tmp/census.json".into(),
                contract_address: [0u8; 20],
            },
            metadata: "ipfs://meta".into(),
            metadata_hash: metadata_hash(META_DOC),
            enc_key: pk,
        })
        .await
        .unwrap();
    assert!(
        t.elapsed() >= Duration::from_secs(RETRY_AFTER),
        "{:?}",
        t.elapsed()
    );
    // The refused read and one after the rest; none reached the RPC during it.
    assert_eq!(reads.load(SeqCst), 2);
    assert_eq!(r.replacements, 0);
    assert_eq!(c.process(&pid).await.unwrap().max_voters, 10);
}

alloy::sol! {
    #[sol(rpc)]
    interface IMockDKG {
        function newEpoch(bool live, uint256[2][] memory keys) external returns (bytes12 eid);
        function setPlaintext(bytes12 eid, bytes32 aid, uint16 index, uint256 plaintext) external;
        function ctCount(bytes12 eid, bytes32 aid) external view returns (uint16);
    }
}

// DKG_AUTOMATIC on the real registry with the MockDKG: the organizer creates
// through the client, the node settles a transition built by davinci-state,
// then requests the decryption from its own tree and finalizes once the
// (mock) committee combined.
#[tokio::test(flavor = "multi_thread")]
async fn dkg_results_on_anvil() {
    use alloy::primitives::U256 as AU256;
    use davinci_client::organizer::{KeyMode as OrgKeyMode, NewProcess as OrgProcess, Organizer};
    use davinci_sequencer::web3::KeyMode;
    use davinci_state::{CensusOrigin, ProcessConfig, ProcessState};
    use davinci_zkvm_sdk::crypto::babyjubjub::Point;
    use davinci_zkvm_sdk::crypto::field::u256_to_be;
    use davinci_zkvm_sdk::dkg::point_to_rte;

    if !enabled() {
        eprintln!("skipped: set ANVIL=1");
        return;
    }
    let anvil = Anvil::at(anvil_bin())
        .args(["--hardfork", "osaka"])
        .try_spawn()
        .expect("spawn anvil");
    let url = anvil.endpoint_url();
    let key: PrivateKeySigner = anvil.keys()[0].clone().into();
    let secret = SecretString::new(hex::encode(key.to_bytes()));
    let deployer = ProviderBuilder::new()
        .wallet(EthereumWallet::from(key))
        .connect_http(url.clone());
    // The DKG admin sends from its own account: the node's sender tracks
    // account 0's nonces itself.
    let admin: PrivateKeySigner = anvil.keys()[2].clone().into();
    let admin = ProviderBuilder::new()
        .wallet(EthereumWallet::from(admin))
        .connect_http(url.clone());

    let verifier = deploy(
        &deployer,
        bytecode("MockZiskVerifier.sol/MockZiskVerifier.json"),
    )
    .await;
    let mock = deploy(&admin, bytecode("MockDKG.sol/MockDKG.json")).await;
    let mut code = bytecode("ProcessRegistry.sol/ProcessRegistry.json");
    code.extend(
        (
            anvil.chain_id() as u32,
            verifier,
            B256::from(BATCH_VK),
            B256::from(RESULTS_VK),
            B256::from(ROOT_C),
            // davinci-state insists on the embedded ballot VK.
            B256::from(common::vk_hash()),
            mock,
        )
            .abi_encode_params(),
    );
    code.extend(ctor_tail(Address::ZERO));
    let registry = deploy(&deployer, code).await;
    let c = Contracts::new(std::slice::from_ref(&url), registry, Some(&secret))
        .await
        .unwrap();
    let observer = Contracts::new(std::slice::from_ref(&url), registry, None)
        .await
        .unwrap();
    let dkg = IMockDKG::new(mock, &admin);
    assert_ne!(c.dkg_adapter().await.unwrap(), Address::ZERO);
    // Two pool keys k·G in the DKG's reduced form; P_0 = 1000003·B8 in TE.
    let pool: Vec<[AU256; 2]> = [1_000_003u64, 1_000_004]
        .iter()
        .map(|k| {
            let (x, y) = point_to_rte(&Point::generator().mul(&U256::from(*k)));
            [AU256::from_be_bytes(x), AU256::from_be_bytes(y)]
        })
        .collect();
    let eid = dkg.newEpoch(true, pool.clone()).call().await.unwrap();
    dkg.newEpoch(true, pool)
        .send()
        .await
        .unwrap()
        .get_receipt()
        .await
        .unwrap();

    // The organizer (account 1) creates a DKG_AUTOMATIC process.
    let org = Organizer::connect(url.as_str(), anvil.keys()[1].clone().into(), registry).unwrap();
    let env = common::env(NF, 8, None);
    let census_root = env.imt.root();
    let mode = common::ballot_mode(NF);
    let params = |pid| OrgProcess {
        process_id: pid,
        start_time: 0,
        duration: 3600,
        max_voters: 100,
        ballot_mode: mode,
        census_origin: 1,
        census_root,
        census_contract: [0; 20],
        census_uri: "file:///tmp/census.json".into(),
        metadata: "ipfs://meta".into(),
        metadata_hash: metadata_hash(META_DOC),
        key_mode: OrgKeyMode::DkgAutomatic,
    };
    let pid = org
        .create_process(&params(org.next_process_id().await.unwrap()))
        .await
        .unwrap()
        .pid;
    let p = c.process(&pid).await.unwrap();
    assert_eq!(p.key_mode, KeyMode::DkgAutomatic);
    assert_eq!(p.dkg.epoch_id, eid.0);
    assert_ne!(p.dkg.aid, [0u8; 32]);
    assert!(!p.dkg.requested);
    // The registry's RTE -> TE map agrees with the SDK's.
    let pk = Point::generator().mul(&U256::from(1_000_003u64));
    assert_eq!(p.enc_key, pk);

    // The node's tree: genesis matches, then one real transition settles.
    let cfg = ProcessConfig {
        process_id: pid_fr(&pid),
        ballot_mode: mode,
        enc_key: p.enc_key,
        census_origin: CensusOrigin::MerkleStatic,
        census_root,
        ballot_vk_hash: common::vk_hash(),
    };
    let env = common::Env {
        cfg: cfg.clone(),
        imt: env.imt,
    };
    let mut st = ProcessState::create(cfg, arbo::MemoryStorage::new()).unwrap();
    assert_eq!(st.committed().root, p.state_root, "genesis root");
    let votes: Vec<_> = (0..3)
        .map(|i| common::fake_vote(&env, i, &[1, 2, 0, 3], 70 + i as u64))
        .collect();
    let b = st
        .prepare(&votes, &mut rand::rngs::OsRng, &Default::default())
        .unwrap();
    let snark = batch_snark(
        &Batch {
            before: p.state_root,
            after: b.new_root,
            voters: 3,
            overwrites: 0,
            occupied_before: 0,
        },
        &census_root,
        &b.blobs,
    );
    c.submit_transition(&pid, &snark, &b.blobs).await.unwrap();
    st.commit(&b).unwrap();
    assert_eq!(c.process(&pid).await.unwrap().state_root, b.new_root);

    let (acc, sibs) = st.dkg_results_inputs().unwrap();
    let acc = acc.map(|x| u256_to_be(&x));
    let name = |e: Web3Error| match e {
        Web3Error::Revert(r) => r.name().unwrap_or_default().to_string(),
        e => panic!("want a revert, got {e}"),
    };
    // Still open.
    let e = c
        .request_results_decryption(&pid, &acc, &sibs)
        .await
        .unwrap_err();
    assert_eq!(name(e), "InvalidTimeBounds");
    // READY past its end, not ENDED: the request itself moves it to ENDED.
    let rpc = ProviderBuilder::new().connect_http(url.clone());
    let _: serde_json::Value = rpc
        .raw_request("evm_increaseTime".into(), (3601u64,))
        .await
        .unwrap();
    let _: serde_json::Value = rpc.raw_request("evm_mine".into(), ()).await.unwrap();
    assert_eq!(c.process(&pid).await.unwrap().status, ProcessStatus::Ready);
    // Past the end, inside the grace window.
    let e = c
        .request_results_decryption(&pid, &acc, &sibs)
        .await
        .unwrap_err();
    assert_eq!(name(e), "GraceOpen");
    advance(&rpc, 60).await;
    // Observers never send.
    let e = observer
        .request_results_decryption(&pid, &acc, &sibs)
        .await
        .unwrap_err();
    assert!(matches!(e, Web3Error::NoSigner), "{e}");
    // The inclusion proof binds every coordinate and sibling to the root.
    let mut bad = acc;
    bad[5][31] ^= 1;
    let e = c
        .request_results_decryption(&pid, &bad, &sibs)
        .await
        .unwrap_err();
    assert_eq!(name(e), "InvalidInclusionProof");
    let d = sibs.iter().rposition(|s| *s != [0u8; 32]).unwrap();
    let mut bad = sibs.clone();
    bad[d][0] ^= 1;
    let e = c
        .request_results_decryption(&pid, &acc, &bad)
        .await
        .unwrap_err();
    assert_eq!(name(e), "InvalidInclusionProof");
    // Unpadded: the leaf would sit on the last slot.
    let e = c
        .request_results_decryption(&pid, &acc, &sibs[..d + 1])
        .await
        .unwrap_err();
    assert_eq!(name(e), "InvalidInclusionProof");
    let mut bad = acc;
    bad[0] = [0xff; 32];
    let e = c
        .request_results_decryption(&pid, &bad, &sibs)
        .await
        .unwrap_err();
    assert_eq!(name(e), "InvalidAccumulator");
    assert!(!c.dkg_results_ready(&pid).await.unwrap());

    let r = c
        .request_results_decryption(&pid, &acc, &sibs)
        .await
        .unwrap();
    let evs = c.events(r.block, r.block).await.unwrap();
    let p = c.process(&pid).await.unwrap();
    // READY -> ENDED first, then the request, in the same tx.
    let kinds: Vec<_> = evs
        .iter()
        .filter(|e| e.tx_hash == r.tx_hash)
        .map(|e| e.kind.clone())
        .collect();
    assert_eq!(
        kinds,
        [
            EventKind::StatusChanged {
                pid,
                old: ProcessStatus::Ready,
                new: ProcessStatus::Ended,
            },
            EventKind::ResultsDecryptionRequested {
                pid,
                epoch_id: eid.0,
                aid: p.dkg.aid,
                first_index: 1,
                count: NF,
            },
        ]
    );
    assert_eq!(p.status, ProcessStatus::Ended);
    assert!(p.dkg.requested);
    assert_eq!((p.dkg.first_index, p.dkg.count), (1, NF));
    // Every active field reached the DKG, on the reduced curve.
    let n = dkg
        .ctCount(eid, FixedBytes(p.dkg.aid))
        .call()
        .await
        .unwrap();
    assert_eq!(n, NF as u16);
    let e = c
        .request_results_decryption(&pid, &acc, &sibs)
        .await
        .unwrap_err();
    assert_eq!(name(e), "ResultsAlreadyRequested");

    // Not combined yet.
    assert!(!c.dkg_results_ready(&pid).await.unwrap());
    let e = c.finalize_results_from_dkg(&pid).await.unwrap_err();
    assert_eq!(name(e), "ResultsNotReady");
    let tally = [3u64, 6, 0, 9];
    for (i, v) in tally.iter().enumerate() {
        assert!(!c.dkg_results_ready(&pid).await.unwrap());
        // The wait log's count, one adapter read per ciphertext.
        assert_eq!(
            c.dkg_combined(&pid).await.unwrap(),
            Some((i as u16, u16::from(NF)))
        );
        dkg.setPlaintext(eid, FixedBytes(p.dkg.aid), 1 + i as u16, AU256::from(*v))
            .send()
            .await
            .unwrap()
            .get_receipt()
            .await
            .unwrap();
    }
    assert!(c.dkg_results_ready(&pid).await.unwrap());
    assert_eq!(
        c.dkg_combined(&pid).await.unwrap(),
        Some((u16::from(NF), u16::from(NF)))
    );
    let r = c.finalize_results_from_dkg(&pid).await.unwrap();
    let want = tally.to_vec();
    let evs = c.events(r.block, r.block).await.unwrap();
    assert!(evs.iter().any(|e| e.kind
        == EventKind::ResultsSet {
            pid,
            sender: c.signer().unwrap(),
            results: want.clone(),
        }));
    let p = c.process(&pid).await.unwrap();
    assert_eq!(p.status, ProcessStatus::Results);
    assert_eq!(p.results, want);
    let e = c.finalize_results_from_dkg(&pid).await.unwrap_err();
    assert_eq!(name(e), "InvalidStatus");

    // A zero-vote process: the genesis tree's proof, and the request
    // finalizes to zeros by itself.
    let pid2 = org
        .create_process(&params(org.next_process_id().await.unwrap()))
        .await
        .unwrap()
        .pid;
    let p2 = c.process(&pid2).await.unwrap();
    let st2 = ProcessState::create(
        ProcessConfig {
            process_id: pid_fr(&pid2),
            enc_key: p2.enc_key,
            ..env.cfg.clone()
        },
        arbo::MemoryStorage::new(),
    )
    .unwrap();
    assert_eq!(st2.committed().root, p2.state_root);
    org.end_process(&pid2).await.unwrap();
    advance(&rpc, u64::from(GRACE_ARGS.0)).await;
    let (acc2, sibs2) = st2.dkg_results_inputs().unwrap();
    c.request_results_decryption(&pid2, &acc2.map(|x| u256_to_be(&x)), &sibs2)
        .await
        .unwrap();
    let p2 = c.process(&pid2).await.unwrap();
    assert_eq!(p2.status, ProcessStatus::Results);
    assert_eq!(p2.results, vec![0u64; p2.ballot_mode.num_fields as usize]);
    assert_eq!(p2.dkg.count, 0);
}

alloy::sol! {
    #[sol(rpc)]
    interface IMockCouncil {
        function newCeremony(bytes12 cid, uint256 pkX, uint256 pkY) external;
        function allowAdapter(bytes12 cid, address adapter) external;
        function authorizeCreator(bytes12 cid, address creator) external;
        function setPlaintext(bytes32 requestId, uint8 field, uint64 value) external;
        function setDecryptionPolicy(bytes12 cid, uint8 mode, uint64 openAt, uint64 fallbackAt)
            external;
        function openDecryption(bytes12 cid) external;
        function isDecryptionOpen(bytes12 cid) external view returns (bool);
        function requestIdFor(bytes12 cid, address adapter, bytes31 processId)
            external view returns (bytes32);
        function getRequestMeta(bytes32 requestId) external view returns (
            bytes12 cid, uint8 fieldCount, uint16 completedBitmap, uint16 partialBitmap
        );
        function requestCts(bytes32 requestId) external view returns (uint256[4][] memory cts);
        error DecryptionNotOpen();
    }
}

// COUNCIL on the real registry with the contracts' MockCouncilManager: the
// organizer binds a process to a ceremony through the client, the node
// settles a transition, requests the decryption through the Council adapter
// (one request, TE words, first index 0) and finalizes once every field is
// combined; readiness comes from the Council adapter, never the DKG one.
#[tokio::test(flavor = "multi_thread")]
async fn council_results_on_anvil() {
    use alloy::primitives::U256 as AU256;
    use davinci_client::organizer::{KeyMode as OrgKeyMode, NewProcess as OrgProcess, Organizer};
    use davinci_sequencer::web3::KeyMode;
    use davinci_state::{CensusOrigin, ProcessConfig, ProcessState};
    use davinci_zkvm_sdk::crypto::babyjubjub::Point;
    use davinci_zkvm_sdk::crypto::field::{fr_to_be, u256_to_be};

    if !enabled() {
        eprintln!("skipped: set ANVIL=1");
        return;
    }
    let anvil = Anvil::at(anvil_bin())
        .args(["--hardfork", "osaka"])
        .try_spawn()
        .expect("spawn anvil");
    let url = anvil.endpoint_url();
    let key: PrivateKeySigner = anvil.keys()[0].clone().into();
    let secret = SecretString::new(hex::encode(key.to_bytes()));
    let deployer = ProviderBuilder::new()
        .wallet(EthereumWallet::from(key))
        .connect_http(url.clone());
    let admin: PrivateKeySigner = anvil.keys()[2].clone().into();
    let admin = ProviderBuilder::new()
        .wallet(EthereumWallet::from(admin))
        .connect_http(url.clone());

    let verifier = deploy(
        &deployer,
        bytecode("MockZiskVerifier.sol/MockZiskVerifier.json"),
    )
    .await;
    let mock = deploy(
        &admin,
        bytecode("MockCouncilManager.sol/MockCouncilManager.json"),
    )
    .await;
    let mut code = bytecode("ProcessRegistry.sol/ProcessRegistry.json");
    code.extend(
        (
            anvil.chain_id() as u32,
            verifier,
            B256::from(BATCH_VK),
            B256::from(RESULTS_VK),
            B256::from(ROOT_C),
            B256::from(common::vk_hash()),
            Address::ZERO,
        )
            .abi_encode_params(),
    );
    code.extend(ctor_tail(mock));
    let registry = deploy(&deployer, code).await;
    let c = Contracts::new(std::slice::from_ref(&url), registry, Some(&secret))
        .await
        .unwrap();
    let adapter = c.council_adapter().await.unwrap();
    assert!(c.dkg_adapter().await.is_err(), "no DKG manager");

    // A Live ceremony under k·B8 (TE, as Council keeps it) that allows the
    // adapter and authorizes the organizer, account 1.
    let council = IMockCouncil::new(mock, &admin);
    let cid = FixedBytes([0xc1; 12]);
    let pk = Point::generator().mul(&U256::from(4_242_424u64));
    let (px, py) = (
        AU256::from_be_bytes(fr_to_be(&pk.x)),
        AU256::from_be_bytes(fr_to_be(&pk.y)),
    );
    let organizer: PrivateKeySigner = anvil.keys()[1].clone().into();
    for call in [
        council.newCeremony(cid, px, py).into_transaction_request(),
        council
            .allowAdapter(cid, adapter)
            .into_transaction_request(),
        council
            .authorizeCreator(cid, organizer.address())
            .into_transaction_request(),
    ] {
        admin
            .send_transaction(call)
            .await
            .unwrap()
            .get_receipt()
            .await
            .unwrap();
    }

    let env = common::env(NF, 8, None);
    let census_root = env.imt.root();
    let mode = common::ballot_mode(NF);
    let params = |pid| OrgProcess {
        process_id: pid,
        start_time: 0,
        duration: 3600,
        max_voters: 100,
        ballot_mode: mode,
        census_origin: 1,
        census_root,
        census_contract: [0; 20],
        census_uri: "file:///tmp/census.json".into(),
        metadata: "ipfs://meta".into(),
        metadata_hash: metadata_hash(META_DOC),
        key_mode: OrgKeyMode::Council(cid.0),
    };
    // A creator the ceremony does not authorize: refused by name.
    let stranger =
        Organizer::connect(url.as_str(), anvil.keys()[3].clone().into(), registry).unwrap();
    let e = stranger
        .create_process(&params(stranger.next_process_id().await.unwrap()))
        .await
        .unwrap_err();
    assert!(
        matches!(&e, davinci_client::Error::Reverted(n) if n == "NotAuthorizedCreator"),
        "{e}"
    );
    let org = Organizer::connect(url.as_str(), organizer, registry).unwrap();
    let pid = org
        .create_process(&params(org.next_process_id().await.unwrap()))
        .await
        .unwrap()
        .pid;
    let rid = council
        .requestIdFor(cid, adapter, FixedBytes(pid))
        .call()
        .await
        .unwrap();
    let p = c.process(&pid).await.unwrap();
    assert_eq!(p.key_mode, KeyMode::Council);
    assert_eq!(p.dkg.epoch_id, cid.0, "dkgEpochId is the ceremony id");
    assert_eq!(p.dkg.aid, rid.0, "dkgAid is the request id");
    assert!(!p.dkg.requested);
    assert_eq!(p.enc_key, pk, "the ceremony key, unconverted");
    let read = davinci_client::organizer::RegistryReader::connect(url.as_str(), registry)
        .unwrap()
        .process(&pid)
        .await
        .unwrap();
    let dkg = read.dkg.expect("a committee-keyed process");
    assert!(dkg.council && !dkg.locked);

    // The node's tree: genesis matches, then one real transition settles.
    let cfg = ProcessConfig {
        process_id: pid_fr(&pid),
        ballot_mode: mode,
        enc_key: p.enc_key,
        census_origin: CensusOrigin::MerkleStatic,
        census_root,
        ballot_vk_hash: common::vk_hash(),
    };
    let env = common::Env {
        cfg: cfg.clone(),
        imt: env.imt,
    };
    let mut st = ProcessState::create(cfg, arbo::MemoryStorage::new()).unwrap();
    assert_eq!(st.committed().root, p.state_root, "genesis root");
    let votes: Vec<_> = (0..3)
        .map(|i| common::fake_vote(&env, i, &[1, 2, 0, 3], 70 + i as u64))
        .collect();
    let b = st
        .prepare(&votes, &mut rand::rngs::OsRng, &Default::default())
        .unwrap();
    let snark = batch_snark(
        &Batch {
            before: p.state_root,
            after: b.new_root,
            voters: 3,
            overwrites: 0,
            occupied_before: 0,
        },
        &census_root,
        &b.blobs,
    );
    c.submit_transition(&pid, &snark, &b.blobs).await.unwrap();
    st.commit(&b).unwrap();

    let (acc, sibs) = st.dkg_results_inputs().unwrap();
    let acc = acc.map(|x| u256_to_be(&x));
    let rpc = ProviderBuilder::new().connect_http(url.clone());
    advance(&rpc, 3601).await;
    advance(&rpc, 60).await;
    assert!(!c.dkg_results_ready(&pid).await.unwrap());
    let r = c
        .request_results_decryption(&pid, &acc, &sibs)
        .await
        .unwrap();
    let evs = c.events(r.block, r.block).await.unwrap();
    assert!(evs.iter().any(|e| e.kind
        == EventKind::ResultsDecryptionRequested {
            pid,
            epoch_id: cid.0,
            aid: rid.0,
            first_index: 0,
            count: NF,
        }));
    let p = c.process(&pid).await.unwrap();
    assert_eq!(p.status, ProcessStatus::Ended);
    assert_eq!((p.dkg.first_index, p.dkg.count), (0, NF));
    // One request with every active field, in TE as the accumulator holds it.
    let req = council.getRequestMeta(rid).call().await.unwrap();
    assert_eq!((req.cid, req.fieldCount), (cid, NF));
    let cts = council.requestCts(rid).call().await.unwrap();
    assert_eq!(cts.len(), NF as usize);
    for (k, ct) in cts.iter().enumerate() {
        for w in 0..4 {
            assert_eq!(
                ct[w],
                AU256::from_be_bytes(acc[4 * k + w]),
                "field {k} word {w}"
            );
        }
    }

    let name = |e: Web3Error| match e {
        Web3Error::Revert(r) => r.name().unwrap_or_default().to_string(),
        e => panic!("want a revert, got {e}"),
    };
    let e = c.finalize_results_from_dkg(&pid).await.unwrap_err();
    assert_eq!(name(e), "ResultsNotReady");
    let tally = [3u64, 6, 0, 9];
    for (i, v) in tally.iter().enumerate() {
        assert!(!c.dkg_results_ready(&pid).await.unwrap());
        let call = council
            .setPlaintext(rid, i as u8, *v)
            .into_transaction_request();
        admin
            .send_transaction(call)
            .await
            .unwrap()
            .get_receipt()
            .await
            .unwrap();
    }
    assert!(c.dkg_results_ready(&pid).await.unwrap());
    c.finalize_results_from_dkg(&pid).await.unwrap();
    let p = c.process(&pid).await.unwrap();
    assert_eq!(p.status, ProcessStatus::Results);
    assert_eq!(p.results, tally.to_vec());
}

// The Council decryption gate on the real registry: with the ceremony's
// decryption closed (Manual, no fallback), a zero-vote process and a voted
// one are both requested and ended, but nothing publishes: readiness is
// false, the manager refuses combines and the registry refuses the finalize
// with `DecryptionNotOpen`, all-zero results included. Once the organizer
// opens decryption the zeros publish at once and the tally after its
// combines.
#[tokio::test(flavor = "multi_thread")]
async fn council_gate_on_anvil() {
    use alloy::primitives::U256 as AU256;
    use davinci_client::organizer::{KeyMode as OrgKeyMode, NewProcess as OrgProcess, Organizer};
    use davinci_state::{CensusOrigin, ProcessConfig, ProcessState};
    use davinci_zkvm_sdk::crypto::babyjubjub::Point;
    use davinci_zkvm_sdk::crypto::field::{fr_to_be, u256_to_be};

    if !enabled() {
        eprintln!("skipped: set ANVIL=1");
        return;
    }
    let anvil = Anvil::at(anvil_bin())
        .args(["--hardfork", "osaka"])
        .try_spawn()
        .expect("spawn anvil");
    let url = anvil.endpoint_url();
    let key: PrivateKeySigner = anvil.keys()[0].clone().into();
    let secret = SecretString::new(hex::encode(key.to_bytes()));
    let deployer = ProviderBuilder::new()
        .wallet(EthereumWallet::from(key))
        .connect_http(url.clone());
    let admin: PrivateKeySigner = anvil.keys()[2].clone().into();
    let admin = ProviderBuilder::new()
        .wallet(EthereumWallet::from(admin))
        .connect_http(url.clone());
    let verifier = deploy(
        &deployer,
        bytecode("MockZiskVerifier.sol/MockZiskVerifier.json"),
    )
    .await;
    let mock = deploy(
        &admin,
        bytecode("MockCouncilManager.sol/MockCouncilManager.json"),
    )
    .await;
    let mut code = bytecode("ProcessRegistry.sol/ProcessRegistry.json");
    code.extend(
        (
            anvil.chain_id() as u32,
            verifier,
            B256::from(BATCH_VK),
            B256::from(RESULTS_VK),
            B256::from(ROOT_C),
            B256::from(common::vk_hash()),
            Address::ZERO,
        )
            .abi_encode_params(),
    );
    code.extend(ctor_tail(mock));
    let registry = deploy(&deployer, code).await;
    let c = Contracts::new(std::slice::from_ref(&url), registry, Some(&secret))
        .await
        .unwrap();
    let adapter = c.council_adapter().await.unwrap();

    // A Live ceremony whose decryption only its organizer opens (Manual,
    // no fallback date), not yet.
    let council = IMockCouncil::new(mock, &admin);
    let cid = FixedBytes([0xc2; 12]);
    let pk = Point::generator().mul(&U256::from(777_777u64));
    let organizer: PrivateKeySigner = anvil.keys()[1].clone().into();
    let send = async |call: TransactionRequest| {
        admin
            .send_transaction(call)
            .await
            .unwrap()
            .get_receipt()
            .await
            .unwrap()
    };
    for call in [
        council
            .newCeremony(
                cid,
                AU256::from_be_bytes(fr_to_be(&pk.x)),
                AU256::from_be_bytes(fr_to_be(&pk.y)),
            )
            .into_transaction_request(),
        council
            .allowAdapter(cid, adapter)
            .into_transaction_request(),
        council
            .authorizeCreator(cid, organizer.address())
            .into_transaction_request(),
        council
            .setDecryptionPolicy(cid, 0, 0, 0)
            .into_transaction_request(),
    ] {
        assert!(send(call).await.status());
    }
    assert!(!c.council_decryption_open(&cid.0).await.unwrap());

    let env = common::env(NF, 8, None);
    let census_root = env.imt.root();
    let mode = common::ballot_mode(NF);
    let org = Organizer::connect(url.as_str(), organizer, registry).unwrap();
    let mut pids = Vec::new();
    for _ in 0..2 {
        let p = OrgProcess {
            process_id: org.next_process_id().await.unwrap(),
            start_time: 0,
            duration: 3600,
            max_voters: 100,
            ballot_mode: mode,
            census_origin: 1,
            census_root,
            census_contract: [0; 20],
            census_uri: "file:///tmp/census.json".into(),
            metadata: "ipfs://meta".into(),
            metadata_hash: metadata_hash(META_DOC),
            key_mode: OrgKeyMode::Council(cid.0),
        };
        pids.push(org.create_process(&p).await.unwrap().pid);
    }
    let (zero, voted) = (pids[0], pids[1]);

    // The node's trees: nothing settles for `zero`, three votes for `voted`.
    let state = |pid: &[u8; 31]| {
        let cfg = ProcessConfig {
            process_id: pid_fr(pid),
            ballot_mode: mode,
            enc_key: pk,
            census_origin: CensusOrigin::MerkleStatic,
            census_root,
            ballot_vk_hash: common::vk_hash(),
        };
        (
            cfg.clone(),
            ProcessState::create(cfg, arbo::MemoryStorage::new()).unwrap(),
        )
    };
    let (_, st_zero) = state(&zero);
    let (cfg, mut st) = state(&voted);
    let p = c.process(&voted).await.unwrap();
    assert_eq!(st.committed().root, p.state_root, "genesis root");
    let env = common::Env { cfg, imt: env.imt };
    let votes: Vec<_> = (0..3)
        .map(|i| common::fake_vote(&env, i, &[1, 2, 0, 3], 90 + i as u64))
        .collect();
    let b = st
        .prepare(&votes, &mut rand::rngs::OsRng, &Default::default())
        .unwrap();
    let snark = batch_snark(
        &Batch {
            before: p.state_root,
            after: b.new_root,
            voters: 3,
            overwrites: 0,
            occupied_before: 0,
        },
        &census_root,
        &b.blobs,
    );
    c.submit_transition(&voted, &snark, &b.blobs).await.unwrap();
    st.commit(&b).unwrap();

    let rpc = ProviderBuilder::new().connect_http(url.clone());
    advance(&rpc, 3601).await;
    advance(&rpc, 60).await;
    let name = |e: Web3Error| match e {
        Web3Error::Revert(r) => r.name().unwrap_or_default().to_string(),
        e => panic!("want a revert, got {e}"),
    };

    // Both requests land with the gate closed and end the processes; the
    // all-identity one is not finalized.
    for (pid, st) in [(&zero, &st_zero), (&voted, &st)] {
        let (acc, sibs) = st.dkg_results_inputs().unwrap();
        c.request_results_decryption(pid, &acc.map(|x| u256_to_be(&x)), &sibs)
            .await
            .unwrap();
    }
    let p = c.process(&zero).await.unwrap();
    assert_eq!(p.status, ProcessStatus::Ended, "zeros wait for the gate");
    assert!(p.dkg.requested && p.dkg.count == 0 && p.results.is_empty());
    let p = c.process(&voted).await.unwrap();
    assert_eq!(p.status, ProcessStatus::Ended);
    assert_eq!(p.dkg.count, NF);
    let rid = FixedBytes(p.dkg.aid);
    let meta = council.getRequestMeta(rid).call().await.unwrap();
    assert_eq!(meta.fieldCount, NF, "admitted while closed");
    for pid in [&zero, &voted] {
        assert!(!c.dkg_results_ready(pid).await.unwrap());
        let e = c.finalize_results_from_dkg(pid).await.unwrap_err();
        assert_eq!(name(e), "DecryptionNotOpen");
    }
    // The manager refuses combines while closed.
    let Err(combine) = council.setPlaintext(rid, 0, 3).call().await else {
        panic!("a combine went through a closed gate");
    };
    assert!(
        matches!(
            combine.as_decoded_interface_error::<IMockCouncil::IMockCouncilErrors>(),
            Some(IMockCouncil::IMockCouncilErrors::DecryptionNotOpen(_))
        ),
        "{combine}"
    );

    // Months later the organizer opens decryption: the zeros publish
    // right away, the tally once the committee has combined it.
    advance(&rpc, 182 * 24 * 3600).await;
    assert!(!c.council_decryption_open(&cid.0).await.unwrap());
    assert!(
        send(council.openDecryption(cid).into_transaction_request())
            .await
            .status()
    );
    assert!(c.council_decryption_open(&cid.0).await.unwrap());
    assert!(c.dkg_results_ready(&zero).await.unwrap());
    c.finalize_results_from_dkg(&zero).await.unwrap();
    let p = c.process(&zero).await.unwrap();
    assert_eq!(p.status, ProcessStatus::Results);
    assert_eq!(p.results, vec![0u64; NF as usize]);

    assert!(!c.dkg_results_ready(&voted).await.unwrap());
    let e = c.finalize_results_from_dkg(&voted).await.unwrap_err();
    assert_eq!(name(e), "ResultsNotReady");
    let tally = [3u64, 6, 0, 9];
    for (i, v) in tally.iter().enumerate() {
        let call = council
            .setPlaintext(rid, i as u8, *v)
            .into_transaction_request();
        assert!(send(call).await.status());
    }
    assert!(c.dkg_results_ready(&voted).await.unwrap());
    c.finalize_results_from_dkg(&voted).await.unwrap();
    let p = c.process(&voted).await.unwrap();
    assert_eq!(p.status, ProcessStatus::Results);
    assert_eq!(p.results, tally.to_vec());
}

/// L1-blob mode on two anvils: Ethereum (blobs, Osaka) and an OP-like chain
/// whose registry proves blob transactions through `L1BlobVerifier`, reading
/// its origin from a `MockL1Block` the tests point at Ethereum blocks. The
/// mock cannot reproduce OP's derivation timing, real blob-pool replacement,
/// RPC body limits or PeerDAS beacons: the live phase covers those.
mod l1_blob_mode {
    use std::sync::Arc;

    use alloy::consensus::Transaction as _;
    use alloy::node_bindings::AnvilInstance;
    use alloy::providers::DynProvider;
    use davinci_sequencer::l1da::{
        Anchor, L1Da, L1Job, Posting, Settled, choose_anchor, settle, with_post,
    };
    use davinci_sequencer::storage::Db;
    use davinci_sequencer::web3::l1proof::verify_local;
    use davinci_sequencer::web3::{L1Blobs, L1Chain, L1Rpc, OnchainProcess, RegistryDa};
    use davinci_zkvm_sdk::crypto::babyjubjub::Point;
    use tokio_util::sync::CancellationToken;

    use super::*;

    alloy::sol! {
        #[sol(rpc)]
        interface MockL1Block {
            function set(uint64 n, bytes32 h) external;
        }
    }

    const CENSUS_ROOT: [u8; 32] = {
        let mut r = [0u8; 32];
        r[0] = 0x01;
        r[31] = 0x2a;
        r
    };

    struct TwoChains {
        _l1: AnvilInstance,
        _op: AnvilInstance,
        l1_url: url::Url,
        verifier: Address,
        l1_block: Address,
        me: Address,
        c: Contracts,
        l1c: Arc<L1Rpc>,
        /// OP key 2: moves the mock origin without racing `c`'s nonces.
        relay: DynProvider,
        l1_read: DynProvider,
        db: Db,
        _dir: tempfile::TempDir,
    }

    async fn two_chains() -> TwoChains {
        let l1 = Anvil::at(anvil_bin())
            .args(["--hardfork", "osaka"])
            .try_spawn()
            .expect("spawn the Ethereum anvil");
        let op = Anvil::at(anvil_bin())
            .args(["--hardfork", "osaka"])
            .try_spawn()
            .expect("spawn the OP anvil");
        let (l1_url, op_url) = (l1.endpoint_url(), op.endpoint_url());
        let key: PrivateKeySigner = op.keys()[0].clone().into();
        let me = key.address();
        let secret = SecretString::new(hex::encode(key.to_bytes()));
        let deployer = ProviderBuilder::new()
            .wallet(EthereumWallet::from(key))
            .connect_http(op_url.clone());
        let relay_key: PrivateKeySigner = op.keys()[2].clone().into();
        let relay = ProviderBuilder::new()
            .wallet(EthereumWallet::from(relay_key))
            .connect_http(op_url.clone())
            .erased();
        // The dedicated Ethereum key: L1 anvil key 1.
        let l1_key: PrivateKeySigner = l1.keys()[1].clone().into();
        let l1_secret = SecretString::new(hex::encode(l1_key.to_bytes()));
        let l1_read = ProviderBuilder::new().connect_http(l1_url.clone()).erased();

        let zisk = deploy(
            &deployer,
            bytecode("MockZiskVerifier.sol/MockZiskVerifier.json"),
        )
        .await;
        let l1_block = deploy(&deployer, bytecode("MockL1Block.sol/MockL1Block.json")).await;
        let mut code = bytecode("L1BlobVerifier.sol/L1BlobVerifier.json");
        code.extend(l1_block.abi_encode());
        let verifier = deploy(&deployer, code).await;
        let mut code = bytecode("ProcessRegistry.sol/ProcessRegistry.json");
        code.extend(
            (
                op.chain_id() as u32,
                zisk,
                B256::from(BATCH_VK),
                B256::from(RESULTS_VK),
                B256::from(ROOT_C),
                B256::from(BALLOT_VK_HASH),
                Address::ZERO,
            )
                .abi_encode_params(),
        );
        code.extend(ctor_tail_l1(Address::ZERO, verifier));
        let registry = deploy(&deployer, code).await;

        let c = Contracts::new(std::slice::from_ref(&op_url), registry, Some(&secret))
            .await
            .unwrap();
        let l1c = Arc::new(
            L1Rpc::connect(std::slice::from_ref(&l1_url), &[], Some(&l1_secret))
                .await
                .unwrap(),
        );
        let dir = tempfile::TempDir::new().unwrap();
        let db = Db::open_in(dir.path()).unwrap();
        TwoChains {
            _l1: l1,
            _op: op,
            l1_url,
            verifier,
            l1_block,
            me,
            c,
            l1c,
            relay,
            l1_read,
            db,
            _dir: dir,
        }
    }

    impl TwoChains {
        /// Posts in chunks of `chunk` blobs.
        fn da(&self, chunk: usize) -> L1Da {
            L1Da::new(
                self.l1c.clone(),
                self.db.clone(),
                u128::MAX,
                Duration::from_millis(100),
                chunk,
            )
            .unwrap()
        }

        async fn l1_head(&self) -> u64 {
            self.l1_read.get_block_number().await.unwrap()
        }

        async fn mine_l1(&self, n: u64) {
            let _: serde_json::Value = self
                .l1_read
                .raw_request("anvil_mine".into(), (alloy::primitives::U256::from(n),))
                .await
                .unwrap();
        }

        /// Points OP's origin at Ethereum block `n`.
        async fn set_origin(&self, n: u64) {
            let h = self
                .l1_read
                .get_block_by_number(n.into())
                .await
                .unwrap()
                .unwrap()
                .header
                .hash;
            self.set_origin_raw(n, h).await;
        }

        async fn set_origin_raw(&self, n: u64, h: B256) {
            let r = MockL1Block::new(self.l1_block, &self.relay)
                .set(n, h)
                .send()
                .await
                .unwrap()
                .get_receipt()
                .await
                .unwrap();
            assert!(r.status());
        }

        /// A READY process with a fresh election key.
        async fn process(&self) -> ([u8; 31], OnchainProcess, Point) {
            let (_sk, pk) = keygen(&mut rand::rngs::OsRng);
            let np = NewProcess {
                status: ProcessStatus::Ready,
                start_time: 0,
                duration: 3600,
                max_voters: 1000,
                ballot_mode: BallotMode {
                    num_fields: NF,
                    group_size: 1,
                    unique_values: false,
                    cost_exponent: 1,
                    max_value: 5,
                    min_value: 0,
                    max_value_sum: 20,
                    min_value_sum: 0,
                },
                census: OnchainCensus {
                    origin: 1,
                    root: CENSUS_ROOT,
                    uri: "file:///tmp/census.json".into(),
                    contract_address: [0u8; 20],
                },
                metadata: "ipfs://meta".into(),
                metadata_hash: metadata_hash(META_DOC),
                enc_key: pk,
            };
            let (pid, _) = self.c.create_process(&np).await.unwrap();
            (pid, self.c.process(&pid).await.unwrap(), pk)
        }
    }

    /// A two-vote transition from `before`, its blobs and its snark.
    fn transition(
        pid: &[u8; 31],
        before: [u8; 32],
        after: [u8; 32],
        occupied_before: u32,
        pk: &Point,
        seed: u64,
    ) -> (TransitionBlobs, PlonkSnark) {
        let t = TransitionData {
            vote_ids: vec![VOTE_ID_MIN + seed, VOTE_ID_MIN + seed + 1],
            updates: vec![
                (0x100 + seed, some_ballot(pk, seed)),
                (0x101 + seed, some_ballot(pk, seed + 1)),
            ],
            num_fields: NF,
        };
        let blobs = build_blobs(&t, &pid_fr(pid), &before).unwrap();
        let snark = batch_snark(
            &Batch {
                before,
                after,
                voters: 2,
                overwrites: 0,
                occupied_before,
            },
            &CENSUS_ROOT,
            &blobs,
        );
        (blobs, snark)
    }

    /// A 500-vote transition from `before`: two blobs at NF 4.
    fn two_blob_transition(
        pid: &[u8; 31],
        before: [u8; 32],
        after: [u8; 32],
    ) -> (TransitionBlobs, PlonkSnark) {
        let n = 500u64;
        let t = TransitionData {
            vote_ids: (0..n).map(|i| VOTE_ID_MIN + 5000 + i).collect(),
            updates: (0..n)
                .map(|i| (0x400 + i, Ballot([Ciphertext::IDENTITY; 16])))
                .collect(),
            num_fields: NF,
        };
        let blobs = build_blobs(&t, &pid_fr(pid), &before).unwrap();
        assert_eq!(blobs.blobs.len(), 2);
        let snark = batch_snark(
            &Batch {
                before,
                after,
                voters: n as u32,
                overwrites: 0,
                occupied_before: 0,
            },
            &CENSUS_ROOT,
            &blobs,
        );
        (blobs, snark)
    }

    /// Ethereum produces a block every 400 ms and OP's origin follows its
    /// head, until `stop`.
    async fn relay_origin(
        l1: DynProvider,
        op: DynProvider,
        l1_block: Address,
        stop: CancellationToken,
    ) {
        let mock = MockL1Block::new(l1_block, &op);
        loop {
            tokio::select! {
                _ = stop.cancelled() => return,
                _ = tokio::time::sleep(Duration::from_millis(400)) => {}
            }
            let _: serde_json::Value = l1.raw_request("evm_mine".into(), ()).await.unwrap();
            let b = l1
                .get_block_by_number(alloy::eips::BlockNumberOrTag::Latest)
                .await
                .unwrap()
                .unwrap();
            mock.set(b.header.number, b.header.hash)
                .send()
                .await
                .unwrap()
                .get_receipt()
                .await
                .unwrap();
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn l1_settlement_on_two_anvils() {
        if !enabled() {
            eprintln!("skipped: set ANVIL=1");
            return;
        }
        let t = two_chains().await;
        assert_eq!(
            t.c.da_mode().await.unwrap(),
            RegistryDa::L1Blobs {
                verifier: t.verifier,
                l1_block: t.l1_block
            }
        );
        assert!(t.l1c.cell_proofs(), "anvil --hardfork osaka: v1 sidecars");
        let da = t.da(6);
        let (pid, p, pk) = t.process().await;
        let (blobs1, snark1) = transition(&pid, p.state_root, [0x91; 32], 0, &pk, 1);
        // Blob mode's entry point is refused on this registry.
        let r =
            t.c.simulate_transition(&pid, &snark1, &blobs1)
                .await
                .unwrap_err();
        assert_eq!(r.name(), Some("WrongDAMode"), "{r}");

        // The blobs go to Ethereum, then one more block on top.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
        let posted = da
            .poster
            .post(&pid, &p.state_root, &blobs1, deadline)
            .await
            .unwrap();
        assert!(
            t.db.l1_posts().unwrap().is_empty(),
            "a mined post leaves no record"
        );
        t.mine_l1(1).await;
        let head = t.l1_head().await;
        let proof = t.l1c.build_proof(&[posted.tx], head).await.unwrap();
        // An origin that is no header of the proof is named.
        t.set_origin_raw(head, B256::repeat_byte(7)).await;
        let r =
            t.c.simulate_transition_l1(&pid, &snark1, &blobs1, &proof)
                .await
                .unwrap_err();
        assert_eq!(r.name(), Some("L1OriginMismatch"), "{r}");
        // OP's origin at the head: the proof anchors there.
        t.set_origin(head).await;
        let origin = t.c.l1_origin().await.unwrap();
        assert_eq!(origin.number, head);
        let want: Vec<B256> = blobs1
            .versioned_hashes
            .iter()
            .map(|h| B256::from(*h))
            .collect();
        let (vhs, blocks) = verify_local(&proof, origin.hash).unwrap();
        assert_eq!(vhs, want);
        assert_eq!(blocks, vec![posted.block; want.len()]);
        t.c.simulate_transition_l1(&pid, &snark1, &blobs1, &proof)
            .await
            .unwrap();
        let r1 =
            t.c.submit_transition_l1(&pid, &snark1, &blobs1, &proof)
                .await
                .unwrap();
        // The transition and its L1 event come back as one event.
        let evs = t.c.events(r1.block, r1.block).await.unwrap();
        assert_eq!(evs.len(), 1, "{evs:?}");
        assert_eq!(
            evs[0].kind,
            EventKind::StateTransitioned {
                pid,
                sender: t.me,
                old_root: p.state_root,
                new_root: [0x91; 32],
                voters: 2,
                overwrites: 0,
                n_blobs: want.len() as u64,
                l1: Some(L1Blobs {
                    blocks: vec![posted.block; want.len()],
                    vhs: want.clone(),
                }),
            }
        );
        // A follower reads the blobs from the Ethereum anvil.
        let got = AnvilBlobs::new(std::slice::from_ref(&t.l1_url))
            .blobs_for_l1(&blocks, &want)
            .await
            .unwrap();
        assert_eq!(got, blobs1.blobs);

        // Second transition: OP's origin runs ahead; the proof anchors at the
        // origin recorded when it reached the blobs.
        let (blobs2, snark2) = transition(&pid, [0x91; 32], [0x92; 32], 2, &pk, 10);
        let posted2 = da
            .poster
            .post(&pid, &[0x91; 32], &blobs2, deadline)
            .await
            .unwrap();
        assert_eq!(posted2.nonce, posted.nonce + 1);
        t.set_origin(posted2.block).await;
        let rec = t.c.record_l1_origin().await.unwrap();
        assert_eq!(rec.number, posted2.block);
        t.mine_l1(20).await;
        let head = t.l1_head().await;
        t.set_origin(head).await;
        let origin = t.c.l1_origin().await.unwrap();
        let Anchor::At { top, hash } =
            choose_anchor(posted2.block, posted2.block, head, origin, Some(rec))
        else {
            panic!("a deep proof anchors at the record");
        };
        assert_eq!((top, hash), (rec.number, rec.hash));
        let proof2 = t.l1c.build_proof(&[posted2.tx], top).await.unwrap();
        assert_eq!(proof2.headers.len(), 1);
        assert!(
            verify_local(&proof2, origin.hash).is_err(),
            "not at the current origin"
        );
        t.c.simulate_transition_l1(&pid, &snark2, &blobs2, &proof2)
            .await
            .unwrap();
        let r2 =
            t.c.submit_transition_l1(&pid, &snark2, &blobs2, &proof2)
                .await
                .unwrap();
        let p = t.c.process(&pid).await.unwrap();
        assert_eq!((p.state_root, p.voters_count), ([0x92; 32], 4));
        let evs = t.c.events(r2.block, r2.block).await.unwrap();
        assert!(
            matches!(&evs[0].kind, EventKind::StateTransitioned { l1: Some(b), .. }
                if b.blocks[0] == posted2.block),
            "{evs:?}"
        );
    }

    // One blob per Ethereum transaction: a two-blob transition goes out in
    // two transactions, in two blocks, and settles with one L1 proof of both.
    #[tokio::test(flavor = "multi_thread")]
    async fn l1_settlement_spans_two_transactions() {
        if !enabled() {
            eprintln!("skipped: set ANVIL=1");
            return;
        }
        let t = two_chains().await;
        // Initialises the mode cell: events() pairs L1 events strictly from here on.
        t.c.da_mode().await.unwrap();
        let da = t.da(1);
        let (pid, p, _pk) = t.process().await;
        let (blobs, snark) = two_blob_transition(&pid, p.state_root, [0x93; 32]);
        let want: Vec<B256> = blobs
            .versioned_hashes
            .iter()
            .map(|h| B256::from(*h))
            .collect();
        let stop = CancellationToken::new();
        let relay = tokio::spawn(relay_origin(
            t.l1_read.clone(),
            t.relay.clone(),
            t.l1_block,
            stop.clone(),
        ));
        let job = L1Job {
            pid,
            root_before: p.state_root,
            blobs: &blobs,
            deadline: tokio::time::Instant::now() + Duration::from_secs(120),
            cancel: CancellationToken::new(),
            posted: Default::default(),
        };
        let (_, posting) = with_post(&da, &t.c, &job, std::future::ready(())).await;
        let posts = posting.posts.clone();
        assert_eq!(posts.len(), 2, "{posting:?}");
        assert_eq!(posts[1].nonce, posts[0].nonce + 1);
        assert!(posts[1].block > posts[0].block);
        let out = settle(&da, &t.c, &job, &snark, posting).await;
        stop.cancel();
        relay.await.unwrap();
        let Settled::Landed(rc) = out else {
            panic!("{out:?}");
        };
        let evs = t.c.events(rc.block, rc.block).await.unwrap();
        let EventKind::StateTransitioned {
            l1: Some(b),
            n_blobs,
            new_root,
            ..
        } = &evs[0].kind
        else {
            panic!("{evs:?}");
        };
        assert_eq!((*n_blobs, *new_root), (2, [0x93; 32]));
        assert_eq!(b.blocks, vec![posts[0].block, posts[1].block]);
        assert_eq!(b.vhs, want);
        // A follower reads them back from the two Ethereum blocks.
        let got = AnvilBlobs::new(std::slice::from_ref(&t.l1_url))
            .blobs_for_l1(&b.blocks, &b.vhs)
            .await
            .unwrap();
        assert_eq!(got, blobs.blobs);
        assert_eq!(t.c.process(&pid).await.unwrap().state_root, [0x93; 32]);
        assert!(t.db.l1_posts().unwrap().is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn l1_settle_loop_posts_again_after_an_l1_reorg() {
        if !enabled() {
            eprintln!("skipped: set ANVIL=1");
            return;
        }
        let t = two_chains().await;
        // Initialises the mode cell: events() pairs L1 events strictly from here on.
        t.c.da_mode().await.unwrap();
        let da = t.da(6);
        let (pid, p, pk) = t.process().await;
        let (blobs, snark) = transition(&pid, p.state_root, [0x91; 32], 0, &pk, 1);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
        let posted = da
            .poster
            .post(&pid, &p.state_root, &blobs, deadline)
            .await
            .unwrap();
        // A reorg of depth one drops the block holding the blob transaction.
        assert_eq!(t.l1_head().await, posted.block);
        let _: serde_json::Value = t
            .l1_read
            .raw_request(
                "anvil_reorg".into(),
                (1u64, Vec::<serde_json::Value>::new()),
            )
            .await
            .unwrap();
        assert!(t.l1c.receipt(posted.tx).await.unwrap().is_none());
        // From now on Ethereum produces blocks and OP's origin follows its head.
        let stop = CancellationToken::new();
        let relay = tokio::spawn(relay_origin(
            t.l1_read.clone(),
            t.relay.clone(),
            t.l1_block,
            stop.clone(),
        ));
        let job = L1Job {
            pid,
            root_before: p.state_root,
            blobs: &blobs,
            deadline,
            cancel: CancellationToken::new(),
            posted: Default::default(),
        };
        let posting = Posting {
            posts: vec![posted],
            ..Posting::default()
        };
        let out = settle(&da, &t.c, &job, &snark, posting).await;
        stop.cancel();
        relay.await.unwrap();
        let Settled::Landed(rc) = out else {
            panic!("{out:?}");
        };
        let evs = t.c.events(rc.block, rc.block).await.unwrap();
        let EventKind::StateTransitioned { l1: Some(b), .. } = &evs[0].kind else {
            panic!("{evs:?}");
        };
        assert!(b.blocks[0] > posted.block, "the blobs of the new chain");
        assert!(t.db.l1_posts().unwrap().is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn l1_post_replaces_a_dropped_transaction() {
        if !enabled() {
            eprintln!("skipped: set ANVIL=1");
            return;
        }
        let t = two_chains().await;
        let da = Arc::new(t.da(6));
        let (pid, p, pk) = t.process().await;
        let (blobs, _) = transition(&pid, p.state_root, [0x91; 32], 0, &pk, 1);
        let _: serde_json::Value = t
            .l1_read
            .raw_request("evm_setAutomine".into(), (false,))
            .await
            .unwrap();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
        let task = {
            let (da, blobs, root) = (da.clone(), blobs.clone(), p.state_root);
            tokio::spawn(async move { da.poster.post(&pid, &root, &blobs, deadline).await })
        };
        common::wait_until("first send recorded", async || {
            t.db.l1_posts().unwrap().len() == 1
        })
        .await;
        let first = t.db.l1_posts().unwrap().remove(0);
        // The record precedes the send: drop the transaction once it is pooled.
        common::wait_until("first tx pooled", async || {
            t.l1_read
                .get_transaction_by_hash(first.tx_hash)
                .await
                .unwrap()
                .is_some()
        })
        .await;
        let _: serde_json::Value = t
            .l1_read
            .raw_request("anvil_dropTransaction".into(), (first.tx_hash,))
            .await
            .unwrap();
        for _ in 0..3 {
            t.mine_l1(1).await;
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
        common::wait_until("replacement sent", async || {
            t.db.l1_posts()
                .unwrap()
                .first()
                .is_some_and(|r| r.tx_hash != first.tx_hash)
        })
        .await;
        t.mine_l1(1).await;
        let posted = task.await.unwrap().unwrap();
        assert_ne!(posted.tx, first.tx_hash);
        assert_eq!(posted.nonce, first.nonce);
        let tx = t
            .l1_read
            .get_transaction_by_hash(posted.tx)
            .await
            .unwrap()
            .unwrap();
        assert!(tx.inner.max_fee_per_blob_gas().unwrap() >= 2 * first.fees.blob_fee);
        assert!(
            t.l1_read
                .get_transaction_by_hash(first.tx_hash)
                .await
                .unwrap()
                .is_none()
        );
    }
}

// `finalized_block` as the monitor reads it: anvil's `finalized` (the head
// minus two 32-slot epochs), `None` from an endpoint that refuses the tag,
// and an error when the endpoint is down.
#[tokio::test(flavor = "multi_thread")]
async fn finalized_block_on_anvil() {
    use common::anvil::RpcProxy;
    use std::sync::atomic::Ordering::Relaxed;
    if !enabled() {
        eprintln!("skipped: set ANVIL=1");
        return;
    }
    let anvil = Anvil::at(anvil_bin())
        .args(["--hardfork", "osaka"])
        .try_spawn()
        .expect("spawn anvil");
    let url = anvil.endpoint_url();
    let key: PrivateKeySigner = anvil.keys()[0].clone().into();
    let deployer = ProviderBuilder::new()
        .wallet(EthereumWallet::from(key))
        .connect_http(url.clone());
    // Any contract does: only the code at the address is checked.
    let target = deploy(
        &deployer,
        bytecode("MockZiskVerifier.sol/MockZiskVerifier.json"),
    )
    .await;
    let _: serde_json::Value = deployer
        .raw_request("anvil_mine".into(), (100u64,))
        .await
        .unwrap();
    let head = deployer.get_block_number().await.unwrap();
    let proxy = RpcProxy::start(anvil.endpoint()).await;
    let purl: url::Url = proxy.url.parse().unwrap();
    let c = Contracts::open(std::slice::from_ref(&purl), target, None, false)
        .await
        .unwrap();
    assert_eq!(c.finalized_block().await.unwrap(), Some(head - 64));
    proxy.knobs.refuse_finalized.store(true, Relaxed);
    assert_eq!(c.finalized_block().await.unwrap(), None);
    // A rate limit is no refusal of the tag: an error, retried later.
    proxy.knobs.refuse_finalized.store(false, Relaxed);
    proxy.knobs.limit_finalized.store(true, Relaxed);
    assert!(c.finalized_block().await.is_err());
    proxy.knobs.limit_finalized.store(false, Relaxed);
    assert_eq!(c.finalized_block().await.unwrap(), Some(head - 64));
    proxy.knobs.down.store(true, Relaxed);
    assert!(c.finalized_block().await.is_err());
}

// A reorg deeper than the old fixed 128-block rewind, whose new branch
// creates a process right after the fork: the monitor rewinds to the
// finalized block and routes the creation, and after a restart resumes at
// the saved cursor. Anvil's `finalized` is the head minus two epochs; with
// 100-slot epochs it stays below the fork, as finality does on OP.
#[tokio::test(flavor = "multi_thread")]
async fn deep_reorg_rewinds_to_the_finalized_block() {
    use alloy::primitives::U256 as AU256;
    use davinci_sequencer::actor::{Deps, SystemClock};
    use davinci_sequencer::config::Config;
    use davinci_sequencer::monitor::spawn_node;
    use davinci_sequencer::storage::Db;
    use std::sync::Arc;
    use tokio_util::sync::CancellationToken;
    if !enabled() {
        eprintln!("skipped: set ANVIL=1");
        return;
    }
    let anvil = Anvil::at(anvil_bin())
        .args(["--hardfork", "osaka", "--slots-in-an-epoch", "100"])
        .try_spawn()
        .expect("spawn anvil");
    let url = anvil.endpoint_url();
    let key: PrivateKeySigner = anvil.keys()[0].clone().into();
    let secret = SecretString::new(hex::encode(key.to_bytes()));
    let deployer = ProviderBuilder::new()
        .wallet(EthereumWallet::from(key))
        .connect_http(url.clone());
    let verifier = deploy(
        &deployer,
        bytecode("MockZiskVerifier.sol/MockZiskVerifier.json"),
    )
    .await;
    let mut code = bytecode("ProcessRegistry.sol/ProcessRegistry.json");
    code.extend(
        (
            anvil.chain_id() as u32,
            verifier,
            B256::from(BATCH_VK),
            B256::from(RESULTS_VK),
            B256::from(ROOT_C),
            B256::from(common::vk_hash()),
            Address::ZERO,
        )
            .abi_encode_params(),
    );
    code.extend(ctor_tail(Address::ZERO));
    let registry = deploy(&deployer, code).await;
    let start_block = deployer.get_block_number().await.unwrap();
    let mine = async |n: u64| {
        let _: serde_json::Value = deployer
            .raw_request("anvil_mine".into(), (n,))
            .await
            .unwrap();
    };
    // Enough history that the finalized block sits above the start block.
    mine(250).await;
    // CSP processes: nothing to download.
    let create = async || {
        let c = Contracts::new(std::slice::from_ref(&url), registry, Some(&secret))
            .await
            .unwrap();
        let (_sk, pk) = keygen(&mut rand::rngs::OsRng);
        let mut root = [0u8; 32];
        root[12..].copy_from_slice(&[0x77; 20]);
        let np = NewProcess {
            status: ProcessStatus::Ready,
            start_time: 0,
            duration: 3600,
            max_voters: 1000,
            ballot_mode: common::ballot_mode(NF),
            census: OnchainCensus {
                origin: 4,
                root,
                uri: "https://csp.invalid".into(),
                contract_address: [0u8; 20],
            },
            metadata: "ipfs://meta".into(),
            metadata_hash: metadata_hash(META_DOC),
            enc_key: pk,
        };
        c.create_process(&np).await.unwrap().0
    };
    let datadir = tempfile::TempDir::new().unwrap();
    let census_dir = tempfile::TempDir::new().unwrap();
    let start = async |sd: &CancellationToken| {
        let sb = start_block.to_string();
        let reg = registry.to_string();
        let cfg = Config::parse_args([
            "davinci-sequencer",
            "--datadir",
            datadir.path().to_str().unwrap(),
            "--network",
            "custom",
            "--rpc-url",
            url.as_str(),
            "--registry",
            &reg,
            "--start-block",
            &sb,
            "--blob-source",
            "anvil",
            "--confirmations",
            "0",
            "--poll-interval",
            "50ms",
            "--census-dir",
            census_dir.path().to_str().unwrap(),
            "--census-sync-every",
            "0s",
        ])
        .unwrap();
        let contracts = Contracts::new(std::slice::from_ref(&url), registry, None)
            .await
            .unwrap();
        let grace = contracts.grace_params().await.unwrap();
        let mut db = None;
        for _ in 0..50 {
            match Db::open_in(datadir.path()) {
                Ok(d) => {
                    db = Some(d);
                    break;
                }
                Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
            }
        }
        let deps = Deps {
            db: db.expect("datadir lock"),
            contracts: Arc::new(contracts),
            prover: common::FakeProver::open(),
            blobs: Arc::new(AnvilBlobs::new(std::slice::from_ref(&url))),
            clock: Arc::new(SystemClock),
            grace,
            tasks: tokio_util::task::TaskTracker::new(),
            l1: None,
        };
        spawn_node(cfg, deps, sd.clone()).await.unwrap()
    };
    let served = async |node: &davinci_sequencer::monitor::Node, pid: &[u8; 31]| {
        node.processes.read().await.contains_key(&pid_fr(pid))
    };
    let cursor =
        |node: &davinci_sequencer::monitor::Node| node.db.meta_u64("monitor_last_block").unwrap();

    let a = create().await;
    let sd = CancellationToken::new();
    let node = start(&sd).await;
    common::wait_until("A served", async || served(&node, &a).await).await;
    // The old branch: 150 blocks the node routes.
    let snap: AU256 = deployer
        .raw_request("evm_snapshot".into(), ())
        .await
        .unwrap();
    let fork = deployer.get_block_number().await.unwrap();
    mine(150).await;
    let old_head = deployer.get_block_number().await.unwrap();
    common::wait_until("cursor at the old head", async || {
        cursor(&node) == Some(old_head)
    })
    .await;
    // The new branch: B right after the fork, 128 + 21 blocks below the
    // cursor, then past the old head.
    let reverted: bool = deployer
        .raw_request("evm_revert".into(), (snap,))
        .await
        .unwrap();
    assert!(reverted);
    let b = create().await;
    mine(old_head - fork).await;
    let new_head = deployer.get_block_number().await.unwrap();
    // Finalized: 49 blocks below the fork, far above the start block.
    assert!(new_head > old_head && new_head - 200 < fork && new_head - 200 > start_block);
    common::wait_until("B served", async || served(&node, &b).await).await;
    assert_eq!(
        node.metrics
            .reorgs
            .load(std::sync::atomic::Ordering::SeqCst),
        1
    );
    common::wait_until("cursor at the new head", async || {
        cursor(&node) == Some(new_head)
    })
    .await;
    sd.cancel();
    drop(node);

    // A restart resumes at the saved cursor: both served, no reorg.
    let sd = CancellationToken::new();
    let node = start(&sd).await;
    common::wait_until("A and B served", async || {
        served(&node, &a).await && served(&node, &b).await
    })
    .await;
    mine(3).await;
    common::wait_until("cursor at the head", async || {
        cursor(&node) == Some(new_head + 3)
    })
    .await;
    assert_eq!(
        node.metrics
            .reorgs
            .load(std::sync::atomic::Ordering::SeqCst),
        0
    );
    sd.cancel();
}
