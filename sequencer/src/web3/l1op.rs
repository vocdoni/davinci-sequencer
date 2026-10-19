//! L1-blob mode on the registry's chain (OP): the mode read, OP's L1 origin,
//! `record()` on the verifier, and `submitStateTransitionL1` with its
//! pre-flight. The registry bubbles the verifier's reverts up unchanged.

use alloy::network::TransactionBuilder;
use alloy::primitives::{Address, Bytes, FixedBytes, U256};
use alloy::providers::Provider;
use alloy::rpc::types::TransactionRequest;
use alloy::sol_types::{SolCall, SolEvent};
use alloy::transports::{RpcError, TransportErrorKind};
use davinci_zkvm_sdk::blob::TransitionBlobs;
use davinci_zkvm_sdk::client::PlonkSnark;

use super::contracts::{Contracts, sat_u64};
use super::l1proof::L1BlobProof;
use super::l1v::L1BlobVerifier as LV;
use super::tx::check_shape;
use super::{
    IL1BlobVerifier as IL1, L1Origin, ProcessRegistry as PR, RegistryDa, Result, RevertReason,
    TxReceipt, Web3Error, rpc_err,
};

alloy::sol! {
    /// OP Stack's `L1Block` predeploy (a mock on test chains).
    interface IL1Block {
        function number() external view returns (uint64);
        function hash() external view returns (bytes32);
    }
}

/// The message of an `Error(string)` revert: the vendored `MerkleTrie`
/// reverts with strings.
pub fn revert_message(data: &[u8]) -> Option<String> {
    use alloy::sol_types::SolError;
    alloy::sol_types::Revert::abi_decode(data)
        .ok()
        .map(|r| r.reason)
}

fn proof_to_abi(p: &L1BlobProof) -> IL1::L1BlobProof {
    IL1::L1BlobProof {
        headers: p.headers.clone(),
        txs: p
            .txs
            .iter()
            .map(|t| IL1::L1TxProof {
                headerIndex: U256::from(t.header_index),
                key: t.key.clone(),
                proof: t.proof.clone(),
            })
            .collect(),
    }
}

/// `submitStateTransitionL1` calldata.
pub(super) fn transition_call_l1(
    pid: &[u8; 31],
    snark: &PlonkSnark,
    blobs: &TransitionBlobs,
    proof: &L1BlobProof,
) -> Bytes {
    PR::submitStateTransitionL1Call {
        processId: FixedBytes(*pid),
        publicValues: snark.public_values.clone().into(),
        proofBytes: snark.proof_bytes.clone().into(),
        commitments: blobs
            .commitments
            .iter()
            .map(|c| Bytes::copy_from_slice(c))
            .collect(),
        ys: blobs.ys.iter().map(|y| FixedBytes(*y)).collect(),
        kzgProofs: blobs
            .proofs
            .iter()
            .map(|p| Bytes::copy_from_slice(p))
            .collect(),
        l1: proof_to_abi(proof),
    }
    .abi_encode()
    .into()
}

// What a registry from before the mode answers `l1BlobVerifier()` with: a
// revert without data. A transport failure or a revert with data is not that.
fn missing_function(e: &RpcError<TransportErrorKind>) -> bool {
    e.as_error_resp().is_some_and(|p| {
        p.message.to_ascii_lowercase().contains("revert")
            && p.as_revert_data().is_none_or(|d| d.is_empty())
    })
}

impl Contracts {
    /// The registry's DA mode: `l1BlobVerifier()` and, when set, the
    /// verifier's `l1Block()`. A registry without the function settles with
    /// blobs. Read once; [`Contracts::events`] pairs L1 events strictly from
    /// then on.
    pub async fn da_mode(&self) -> Result<RegistryDa> {
        self.da
            .get_or_try_init(|| async {
                let tx = TransactionRequest::default()
                    .with_to(self.registry)
                    .with_input(PR::l1BlobVerifierCall {}.abi_encode());
                let verifier = match self
                    .provider
                    .call(tx)
                    .block(alloy::eips::BlockId::latest())
                    .await
                {
                    Ok(out) => PR::l1BlobVerifierCall::abi_decode_returns(&out)
                        .map_err(|e| Web3Error::Data(format!("l1BlobVerifier(): {e}")))?,
                    Err(e) if missing_function(&e) => Address::ZERO,
                    Err(e) => return Err(rpc_err(format!("l1BlobVerifier(): {e}"))),
                };
                if verifier == Address::ZERO {
                    return Ok(RegistryDa::Blobs);
                }
                let l1_block = self.view(verifier, LV::l1BlockCall {}).await?;
                Ok::<_, Web3Error>(RegistryDa::L1Blobs { verifier, l1_block })
            })
            .await
            .copied()
    }

