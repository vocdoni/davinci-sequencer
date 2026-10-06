//! Organizer against the real ProcessRegistry on anvil. Gated by `ANVIL=1`;
//! needs the forge artifacts (`DAVINCI_CONTRACTS_DIR`, default
//! `../davinci-contracts`).

use std::path::PathBuf;

use alloy::network::{EthereumWallet, TransactionBuilder};
use alloy::node_bindings::Anvil;
use alloy::primitives::{Address, B256, Bytes, FixedBytes};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::rpc::types::{Filter, TransactionRequest};
use alloy::signers::local::PrivateKeySigner;
use alloy::sol_types::{SolEvent, SolValue};
use davinci_client::Error;
use davinci_client::api::{CensusView, ProcessId, ProcessStatus, ProcessView};
use davinci_client::organizer::{
    KeyMode, NewProcess, Organizer, OrganizerSecret, ProcessRegistry, RegistryReader,
    metadata_hash, verify_registry,
};
use davinci_zkvm_sdk::ballot::BallotMode;
use davinci_zkvm_sdk::crypto::babyjubjub::Point;
use davinci_zkvm_sdk::crypto::field::{Fr, U256};
use davinci_zkvm_sdk::groth16::BallotVerifier;
use davinci_zkvm_sdk::release;

fn contracts_dir() -> PathBuf {
    std::env::var_os("DAVINCI_CONTRACTS_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../davinci-contracts")
        })
}

fn anvil_bin() -> PathBuf {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    let p = home.join(".foundry/bin/anvil");
    if p.exists() {
        p
    } else {
        PathBuf::from("anvil")
    }
}

fn bytecode(artifact: &str) -> Vec<u8> {
    let p = contracts_dir().join("out").join(artifact);
    let j: serde_json::Value = serde_json::from_slice(&std::fs::read(&p).unwrap()).unwrap();
    let hex = j["bytecode"]["object"].as_str().unwrap();
    hex::decode(hex.trim_start_matches("0x")).unwrap()
}

/// Registry grace constructor args: default, floor, ceil, max total, notice.
const GRACE_ARGS: (u32, u32, u32, u32, u32) = (10, 2, 60, 60, 5);

/// A metadata document nobody fetches: the tests only need its hash.
const DOC: &[u8] = br#"{"title":{"default":"organizer test"}}"#;

fn registry_bytecode() -> Vec<u8> {
    bytecode("ProcessRegistry.sol/ProcessRegistry.json")
}

async fn deploy(p: &impl Provider, code: Vec<u8>) -> anyhow::Result<Address> {
    let tx = TransactionRequest::default().with_deploy_code(Bytes::from(code));
    let receipt = p.send_transaction(tx).await?.get_receipt().await?;
    Ok(receipt.contract_address.expect("deployed"))
}

/// A registry pinned to the release, except for `chain_id`, `verifier` and
/// `batch_vk`.
async fn deploy_registry(
    p: &impl Provider,
    chain_id: u32,
    verifier: Address,
    batch_vk: [u8; 32],
) -> anyhow::Result<Address> {
    let vk_hash = BallotVerifier::from_snarkjs_json(release::ballot_vk_json())?.vk_hash();
    let args = (
        chain_id,
        verifier,
        B256::from(batch_vk),
        B256::from(release::RESULTS_PROGRAM_VK),
        B256::from(release::ROOT_C_VADCOP_FINAL),
        B256::from(vk_hash),
        Address::ZERO, // _dkgManager
        Address::ZERO, // _councilManager
    )
        .abi_encode_params();
    let mut code = registry_bytecode();
    code.extend_from_slice(&args);
    code.extend_from_slice(&GRACE_ARGS.abi_encode_params());
    deploy(p, code).await
}

#[tokio::test]
async fn registry_pins() -> anyhow::Result<()> {
    if std::env::var("ANVIL").as_deref() != Ok("1") {
        eprintln!("ANVIL not set, skipping");
        return Ok(());
    }
    let anvil = Anvil::at(anvil_bin()).try_spawn()?;
    let signer: PrivateKeySigner = anvil.keys()[0].clone().into();
    let url = anvil.endpoint();
    let p = ProviderBuilder::new()
        .wallet(EthereumWallet::from(signer))
        .connect_http(url.parse()?);
    let verifier = deploy(&p, bytecode("ZiskVerifier.sol/ZiskVerifier.json")).await?;
    let vk = release::BATCH_PROGRAM_VK;

    let good = deploy_registry(&p, 31337, verifier, vk).await?;
    let info = verify_registry(&url, good).await?;
    assert_eq!(
        (info.chain_id, info.verifier, info.dkg_adapter),
        (31337, verifier, None)
    );

    let pin_of = |e: Error| match e {
        Error::Pin { field, .. } => field,
        e => panic!("not a pin error: {e}"),
    };
    let mut wrong = vk;
    wrong[31] ^= 1;
    let r = deploy_registry(&p, 31337, verifier, wrong).await?;
    let e = verify_registry(&url, r).await.unwrap_err();
    eprintln!("{e}");
    assert_eq!(pin_of(e), "batchProgramVK");
    let r = deploy_registry(&p, 100, verifier, vk).await?;
    assert_eq!(
        pin_of(verify_registry(&url, r).await.unwrap_err()),
        "chainID"
    );
    // Any other contract as the verifier: here, the good registry.
    let r = deploy_registry(&p, 31337, good, vk).await?;
    assert_eq!(
        pin_of(verify_registry(&url, r).await.unwrap_err()),
        "verifier code hash"
    );
    Ok(())
}

