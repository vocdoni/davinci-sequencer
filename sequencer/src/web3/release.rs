//! Boot check: the registry, and the verifier it settles through, must be the
//! pinned release. A registry with other vks or another verifier accepts
//! proofs of some other program, so the node refuses to run against it.

use alloy::network::TransactionBuilder;
use alloy::primitives::{Address, keccak256};
use alloy::providers::Provider;
use alloy::rpc::types::TransactionRequest;
use alloy::sol_types::SolCall;
use davinci_zkvm_sdk::release;

use super::ProcessRegistry as PR;
use super::ZiskVerifier as ZV;
use super::failover::is_node_side;
use super::{Contracts, Result, Web3Error, rpc_err};

/// What the chain holds where the release pins apply.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeployedRelease {
    pub batch_program_vk: [u8; 32],
    pub results_program_vk: [u8; 32],
    pub root_c_vadcop_final: [u8; 32],
    pub ballot_vk_hash: [u8; 32],
    /// `registry.chainID()`.
    pub registry_chain_id: u32,
    /// The RPC's `eth_chainId`.
    pub rpc_chain_id: u64,
    /// `registry.ziskVerifier()`.
    pub verifier: Address,
    /// `verifier.getRootCVadcopFinal()`, or why the call failed.
    pub verifier_root_c: std::result::Result<[u8; 32], String>,
    /// keccak256 of the verifier's runtime code.
    pub verifier_codehash: [u8; 32],
}

/// A pinned value the chain does not match.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mismatch {
    pub field: &'static str,
    pub expected: String,
    pub got: String,
}

pub(super) fn describe(m: &[Mismatch]) -> String {
    m.iter()
        .map(|m| format!("{}: expected {}, got {}", m.field, m.expected, m.got))
        .collect::<Vec<_>>()
        .join("; ")
}

fn hex32(b: &[u8; 32]) -> String {
    format!("0x{}", hex::encode(b))
}

fn check32(out: &mut Vec<Mismatch>, field: &'static str, expected: &[u8; 32], got: &[u8; 32]) {
    if expected != got {
        out.push(Mismatch {
            field,
            expected: hex32(expected),
            got: hex32(got),
        });
    }
}

impl DeployedRelease {
    /// Every value that differs from the release pins, the node's ballot VK
    /// hash or the RPC's chain id. Empty means the registry is the release.
    pub fn mismatches(&self, ballot_vk_hash: &[u8; 32]) -> Vec<Mismatch> {
        let mut out = Vec::new();
        check32(
            &mut out,
            "registry.batchProgramVK",
            &release::BATCH_PROGRAM_VK,
            &self.batch_program_vk,
        );
        check32(
            &mut out,
            "registry.resultsProgramVK",
            &release::RESULTS_PROGRAM_VK,
            &self.results_program_vk,
        );
        check32(
            &mut out,
            "registry.rootCVadcopFinal",
            &release::ROOT_C_VADCOP_FINAL,
            &self.root_c_vadcop_final,
        );
        check32(
            &mut out,
            "registry.ballotVKHash",
            ballot_vk_hash,
            &self.ballot_vk_hash,
        );
        if u64::from(self.registry_chain_id) != self.rpc_chain_id {
            out.push(Mismatch {
                field: "registry.chainID",
                expected: format!("{} (eth_chainId)", self.rpc_chain_id),
                got: self.registry_chain_id.to_string(),
            });
        }
        match &self.verifier_root_c {
            Ok(r) => check32(
                &mut out,
                "verifier.getRootCVadcopFinal",
                &release::ROOT_C_VADCOP_FINAL,
                r,
            ),
            Err(e) => out.push(Mismatch {
                field: "verifier.getRootCVadcopFinal",
                expected: hex32(&release::ROOT_C_VADCOP_FINAL),
                got: format!("call to {} failed ({e})", self.verifier),
            }),
        }
        if self.verifier_codehash != release::ZISK_VERIFIER_CODEHASH {
            out.push(Mismatch {
                field: "verifier.codehash",
                expected: hex32(&release::ZISK_VERIFIER_CODEHASH),
                got: format!(
                    "{} (code at {})",
                    hex32(&self.verifier_codehash),
                    self.verifier
                ),
            });
        }
        out
    }
}

impl Contracts {
    /// `eth_call` of a view at the latest block.
    pub(super) async fn view<C: SolCall>(&self, to: Address, call: C) -> Result<C::Return> {
        let tx = TransactionRequest::default()
            .with_to(to)
            .with_input(call.abi_encode());
        let out = self
            .provider
            .call(tx)
            .block(alloy::eips::BlockId::latest())
            .await
            .map_err(|e| Web3Error::Rpc(format!("{}: {e}", C::SIGNATURE)))?;
        C::abi_decode_returns(&out).map_err(|e| Web3Error::Data(format!("{}: {e}", C::SIGNATURE)))
    }

