//! Blobs of a settled transition, from anvil or a beacon node. Every source
//! returns the transaction's first `n_blobs` blobs, in its order, each
//! checked against the transaction's versioned hash. `n_blobs` comes from
//! the `StateTransitioned` event: the registry opened exactly that many, so
//! a junk blob appended to the settling tx must not poison the sync.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use alloy::consensus::Transaction as _;
use alloy::eips::BlockNumberOrTag;
use alloy::eips::eip4844::env_settings::EnvKzgSettings;
use alloy::primitives::B256;
use alloy::providers::{DynProvider, Provider};
use async_trait::async_trait;
use davinci_zkvm_sdk::blob::{BLOB_SIZE, Blob, versioned_hash};
use serde::Deserialize;
use tokio::sync::OnceCell;
use url::Url;

use super::{Result, Web3Error, rpc_err};
use crate::config::{BlobSourceKind, Config};

/// Largest beacon response read (a full Osaka block of blobs in hex is ~12 MiB).
const MAX_BEACON_BODY: usize = 64 << 20;

#[async_trait]
pub trait BlobSource: Send + Sync {
    /// The first `n_blobs` blobs of `tx`, mined in `block`, verified against
    /// its versioned hashes. Extra blobs in the tx are ignored, fewer (or a
    /// zero count) is an error.
    async fn blobs_for_tx(&self, tx: B256, block: u64, n_blobs: u64) -> Result<Vec<Blob>>;
}

/// The source the configuration names, reading transactions from `cfg.rpc_url`.
pub fn blob_source(cfg: &Config) -> Arc<dyn BlobSource> {
    match &cfg.blob_source {
        BlobSourceKind::Anvil => Arc::new(AnvilBlobs::new(&cfg.rpc_url)),
        BlobSourceKind::Beacon(urls) => Arc::new(BeaconBlobs::new(urls.clone(), &cfg.rpc_url)),
    }
}

fn to_blob(b: &[u8]) -> Result<Blob> {
    b.to_vec()
        .into_boxed_slice()
        .try_into()
        .map_err(|_| Web3Error::Blob(format!("{} bytes, want {BLOB_SIZE}", b.len())))
}

// Versioned hash of a blob's KZG commitment; `None` if it is not a valid blob.
fn blob_versioned_hash(b: &Blob) -> Option<B256> {
    let kb = alloy::eips::eip4844::c_kzg::Blob::from_bytes(&b[..]).ok()?;
    let com = EnvKzgSettings::Default
        .get()
        .blob_to_kzg_commitment(&kb)
        .ok()?;
    Some(B256::from(versioned_hash(com.to_bytes().as_ref())))
}

/// Picks, for each versioned hash in order, the candidate that commits to it
/// (one commitment per candidate). Fails if any hash has no matching blob.
pub fn match_blobs(hashes: &[B256], candidates: Vec<Blob>) -> Result<Vec<Blob>> {
    let mut pool: Vec<(Option<B256>, Option<Blob>)> = candidates
        .into_iter()
        .map(|b| (blob_versioned_hash(&b), Some(b)))
        .collect();
    let mut out = Vec::with_capacity(hashes.len());
    for (i, h) in hashes.iter().enumerate() {
        let found = pool
            .iter_mut()
            .find(|(vh, b)| b.is_some() && vh.as_ref() == Some(h))
            .and_then(|(_, b)| b.take());
        match found {
            Some(b) => out.push(b),
            None => {
                return Err(Web3Error::Blob(format!(
                    "no blob matches versioned hash {i} ({h})"
                )));
            }
        }
    }
    Ok(out)
}

/// [`match_blobs`] on the blocking pool (KZG commitments are CPU work).
pub async fn match_blobs_blocking(hashes: Vec<B256>, candidates: Vec<Blob>) -> Result<Vec<Blob>> {
    tokio::task::spawn_blocking(move || match_blobs(&hashes, candidates))
        .await
        .map_err(|e| Web3Error::Blob(format!("blob match task: {e}")))?
}