#[tokio::test]
async fn create_end_and_read_a_process() -> anyhow::Result<()> {
    if std::env::var("ANVIL").as_deref() != Ok("1") {
        eprintln!("ANVIL not set, skipping");
        return Ok(());
    }
    let anvil = Anvil::at(anvil_bin()).try_spawn()?;
    let signer: PrivateKeySigner = anvil.keys()[0].clone().into();
    let url = anvil.endpoint();

    // Deploy the registry. The verifier is never called here, so any
    // non-zero address passes the constructor.
    let vk_hash = BallotVerifier::from_snarkjs_json(release::ballot_vk_json())?.vk_hash();
    let args = (
        31337u32,
        Address::repeat_byte(0x11),
        B256::from(release::BATCH_PROGRAM_VK),
        B256::from(release::RESULTS_PROGRAM_VK),
        B256::from(release::ROOT_C_VADCOP_FINAL),
        B256::from(vk_hash),
        Address::ZERO, // _dkgManager
        Address::ZERO, // _councilManager
    )
        .abi_encode_params();
    let mut code = registry_bytecode();
    code.extend_from_slice(&args);
    code.extend_from_slice(&GRACE_ARGS.abi_encode_params());
    let provider = ProviderBuilder::new()
        .wallet(EthereumWallet::from(signer.clone()))
        .connect_http(url.parse()?);
    let registry = deploy(&provider, code).await?;

    let org = Organizer::connect(&url, signer, registry)?;
    let pk = Point::generator().mul(&U256::from(4242u64));
    let next = org.next_process_id().await?;
    let params = NewProcess {
        process_id: next,
        start_time: 0,
        duration: 3600,
        max_voters: 24,
        ballot_mode: BallotMode {
            num_fields: 4,
            group_size: 1,
            unique_values: false,
            cost_exponent: 1,
            max_value: 5,
            min_value: 0,
            max_value_sum: 0,
            min_value_sum: 0,
        },
        census_origin: 1,
        census_root: Fr::from(123456789u64),
        census_contract: [0; 20],
        census_uri: "file:///tmp/census.json".into(),
        metadata: "ipfs://metadata".into(),
        metadata_hash: metadata_hash(DOC),
        key_mode: KeyMode::Sequencer(pk),
    };
    let created = org.create_process(&params).await?;
    assert!(created.organizer_secret.is_none());
    let pid = created.pid;
    assert_eq!(pid, next);
    let p = org.process(&pid).await?;
    assert_eq!(p.id, pid);
    assert_eq!(p.status, ProcessStatus::Ready);
    assert_eq!(p.encryption_key, pk);
    assert_eq!(p.ballot_mode, params.ballot_mode);
    assert_eq!(p.census_root, params.census_root);
    assert_eq!(p.census_origin, 1);
    assert_eq!(p.max_voters, 24);
    assert_eq!(p.metadata_uri, "ipfs://metadata");
    assert_eq!(p.metadata_hash, metadata_hash(DOC));
    assert_ne!(p.state_root, [0u8; 32], "genesis root computed on-chain");
    assert_eq!(org.results(&pid).await?, None);

    // A voter reads the same parameters without any key.
    let reader = RegistryReader::connect(&url, registry)?;
    let chain = reader.process(&pid).await?;
    assert_eq!(chain, p);
    assert!(matches!(
        reader.process(&[0x77; 31]).await,
        Err(Error::Chain(_))
    ));

    // A sequencer's view is checked against it.
    let honest = ProcessView {
        id: ProcessId(pid),
        status: chain.status,
        is_accepting_votes: true,
        organization_id: chain.organization_id,
        encryption_key: chain.encryption_key,
        ballot_mode: chain.ballot_mode,
        census: CensusView {
            census_origin: chain.census_origin,
            census_root: chain.census_root,
            census_uri: chain.census_uri.clone(),
        },
        state_root: chain.state_root,
        local_state_root: None,
        synced: false,
        pending_votes: None,
        next_seal_not_before: None,
        voters_count: 0,
        overwritten_votes_count: 0,
        max_voters: chain.max_voters,
        start_time: chain.start_time,
        duration: chain.duration,
        result: None,
        ignored: false,
        note: None,
    };
    honest.check_against(&chain)?;
    let mut lying = honest.clone();
    lying.encryption_key = Point::generator().mul(&U256::from(4243u64));
    assert!(matches!(lying.check_against(&chain), Err(Error::Decode(_))));

    // No DKG manager: the DKG modes are refused before sending.
    let mut dkg = params.clone();
    dkg.process_id = org.next_process_id().await?;
    dkg.key_mode = KeyMode::DkgAutomatic;
    let e = org.create_process(&dkg).await.unwrap_err();
    assert!(matches!(e, Error::DkgDisabled), "{e}");

    // The key was for the first id: reusing it is refused before sending.
    let e = org.create_process(&params).await.unwrap_err();
    assert!(matches!(e, Error::Invalid(_)), "{e}");
    // A second process gets a different id.
    let mut params2 = params.clone();
    params2.process_id = org.next_process_id().await?;
    let pid2 = org.create_process(&params2).await?.pid;
    assert_ne!(pid, pid2);
    assert_eq!(pid2, params2.process_id);

    org.end_process(&pid).await?;
    assert_eq!(org.process(&pid).await?.status, ProcessStatus::Ended);
    assert_eq!(org.results(&pid).await?, None);
    // Ending twice is a revert, surfaced by name.
    let e = org.end_process(&pid).await.unwrap_err().to_string();
    assert!(e.contains("InvalidStatus"), "{e}");

    // Bad parameters revert before a transaction is mined.
    let mut bad = params.clone();
    bad.process_id = org.next_process_id().await?;
    bad.ballot_mode.num_fields = 17;
    bad.ballot_mode.group_size = 1;
    let e = org.create_process(&bad).await.unwrap_err().to_string();
    assert!(e.contains("reverted"), "{e}");
    eprintln!("numFields 17: {e}");

    // Only origin 3 may name a census contract; origin 3 needs a live one.
    let mut bad = params.clone();
    bad.process_id = org.next_process_id().await?;
    bad.census_contract = [0x22; 20];
    let e = org.create_process(&bad).await.unwrap_err();
    assert!(
        matches!(&e, Error::Reverted(n) if n == "InvalidCensusAddress"),
        "{e}"
    );
    bad.census_origin = 3;
    let e = org.create_process(&bad).await.unwrap_err();
    assert!(
        matches!(&e, Error::Reverted(n) if n == "InvalidCensusAddress"),
        "{e}"
    );

    // An origin-1 census is fixed.
    let e = org
        .set_process_census(&pid2, Fr::from(7u64), "file:///tmp/c2.jsonl")
        .await
        .unwrap_err();
    assert!(
        matches!(&e, Error::Reverted(n) if n == "CensusNotUpdatable"),
        "{e}"
    );

    // Origin 2: the organizer, and only the organizer, replaces the census.
    let mut dynamic = params.clone();
    dynamic.process_id = org.next_process_id().await?;
    dynamic.census_origin = 2;
    let pid3 = org.create_process(&dynamic).await?.pid;
    let other = Organizer::connect(&url, anvil.keys()[1].clone().into(), registry)?;
    let e = other
        .set_process_census(&pid3, Fr::from(7u64), "file:///tmp/c2.jsonl")
        .await
        .unwrap_err();
    assert!(
        matches!(&e, Error::Reverted(n) if n == "Unauthorized"),
        "{e}"
    );
    org.set_process_census(&pid3, Fr::from(7u64), "file:///tmp/c2.jsonl")
        .await?;
    let p3 = reader.process(&pid3).await?;
    assert_eq!(p3.census_origin, 2);
    assert_eq!(p3.census_root, Fr::from(7u64));
    assert_eq!(p3.census_uri, "file:///tmp/c2.jsonl");
    assert_eq!(p3.census_contract, [0; 20]);
    org.cancel_process(&pid3).await?;
    assert_eq!(reader.process(&pid3).await?.status, ProcessStatus::Canceled);
    let e = org.cancel_process(&pid3).await.unwrap_err().to_string();
    assert!(e.contains("InvalidStatus"), "{e}");

    // Mined transactions only: reverts caught by the simulation never went out.
    let receipts = org.receipts();
    assert_eq!(receipts.len(), 6);
    assert!(receipts.iter().all(|r| r.status() && r.gas_used > 0));

    // Only the READY one is left to cancel, once.
    assert_eq!(org.created(), vec![pid, pid2, pid3]);
    assert_eq!(org.cancel_open().await?, vec![pid2]);
    assert_eq!(reader.process(&pid2).await?.status, ProcessStatus::Canceled);
    assert!(org.cancel_open().await?.is_empty());
    Ok(())
}

