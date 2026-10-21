//! Organizer calls against the hardened registry on anvil (`ANVIL=1`; needs
//! the forge artifacts in `DAVINCI_CONTRACTS_DIR`, default
//! `../davinci-contracts`).

use std::path::PathBuf;

use alloy::network::{EthereumWallet, TransactionBuilder};
use alloy::node_bindings::Anvil;
use alloy::primitives::{Address, B256, Bytes, U256 as AU256};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::rpc::types::TransactionRequest;
use alloy::signers::local::PrivateKeySigner;
use alloy::sol_types::SolValue;
use davinci_client::api::ProcessStatus;
use davinci_client::organizer::{KeyMode, NewProcess, Organizer, metadata_hash};
use davinci_zkvm_sdk::ballot::BallotMode;
use davinci_zkvm_sdk::crypto::babyjubjub::Point;
use davinci_zkvm_sdk::crypto::field::U256;
use davinci_zkvm_sdk::groth16::BallotVerifier;
use davinci_zkvm_sdk::release;

/// Grace constructor args: default, floor, ceil, max total, notice.
const GRACE_ARGS: (u32, u32, u32, u32, u32) = (10, 2, 60, 60, 5);

fn enabled() -> bool {
    std::env::var("ANVIL").as_deref() == Ok("1")
}

fn contracts_dir() -> PathBuf {
    std::env::var_os("DAVINCI_CONTRACTS_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../davinci-contracts")
        })
}

fn anvil_bin() -> PathBuf {
    let p = PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".foundry/bin/anvil");
    if p.exists() {
        p
    } else {
        PathBuf::from("anvil")
    }
}

fn registry_code() -> Vec<u8> {
    let p = contracts_dir().join("out/ProcessRegistry.sol/ProcessRegistry.json");
    let j: serde_json::Value = serde_json::from_slice(&std::fs::read(p).unwrap()).unwrap();
    let hex = j["bytecode"]["object"].as_str().unwrap();
    let mut code = hex::decode(hex.trim_start_matches("0x")).unwrap();
    let vk_hash = BallotVerifier::from_snarkjs_json(release::ballot_vk_json())
        .unwrap()
        .vk_hash();
    code.extend_from_slice(
        &(
            31337u32,
            Address::repeat_byte(0x11),
            B256::from(release::BATCH_PROGRAM_VK),
            B256::from(release::RESULTS_PROGRAM_VK),
            B256::from(release::ROOT_C_VADCOP_FINAL),
            B256::from(vk_hash),
            Address::ZERO,
            Address::ZERO,
        )
            .abi_encode_params(),
    );
    code.extend_from_slice(&GRACE_ARGS.abi_encode_params());
    code.extend_from_slice(&Address::ZERO.abi_encode());
    code
}

fn params(pid: [u8; 31], duration: u64) -> NewProcess {
    NewProcess {
        process_id: pid,
        start_time: 0,
        duration,
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
        census_root: [0xc5; 32],
        census_contract: [0; 20],
        census_uri: "file:///tmp/census.json".into(),
        metadata: "ipfs://metadata".into(),
        metadata_hash: metadata_hash(b"{}"),
        key_mode: KeyMode::Sequencer(Point::generator().mul(&U256::from(4242u64))),
    }
}

// The hardened registry refuses CANCELED from a process's grace end on
// (results may be on their way). `cancel_open` cleans up after a run: it
// must leave such a process alone, cancel the open ones and succeed.
#[tokio::test]
async fn cancel_open_leaves_processes_past_their_grace_end() -> anyhow::Result<()> {
    if !enabled() {
        eprintln!("ANVIL not set, skipping");
        return Ok(());
    }
    let anvil = Anvil::at(anvil_bin()).try_spawn()?;
    let signer: PrivateKeySigner = anvil.keys()[0].clone().into();
    let url = anvil.endpoint();
    let p = ProviderBuilder::new()
        .wallet(EthereumWallet::from(signer.clone()))
        .connect_http(url.parse()?);
    let tx = TransactionRequest::default().with_deploy_code(Bytes::from(registry_code()));
    let registry = p
        .send_transaction(tx)
        .await?
        .get_receipt()
        .await?
        .contract_address
        .unwrap();
    let org = Organizer::connect(&url, signer, registry)?;

    // Past its end and its whole grace window, results not set yet.
    let late = org
        .create_process(&params(org.next_process_id().await?, 100))
        .await?
        .pid;
    let _: serde_json::Value = p
        .raw_request("evm_increaseTime".into(), (AU256::from(100 + 61),))
        .await?;
    let _: serde_json::Value = p.raw_request("evm_mine".into(), ()).await?;
    // Past its end, still inside the grace window: cancelable.
    let ended = org
        .create_process(&params(org.next_process_id().await?, 30))
        .await?
        .pid;
    let open = org
        .create_process(&params(org.next_process_id().await?, 3600))
        .await?
        .pid;
    let _: serde_json::Value = p
        .raw_request("evm_increaseTime".into(), (AU256::from(31),))
        .await?;
    let _: serde_json::Value = p.raw_request("evm_mine".into(), ()).await?;
    let e = org.cancel_process(&late).await.unwrap_err().to_string();
    assert!(e.contains("InvalidTimeBounds"), "{e}");

    // BUG: cancel_open sends the refused cancel and fails
    let done = org.cancel_open().await?;
    assert_eq!(done, vec![ended, open]);
    assert_eq!(org.process(&late).await?.status, ProcessStatus::Ready);
    assert_eq!(org.process(&ended).await?.status, ProcessStatus::Canceled);
    assert_eq!(org.process(&open).await?.status, ProcessStatus::Canceled);
    assert!(org.cancel_open().await?.is_empty());
    Ok(())
}