    /// Reads the registry's pins, its verifier and the verifier's code.
    pub async fn deployed_release(&self) -> Result<DeployedRelease> {
        let r = self.registry;
        let verifier = self.view(r, PR::ziskVerifierCall {}).await?;
        let code = self.provider.get_code_at(verifier).await.map_err(rpc_err)?;
        Ok(DeployedRelease {
            batch_program_vk: self.view(r, PR::batchProgramVKCall {}).await?.0,
            results_program_vk: self.view(r, PR::resultsProgramVKCall {}).await?.0,
            root_c_vadcop_final: self.view(r, PR::rootCVadcopFinalCall {}).await?.0,
            ballot_vk_hash: self.view(r, PR::ballotVKHashCall {}).await?.0,
            registry_chain_id: self.view(r, PR::chainIDCall {}).await?,
            rpc_chain_id: self.chain_id,
            verifier,
            verifier_root_c: self.verifier_root_c(verifier).await?,
            verifier_codehash: keccak256(&code).0,
        })
    }

    /// `verifier.getRootCVadcopFinal()`, or why the verifier cannot answer
    /// it (a revert, a bad return): a mismatch. An RPC that does not answer
    /// or cannot serve it now (a rate limit, a timeout) is an error, not a
    /// verdict on the verifier.
    async fn verifier_root_c(
        &self,
        verifier: Address,
    ) -> Result<std::result::Result<[u8; 32], String>> {
        let tx = TransactionRequest::default()
            .with_to(verifier)
            .with_input(ZV::getRootCVadcopFinalCall {}.abi_encode());
        match self
            .provider
            .call(tx)
            .block(alloy::eips::BlockId::latest())
            .await
        {
            Ok(out) => Ok(ZV::getRootCVadcopFinalCall::abi_decode_returns(&out)
                .map(|r| r.0)
                .map_err(|e| e.to_string())),
            Err(e) => match e.as_error_resp() {
                Some(p) if !p.is_retry_err() && !is_node_side(p.code, &p.message) => {
                    Ok(Err(e.to_string()))
                }
                _ => Err(rpc_err(format!("getRootCVadcopFinal(): {e}"))),
            },
        }
    }