/// Every `ProcessMetadataUpdated` of `pid` on `registry`, oldest first.
async fn metadata_log(
    p: &impl Provider,
    registry: Address,
    pid: &[u8; 31],
) -> anyhow::Result<Vec<(String, [u8; 32])>> {
    use ProcessRegistry::ProcessMetadataUpdated as Ev;
    let filter = Filter::new()
        .address(registry)
        .event_signature(Ev::SIGNATURE_HASH)
        .from_block(0);
    let mut out = Vec::new();
    for log in p.get_logs(&filter).await? {
        let e = log.log_decode::<Ev>()?.inner.data;
        if e.processId.0 == *pid {
            out.push((e.metadataURI, e.metadataHash.0));
        }
    }
    Ok(out)
}

// setProcessMetadata: the organizer replaces the metadata while the process
// is open, getProcess and the event log agree, and it is frozen after the end.
#[tokio::test]
async fn process_metadata() -> anyhow::Result<()> {
    if std::env::var("ANVIL").as_deref() != Ok("1") {
        eprintln!("ANVIL not set, skipping");
        return Ok(());
    }
    let anvil = Anvil::at(anvil_bin()).try_spawn()?;
    let signer: PrivateKeySigner = anvil.keys()[0].clone().into();
    let url = anvil.endpoint();
    let p = ProviderBuilder::new()
        .wallet(EthereumWallet::from(signer.clone()))
        .connect_http(url.parse()?);
    // The verifier is never called: any non-zero address passes.
    let vk = release::BATCH_PROGRAM_VK;
    let registry = deploy_registry(&p, 31337, Address::repeat_byte(0x11), vk).await?;
    let org = Organizer::connect(&url, signer, registry)?;
    let reader = RegistryReader::connect(&url, registry)?;

    let doc2 = br#"{"title":{"default":"organizer test, v2"}}"#;
    let doc3 = br#"{"title":{"default":"organizer test, v3"}}"#;
    let (h1, h2, h3) = (metadata_hash(DOC), metadata_hash(doc2), metadata_hash(doc3));
    let mut params = NewProcess {
        process_id: org.next_process_id().await?,
        start_time: 0,
        duration: 600,
        max_voters: 24,
        ballot_mode: BallotMode {
            num_fields: 4,
            group_size: 1,
            unique_values: false,
            cost_exponent: 1,
            max_value: 5,
            min_value: 0,
            max_value_sum: 0,
            min_value_sum: 0,
        },
        census_origin: 1,
        census_root: Fr::from(123456789u64),
        census_contract: [0; 20],
        census_uri: "file:///tmp/census.json".into(),
        metadata: "ipfs://v1".into(),
        metadata_hash: h1,
        key_mode: KeyMode::Sequencer(Point::generator().mul(&U256::from(4242u64))),
    };
    // No process without a URI and a hash.
    let mut bad = params.clone();
    bad.metadata = String::new();
    let e = org.create_process(&bad).await.unwrap_err();
    assert!(
        matches!(&e, Error::Reverted(n) if n == "InvalidMetadata"),
        "{e}"
    );
    bad.metadata = "ipfs://v1".into();
    bad.metadata_hash = [0; 32];
    let e = org.create_process(&bad).await.unwrap_err();
    assert!(
        matches!(&e, Error::Reverted(n) if n == "InvalidMetadata"),
        "{e}"
    );

    // Creation stores the metadata and logs it.
    let pid = org.create_process(&params).await?.pid;
    let got = |p: davinci_client::organizer::OnchainProcess| (p.metadata_uri, p.metadata_hash);
    assert_eq!(got(reader.process(&pid).await?), ("ipfs://v1".into(), h1));
    assert_eq!(
        metadata_log(&p, registry, &pid).await?,
        [("ipfs://v1".into(), h1)]
    );

    // READY, then PAUSED: the organizer replaces it.
    org.set_process_metadata(&pid, "ipfs://v2", h2).await?;
    assert_eq!(got(reader.process(&pid).await?), ("ipfs://v2".into(), h2));
    ProcessRegistry::new(registry, org.provider())
        .setProcessStatus(FixedBytes(pid), 3)
        .send()
        .await?
        .get_receipt()
        .await?;
    assert_eq!(reader.process(&pid).await?.status, ProcessStatus::Paused);
    org.set_process_metadata(&pid, "https://example.com/v3.json", h3)
        .await?;
    assert_eq!(
        got(reader.process(&pid).await?),
        ("https://example.com/v3.json".into(), h3)
    );

    // Nobody else, and never empty.
    let other = Organizer::connect(&url, anvil.keys()[1].clone().into(), registry)?;
    let e = other
        .set_process_metadata(&pid, "ipfs://x", h2)
        .await
        .unwrap_err();
    assert!(
        matches!(&e, Error::Reverted(n) if n == "Unauthorized"),
        "{e}"
    );
    for (uri, hash) in [("", h2), ("ipfs://x", [0; 32])] {
        let e = org.set_process_metadata(&pid, uri, hash).await.unwrap_err();
        assert!(
            matches!(&e, Error::Reverted(n) if n == "InvalidMetadata"),
            "{e}"
        );
    }

    // Past the end the metadata is frozen.
    let rpc = ProviderBuilder::new().connect_http(url.parse()?);
    let _: serde_json::Value = rpc.raw_request("evm_increaseTime".into(), (601,)).await?;
    let _: serde_json::Value = rpc.raw_request("evm_mine".into(), ()).await?;
    let e = org
        .set_process_metadata(&pid, "ipfs://late", h1)
        .await
        .unwrap_err();
    assert!(
        matches!(&e, Error::Reverted(n) if n == "InvalidTimeBounds"),
        "{e}"
    );

    // The log alone is the history, and its last entry is what getProcess says.
    let log = metadata_log(&p, registry, &pid).await?;
    assert_eq!(
        log,
        [
            ("ipfs://v1".into(), h1),
            ("ipfs://v2".into(), h2),
            ("https://example.com/v3.json".into(), h3),
        ]
    );
    assert_eq!(log.last().cloned(), Some(got(reader.process(&pid).await?)));

    // A second process: an ended one is frozen too.
    params.process_id = org.next_process_id().await?;
    let pid2 = org.create_process(&params).await?.pid;
    org.end_process(&pid2).await?;
    let e = org
        .set_process_metadata(&pid2, "ipfs://v2", h2)
        .await
        .unwrap_err();
    assert!(
        matches!(&e, Error::Reverted(n) if n == "InvalidStatus"),
        "{e}"
    );
    assert_eq!(got(reader.process(&pid2).await?), ("ipfs://v1".into(), h1));
    Ok(())
}

