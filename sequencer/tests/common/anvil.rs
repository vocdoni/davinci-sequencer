//! `OwnedCensus` on a local anvil, for the `ANVIL=1` census tests. Needs
//! `forge build` in `DAVINCI_CENSUS_CONTRACT_DIR` (default: the
//! davinci-onchain-census-contract checkout beside this repo, then in $HOME).

use std::path::PathBuf;

use alloy::network::{EthereumWallet, TransactionBuilder};
use alloy::node_bindings::{Anvil, AnvilInstance};
use alloy::primitives::{Address, Bytes, U256, aliases::U88};
use alloy::providers::{DynProvider, Provider, ProviderBuilder};
use alloy::rpc::types::TransactionRequest;
use alloy::signers::local::PrivateKeySigner;

alloy::sol!(
    #[sol(rpc)]
    OwnedCensus,
    "abi/OwnedCensus.json"
);

pub fn enabled() -> bool {
    std::env::var("ANVIL").is_ok_and(|v| v == "1")
}

fn census_dir() -> PathBuf {
    if let Ok(d) = std::env::var("DAVINCI_CENSUS_CONTRACT_DIR") {
        return d.into();
    }
    let sibling =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../davinci-onchain-census-contract");
    if sibling.exists() {
        return sibling;
    }
    PathBuf::from(std::env::var("HOME").unwrap_or_default()).join("davinci-onchain-census-contract")
}

fn anvil_bin() -> PathBuf {
    let p = PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".foundry/bin/anvil");
    if p.exists() {
        p
    } else {
        PathBuf::from("anvil")
    }
}

fn artifact(name: &str) -> serde_json::Value {
    let path = census_dir().join("out").join(name);
    serde_json::from_slice(
        &std::fs::read(&path)
            .unwrap_or_else(|e| panic!("{}: {e}; run forge build", path.display())),
    )
    .unwrap()
}