// The first `n_blobs` versioned hashes of a blob transaction, which must be
// mined in `block` and carry at least that many blobs.
async fn tx_blob_hashes(p: &DynProvider, tx: B256, block: u64, n_blobs: u64) -> Result<Vec<B256>> {
    let t = p
        .get_transaction_by_hash(tx)
        .await
        .map_err(rpc_err)?
        .ok_or_else(|| Web3Error::Blob(format!("transaction {tx} not found")))?;
    if t.block_number != Some(block) {
        return Err(Web3Error::Blob(format!(
            "transaction {tx} is not in block {block}"
        )));
    }
    let mut hashes = t.inner.blob_versioned_hashes().unwrap_or_default().to_vec();
    if n_blobs == 0 || (hashes.len() as u64) < n_blobs {
        return Err(Web3Error::Blob(format!(
            "transaction {tx} carries {} blobs, the transition names {n_blobs}",
            hashes.len()
        )));
    }
    hashes.truncate(n_blobs as usize);
    Ok(hashes)
}

/// anvil's `anvil_getBlobsByTransactionHash`.
#[derive(Clone)]
pub struct AnvilBlobs {
    provider: DynProvider,
}

impl AnvilBlobs {
    pub fn new(rpcs: &[Url]) -> Self {
        AnvilBlobs {
            provider: super::rpc_provider(rpcs),
        }
    }
}

#[async_trait]
impl BlobSource for AnvilBlobs {
    async fn blobs_for_tx(&self, tx: B256, block: u64, n_blobs: u64) -> Result<Vec<Blob>> {
        let hashes = tx_blob_hashes(&self.provider, tx, block, n_blobs).await?;
        // Deserialize into heap `Bytes`, not `eip4844::Blob` (a 128 KiB
        // FixedBytes): the by-value array through serde's frames overflows
        // a 2 MiB tokio worker stack in debug builds.
        let raw: Option<Vec<alloy::primitives::Bytes>> = self
            .provider
            .raw_request("anvil_getBlobsByTransactionHash".into(), (tx,))
            .await
            .map_err(rpc_err)?;
        let raw = raw.ok_or_else(|| Web3Error::Blob(format!("anvil has no blobs for {tx}")))?;
        let candidates = raw
            .iter()
            .map(|b| to_blob(b.as_ref()))
            .collect::<Result<Vec<_>>>()?;
        match_blobs_blocking(hashes, candidates).await
    }
}

/// Consensus-layer beacon APIs, with the RPC failover rule: requests go to the
/// last one that worked and move on only on a transport error, 429 or 5xx (a
/// 404 for a slot is an answer). The slot of a block is
/// `(timestamp - genesis_time) / SECONDS_PER_SLOT`, as davinci-node derives it.
pub struct BeaconBlobs {
    bases: Vec<Url>,
    current: AtomicUsize,
    http: reqwest::Client,
    provider: Option<DynProvider>,
    timing: OnceCell<(u64, u64)>,
}

#[derive(Deserialize)]
struct Data<T> {
    data: T,
}

#[derive(Deserialize)]
struct Genesis {
    genesis_time: String,
}

#[derive(Deserialize)]
struct Sidecar {
    blob: String,
    kzg_commitment: String,
}

fn hex_bytes(s: &str) -> Result<Vec<u8>> {
    hex::decode(s.trim_start_matches("0x")).map_err(|_| Web3Error::Blob("beacon: bad hex".into()))
}

impl BeaconBlobs {
    /// `bases` are the beacon APIs in order of preference, `rpcs` the
    /// execution nodes the transactions and block timestamps are read from.
    pub fn new(bases: Vec<Url>, rpcs: &[Url]) -> Self {
        BeaconBlobs {
            provider: Some(super::rpc_provider(rpcs)),
            ..Self::beacon_only(bases)
        }
    }

