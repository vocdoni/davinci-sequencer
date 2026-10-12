//! L1 proof builder against Ethereum mainnet. The live test runs with `L1PROOF_LIVE=1`
//! (`L1PROOF_RPC` overrides the RPC); the offline tests use blocks stored by
//! `examples/l1proof_fixtures.rs`.

use alloy::eips::eip2718::Encodable2718;
use alloy::primitives::{Bytes, keccak256};
use alloy::providers::{Provider, ProviderBuilder};
use davinci_sequencer::web3::l1proof::{header_rlp, tx_key, tx_trie_proof, verify_tx};

#[tokio::test(flavor = "multi_thread")]
async fn live_hundred_random_blocks() {
    if std::env::var("L1PROOF_LIVE").as_deref() != Ok("1") {
        eprintln!("skip: set L1PROOF_LIVE=1");
        return;
    }
    let url = std::env::var("L1PROOF_RPC")
        .unwrap_or_else(|_| "https://ethereum-rpc.publicnode.com".into());
    let l1 = ProviderBuilder::new().connect_http(url.parse().unwrap());
    let head = l1.get_block_number().await.unwrap();
    // Cancun (19,426,587 .. 22,431,083), Prague and later (22,431,084 .. head): 50 blocks each.
    let mut blocks: Vec<u64> = (0..50)
        .map(|k| 19_426_587 + k * (22_431_083 - 19_426_587) / 50 + k)
        .collect();
    blocks.extend((0..50).map(|k| 22_431_084 + k * (head - 22_431_084) / 50 + 7 * k));
    let (mut big, mut checked) = (false, 0);
    for n in blocks {
        let b = l1
            .get_block_by_number(n.into())
            .full()
            .await
            .unwrap()
            .unwrap();
        let rlp =
            header_rlp(&b.header.inner, b.header.hash).expect("header hashes to its block hash");
        assert_eq!(keccak256(&rlp), b.header.hash);
        let envs: Vec<Bytes> = b
            .transactions
            .txns()
            .map(|t| t.inner.encoded_2718().into())
            .collect();
        if envs.is_empty() {
            continue;
        }
        big |= envs.len() > 256;
        for i in [0, envs.len() / 2, envs.len() - 1, 128.min(envs.len() - 1)] {
            let (root, proof) = tx_trie_proof(&envs, i);
            assert_eq!(root, b.header.transactions_root, "block {n}");
            verify_tx(root, &tx_key(i), &proof, &envs[i]).unwrap();
        }
        checked += 1;
    }
    assert!(checked >= 95, "only {checked} blocks checked");
    eprintln!("checked {checked} blocks; a block with more than 256 txs seen: {big}");
}

#[test]
fn stored_block_root_and_proofs() {
    let v: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/l1proof/recent.json")).unwrap();
    let envs: Vec<Bytes> = v["envelopes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e.as_str().unwrap().parse().unwrap())
        .collect();
    let want: alloy::primitives::B256 = v["transactions_root"].as_str().unwrap().parse().unwrap();
    let hdr: Bytes = v["header_rlp"].as_str().unwrap().parse().unwrap();
    assert_eq!(keccak256(&hdr).to_string(), v["hash"].as_str().unwrap());
    for i in [0, envs.len() / 2, envs.len() - 1] {
        let (root, proof) = tx_trie_proof(&envs, i);
        assert_eq!(root, want);
        verify_tx(root, &tx_key(i), &proof, &envs[i]).unwrap();
    }
}
