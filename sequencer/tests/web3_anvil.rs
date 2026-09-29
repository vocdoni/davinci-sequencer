//! web3 layer against a real anvil (Osaka) with the zkvm ProcessRegistry,
//! the accept-all mock verifier and, for the DKG modes, the contracts'
//! `MockDKG`. Gated by `ANVIL=1`; needs `anvil` on PATH (or in
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
use alloy::sol_types::SolValue;
use davinci_client::organizer::metadata_hash;
use davinci_sequencer::config::SecretString;
use davinci_sequencer::web3::{
    AnvilBlobs, BlobSource, Contracts, EventKind, NewProcess, OnchainCensus, ProcessStatus,
    RevertReason, Web3Error,
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
/// The metadata document behind the `ipfs://` URIs, which nothing fetches.
const META_DOC: &[u8] = br#"{"title":{"default":"web3 test"}}"#;

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

    // First transition: one blob, real KZG data over real ciphertexts.
    let t1 = TransitionData {
        vote_ids: vec![VOTE_ID_MIN + 42, VOTE_ID_MIN + 7],
        updates: vec![(0x12, some_ballot(&pk, 1)), (0x11, some_ballot(&pk, 2))],
        accumulator: some_ballot(&pk, 3),
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
    let n = 460u64;
    let t2 = TransitionData {
        vote_ids: (0..n).map(|i| VOTE_ID_MIN + 1000 + i).collect(),
        updates: (0..n)
            .map(|i| (0x20 + i, Ballot([Ciphertext::IDENTITY; 16])))
            .collect(),
        accumulator: some_ballot(&pk, 4),
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
        }
    );

    // Results: end the process and settle the tally.
    c.end_process(&pid).await.unwrap();
    let values = [7u64, (1 << 32) + 5, 0, 3];
    let r = c
        .submit_results(&pid, &results_snark(&root2, &values))
        .await
        .unwrap();
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
    let registry = deploy(&deployer, code).await;
    let node = Contracts::new(std::slice::from_ref(&url), registry, None)
        .await
        .unwrap();

    // Account 1 organizes through the client.
    let org = Organizer::connect(url.as_str(), anvil.keys()[1].clone().into(), registry).unwrap();
    let (_sk, pk) = keygen(&mut rand::rngs::OsRng);
    let pid = org
        .create_process(&OrgProcess {
            process_id: org.next_process_id().await.unwrap(),
            start_time: 0,
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
            census_root: Fr::from(42u64),
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

// Two sequencers settle the same root in one block (live on Gnosis in block
// 48495639). The loser's RPC lags behind that block, so a replay at
// `latest` passes and once left the revert unnamed, erroring the votes.
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
                accumulator: some_ballot(&pk, 20 + tag as u64),
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
    use davinci_zkvm_sdk::crypto::field::{fr_to_be, u256_to_be};
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
        &fr_to_be(&census_root),
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
        dkg.setPlaintext(eid, FixedBytes(p.dkg.aid), 1 + i as u16, AU256::from(*v))
            .send()
            .await
            .unwrap()
            .get_receipt()
            .await
            .unwrap();
    }
    assert!(c.dkg_results_ready(&pid).await.unwrap());
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
    let (acc2, sibs2) = st2.dkg_results_inputs().unwrap();
    c.request_results_decryption(&pid2, &acc2.map(|x| u256_to_be(&x)), &sibs2)
        .await
        .unwrap();
    let p2 = c.process(&pid2).await.unwrap();
    assert_eq!(p2.status, ProcessStatus::Results);
    assert_eq!(p2.results, vec![0u64; p2.ballot_mode.num_fields as usize]);
    assert_eq!(p2.dkg.count, 0);
}