    /// Without an execution node: only [`BeaconBlobs::blobs_at`] works.
    pub fn beacon_only(bases: Vec<Url>) -> Self {
        let http = reqwest::Client::builder()
            .user_agent(super::USER_AGENT)
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(60))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        BeaconBlobs {
            bases,
            current: AtomicUsize::new(0),
            http,
            provider: None,
            timing: OnceCell::new(),
        }
    }

    // GET `path` with `query` from the current beacon, failing over to the
    // next (each at most once); capped body, returned with its status. A 404
    // (slot pruned here) asks the next beacon once without moving: an archive
    // beacon may still serve it.
    async fn get(&self, path: &str, query: &[(&str, String)]) -> Result<(u16, Vec<u8>)> {
        let n = self.bases.len();
        let url = |i: usize| -> Result<Url> {
            let base = self.bases[i].as_str().trim_end_matches('/');
            let mut url = Url::parse(&format!("{base}{path}"))
                .map_err(|e| Web3Error::Config(e.to_string()))?;
            for (k, v) in query {
                url.query_pairs_mut().append_pair(k, v);
            }
            Ok(url)
        };
        let start = self.current.load(Ordering::Acquire);
        let mut last = Web3Error::Config("no beacon URL configured".into());
        for k in 0..n {
            let i = (start + k) % n;
            let host = super::failover::host(&self.bases[i]);
            let why = match self.get_one(url(i)?).await {
                Ok((404, body)) if k == 0 && n > 1 => {
                    return match self.get_one(url((i + 1) % n)?).await {
                        Ok((200, alt)) => Ok((200, alt)),
                        _ => Ok((404, body)),
                    };
                }
                Ok((st, body)) if st != 429 && st < 500 => return Ok((st, body)),
                Ok((st, _)) => format!("status {st}"),
                Err(e) => e,
            };
            last = Web3Error::Blob(format!("beacon {host}{path}: {why}"));
            if n > 1 {
                let next = (i + 1) % n;
                if self
                    .current
                    .compare_exchange(i, next, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
                {
                    tracing::warn!(from = %host, to = %super::failover::host(&self.bases[next]),
                        error = %why, "beacon failover");
                }
            }
        }
        Err(last)
    }

    // One GET; errors are the failover kind (transport, oversized body).
    async fn get_one(&self, url: Url) -> std::result::Result<(u16, Vec<u8>), String> {
        let e = |e: reqwest::Error| super::failover::describe(&e.without_url());
        let mut resp = self
            .http
            .get(url)
            .header("accept", "application/json")
            .send()
            .await
            .map_err(e)?;
        let status = resp.status().as_u16();
        let mut body = Vec::new();
        while let Some(chunk) = resp.chunk().await.map_err(e)? {
            if body.len() + chunk.len() > MAX_BEACON_BODY {
                return Err("response too large".into());
            }
            body.extend_from_slice(&chunk);
        }
        Ok((status, body))
    }

    async fn get_json<T: for<'de> Deserialize<'de>>(&self, path: &str) -> Result<T> {
        let (status, body) = self.get(path, &[]).await?;
        if status != 200 {
            return Err(Web3Error::Blob(format!("beacon {path}: status {status}")));
        }
        serde_json::from_slice(&body).map_err(|e| Web3Error::Blob(format!("beacon {path}: {e}")))
    }

    async fn timing(&self) -> Result<(u64, u64)> {
        self.timing
            .get_or_try_init(|| async {
                let g: Data<Genesis> = self.get_json("/eth/v1/beacon/genesis").await?;
                let genesis = g
                    .data
                    .genesis_time
                    .parse::<u64>()
                    .map_err(|_| Web3Error::Blob("beacon: bad genesis_time".into()))?;
                let spec: Data<serde_json::Value> = self.get_json("/eth/v1/config/spec").await?;
                let secs = spec.data["SECONDS_PER_SLOT"]
                    .as_str()
                    .and_then(|s| s.parse::<u64>().ok())
                    .filter(|s| *s > 0)
                    .ok_or_else(|| Web3Error::Blob("beacon: bad SECONDS_PER_SLOT".into()))?;
                Ok((genesis, secs))
            })
            .await
            .copied()
    }

    /// Slot of a block with this timestamp.
    pub async fn slot_for(&self, timestamp: u64) -> Result<u64> {
        let (genesis, secs) = self.timing().await?;
        let since = timestamp
            .checked_sub(genesis)
            .ok_or_else(|| Web3Error::Blob("block before the beacon genesis".into()))?;
        Ok(since / secs)
    }

    /// Blobs matching `hashes` from the block at `timestamp`. Uses
    /// `/eth/v1/beacon/blobs/{slot}` and falls back to `blob_sidecars`.
    pub async fn blobs_at(&self, timestamp: u64, hashes: &[B256]) -> Result<Vec<Blob>> {
        let slot = self.slot_for(timestamp).await?;
        let query: Vec<_> = hashes
            .iter()
            .map(|h| ("versioned_hashes", h.to_string()))
            .collect();
        let (status, body) = self
            .get(&format!("/eth/v1/beacon/blobs/{slot}"), &query)
            .await?;
        if status == 200 {
            let d: Data<Vec<String>> = serde_json::from_slice(&body)
                .map_err(|e| Web3Error::Blob(format!("beacon blobs: {e}")))?;
            let candidates = d
                .data
                .iter()
                .map(|s| to_blob(&hex_bytes(s)?))
                .collect::<Result<Vec<_>>>()?;
            return match_blobs_blocking(hashes.to_vec(), candidates).await;
        }
        let path = format!("/eth/v1/beacon/blob_sidecars/{slot}");
        let (status2, body) = self.get(&path, &[]).await?;
        if status2 != 200 {
            return Err(Web3Error::Blob(format!(
                "beacon: blobs {status}, blob_sidecars {status2} for slot {slot}"
            )));
        }
        let d: Data<Vec<Sidecar>> = serde_json::from_slice(&body)
            .map_err(|e| Web3Error::Blob(format!("beacon blob_sidecars: {e}")))?;
        // Keep the sidecars whose claimed commitment is one we want; the blob
        // itself is still checked against the hash in `match_blobs`.
        let mut candidates = Vec::new();
        for s in &d.data {
            let com: [u8; 48] = hex_bytes(&s.kzg_commitment)?
                .try_into()
                .map_err(|_| Web3Error::Blob("beacon: commitment is not 48 bytes".into()))?;
            if hashes.contains(&B256::from(versioned_hash(&com))) {
                candidates.push(to_blob(&hex_bytes(&s.blob)?)?);
            }
        }
        match_blobs_blocking(hashes.to_vec(), candidates).await
    }
}