    /// Fails with [`Web3Error::Release`] unless the registry and its verifier
    /// are the pinned release and the registry uses `ballot_vk_hash`, the
    /// hash of the ballot VK this node verifies votes with.
    pub async fn check_release(&self, ballot_vk_hash: &[u8; 32]) -> Result<()> {
        let m = self.deployed_release().await?.mismatches(ballot_vk_hash);
        if m.is_empty() {
            Ok(())
        } else {
            Err(Web3Error::Release(m))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const VK_HASH: [u8; 32] = [0x44; 32];

    fn pinned() -> DeployedRelease {
        DeployedRelease {
            batch_program_vk: release::BATCH_PROGRAM_VK,
            results_program_vk: release::RESULTS_PROGRAM_VK,
            root_c_vadcop_final: release::ROOT_C_VADCOP_FINAL,
            ballot_vk_hash: VK_HASH,
            registry_chain_id: 100,
            rpc_chain_id: 100,
            verifier: Address::repeat_byte(0x78),
            verifier_root_c: Ok(release::ROOT_C_VADCOP_FINAL),
            verifier_codehash: release::ZISK_VERIFIER_CODEHASH,
        }
    }

    fn fields(d: &DeployedRelease, vk_hash: &[u8; 32]) -> Vec<&'static str> {
        d.mismatches(vk_hash).iter().map(|m| m.field).collect()
    }

    #[test]
    fn pinned_release_passes() {
        assert!(pinned().mismatches(&VK_HASH).is_empty());
    }

    #[test]
    fn each_field_is_named() {
        let flip = |mut b: [u8; 32]| {
            b[31] ^= 1;
            b
        };
        type Tamper = fn(&mut DeployedRelease);
        let cases: [(&str, Tamper); 8] = [
            ("registry.batchProgramVK", |d| {
                d.batch_program_vk = d.results_program_vk
            }),
            ("registry.resultsProgramVK", |d| {
                d.results_program_vk = d.batch_program_vk
            }),
            ("registry.rootCVadcopFinal", |d| {
                d.root_c_vadcop_final = [0u8; 32]
            }),
            ("registry.ballotVKHash", |d| d.ballot_vk_hash = [0x45; 32]),
            ("registry.chainID", |d| d.registry_chain_id = 31337),
            ("verifier.getRootCVadcopFinal", |d| {
                d.verifier_root_c = Ok([0u8; 32])
            }),
            ("verifier.getRootCVadcopFinal", |d| {
                d.verifier_root_c = Err("execution reverted".into())
            }),
            ("verifier.codehash", |d| d.verifier_codehash = [0u8; 32]),
        ];
        for (field, tamper) in cases {
            let mut d = pinned();
            tamper(&mut d);
            assert_eq!(fields(&d, &VK_HASH), vec![field]);
        }
        // The node's own ballot VK is the reference, not the embedded one.
        assert_eq!(
            fields(&pinned(), &flip(VK_HASH)),
            vec!["registry.ballotVKHash"]
        );
        // The error names expected and got.
        let mut d = pinned();
        d.registry_chain_id = 31337;
        let err = Web3Error::Release(d.mismatches(&VK_HASH)).to_string();
        assert!(
            err.contains("registry.chainID: expected 100 (eth_chainId), got 31337"),
            "{err}"
        );
        // Everything wrong at once: every field listed.
        let d = DeployedRelease {
            batch_program_vk: [1; 32],
            results_program_vk: [2; 32],
            root_c_vadcop_final: [3; 32],
            ballot_vk_hash: [4; 32],
            registry_chain_id: 1,
            rpc_chain_id: 2,
            verifier: Address::ZERO,
            verifier_root_c: Err("no code".into()),
            verifier_codehash: [5; 32],
        };
        assert_eq!(d.mismatches(&VK_HASH).len(), 7);
    }

    // A JSON-RPC stub standing in for the chain: answers the calls
    // `Contracts::new` and `check_release` make and records the User-Agent.
    #[derive(Default)]
    struct Fake {
        batch_vk: [u8; 32],
        eth_config: Option<serde_json::Value>,
        /// eth_chainId; 0 answers Gnosis (100).
        chain_id: u64,
        /// P256VERIFY calls still to fail with a timeout. (A 429 would rest
        /// the endpoint in the transport, which has its own tests.)
        p256_fail: std::sync::atomic::AtomicUsize,
        /// The JSON-RPC error `getRootCVadcopFinal` answers with, if any.
        /// It carries a Retry-After past the transport's wait, so a rate
        /// limit fails the call at once; other errors ignore it.
        root_c_error: Option<serde_json::Value>,
        p256_calls: std::sync::atomic::AtomicUsize,
        uas: std::sync::Mutex<Vec<String>>,
    }

    const VERIFIER: Address = Address::repeat_byte(0x78);

    async fn rpc(
        axum::extract::State(f): axum::extract::State<std::sync::Arc<Fake>>,
        headers: axum::http::HeaderMap,
        axum::Json(req): axum::Json<serde_json::Value>,
    ) -> axum::response::Response {
        use alloy::sol_types::SolValue;
        use axum::response::IntoResponse;
        let ua = headers.get("user-agent").and_then(|v| v.to_str().ok());
        f.uas.lock().unwrap().push(ua.unwrap_or("").to_string());
        let ok =
            |v: serde_json::Value| serde_json::json!({"jsonrpc":"2.0","id":req["id"],"result":v});
        let hex = |b: Vec<u8>| serde_json::Value::String(format!("0x{}", hex::encode(b)));
        let out = match req["method"].as_str().unwrap_or("") {
            "eth_chainId" => {
                ok(format!("{:#x}", if f.chain_id == 0 { 100 } else { f.chain_id }).into())
            }
            "eth_getCode" => ok(hex(vec![0x60, 0x01])),
            "eth_config" => match &f.eth_config {
                Some(c) => ok(c.clone()),
                None => serde_json::json!({"jsonrpc":"2.0","id":req["id"],
                    "error":{"code":-32601,"message":"the method eth_config does not exist/is not available"}}),
            },
            "eth_call" => {
                let tx = &req["params"][0];
                let input = tx["input"].as_str().or(tx["data"].as_str()).unwrap_or("0x");
                let input = hex::decode(input.trim_start_matches("0x")).unwrap();
                // P256VERIFY, probed when eth_config is missing: an Osaka chain.
                if tx["to"].as_str() == Some("0x0000000000000000000000000000000000000100") {
                    use std::sync::atomic::Ordering::SeqCst;
                    f.p256_calls.fetch_add(1, SeqCst);
                    let failing = f.p256_fail.load(SeqCst);
                    if failing > 0 {
                        f.p256_fail.store(failing - 1, SeqCst);
                        return axum::Json(serde_json::json!({"jsonrpc":"2.0","id":req["id"],
                            "error":{"code":-32603,"message":"request timeout"}}))
                        .into_response();
                    }
                    let mut one = vec![0u8; 32];
                    one[31] = 1;
                    return axum::Json(ok(hex(one))).into_response();
                }
                let sel: [u8; 4] = input[..4].try_into().unwrap();
                if sel == ZV::getRootCVadcopFinalCall::SELECTOR
                    && let Some(e) = &f.root_c_error
                {
                    let e = serde_json::json!({"jsonrpc":"2.0","id":req["id"],"error":e});
                    let after = [(axum::http::header::RETRY_AFTER, "60")];
                    return (after, axum::Json(e)).into_response();
                }
                let b = |x: [u8; 32]| alloy::primitives::B256::from(x).abi_encode();
                ok(hex(match sel {
                    PR::ziskVerifierCall::SELECTOR => VERIFIER.abi_encode(),
                    PR::batchProgramVKCall::SELECTOR => b(f.batch_vk),
                    PR::resultsProgramVKCall::SELECTOR => b(release::RESULTS_PROGRAM_VK),
                    PR::rootCVadcopFinalCall::SELECTOR => b(release::ROOT_C_VADCOP_FINAL),
                    PR::ballotVKHashCall::SELECTOR => b(VK_HASH),
                    PR::chainIDCall::SELECTOR => 100u32.abi_encode(),
                    ZV::getRootCVadcopFinalCall::SELECTOR => b(release::ROOT_C_VADCOP_FINAL),
                    _ => panic!("unexpected call {}", hex::encode(sel)),
                }))
            }
            m => panic!("unexpected method {m}"),
        };
        axum::Json(out).into_response()
    }

    async fn fake_chain(f: Fake) -> (url::Url, std::sync::Arc<Fake>) {
        let f = std::sync::Arc::new(f);
        let app = axum::Router::new()
            .route("/", axum::routing::post(rpc))
            .with_state(f.clone());
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/", l.local_addr().unwrap())
            .parse()
            .unwrap();
        tokio::spawn(async move { axum::serve(l, app).await });
        (url, f)
    }

    #[tokio::test]
    async fn check_over_a_fake_chain() {
        let registry = Address::repeat_byte(0x11);
        // eth_config missing: no chain cap, so the node falls back to six.
        let (url, f) = fake_chain(Fake {
            batch_vk: release::BATCH_PROGRAM_VK,
            ..Default::default()
        })
        .await;
        let c = Contracts::new(std::slice::from_ref(&url), registry, None)
            .await
            .unwrap();
        assert_eq!(c.chain_blob_cap(), None);
        // The P256VERIFY probe stood in for eth_config.
        assert!(c.cell_proofs());
        let d = c.deployed_release().await.unwrap();
        assert_eq!(d.verifier, VERIFIER);
        assert_eq!(d.registry_chain_id, 100);
        assert_eq!(d.rpc_chain_id, 100);
        // Every value read back is the pin; only the stub code differs.
        assert_eq!(fields(&d, &VK_HASH), vec!["verifier.codehash"]);
        let err = c.check_release(&VK_HASH).await.unwrap_err();
        assert!(
            matches!(&err, Web3Error::Release(m) if m.len() == 1),
            "{err}"
        );
        let uas = f.uas.lock().unwrap().clone();
        assert!(uas.len() >= 10);
        assert!(uas.iter().all(|u| u == super::super::USER_AGENT), "{uas:?}");
        assert!(super::super::USER_AGENT.starts_with("davinci-sequencer/"));

        // A registry with another batch vk, on a chain whose eth_config
        // allows two blobs.
        let (url, _) = fake_chain(Fake {
            batch_vk: [0x11; 32],
            eth_config: Some(serde_json::json!({"current":{"blobSchedule":{"max":2}}})),
            ..Default::default()
        })
        .await;
        let c = Contracts::new(std::slice::from_ref(&url), registry, None)
            .await
            .unwrap();
        assert_eq!(c.chain_blob_cap(), Some(2));
        // eth_config without P256VERIFY: pre-Osaka, and no probe.
        assert!(!c.cell_proofs());
        let err = c.check_release(&VK_HASH).await.unwrap_err().to_string();
        assert!(
            err.contains(&format!(
                "registry.batchProgramVK: expected 0x{}, got 0x{}",
                hex::encode(release::BATCH_PROGRAM_VK),
                "11".repeat(32)
            )),
            "{err}"
        );
    }

    // A verifier that reverts is a mismatch; an RPC that refuses the call or
    // cannot serve it now is an RPC error, not a verdict on the release.
    #[tokio::test]
    async fn verifier_call_failures() {
        let registry = Address::repeat_byte(0x11);
        for (error, rpc) in [
            (r#"{"code":-32005,"message":"429 Too Many Requests"}"#, true),
            (r#"{"code":-32603,"message":"internal error"}"#, true),
            (r#"{"code":3,"message":"execution reverted"}"#, false),
            (r#"{"code":-32015,"message":"VM execution error."}"#, false),
        ] {
            let (url, _) = fake_chain(Fake {
                batch_vk: release::BATCH_PROGRAM_VK,
                root_c_error: Some(serde_json::from_str(error).unwrap()),
                ..Default::default()
            })
            .await;
            let c = Contracts::new(std::slice::from_ref(&url), registry, None)
                .await
                .unwrap();
            if rpc {
                let err = c.check_release(&VK_HASH).await.unwrap_err();
                assert!(
                    matches!(&err, Web3Error::Rpc(m) if m.contains("getRootCVadcopFinal")),
                    "{error}: {err}"
                );
            } else {
                let d = c.deployed_release().await.unwrap();
                assert_eq!(
                    fields(&d, &VK_HASH),
                    vec!["verifier.getRootCVadcopFinal", "verifier.codehash"],
                    "{error}"
                );
            }
        }
    }

    // The probe fails closed: retried with backoff, then boot is refused
    // unless the chain is in the known-chain table.
    #[tokio::test]
    async fn probe_retries_then_fails_closed() {
        use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
        let registry = Address::repeat_byte(0x11);
        // Two transient failures, then an Osaka answer.
        let (url, f) = fake_chain(Fake {
            chain_id: 1,
            p256_fail: AtomicUsize::new(2),
            ..Default::default()
        })
        .await;
        let c = Contracts::new(std::slice::from_ref(&url), registry, None)
            .await
            .unwrap();
        assert!(c.cell_proofs());
        assert_eq!(f.p256_calls.load(SeqCst), 3);
        // Failing on every try: boot refused, not v0.
        let (url, f) = fake_chain(Fake {
            chain_id: 1,
            p256_fail: AtomicUsize::new(usize::MAX),
            ..Default::default()
        })
        .await;
        let err = Contracts::new(std::slice::from_ref(&url), registry, None)
            .await
            .unwrap_err();
        assert!(
            matches!(&err, Web3Error::Config(m) if m.contains("P256VERIFY")),
            "{err}"
        );
        assert_eq!(f.p256_calls.load(SeqCst), 4);
        // Same on Gnosis: the known-chain table says cell proofs.
        let (url, _) = fake_chain(Fake {
            p256_fail: AtomicUsize::new(usize::MAX),
            ..Default::default()
        })
        .await;
        let c = Contracts::new(std::slice::from_ref(&url), registry, None)
            .await
            .unwrap();
        assert!(c.cell_proofs());
    }

    // Every endpoint must be on one chain; unreachable ones are kept.
    #[tokio::test]
    async fn boot_checks_every_endpoint() {
        let registry = Address::repeat_byte(0x11);
        let dead: url::Url = "http://127.0.0.1:1/".parse().unwrap();
        let (gnosis, _) = fake_chain(Fake::default()).await;
        let (other, _) = fake_chain(Fake {
            chain_id: 5,
            ..Default::default()
        })
        .await;
        let err = Contracts::new(&[gnosis.clone(), other.clone()], registry, None)
            .await
            .unwrap_err();
        let host = super::super::failover::host(&other);
        assert!(
            matches!(&err, Web3Error::Config(m) if m.contains(&host) && m.contains("chain 5")),
            "{err}"
        );
        // An unreachable first endpoint: boot goes on over the second, and
        // eth_config comes from whichever endpoint serves it.
        let (capped, _) = fake_chain(Fake {
            eth_config: Some(serde_json::json!({"current":{"blobSchedule":{"max":2}}})),
            ..Default::default()
        })
        .await;
        let c = Contracts::new(&[dead.clone(), gnosis.clone(), capped], registry, None)
            .await
            .unwrap();
        assert_eq!(c.chain_id(), 100);
        assert_eq!(c.chain_blob_cap(), Some(2));
        // None answers: refused.
        let err = Contracts::new(std::slice::from_ref(&dead), registry, None)
            .await
            .unwrap_err();
        assert!(matches!(&err, Web3Error::Rpc(_)), "{err}");
    }
}