async fn deploy(p: &DynProvider, code: &str) -> Address {
    let code = hex::decode(code.trim_start_matches("0x")).unwrap();
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

/// A running anvil with a deployed `OwnedCensus` owned by its first key.
pub struct CensusChain {
    pub anvil: AnvilInstance,
    /// Signing provider (the census owner).
    pub owner: DynProvider,
    /// Plain read provider, what the index is given.
    pub reader: DynProvider,
    pub census: Address,
}

impl CensusChain {
    pub async fn start() -> Self {
        let anvil = Anvil::at(anvil_bin()).spawn();
        let signer: PrivateKeySigner = anvil.keys()[0].clone().into();
        // Simple nonces: the reorg test rewinds the chain under the provider.
        let owner = ProviderBuilder::new()
            .with_simple_nonce_management()
            .wallet(EthereumWallet::from(signer))
            .connect_http(anvil.endpoint_url())
            .erased();
        let reader = ProviderBuilder::new()
            .disable_recommended_fillers()
            .connect_http(anvil.endpoint_url())
            .erased();

        // KeccakLeanIMT is an internal library: the bytecode has no link
        // references and deploys as is (no PoseidonT3).
        let owned = artifact("OwnedCensus.sol/OwnedCensus.json");
        let code = owned["bytecode"]["object"].as_str().unwrap();
        assert_eq!(
            owned["bytecode"]["linkReferences"],
            serde_json::json!({}),
            "OwnedCensus must not link a library"
        );
        let census = deploy(&owner, code).await;
        CensusChain {
            anvil,
            owner,
            reader,
            census,
        }
    }

    pub fn contract(&self) -> OwnedCensus::OwnedCensusInstance<DynProvider> {
        OwnedCensus::new(self.census, self.owner.clone())
    }

    /// Adds `members` in one transaction (one block).
    pub async fn add(&self, members: &[([u8; 20], u128)]) {
        let users = members.iter().map(|(a, _)| Address::from(*a)).collect();
        let weights = members.iter().map(|(_, w)| U88::from(*w)).collect();
        let r = self
            .contract()
            .addMembers(users, weights)
            .send()
            .await
            .unwrap()
            .get_receipt()
            .await
            .unwrap();
        assert!(r.status(), "addMembers reverted");
    }

    /// The contract's root (BE) and tree size.
    pub async fn state(&self) -> ([u8; 32], u64) {
        let c = self.contract();
        let root: U256 = c.getCensusRoot().call().await.unwrap();
        let size: U256 = c.treeSize().call().await.unwrap();
        (root.to_be_bytes(), size.to::<u64>())
    }

    pub async fn head(&self) -> u64 {
        self.reader.get_block_number().await.unwrap()
    }
}

/// Knobs of an [`RpcProxy`].
#[derive(Default)]
pub struct ProxyKnobs {
    /// Answer every request with 503.
    pub down: std::sync::atomic::AtomicBool,
    /// Refuse `eth_getLogs` wider than this many blocks (0: no cap).
    pub max_range: std::sync::atomic::AtomicU64,
    /// `eth_getLogs` calls seen.
    pub get_logs: std::sync::atomic::AtomicU64,
    /// Lowest `fromBlock` served.
    pub min_from: std::sync::atomic::AtomicU64,
    /// A backend `lag` blocks behind: `eth_blockNumber` answers head - lag
    /// and `eth_getLogs` is answered as if the chain ended there.
    pub lag: std::sync::atomic::AtomicU64,
    /// Answer the `finalized` block tag with a JSON-RPC error, as an
    /// endpoint without finality does.
    pub refuse_finalized: std::sync::atomic::AtomicBool,
    /// Answer the `finalized` tag with a -32005 "limit exceeded" error.
    pub limit_finalized: std::sync::atomic::AtomicBool,
}

/// A JSON-RPC proxy in front of anvil that can go down, cap the
/// `eth_getLogs` range the way hosted providers do, or refuse the
/// `finalized` tag.
pub struct RpcProxy {
    pub url: String,
    pub knobs: std::sync::Arc<ProxyKnobs>,
}

impl RpcProxy {
    pub async fn start(upstream: String) -> Self {
        use std::sync::atomic::Ordering::Relaxed;
        let knobs = std::sync::Arc::new(ProxyKnobs::default());
        knobs.min_from.store(u64::MAX, Relaxed);
        let (k, http) = (knobs.clone(), reqwest::Client::new());
        let app = axum::Router::new().route(
            "/",
            axum::routing::post(move |body: axum::body::Bytes| {
                let (k, http, upstream) = (k.clone(), http.clone(), upstream.clone());
                async move {
                    use axum::http::{StatusCode, header};
                    use axum::response::IntoResponse;
                    if k.down.load(Relaxed) {
                        return (StatusCode::SERVICE_UNAVAILABLE, "down").into_response();
                    }
                    let mut req: serde_json::Value = serde_json::from_slice(&body).unwrap();
                    if k.limit_finalized.load(Relaxed)
                        && req["method"] == "eth_getBlockByNumber"
                        && req["params"][0] == "finalized"
                    {
                        let err = serde_json::json!({
                            "jsonrpc": "2.0",
                            "id": req["id"],
                            "error": {"code": -32005, "message": "daily request count exceeded"},
                        });
                        return axum::Json(err).into_response();
                    }
                    if k.refuse_finalized.load(Relaxed)
                        && req["method"] == "eth_getBlockByNumber"
                        && req["params"][0] == "finalized"
                    {
                        let err = serde_json::json!({
                            "jsonrpc": "2.0",
                            "id": req["id"],
                            "error": {"code": -32602, "message": "invalid block tag: finalized"},
                        });
                        return axum::Json(err).into_response();
                    }
                    let lag = k.lag.load(Relaxed);
                    let hex = |v: u64| format!("0x{v:x}");
                    let num = |s: &str| u64::from_str_radix(s.trim_start_matches("0x"), 16).ok();
                    let mut body = body;
                    if lag > 0 && req["method"] == "eth_getLogs" {
                        let to = req["params"][0]["toBlock"].as_str().and_then(num);
                        let from = req["params"][0]["fromBlock"].as_str().and_then(num);
                        if let (Some(to), Some(from)) = (to, from) {
                            let to = to.saturating_sub(lag);
                            if to < from {
                                let r = serde_json::json!({"jsonrpc": "2.0", "id": req["id"], "result": []});
                                return axum::Json(r).into_response();
                            }
                            req["params"][0]["toBlock"] = hex(to).into();
                            body = serde_json::to_vec(&req).unwrap().into();
                        }
                    }
                    if req["method"] == "eth_getLogs" {
                        let block = |f: &str| {
                            let s = req["params"][0][f].as_str().unwrap();
                            u64::from_str_radix(s.trim_start_matches("0x"), 16).unwrap()
                        };
                        let (from, to) = (block("fromBlock"), block("toBlock"));
                        k.get_logs.fetch_add(1, Relaxed);
                        let cap = k.max_range.load(Relaxed);
                        if cap > 0 && to - from + 1 > cap {
                            let err = serde_json::json!({
                                "jsonrpc": "2.0",
                                "id": req["id"],
                                "error": {"code": -32005, "message": "block range too large"},
                            });
                            return axum::Json(err).into_response();
                        }
                        k.min_from.fetch_min(from, Relaxed);
                    }
                    let r = http
                        .post(&upstream)
                        .header(header::CONTENT_TYPE, "application/json")
                        .body(body)
                        .send()
                        .await
                        .unwrap();
                    let mut body = r.bytes().await.unwrap();
                    if lag > 0 && req["method"] == "eth_blockNumber" {
                        let mut v: serde_json::Value = serde_json::from_slice(&body).unwrap();
                        if let Some(n) = v["result"].as_str().and_then(num) {
                            v["result"] = hex(n.saturating_sub(lag)).into();
                            body = serde_json::to_vec(&v).unwrap().into();
                        }
                    }
                    ([(header::CONTENT_TYPE, "application/json")], body).into_response()
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        RpcProxy { url, knobs }
    }

    /// A read provider through the proxy.
    pub fn provider(&self) -> DynProvider {
        ProviderBuilder::new()
            .disable_recommended_fillers()
            .connect_http(self.url.parse().unwrap())
            .erased()
    }
}