#[async_trait]
impl BlobSource for BeaconBlobs {
    async fn blobs_for_tx(&self, tx: B256, block: u64, n_blobs: u64) -> Result<Vec<Blob>> {
        let p = self
            .provider
            .as_ref()
            .ok_or_else(|| Web3Error::Config("beacon source without an execution node".into()))?;
        let hashes = tx_blob_hashes(p, tx, block, n_blobs).await?;
        let b = p
            .get_block_by_number(BlockNumberOrTag::Number(block))
            .await
            .map_err(rpc_err)?
            .ok_or_else(|| Web3Error::Blob(format!("block {block} not found")))?;
        self.blobs_at(b.header.timestamp, &hashes).await
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use axum::extract::{Path, RawQuery, State};
    use axum::http::StatusCode;
    use axum::response::IntoResponse;
    use axum::{Json, Router, routing::get};
    use davinci_zkvm_sdk::ballot::Ballot;
    use davinci_zkvm_sdk::blob::{TransitionData, build_blobs};
    use davinci_zkvm_sdk::crypto::field::Fr;
    use davinci_zkvm_sdk::limits::VOTE_ID_MIN;
    use serde_json::json;

    use super::*;

    // Three real blobs (two of one transition, one of another).
    fn fixture() -> (Vec<Blob>, Vec<B256>, Blob) {
        let t = TransitionData {
            vote_ids: (0..500).map(|i| VOTE_ID_MIN + i).collect(),
            updates: (0..500).map(|i| (0x10 + i, Ballot::identity())).collect(),
            accumulator: Ballot::identity(),
            num_fields: 4,
        };
        let b = build_blobs(&t, &Fr::from(7u64), &[1u8; 32]).unwrap();
        assert_eq!(b.blobs.len(), 2);
        let other = TransitionData {
            vote_ids: vec![VOTE_ID_MIN + 1],
            updates: vec![],
            accumulator: Ballot::identity(),
            num_fields: 1,
        };
        let o = build_blobs(&other, &Fr::from(7u64), &[2u8; 32]).unwrap();
        let hashes = b.versioned_hashes.iter().map(|h| B256::from(*h)).collect();
        (b.blobs, hashes, o.blobs[0].clone())
    }

    #[test]
    fn match_blobs_orders_and_verifies() {
        let (blobs, hashes, other) = fixture();
        // Out of order with an unrelated blob in the pool.
        let got = match_blobs(
            &hashes,
            vec![other.clone(), blobs[1].clone(), blobs[0].clone()],
        )
        .unwrap();
        assert_eq!(got, blobs);
        // A missing blob fails.
        assert!(match_blobs(&hashes, vec![blobs[0].clone(), other.clone()]).is_err());
        // A tampered blob does not match its versioned hash.
        let mut bad = blobs[1].clone();
        bad[100] ^= 1;
        assert!(match_blobs(&hashes, vec![blobs[0].clone(), bad]).is_err());
        // One blob cannot answer for two hashes.
        assert!(match_blobs(&[hashes[0], hashes[0]], vec![blobs[0].clone()]).is_err());
    }

    #[derive(Clone)]
    struct Beacon {
        pool: Vec<Blob>,
        blobs_endpoint: bool,
        // Commitments the sidecars claim; the real ones when empty.
        claims: Vec<[u8; 48]>,
        seen: Arc<Mutex<Vec<String>>>,
    }

    fn hexs(b: &[u8]) -> String {
        format!("0x{}", hex::encode(b))
    }

    async fn serve(b: Beacon) -> Url {
        async fn genesis() -> Json<serde_json::Value> {
            Json(json!({"data": {"genesis_time": "1000", "genesis_fork_version": "0x00000000"}}))
        }
        async fn spec() -> Json<serde_json::Value> {
            Json(json!({"data": {"SECONDS_PER_SLOT": "12"}}))
        }
        async fn blobs(
            State(b): State<Beacon>,
            Path(slot): Path<String>,
            RawQuery(q): RawQuery,
        ) -> axum::response::Response {
            b.seen
                .lock()
                .unwrap()
                .push(format!("blobs/{slot}?{}", q.unwrap_or_default()));
            if !b.blobs_endpoint {
                return StatusCode::NOT_FOUND.into_response();
            }
            let data: Vec<String> = b.pool.iter().map(|x| hexs(&x[..])).collect();
            Json(json!({"execution_optimistic": false, "finalized": true, "data": data}))
                .into_response()
        }
        async fn sidecars(
            State(b): State<Beacon>,
            Path(slot): Path<String>,
        ) -> axum::response::Response {
            b.seen.lock().unwrap().push(format!("blob_sidecars/{slot}"));
            let data: Vec<_> = b
                .pool
                .iter()
                .enumerate()
                .map(|(i, x)| {
                    let com = b.claims.get(i).copied().unwrap_or_else(|| blob_commitment(x));
                    json!({"index": i.to_string(), "blob": hexs(&x[..]), "kzg_commitment": hexs(&com), "kzg_proof": hexs(&[0u8; 48])})
                })
                .collect();
            Json(json!({"data": data})).into_response()
        }
        let app = Router::new()
            .route("/eth/v1/beacon/genesis", get(genesis))
            .route("/eth/v1/config/spec", get(spec))
            .route("/eth/v1/beacon/blobs/:slot", get(blobs))
            .route("/eth/v1/beacon/blob_sidecars/:slot", get(sidecars))
            .with_state(b);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Url::parse(&format!("http://{addr}/")).unwrap()
    }

    // Commitment of a blob via c-kzg (test helper only).
    fn blob_commitment(b: &Blob) -> [u8; 48] {
        let s = alloy::eips::eip4844::env_settings::EnvKzgSettings::Default.get();
        let kb = alloy::eips::eip4844::c_kzg::Blob::from_bytes(&b[..]).unwrap();
        *s.blob_to_kzg_commitment(&kb).unwrap().to_bytes()
    }

    #[tokio::test]
    async fn beacon_blobs_endpoint() {
        let (blobs, hashes, other) = fixture();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let url = serve(Beacon {
            pool: vec![other, blobs[1].clone(), blobs[0].clone()],
            blobs_endpoint: true,
            claims: vec![],
            seen: seen.clone(),
        })
        .await;
        let src = BeaconBlobs::beacon_only(vec![url]);
        assert_eq!(src.slot_for(1000 + 12 * 7 + 5).await.unwrap(), 7);
        assert!(src.slot_for(999).await.is_err());
        let got = src.blobs_at(1000 + 12 * 42, &hashes).await.unwrap();
        assert_eq!(got, blobs);
        let s = seen.lock().unwrap().clone();
        assert_eq!(s.len(), 1);
        assert!(s[0].starts_with("blobs/42?versioned_hashes=0x"), "{}", s[0]);
        assert_eq!(s[0].matches("versioned_hashes=").count(), 2);
    }

    #[tokio::test]
    async fn beacon_falls_back_to_sidecars() {
        let (blobs, hashes, other) = fixture();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let url = serve(Beacon {
            pool: vec![blobs[0].clone(), other, blobs[1].clone()],
            blobs_endpoint: false,
            claims: vec![],
            seen: seen.clone(),
        })
        .await;
        let src = BeaconBlobs::beacon_only(vec![url]);
        let got = src.blobs_at(1000 + 12 * 3, &hashes).await.unwrap();
        assert_eq!(got, blobs);
        let s = seen.lock().unwrap().clone();
        assert_eq!(s[1], "blob_sidecars/3");
    }

    #[tokio::test]
    async fn beacon_rejects_wrong_blobs() {
        let (blobs, hashes, other) = fixture();
        let mut bad = blobs[1].clone();
        bad[64] ^= 1;
        // The sidecar variant also lies about the tampered blob's commitment.
        let honest = vec![blob_commitment(&blobs[0]), blob_commitment(&blobs[1])];
        for (endpoint, claims) in [(true, vec![]), (false, vec![]), (false, honest)] {
            let url = serve(Beacon {
                pool: vec![blobs[0].clone(), bad.clone(), other.clone()],
                blobs_endpoint: endpoint,
                claims,
                seen: Arc::new(Mutex::new(Vec::new())),
            })
            .await;
            let src = BeaconBlobs::beacon_only(vec![url]);
            assert!(
                src.blobs_at(1000, &hashes).await.is_err(),
                "blobs endpoint {endpoint}"
            );
        }
        // A beacon source without an execution node cannot resolve a tx.
        let src = BeaconBlobs::beacon_only(vec![Url::parse("http://127.0.0.1:1/").unwrap()]);
        assert!(src.blobs_for_tx(B256::ZERO, 1, 1).await.is_err());
    }

    // A beacon answering every request with `status`; counts requests.
    async fn broken(status: StatusCode) -> (Url, Arc<Mutex<usize>>) {
        let n = Arc::new(Mutex::new(0));
        let c = n.clone();
        let app = Router::new().fallback(move || async move {
            *c.lock().unwrap() += 1;
            status
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (Url::parse(&format!("http://{addr}/key/x?k=1")).unwrap(), n)
    }

    #[tokio::test]
    async fn beacon_fails_over_and_sticks() {
        let (blobs, hashes, _) = fixture();
        let good = |seen| Beacon {
            pool: blobs.clone(),
            blobs_endpoint: true,
            claims: vec![],
            seen,
        };
        let seen = Arc::new(Mutex::new(Vec::new()));
        let url = serve(good(seen.clone())).await;
        for status in [
            StatusCode::SERVICE_UNAVAILABLE,
            StatusCode::TOO_MANY_REQUESTS,
        ] {
            let (bad, hits) = broken(status).await;
            let src = BeaconBlobs::beacon_only(vec![bad, url.clone()]);
            assert_eq!(src.blobs_at(1000 + 12 * 9, &hashes).await.unwrap(), blobs);
            assert_eq!(src.blobs_at(1000 + 12 * 9, &hashes).await.unwrap(), blobs);
            // Moved on at the first request and stayed.
            assert_eq!(*hits.lock().unwrap(), 1, "{status}");
            assert_eq!(src.current.load(Ordering::Relaxed), 1);
        }
        // Unreachable counts too.
        let dead = Url::parse("http://127.0.0.1:1/").unwrap();
        let src = BeaconBlobs::beacon_only(vec![dead, url.clone()]);
        assert_eq!(src.blobs_at(1000, &hashes).await.unwrap(), blobs);
        // A 404 (pruned slot) is served by the next beacon without moving.
        let (missing, hits) = broken(StatusCode::NOT_FOUND).await;
        let src = BeaconBlobs::beacon_only(vec![missing, url.clone()]);
        assert_eq!(src.blobs_at(1000, &hashes).await.unwrap(), blobs);
        assert_eq!(src.current.load(Ordering::Relaxed), 0);
        let h = *hits.lock().unwrap();
        assert_eq!(src.blobs_at(1000, &hashes).await.unwrap(), blobs);
        assert!(
            *hits.lock().unwrap() > h,
            "still asks the current beacon first"
        );
        // Two missing: the 404 stands, nothing moves.
        let (a, a_hits) = broken(StatusCode::NOT_FOUND).await;
        let (b, b_hits) = broken(StatusCode::NOT_FOUND).await;
        let before = seen.lock().unwrap().len();
        let src = BeaconBlobs::beacon_only(vec![a, b, url]);
        assert!(src.blobs_at(1000, &hashes).await.is_err());
        // Genesis failed after one try each; the third beacon is not asked.
        assert_eq!((*a_hits.lock().unwrap(), *b_hits.lock().unwrap()), (1, 1));
        assert_eq!(src.current.load(Ordering::Relaxed), 0);
        assert_eq!(seen.lock().unwrap().len(), before);
    }
}
