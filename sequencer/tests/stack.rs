//! Stack test for the sync path: fetching another sequencer's blobs must
//! fit a small stack. Deserializing the anvil response into `eip4844::Blob`
//! (a 128 KiB by-value array) overflows the 2 MiB tokio worker stack in
//! debug builds, so the source deserializes into heap `Bytes`. The 512 KiB
//! thread here catches the by-value pattern even in the optimized test
//! profile.

use alloy::consensus::transaction::Recovered;
use alloy::consensus::{Signed, TxEip4844, TxEip4844Variant, TxEnvelope};
use alloy::primitives::{Address, B256, U256};
use axum::{Json, Router, routing::post};
use davinci_sequencer::web3::{AnvilBlobs, BlobSource};
use davinci_zkvm_sdk::ballot::Ballot;
use davinci_zkvm_sdk::blob::{Blob, TransitionData, build_blobs};
use davinci_zkvm_sdk::crypto::field::Fr;
use davinci_zkvm_sdk::limits::{BALLOT_MIN, VOTE_ID_MIN};
use serde_json::{Value, json};
use url::Url;

fn multi_blob() -> (Vec<Blob>, Vec<B256>) {
    let t = TransitionData {
        vote_ids: (0..500).map(|i| VOTE_ID_MIN + i).collect(),
        updates: (0..500)
            .map(|i| (BALLOT_MIN + i, Ballot::identity()))
            .collect(),
        accumulator: Ballot::identity(),
        num_fields: 4,
    };
    let b = build_blobs(&t, &Fr::from(7u64), &[1u8; 32]).unwrap();
    assert!(b.blobs.len() >= 2, "fixture must span multiple blobs");
    let hashes = b.versioned_hashes.iter().map(|h| B256::from(*h)).collect();
    (b.blobs, hashes)
}

// A mined EIP-4844 transaction carrying `hashes`, as eth_getTransactionByHash
// returns it.
fn tx_json(tx_hash: B256, hashes: Vec<B256>, block: u64) -> Value {
    let tx = TxEip4844 {
        chain_id: 1,
        nonce: 0,
        gas_limit: 21_000,
        max_fee_per_gas: 1,
        max_priority_fee_per_gas: 1,
        to: Address::ZERO,
        value: U256::ZERO,
        access_list: Default::default(),
        blob_versioned_hashes: hashes,
        max_fee_per_blob_gas: 1,
        input: Default::default(),
    };
    let signed = Signed::new_unchecked(
        TxEip4844Variant::TxEip4844(tx),
        alloy::primitives::Signature::test_signature(),
        tx_hash,
    );
    let rpc = alloy::rpc::types::Transaction {
        inner: Recovered::new_unchecked(TxEnvelope::Eip4844(signed), Address::ZERO),
        block_hash: Some(B256::with_last_byte(9)),
        block_number: Some(block),
        transaction_index: Some(0),
        effective_gas_price: None,
        block_timestamp: None,
    };
    serde_json::to_value(&rpc).unwrap()
}

async fn serve_fake_anvil(tx_hash: B256, hashes: Vec<B256>, blobs: Vec<Blob>, block: u64) -> Url {
    let txj = tx_json(tx_hash, hashes, block);
    let blob_hex: Vec<String> = blobs
        .iter()
        .map(|b| format!("0x{}", hex::encode(&b[..])))
        .collect();
    let app = Router::new().route(
        "/",
        post(move |Json(req): Json<Value>| {
            let (txj, blob_hex) = (txj.clone(), blob_hex.clone());
            async move {
                let id = req["id"].clone();
                let result = match req["method"].as_str() {
                    Some("eth_getTransactionByHash") => txj,
                    Some("anvil_getBlobsByTransactionHash") => json!(blob_hex),
                    m => panic!("unexpected method {m:?}"),
                };
                Json(json!({"jsonrpc": "2.0", "id": id, "result": result}))
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Url::parse(&format!("http://{addr}/")).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn anvil_blob_fetch_fits_a_small_stack() {
    let (blobs, hashes) = multi_blob();
    let tx_hash = B256::with_last_byte(7);
    let url = serve_fake_anvil(tx_hash, hashes, blobs.clone(), 5).await;
    let n = blobs.len() as u64;
    // 512 KiB: well under the 2 MiB worker stack, comfortable for the
    // heap-based decode, fatal for a by-value 128 KiB blob type.
    let handle = std::thread::Builder::new()
        .stack_size(512 << 10)
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(AnvilBlobs::new(std::slice::from_ref(&url)).blobs_for_tx(tx_hash, 5, n))
        })
        .unwrap();
    let got = tokio::task::spawn_blocking(move || handle.join().expect("stack overflow"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(got, blobs);
}
