//! JSON-RPC and beacon-API clients. Errors never contain a URL (it may embed an API key).
use alloy::network::AnyNetwork;
use alloy::primitives::Address;
use alloy::providers::{Provider, RootProvider};
use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[derive(Clone)]
pub struct Rpc {
    p: RootProvider<AnyNetwork>,
    url: String,
}

pub fn host_of(url: &str) -> String {
    let s = url.split("://").last().unwrap_or("");
    let s = s.split('/').next().unwrap_or("");
    s.split('@').last().unwrap_or("").to_string()
}

fn redact(url: &str, m: String) -> String {
    let mut m = m.replace(url, "<rpc>");
    if let Some(path) = url.split("://").last().and_then(|s| s.split_once('/')).map(|x| x.1) {
        if path.len() > 6 {
            m = m.replace(path, "<redacted>");
        }
    }
    m
}

impl Rpc {
    pub fn new(url: String) -> Result<Self> {
        let p = RootProvider::<AnyNetwork>::new_http(url.parse().map_err(|_| anyhow!("invalid RPC url"))?);
        Ok(Self { p, url })
    }
    pub fn host(&self) -> String {
        host_of(&self.url)
    }
    pub async fn call(&self, method: &'static str, params: Value) -> Result<Value> {
        self.p
            .raw_request::<Value, Value>(method.into(), params)
            .await
            .map_err(|e| anyhow!("{method}@{}: {}", self.host(), redact(&self.url, e.to_string())))
    }
    pub async fn chain_id(&self) -> Result<u64> {
        Ok(q(&self.call("eth_chainId", json!([])).await?).context("chain id")? as u64)
    }
    pub async fn nonce(&self, a: Address) -> Result<u64> {
        let v = self.call("eth_getTransactionCount", json!([a.to_string(), "pending"])).await?;
        Ok(q(&v).context("bad nonce")? as u64)
    }
    pub async fn balance(&self, a: Address) -> Result<u128> {
        q(&self.call("eth_getBalance", json!([a.to_string(), "latest"])).await?).context("balance")
    }
    pub async fn block_number(&self) -> Result<u64> {
        Ok(q(&self.call("eth_blockNumber", json!([])).await?).context("block number")? as u64)
    }
    pub async fn block(&self, tag: &str) -> Result<Value> {
        self.call("eth_getBlockByNumber", json!([tag, false])).await
    }
    pub async fn base_fee(&self) -> Result<u128> {
        let b = self.block("latest").await?;
        b.get("baseFeePerGas").and_then(q).context("no baseFeePerGas")
    }
    pub async fn priority_suggestion(&self) -> Result<u128> {
        q(&self.call("eth_maxPriorityFeePerGas", json!([])).await?).context("priority fee")
    }
    pub async fn blob_base_fee(&self) -> Result<u128> {
        q(&self.call("eth_blobBaseFee", json!([])).await?).context("blob base fee")
    }
    pub async fn send_raw(&self, raw: &[u8]) -> Result<String> {
        let v = self.call("eth_sendRawTransaction", json!([format!("0x{}", hex::encode(raw))])).await?;
        Ok(v.as_str().unwrap_or_default().to_string())
    }
    pub async fn eth_call(&self, to: Address, data: &[u8]) -> Result<Vec<u8>> {
        let v = self.call("eth_call", json!([{"to": to.to_string(), "data": format!("0x{}", hex::encode(data))}, "latest"])).await?;
        hex_bytes(&v)
    }
    /// Polls (not retries): an RPC error aborts, an absent receipt is waited for.
    pub async fn wait_receipt(&self, hash: &str, timeout: Duration) -> Result<Value> {
        let t0 = Instant::now();
        loop {
            let v = self.call("eth_getTransactionReceipt", json!([hash])).await?;
            if !v.is_null() {
                return Ok(v);
            }
            if t0.elapsed() > timeout {
                bail!("timeout waiting for receipt {hash} on {}", self.host());
            }
            tokio::time::sleep(Duration::from_millis(1000)).await;
        }
    }
}

pub fn q(v: &Value) -> Option<u128> {
    match v {
        Value::String(s) => match s.strip_prefix("0x") {
            Some(h) => u128::from_str_radix(h, 16).ok(),
            None => s.parse().ok(),
        },
        Value::Number(n) => n.as_u64().map(|x| x as u128),
        _ => None,
    }
}

pub fn hex_bytes(v: &Value) -> Result<Vec<u8>> {
    let s = v.as_str().context("expected hex string")?;
    Ok(hex::decode(s.trim_start_matches("0x"))?)
}

pub fn h32(v: &Value) -> Result<[u8; 32]> {
    hex_bytes(v)?.try_into().map_err(|_| anyhow!("expected 32 bytes"))
}

pub fn now_ms() -> u128 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis()
}

// ------------------------------------------------------------ beacon API

pub struct Beacon {
    c: reqwest::Client,
    base: String,
}

impl Beacon {
    pub fn new(base: String) -> Result<Self> {
        let c = reqwest::Client::builder().timeout(Duration::from_secs(60)).build()?;
        Ok(Self { c, base: base.trim_end_matches('/').to_string() })
    }
    pub fn host(&self) -> String {
        host_of(&self.base)
    }
    /// None on 404.
    pub async fn get(&self, path: &str) -> Result<Option<Value>> {
        let r = self
            .c
            .get(format!("{}{}", self.base, path))
            .header("accept", "application/json")
            .send()
            .await
            .map_err(|e| anyhow!("beacon {path}@{}: {}", self.host(), e.without_url()))?;
        if r.status().as_u16() == 404 {
            return Ok(None);
        }
        if !r.status().is_success() {
            bail!("beacon {path}@{}: HTTP {}", self.host(), r.status());
        }
        Ok(Some(r.json().await.map_err(|e| anyhow!("beacon {path}: bad JSON: {}", e.without_url()))?))
    }
    /// Polls until the path exists (the node may not have the block yet).
    pub async fn get_wait(&self, path: &str, timeout: Duration) -> Result<Value> {
        let t0 = Instant::now();
        loop {
            if let Some(v) = self.get(path).await? {
                return Ok(v);
            }
            if t0.elapsed() > timeout {
                bail!("beacon {path}@{}: still 404 after {timeout:?}", self.host());
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    }
}