    async fn l1_parts(&self) -> Result<(Address, Address)> {
        match self.da_mode().await? {
            RegistryDa::L1Blobs { verifier, l1_block } => Ok((verifier, l1_block)),
            RegistryDa::Blobs => Err(Web3Error::Config(
                "the registry settles with blobs, not in L1-blob mode".into(),
            )),
        }
    }

    /// OP's L1 origin, `number()` and `hash()` read under one block tag so
    /// they belong together. A backend behind the one that answered the head
    /// fails the pinned read; then both are read again at `latest`.
    pub async fn l1_origin(&self) -> Result<L1Origin> {
        let (_, l1_block) = self.l1_parts().await?;
        let read = async |data: Vec<u8>, at: alloy::eips::BlockId| {
            self.provider
                .call(
                    TransactionRequest::default()
                        .with_to(l1_block)
                        .with_input(data),
                )
                .block(at)
                .await
        };
        let both = async |at: alloy::eips::BlockId| {
            let n = read(IL1Block::numberCall {}.abi_encode(), at).await?;
            let h = read(IL1Block::hashCall {}.abi_encode(), at).await?;
            Ok::<_, RpcError<TransportErrorKind>>((n, h))
        };
        let head = self.provider.get_block_number().await.map_err(rpc_err)?;
        let (n, h) = match both(alloy::eips::BlockId::number(head)).await {
            Ok(v) => v,
            Err(_) => both(alloy::eips::BlockId::latest())
                .await
                .map_err(rpc_err)?,
        };
        let bad = |e: alloy::sol_types::Error| Web3Error::Data(format!("L1Block: {e}"));
        Ok(L1Origin {
            number: IL1Block::numberCall::abi_decode_returns(&n).map_err(bad)?,
            hash: IL1Block::hashCall::abi_decode_returns(&h).map_err(bad)?,
        })
    }

    /// `record()` on the verifier: stores OP's current origin so later
    /// proofs may anchor at it. Returns the origin its event names.
    pub async fn record_l1_origin(&self) -> Result<L1Origin> {
        let (verifier, _) = self.l1_parts().await?;
        let tx = TransactionRequest::default()
            .with_to(verifier)
            .with_input(LV::recordCall {}.abi_encode());
        let (_, logs) = self.send(tx, None).await?;
        logs.iter()
            .filter(|l| l.address() == verifier)
            .find_map(|l| LV::OriginRecorded::decode_log(&l.inner).ok())
            .map(|e| L1Origin {
                number: sat_u64(e.number),
                hash: e.hash,
            })
            .ok_or_else(|| Web3Error::Data("no OriginRecorded log".into()))
    }

    /// `eth_call` of `submitStateTransitionL1`; the revert name on failure.
    pub async fn simulate_transition_l1(
        &self,
        pid: &[u8; 31],
        snark: &PlonkSnark,
        blobs: &TransitionBlobs,
        proof: &L1BlobProof,
    ) -> std::result::Result<(), RevertReason> {
        check_shape(blobs).map_err(|e| RevertReason::Rpc(e.to_string()))?;
        let mut tx = TransactionRequest::default()
            .with_to(self.registry)
            .with_input(transition_call_l1(pid, snark, blobs, proof));
        if let Some(from) = self.signer {
            tx = tx.with_from(from);
        }
        self.provider
            .call(tx)
            .block(alloy::eips::BlockId::latest())
            .await
            .map(|_| ())
            .map_err(|e| self.reason(&e))
    }

    /// `submitStateTransitionL1`: a plain transaction (no blobs on this
    /// chain), gas estimated plus the usual margin.
    pub async fn submit_transition_l1(
        &self,
        pid: &[u8; 31],
        snark: &PlonkSnark,
        blobs: &TransitionBlobs,
        proof: &L1BlobProof,
    ) -> Result<TxReceipt> {
        if self.signer.is_none() {
            return Err(Web3Error::NoSigner);
        }
        check_shape(blobs)?;
        let tx = TransactionRequest::default()
            .with_to(self.registry)
            .with_input(transition_call_l1(pid, snark, blobs, proof));
        Ok(self.send(tx, None).await?.0)
    }
}

#[cfg(test)]
mod tests {
    use alloy::primitives::{Bytes, U256, keccak256};
    use alloy::sol_types::{SolCall, SolError};
    use davinci_zkvm_sdk::blob::TransitionBlobs;
    use davinci_zkvm_sdk::client::PlonkSnark;
    use serde_json::{Value, json};

    use super::*;
    use crate::web3::l1proof::{L1BlobProof, L1TxProof};