#[test]
fn organizer_secret_is_range_checked() {
    use davinci_zkvm_sdk::crypto::babyjubjub::SUBGROUP_ORDER;
    use davinci_zkvm_sdk::crypto::field::u256_to_be;
    assert!(OrganizerSecret::from_be_bytes(&[0u8; 32]).is_err());
    assert!(OrganizerSecret::from_be_bytes(&u256_to_be(&SUBGROUP_ORDER)).is_err());
    let mut b = [0u8; 32];
    b[31] = 7;
    let sk = OrganizerSecret::from_be_bytes(&b).unwrap();
    assert_eq!(sk.to_be_bytes(), b);
    assert_eq!(sk.public_key(), Point::generator().mul(&U256::from(7u64)));
    assert_eq!(format!("{sk:?}"), "OrganizerSecret(<redacted>)");
}

alloy::sol! {
    #[sol(rpc)]
    interface IMockDKG {
        function newEpoch(bool live, uint256[2][] memory keys) external returns (bytes12 eid);
        function revealed(bytes12 eid, bytes32 aid) external view returns (bool);
    }
}

/// Pause and resume, the duration, max voters and grace, as the registry
/// allows them: the end moves earlier only with notice, max voters never
/// drops below the count, the grace stays within the registry's bounds.
#[tokio::test]
async fn process_controls() -> anyhow::Result<()> {
    if std::env::var("ANVIL").as_deref() != Ok("1") {
        eprintln!("ANVIL not set, skipping");
        return Ok(());
    }
    let anvil = Anvil::at(anvil_bin()).try_spawn()?;
    let signer: PrivateKeySigner = anvil.keys()[0].clone().into();
    let url = anvil.endpoint();
    let p = ProviderBuilder::new()
        .wallet(EthereumWallet::from(signer.clone()))
        .connect_http(url.parse()?);
    let vk = release::BATCH_PROGRAM_VK;
    let registry = deploy_registry(&p, 31337, Address::repeat_byte(0x11), vk).await?;
    let org = Organizer::connect(&url, signer, registry)?;
    let reader = RegistryReader::connect(&url, registry)?;
    let params = NewProcess {
        process_id: org.next_process_id().await?,
        start_time: 0,
        duration: 3600,
        max_voters: 24,
        ballot_mode: BallotMode {
            num_fields: 4,
            group_size: 4,
            unique_values: false,
            cost_exponent: 1,
            max_value: 1,
            min_value: 0,
            max_value_sum: 1,
            min_value_sum: 1,
        },
        census_origin: 1,
        census_root: Fr::from(123456789u64),
        census_contract: [0; 20],
        census_uri: "file:///tmp/census.json".into(),
        metadata: "ipfs://v1".into(),
        metadata_hash: metadata_hash(DOC),
        key_mode: KeyMode::Sequencer(Point::generator().mul(&U256::from(4242u64))),
    };
    let pid = org.create_process(&params).await?.pid;
    let reverted = |e: Error| match e {
        Error::Reverted(n) => n,
        e => panic!("not a revert: {e}"),
    };

    org.pause_process(&pid).await?;
    assert_eq!(reader.process(&pid).await?.status, ProcessStatus::Paused);
    let e = org.pause_process(&pid).await.unwrap_err();
    assert_eq!(reverted(e), "InvalidStatus");
    // Paused, the organizer still sets the duration and max voters.
    org.set_process_duration(&pid, 7200).await?;
    org.set_process_max_voters(&pid, 50).await?;
    org.resume_process(&pid).await?;
    let got = reader.process(&pid).await?;
    assert_eq!(
        (got.status, got.duration, got.max_voters),
        (ProcessStatus::Ready, 7200, 50)
    );
    let e = org.resume_process(&pid).await.unwrap_err();
    assert_eq!(reverted(e), "InvalidStatus");

    // The end moves earlier only with notice, and must move; max voters
    // can shrink, never to zero.
    org.set_process_duration(&pid, 3000).await?;
    let e = org.set_process_duration(&pid, 3000).await.unwrap_err();
    assert_eq!(reverted(e), "InvalidDuration");
    let e = org.set_process_duration(&pid, 1).await.unwrap_err();
    assert_eq!(reverted(e), "InvalidDuration");
    org.set_process_max_voters(&pid, 10).await?;
    let e = org.set_process_max_voters(&pid, 0).await.unwrap_err();
    assert_eq!(reverted(e), "InvalidMaxVoters");
    assert_eq!(reader.process(&pid).await?.max_voters, 10);

    // The grace window: the registry's immutables, a grace within
    // floor..=ceil, and a window closing at end + grace with no landing.
    let g = reader.grace_params().await?;
    assert_eq!(
        (
            g.default_grace,
            g.grace_floor,
            g.grace_ceil,
            g.grace_max_total,
            g.notice_min
        ),
        GRACE_ARGS
    );
    assert_eq!(org.grace_params().await?, g);
    assert_eq!(reader.process(&pid).await?.grace, 10);
    org.set_process_grace(&pid, 60).await?;
    for bad in [1, 61] {
        let e = org.set_process_grace(&pid, bad).await.unwrap_err();
        assert_eq!(reverted(e), "InvalidGrace");
    }
    let got = reader.process(&pid).await?;
    assert_eq!((got.grace, got.last_vote_at), (60, 0));
    assert_eq!(
        reader.grace_end(&pid).await?,
        got.start_time + got.duration + 60
    );
    assert_eq!(org.grace_end(&pid).await?, reader.grace_end(&pid).await?);

    // Only the organizer.
    let other = Organizer::connect(&url, anvil.keys()[1].clone().into(), registry)?;
    for e in [
        other.pause_process(&pid).await.unwrap_err(),
        other.set_process_duration(&pid, 9000).await.unwrap_err(),
        other.set_process_max_voters(&pid, 40).await.unwrap_err(),
        other.set_process_grace(&pid, 30).await.unwrap_err(),
    ] {
        assert_eq!(reverted(e), "Unauthorized");
    }

    // Once ended, nothing moves.
    org.end_process(&pid).await?;
    let e = org.set_process_duration(&pid, 9000).await.unwrap_err();
    assert_eq!(reverted(e), "InvalidStatus");
    let e = org.resume_process(&pid).await.unwrap_err();
    assert_eq!(reverted(e), "InvalidStatus");
    let e = org.set_process_grace(&pid, 30).await.unwrap_err();
    assert_eq!(reverted(e), "InvalidStatus");
    // An early END pulls the end to now: the window closes a grace later.
    let got = reader.process(&pid).await?;
    assert_eq!(
        reader.grace_end(&pid).await?,
        got.start_time + got.duration + 60
    );
    // Reverts are caught before sending: only the good calls were mined.
    assert_eq!(org.receipts().len(), 9);
    Ok(())
}