    // An RPC whose `eth_call` to the registry (0x01..01) answers `registry`
    // and to anything else returns 0x22..22 (the verifier's `l1Block()`).
    async fn stub(registry: Value) -> url::Url {
        let app = axum::Router::new().route(
            "/",
            axum::routing::post(move |axum::Json(req): axum::Json<Value>| {
                let mut answer = registry.clone();
                async move {
                    let id = req["id"].clone();
                    let to = req["params"][0]["to"]
                        .as_str()
                        .unwrap_or_default()
                        .to_ascii_lowercase();
                    let reg = format!("{:#x}", Address::repeat_byte(1));
                    let body = match req["method"].as_str() {
                        Some("eth_call") if to == reg => {
                            answer["jsonrpc"] = "2.0".into();
                            answer["id"] = id;
                            answer
                        }
                        Some("eth_call") => json!({"jsonrpc": "2.0", "id": id,
                            "result": format!("0x{}{}", "00".repeat(12), "22".repeat(20))}),
                        m => panic!("unexpected {m:?}"),
                    };
                    axum::Json(body)
                }
            }),
        );
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        format!("http://{addr}").parse().unwrap()
    }

    #[tokio::test]
    async fn the_mode_is_read_from_the_registry() {
        let verifier = format!("0x{}{}", "00".repeat(12), "11".repeat(20));
        let c = Contracts::at(stub(json!({ "result": verifier })).await);
        assert_eq!(
            c.da_mode().await.unwrap(),
            RegistryDa::L1Blobs {
                verifier: Address::repeat_byte(0x11),
                l1_block: Address::repeat_byte(0x22),
            }
        );
        let zero = format!("0x{}", "00".repeat(32));
        let c = Contracts::at(stub(json!({ "result": zero })).await);
        assert_eq!(c.da_mode().await.unwrap(), RegistryDa::Blobs);
        // A registry from before the mode: the call reverts without data.
        for err in [
            json!({"code": 3, "message": "execution reverted"}),
            json!({"code": 3, "message": "execution reverted", "data": "0x"}),
        ] {
            let c = Contracts::at(stub(json!({ "error": err })).await);
            assert_eq!(c.da_mode().await.unwrap(), RegistryDa::Blobs);
        }
        // Anything else is an error, never "blob mode".
        let err = json!({"code": -32000, "message": "header not found"});
        let c = Contracts::at(stub(json!({ "error": err })).await);
        assert!(c.da_mode().await.is_err());
    }

    #[test]
    fn the_l1_settlement_call_carries_the_proof() {
        let proof = L1BlobProof {
            headers: vec![Bytes::from(vec![0xc0])],
            txs: vec![L1TxProof {
                header_index: 0,
                key: Bytes::from(vec![0x80]),
                proof: vec![Bytes::from(vec![1, 2])],
            }],
        };
        let snark = PlonkSnark {
            program_vk: [0; 32],
            root_c_vadcop_final: [0; 32],
            public_values: vec![1; 512],
            proof_bytes: vec![2; 768],
        };
        let blobs = TransitionBlobs {
            blobs: vec![],
            commitments: vec![[3; 48]],
            zs: vec![[0; 32]],
            ys: vec![[4; 32]],
            proofs: vec![[5; 48]],
            versioned_hashes: vec![[1; 32]],
            digest: [0; 32],
            cells_digest: [0; 32],
        };
        let data = transition_call_l1(&[7; 31], &snark, &blobs, &proof);
        let call = PR::submitStateTransitionL1Call::abi_decode(&data).unwrap();
        assert_eq!(call.processId.0, [7; 31]);
        assert_eq!(call.ys[0].0, [4; 32]);
        assert_eq!(call.l1.headers, proof.headers);
        assert_eq!(call.l1.txs[0].headerIndex, U256::ZERO);
        assert_eq!(call.l1.txs[0].key, proof.txs[0].key);
        assert_eq!(call.l1.txs[0].proof, proof.txs[0].proof);
    }

    #[test]
    fn l1_verifier_errors_are_named() {
        let names = super::super::contracts::error_names().unwrap();
        let sel = |sig: &str| <[u8; 4]>::try_from(&keccak256(sig)[..4]).unwrap();
        for sig in [
            "L1OriginMismatch()",
            "TxTooOld(uint256)",
            "BrokenHeaderChain(uint256)",
            "BadHeader(uint256)",
            "TxAboveOrigin(uint256)",
            "NotBlobTx(uint256)",
            "BadVersionedHash(uint256)",
            "WrongDAMode()",
        ] {
            let name = sig.split('(').next().unwrap();
            assert_eq!(
                names.get(&sel(sig)).map(String::as_str),
                Some(name),
                "{sig}"
            );
        }
        let msg = alloy::sol_types::Revert::from("MerkleTrie: invalid root hash").abi_encode();
        assert_eq!(
            revert_message(&msg).as_deref(),
            Some("MerkleTrie: invalid root hash")
        );
        assert_eq!(revert_message(&[1, 2, 3]), None);
    }
}