// Both DKG modes on the real registry with the contracts' MockDKG (real
// curve math; only the Schnorr PoP is not checked): the keys the registry
// stores, the organizer secret and its reveal.
#[tokio::test]
async fn dkg_key_modes() -> anyhow::Result<()> {
    use alloy::primitives::U256 as AU256;
    use davinci_zkvm_sdk::dkg::point_to_rte;
    if std::env::var("ANVIL").as_deref() != Ok("1") {
        eprintln!("ANVIL not set, skipping");
        return Ok(());
    }
    let anvil = Anvil::at(anvil_bin()).try_spawn()?;
    let url = anvil.endpoint();
    // Account 2 deploys and runs the mock; account 0 organizes.
    let admin: PrivateKeySigner = anvil.keys()[2].clone().into();
    let p = ProviderBuilder::new()
        .wallet(EthereumWallet::from(admin))
        .connect_http(url.parse()?);
    let verifier = deploy(&p, bytecode("ZiskVerifier.sol/ZiskVerifier.json")).await?;
    let mock = deploy(&p, bytecode("MockDKG.sol/MockDKG.json")).await?;
    let vk_hash = BallotVerifier::from_snarkjs_json(release::ballot_vk_json())?.vk_hash();
    let mut code = registry_bytecode();
    code.extend_from_slice(
        &(
            31337u32,
            verifier,
            B256::from(release::BATCH_PROGRAM_VK),
            B256::from(release::RESULTS_PROGRAM_VK),
            B256::from(release::ROOT_C_VADCOP_FINAL),
            B256::from(vk_hash),
            mock,
            Address::ZERO, // _councilManager
        )
            .abi_encode_params(),
    );
    code.extend_from_slice(&GRACE_ARGS.abi_encode_params());
    let registry = deploy(&p, code).await?;
    let info = verify_registry(&url, registry).await?;
    assert!(info.dkg_adapter.is_some(), "registry has a DKG adapter");
    let dkg = IMockDKG::new(mock, &p);
    let key = |k: u64| Point::generator().mul(&U256::from(k));
    let pool: Vec<[AU256; 2]> = [1_000_003u64, 1_000_004]
        .iter()
        .map(|k| {
            let (x, y) = point_to_rte(&key(*k));
            [AU256::from_be_bytes(x), AU256::from_be_bytes(y)]
        })
        .collect();
    let eid = dkg.newEpoch(true, pool.clone()).call().await?;
    dkg.newEpoch(true, pool).send().await?.get_receipt().await?;

    let org = Organizer::connect(&url, anvil.keys()[0].clone().into(), registry)?;
    let params = NewProcess {
        process_id: org.next_process_id().await?,
        start_time: 0,
        duration: 3600,
        max_voters: 24,
        ballot_mode: BallotMode {
            num_fields: 4,
            group_size: 1,
            unique_values: false,
            cost_exponent: 1,
            max_value: 5,
            min_value: 0,
            max_value_sum: 0,
            min_value_sum: 0,
        },
        census_origin: 1,
        census_root: Fr::from(123456789u64),
        census_contract: [0; 20],
        census_uri: "file:///tmp/census.json".into(),
        metadata: "ipfs://metadata".into(),
        metadata_hash: metadata_hash(DOC),
        key_mode: KeyMode::DkgAutomatic,
    };

    // Automatic: pool key 0, no secret.
    let auto = org.create_process(&params).await?;
    assert!(auto.organizer_secret.is_none());
    let pa = org.process(&auto.pid).await?;
    assert_eq!(pa.encryption_key, key(1_000_003));
    let da = pa.dkg.expect("DKG process");
    assert!(!da.locked && !da.results_requested);
    assert_eq!(da.epoch_id, eid.0);

    // Locked: pool key 1 plus the organizer key, whose secret comes back.
    let mut locked = params.clone();
    locked.process_id = org.next_process_id().await?;
    locked.key_mode = KeyMode::DkgLocked;
    let lp = org.create_process(&locked).await?;
    let sk = lp.organizer_secret.expect("locked mode returns the secret");
    let pl = org.process(&lp.pid).await?;
    assert_eq!(pl.encryption_key, key(1_000_004).add(&sk.public_key()));
    let dl = pl.dkg.expect("DKG process");
    assert!(dl.locked);
    assert_eq!(dl.epoch_id, eid.0);
    assert_ne!(dl.aid, da.aid);

    // The pool is spent: no epoch to register with.
    let mut more = locked.clone();
    more.process_id = org.next_process_id().await?;
    let e = org.create_process(&more).await.unwrap_err();
    assert!(
        matches!(&e, Error::Reverted(n) if n == "NoLiveEpoch"),
        "{e}"
    );

    // Reveal: only the real secret, only once, only for a locked process.
    let mut b = sk.to_be_bytes();
    b[31] ^= 1;
    let wrong = OrganizerSecret::from_be_bytes(&b)?;
    let e = org.reveal_process_key(&lp.pid, &wrong).await.unwrap_err();
    assert!(
        matches!(&e, Error::Reverted(n) if n == "InvalidOrganizerSecret"),
        "{e}"
    );
    assert!(!dkg.revealed(eid, FixedBytes(dl.aid)).call().await?);
    org.reveal_process_key(&lp.pid, &sk).await?;
    assert!(dkg.revealed(eid, FixedBytes(dl.aid)).call().await?);
    let e = org.reveal_process_key(&lp.pid, &sk).await.unwrap_err();
    assert!(
        matches!(&e, Error::Reverted(n) if n == "AlreadyRevealed"),
        "{e}"
    );
    let e = org.reveal_process_key(&auto.pid, &sk).await.unwrap_err();
    assert!(
        matches!(&e, Error::Reverted(n) if n == "InvalidKeyMode"),
        "{e}"
    );

    // Two locked creates race for the last key of the newest epoch in one
    // block. The loser's mined revert is named `PoolExhausted`, and its one
    // retry registers with the older epoch that still has a key.
    let one = |k: u64| {
        let (x, y) = point_to_rte(&key(k));
        vec![[AU256::from_be_bytes(x), AU256::from_be_bytes(y)]]
    };
    let older = dkg.newEpoch(true, one(1_000_005)).call().await?;
    dkg.newEpoch(true, one(1_000_005))
        .send()
        .await?
        .get_receipt()
        .await?;
    let newest = dkg.newEpoch(true, one(1_000_006)).call().await?;
    dkg.newEpoch(true, one(1_000_006))
        .send()
        .await?
        .get_receipt()
        .await?;
    let org2 = Organizer::connect(&url, anvil.keys()[1].clone().into(), registry)?;
    let (mut a, mut b) = (locked.clone(), locked.clone());
    a.process_id = org.next_process_id().await?;
    b.process_id = org2.next_process_id().await?;
    let rpc = ProviderBuilder::new().connect_http(url.parse()?);
    let _: () = rpc.raw_request("evm_setAutomine".into(), (false,)).await?;
    // One block takes both creates. Bounded, and the block is mined and
    // automine restored either way, so a create that never reaches the pool
    // fails the test instead of leaving both waiting on receipts.
    let miner = || async {
        let both = tokio::time::timeout(std::time::Duration::from_secs(60), async {
            loop {
                let st: serde_json::Value = rpc.raw_request("txpool_status".into(), ()).await?;
                if st["pending"] == "0x2" {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
            anyhow::Ok(())
        })
        .await;
        let _: serde_json::Value = rpc.raw_request("evm_mine".into(), ()).await?;
        let _: () = rpc.raw_request("evm_setAutomine".into(), (true,)).await?;
        both.map_err(|_| anyhow::anyhow!("both creates never reached the pool"))?
    };
    let (ra, rb, m) = tokio::join!(org.create_process(&a), org2.create_process(&b), miner());
    m?;
    let (ra, rb) = (ra?, rb?);
    let mut epochs = Vec::new();
    for (o, r) in [(&org, &ra), (&org2, &rb)] {
        let p = o.process(&r.pid).await?;
        let sk = r.organizer_secret.as_ref().expect("secret");
        let d = p.dkg.expect("DKG process");
        let pool = if d.epoch_id == newest.0 {
            1_000_006
        } else {
            1_000_005
        };
        assert_eq!(p.encryption_key, key(pool).add(&sk.public_key()));
        epochs.push(d.epoch_id);
    }
    epochs.sort();
    let mut want = vec![older.0, newest.0];
    want.sort();
    assert_eq!(epochs, want, "one create retried on the older epoch");
    // Exactly one mined revert: the loser's first newProcess.
    let lost = [org.receipts(), org2.receipts()]
        .iter()
        .flatten()
        .filter(|r| !r.status())
        .count();
    assert_eq!(lost, 1);

    // Same race in automatic mode with one key left anywhere: the loser's
    // mined revert comes back named, and `NoLiveEpoch` is not retried.
    dkg.newEpoch(true, one(1_000_007))
        .send()
        .await?
        .get_receipt()
        .await?;
    let (mut a, mut b) = (params.clone(), params.clone());
    a.process_id = org.next_process_id().await?;
    b.process_id = org2.next_process_id().await?;
    let _: () = rpc.raw_request("evm_setAutomine".into(), (false,)).await?;
    let (ra, rb, m) = tokio::join!(org.create_process(&a), org2.create_process(&b), miner());
    m?;
    let (ok, err): (Vec<_>, Vec<_>) = [ra, rb].into_iter().partition(|r| r.is_ok());
    assert_eq!(ok.len(), 1);
    let e = err.into_iter().next().unwrap().unwrap_err();
    assert!(
        matches!(&e, Error::Reverted(n) if n == "NoLiveEpoch"),
        "{e}"
    );
    Ok(())
}

// Posts a JSON-RPC body to `url`; the raw response body.
async fn forward(url: &str, body: axum::body::Bytes) -> Vec<u8> {
    let resp = reqwest::Client::new()
        .post(url)
        .header("content-type", "application/json")
        .body(body)
        .send()
        .await
        .expect("anvil reachable");
    resp.bytes().await.expect("anvil body").to_vec()
}

// An RPC in front of anvil that forwards the first `eth_sendRawTransaction`
// but answers it 429: the retry layer resends the mined transaction and anvil
// refuses the copy. The organizer must take its own transaction as sent. Then
// a send whose nonce another client took is refused for real, with a hash the
// chain does not know: that one still fails.
#[tokio::test(flavor = "multi_thread")]
async fn resend_of_a_mined_transaction_is_ours() -> anyhow::Result<()> {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
    if std::env::var("ANVIL").as_deref() != Ok("1") {
        eprintln!("ANVIL not set, skipping");
        return Ok(());
    }
    let anvil = Anvil::at(anvil_bin()).try_spawn()?;
    let signer: PrivateKeySigner = anvil.keys()[0].clone().into();
    let url = anvil.endpoint();
    let wallet = || {
        ProviderBuilder::new()
            .wallet(EthereumWallet::from(signer.clone()))
            .connect_http(url.parse().expect("anvil url"))
    };
    let registry = deploy_registry(
        &wallet(),
        31337,
        Address::repeat_byte(0x11),
        release::BATCH_PROGRAM_VK,
    )
    .await?;

    let sends = Arc::new(AtomicUsize::new(0));
    let (up, n) = (url.clone(), sends.clone());
    let app = axum::Router::new().route(
        "/",
        axum::routing::post(move |body: axum::body::Bytes| async move {
            let req: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
            let first = req["method"] == "eth_sendRawTransaction" && n.fetch_add(1, SeqCst) == 0;
            let resp = forward(&up, body).await;
            if first {
                (axum::http::StatusCode::TOO_MANY_REQUESTS, Vec::new())
            } else {
                (axum::http::StatusCode::OK, resp)
            }
        }),
    );
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let proxy = format!("http://{}/", l.local_addr()?);
    tokio::spawn(async move { axum::serve(l, app).await });

    let org = Organizer::connect(&proxy, signer.clone(), registry)?
        .with_receipt_timeout(std::time::Duration::from_secs(30));
    let rpc = ProviderBuilder::new().connect_http(url.parse()?);
    let nonce = rpc.get_transaction_count(org.address()).await?;
    let params = NewProcess {
        process_id: org.next_process_id().await?,
        start_time: 0,
        duration: 3600,
        max_voters: 24,
        ballot_mode: BallotMode {
            num_fields: 4,
            group_size: 1,
            unique_values: false,
            cost_exponent: 1,
            max_value: 5,
            min_value: 0,
            max_value_sum: 0,
            min_value_sum: 0,
        },
        census_origin: 1,
        census_root: Fr::from(123456789u64),
        census_contract: [0; 20],
        census_uri: "file:///tmp/census.json".into(),
        metadata: "ipfs://metadata".into(),
        metadata_hash: metadata_hash(DOC),
        key_mode: KeyMode::Sequencer(Point::generator().mul(&U256::from(4242u64))),
    };
    let pid = org.create_process(&params).await?.pid;
    assert_eq!(pid, params.process_id);
    assert_eq!(sends.load(SeqCst), 2, "the send was retried");
    assert_eq!(rpc.get_transaction_count(org.address()).await?, nonce + 1);
    let receipts = org.receipts();
    assert_eq!(receipts.len(), 1);
    assert!(receipts[0].status());
    assert_eq!(org.process(&pid).await?.status, ProcessStatus::Ready);

    // Another client takes the nonce the organizer has cached next.
    wallet()
        .send_transaction(
            TransactionRequest::default()
                .with_to(Address::repeat_byte(0x33))
                .with_value(alloy::primitives::U256::from(1u64)),
        )
        .await?
        .get_receipt()
        .await?;
    let e = org.end_process(&pid).await.unwrap_err();
    assert!(
        matches!(&e, Error::Chain(m) if m.contains("nonce too low")),
        "{e}"
    );
    assert_eq!(org.receipts().len(), 1);
    assert_eq!(org.process(&pid).await?.status, ProcessStatus::Ready);
    Ok(())
}
