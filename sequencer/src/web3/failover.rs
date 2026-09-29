//! Sticky RPC failover. Every request goes to the current endpoint; a
//! node-side failure (transport error, any non-2xx status such as 429/5xx or
//! 401/403/413, an unparsable 2xx body, a [`NODE_SIDE`] JSON-RPC error) moves
//! `current` to the next one, retries there (each endpoint at most once per
//! request, bar rate limits) and stays: a bad key or a proxy error page on
//! one provider says nothing about the next. A [`DETERMINISTIC`] error never
//! does. No per-call rotation: mixing providers at different heads could make
//! a lagging one's `eth_getLogs` up to our confirmed head silently miss events.
//!
//! A rate limit (429, or a [`RATE_LIMITED`] JSON-RPC error) also rests that
//! endpoint, for its `Retry-After` or a doubling backoff. Requests skip
//! resting endpoints, and when every one rests they wait for the first back
//! (up to [`MAX_RATE_WAIT`]) instead of failing or asking again.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use alloy::rpc::json_rpc::{RequestPacket, ResponsePacket};
use alloy::transports::http::reqwest;
use alloy::transports::{TransportError, TransportErrorKind, TransportFut};
use tokio::time::Instant;
use tower::Service;
use url::Url;

/// JSON-RPC errors that mean "this node cannot serve it now", matched
/// case-insensitively; they fail over, as does code -32603. Deterministic
/// errors (reverts, nonce, funds, underpriced, invalid params) never do, see
/// [`DETERMINISTIC`]. No bare "limit exceeded": a too-wide `eth_getLogs`
/// range is the caller's to halve.
const NODE_SIDE: &[&str] = &[
    "historical state",
    "header not found",
    "missing trie node",
    "rate limit",
    "too many requests",
    "request timeout",
    "internal error",
    // Nethermind (Gnosis): -32002 "No state available for block ...".
    "no state available",
    // Pocket: calls tagged or defaulted to "pending".
    "pending state is not supported",
    // Pruned public endpoints refusing older ranges, e.g. blockreq's -32601
    // "public endpoint only serves recent blocks (last 1024)".
    "recent blocks",
    "archive",
];

/// JSON-RPC errors that mean "you are asking too often"; they rest the
/// endpoint like an HTTP 429 (and fail over, being in [`NODE_SIDE`]).
const RATE_LIMITED: &[&str] = &["rate limit", "too many requests"];

/// First rest of a rate-limited endpoint without `Retry-After`; it doubles
/// on each limit in a row, up to [`MAX_BACKOFF`].
const RATE_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(60);
/// The longest `Retry-After` honoured.
const MAX_RETRY_AFTER: Duration = Duration::from_secs(300);
/// Longest one request waits for a resting endpoint before it fails. Short:
/// the process actors and the monitor await some reads inline.
const MAX_RATE_WAIT: Duration = Duration::from_secs(10);

/// Answers another endpoint would give too; they win over [`NODE_SIDE`] and
/// -32603 (some clients wrap a revert in an internal error).
const DETERMINISTIC: &[&str] = &[
    "revert",
    "nonce too",
    "already known",
    "insufficient funds",
    "underpriced",
    "invalid",
];

/// An error with its causes; strip reqwest's URL first, it may carry a key.
pub(crate) fn describe(e: &dyn std::error::Error) -> String {
    let mut s = e.to_string();
    let mut src = e.source();
    while let Some(c) = src {
        s = format!("{s}: {c}");
        src = c.source();
    }
    s
}

/// Host of an endpoint for logs: URLs may carry API keys in the path or query.
pub(crate) fn host(u: &Url) -> String {
    match (u.host_str(), u.port()) {
        (Some(h), Some(p)) => format!("{h}:{p}"),
        (Some(h), None) => h.to_string(),
        _ => "?".into(),
    }
}

fn is_deterministic(msg: &str) -> bool {
    let m = msg.to_ascii_lowercase();
    DETERMINISTIC.iter().any(|s| m.contains(s))
}

/// Whether an answer carries an error every endpoint would give.
fn deterministic(p: &ResponsePacket) -> bool {
    p.iter_errors().any(|e| is_deterministic(&e.message))
}

/// A JSON-RPC error meaning "this node cannot serve it now".
pub(super) fn is_node_side(code: i64, message: &str) -> bool {
    let m = message.to_ascii_lowercase();
    !is_deterministic(&m) && (code == -32603 || NODE_SIDE.iter().any(|s| m.contains(s)))
}

/// Why a JSON-RPC answer should fail over, if it should.
fn node_side(p: &ResponsePacket) -> Option<String> {
    p.iter_errors()
        .find(|e| is_node_side(e.code, &e.message))
        .map(|e| format!("{} {}", e.code, e.message))
}

/// Whether an answer says the endpoint is rate-limiting us.
fn rate_limited(p: &ResponsePacket) -> bool {
    p.iter_errors().any(|e| {
        let m = e.message.to_ascii_lowercase();
        !is_deterministic(&m) && RATE_LIMITED.iter().any(|s| m.contains(s))
    })
}

/// An endpoint's rate-limit rest.
#[derive(Default)]
struct Rest {
    /// No request goes there before this.
    until: Option<Instant>,
    /// Limits in a row, for the backoff.
    streak: u32,
}

struct Endpoint {
    url: Url,
    host: String,
    rest: Mutex<Rest>,
}

impl Endpoint {
    /// When it can take requests again, if it is resting now.
    fn resting(&self, now: Instant) -> Option<Instant> {
        let r = self.rest.lock().unwrap_or_else(|e| e.into_inner());
        r.until.filter(|t| *t > now)
    }

    /// Rests it after a rate limit: `retry_after`, or the backoff. A limit
    /// hit by a request sent before the rest began does not double it.
    fn limit(&self, retry_after: Option<Duration>) {
        let now = Instant::now();
        let mut r = self.rest.lock().unwrap_or_else(|e| e.into_inner());
        let resting = r.until.is_some_and(|t| t > now);
        if !resting {
            r.streak = r.streak.saturating_add(1);
        }
        let backoff = RATE_BACKOFF
            .saturating_mul(1 << r.streak.saturating_sub(1).min(6))
            .min(MAX_BACKOFF);
        let rest = retry_after.map_or(backoff, |d| d.clamp(RATE_BACKOFF, MAX_RETRY_AFTER));
        r.until = r.until.max(Some(now + rest));
    }

    /// A request sent at `sent` got its answer: the limit is over, unless
    /// the rest began after that request went out.
    fn answered(&self, sent: Instant) {
        let mut r = self.rest.lock().unwrap_or_else(|e| e.into_inner());
        if r.until.is_none_or(|t| t <= sent) {
            *r = Rest::default();
        }
    }
}

/// `Retry-After` in seconds; the HTTP-date form falls back to the backoff.
fn retry_after(h: &reqwest::header::HeaderMap) -> Option<Duration> {
    let v = h.get(reqwest::header::RETRY_AFTER)?.to_str().ok()?;
    v.trim().parse().ok().map(Duration::from_secs)
}

/// The failover transport; clones share `current`.
#[derive(Clone)]
pub struct Failover {
    client: reqwest::Client,
    endpoints: Arc<Vec<Endpoint>>,
    current: Arc<AtomicUsize>,
    epoch: Arc<AtomicU64>,
}

/// One POST's outcome: the HTTP status (0 without an answer), its
/// `Retry-After`, and the JSON-RPC answer or error.
struct Posted {
    status: u16,
    retry_after: Option<Duration>,
    res: Result<ResponsePacket, TransportError>,
}

// One POST, as alloy's `Http` does it, keeping the status: a non-2xx that
// carries a JSON-RPC error body is still `Ok` but must fail over.
async fn post(client: &reqwest::Client, url: &Url, req: &RequestPacket) -> Posted {
    let failed = |status, e: reqwest::Error| Posted {
        status,
        retry_after: None,
        res: Err(TransportErrorKind::custom_str(&describe(&e.without_url()))),
    };
    let resp = match client.post(url.clone()).json(req).send().await {
        Ok(r) => r,
        Err(e) => return failed(0, e),
    };
    let status = resp.status().as_u16();
    let retry_after = retry_after(resp.headers());
    let body = match resp.bytes().await {
        Ok(b) => b,
        Err(e) => return failed(status, e),
    };
    let ok = (200..300).contains(&status);
    let res = match serde_json::from_slice::<ResponsePacket>(&body) {
        // As alloy: a non-2xx body holding a JSON-RPC error is an answer.
        Ok(p) if ok || p.first_error_code().is_some() => Ok(p),
        Err(e) if ok => Err(TransportError::deser_err(e, String::from_utf8_lossy(&body))),
        _ => Err(TransportErrorKind::http_error(
            status,
            String::from_utf8_lossy(&body).into_owned(),
        )),
    };
    Posted {
        status,
        retry_after,
        res,
    }
}

impl Failover {
    /// Over `urls`, in order of preference; empty is a caller bug caught by
    /// the config parser (at least one URL).
    pub fn new(client: reqwest::Client, urls: &[Url]) -> Self {
        let endpoints = urls
            .iter()
            .map(|u| Endpoint {
                url: u.clone(),
                host: host(u),
                rest: Mutex::default(),
            })
            .collect();
        Failover {
            client,
            endpoints: Arc::new(endpoints),
            current: Arc::new(AtomicUsize::new(0)),
            epoch: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Switch counter, bumped on every failover; shared by clones.
    pub fn epoch(&self) -> Arc<AtomicU64> {
        self.epoch.clone()
    }

    /// Index of the endpoint requests go to now.
    pub fn current(&self) -> usize {
        self.current.load(Ordering::Acquire)
    }

    // Moves `current` from `from` to `to`, unless another request did.
    fn switch(&self, from: usize, to: usize, why: &str) {
        if from != to
            && self
                .current
                .compare_exchange(from, to, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
        {
            self.epoch.fetch_add(1, Ordering::SeqCst);
            tracing::warn!(
                from = %self.endpoints[from].host,
                to = %self.endpoints[to].host,
                error = %why,
                "RPC failover"
            );
        }
    }

    async fn send(self, req: RequestPacket) -> Result<ResponsePacket, TransportError> {
        let n = self.endpoints.len();
        // Endpoints this request saw fail other than by a rate limit.
        let mut failed = vec![false; n];
        let mut waited = Duration::ZERO;
        let mut last = None;
        loop {
            let start = self.current();
            let now = Instant::now();
            let left: Vec<usize> = (0..n)
                .map(|k| (start + k) % n)
                .filter(|i| !failed[*i])
                .collect();
            let Some(&i) = left
                .iter()
                .find(|i| self.endpoints[**i].resting(now).is_none())
            else {
                // Every endpoint left is resting: wait for the first back.
                let back = left
                    .iter()
                    .filter_map(|i| self.endpoints[*i].resting(now))
                    .min();
                let wait = back
                    .map(|t| t - now)
                    .filter(|w| waited + *w <= MAX_RATE_WAIT);
                let Some(wait) = wait else {
                    return last.unwrap_or_else(|| {
                        Err(TransportErrorKind::custom_str(
                            "every RPC endpoint is resting after a rate limit",
                        ))
                    });
                };
                tokio::time::sleep(wait).await;
                waited += wait;
                continue;
            };
            let skipped = match failed[start] {
                true => "failed for this request",
                false => "resting after a rate limit",
            };
            self.switch(start, i, skipped);
            let ep = &self.endpoints[i];
            let sent = Instant::now();
            let Posted {
                status,
                retry_after,
                res,
            } = post(&self.client, &ep.url, &req).await;
            let why = match &res {
                // Any non-2xx fails over, but an overloaded node's revert is
                // still a revert.
                Ok(p) if !(200..300).contains(&status) => match deterministic(p) {
                    true => return res,
                    false => format!("HTTP {status}"),
                },
                Ok(p) => match node_side(p) {
                    Some(why) => why,
                    None => {
                        ep.answered(sent);
                        return res;
                    }
                },
                Err(e) => e.to_string(),
            };
            if status == 429 || res.as_ref().is_ok_and(rate_limited) {
                ep.limit(retry_after);
            } else {
                failed[i] = true;
            }
            if n > 1 {
                self.switch(i, (i + 1) % n, &why);
            }
            last = Some(res);
        }
    }
}

impl Service<RequestPacket> for Failover {
    type Response = ResponsePacket;
    type Error = TransportError;
    type Future = TransportFut<'static>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: RequestPacket) -> Self::Future {
        Box::pin(self.clone().send(req))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;

    use alloy::providers::{DynProvider, Provider, ProviderBuilder};
    use alloy::rpc::client::RpcClient;
    use axum::http::StatusCode;
    use serde_json::{Value, json};
    use std::time::Instant;

    use super::*;

    type Reply = fn(&Value) -> (StatusCode, Value);

    // A JSON-RPC stub answering every request with `reply`; counts calls.
    async fn stub(reply: Reply) -> (Url, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();
        let app = axum::Router::new().route(
            "/",
            axum::routing::post(move |axum::Json(req): axum::Json<Value>| async move {
                c.fetch_add(1, Ordering::SeqCst);
                let (st, v) = reply(&req);
                (st, axum::Json(v))
            }),
        );
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/", l.local_addr().unwrap())
            .parse()
            .unwrap();
        tokio::spawn(async move { axum::serve(l, app).await });
        (url, calls)
    }

    fn block(req: &Value) -> (StatusCode, Value) {
        let v = json!({"jsonrpc":"2.0","id":req["id"],"result":"0x10"});
        (StatusCode::OK, v)
    }

    fn error(req: &Value, st: StatusCode, msg: &str) -> (StatusCode, Value) {
        let e = json!({"code":-32000,"message":msg});
        (st, json!({"jsonrpc":"2.0","id":req["id"],"error":e}))
    }

    fn provider(urls: &[Url]) -> (Failover, DynProvider) {
        let f = Failover::new(reqwest::Client::new(), urls);
        let p = ProviderBuilder::new()
            .disable_recommended_fillers()
            .connect_client(RpcClient::new(f.clone(), false))
            .erased();
        (f, p)
    }

    fn n(c: &AtomicUsize) -> usize {
        c.load(Ordering::SeqCst)
    }

    #[tokio::test]
    async fn http_503_fails_over_and_sticks() {
        let (bad, bad_calls) = stub(|_| (StatusCode::SERVICE_UNAVAILABLE, json!("down"))).await;
        let (good, good_calls) = stub(block).await;
        let (f, p) = provider(&[bad, good]);
        assert_eq!(p.get_block_number().await.unwrap(), 16);
        assert_eq!(f.current(), 1);
        // Sticky: the next call goes straight to the second endpoint.
        assert_eq!(p.get_block_number().await.unwrap(), 16);
        assert_eq!((n(&bad_calls), n(&good_calls)), (1, 2));

        // A 503 carrying a JSON-RPC error body fails over too.
        let (bad, _) = stub(|r| error(r, StatusCode::SERVICE_UNAVAILABLE, "upstream down")).await;
        let (good, _) = stub(block).await;
        let (f, p) = provider(&[bad, good]);
        assert_eq!(p.get_block_number().await.unwrap(), 16);
        assert_eq!(f.current(), 1);
    }

    #[tokio::test]
    async fn node_side_errors_fail_over() {
        let replies: [Reply; 5] = [
            |r| error(r, StatusCode::OK, "historical state is not available"),
            |r| error(r, StatusCode::OK, "Rate limit exceeded"),
            |r| {
                let e = json!({"code":-32002,"message":"No state available for block 0x1f2e"});
                (
                    StatusCode::OK,
                    json!({"jsonrpc":"2.0","id":r["id"],"error":e}),
                )
            },
            |r| {
                let e = json!({"code":-32601,"message":"public endpoint only serves recent blocks (last 1024). Register for historical / archive access."});
                (
                    StatusCode::OK,
                    json!({"jsonrpc":"2.0","id":r["id"],"error":e}),
                )
            },
            // -32603 without a known message is node-side as well.
            |r| {
                let e = json!({"code":-32603,"message":"boom"});
                (
                    StatusCode::OK,
                    json!({"jsonrpc":"2.0","id":r["id"],"error":e}),
                )
            },
        ];
        for (i, reply) in replies.into_iter().enumerate() {
            let (bad, _) = stub(reply).await;
            let (good, _) = stub(block).await;
            let (f, p) = provider(&[bad, good]);
            assert_eq!(p.get_block_number().await.unwrap(), 16, "case {i}");
            assert_eq!(f.current(), 1, "case {i}");
        }
    }

    #[tokio::test]
    async fn deterministic_errors_pass_through() {
        let replies: [Reply; 3] = [
            |r| error(r, StatusCode::OK, "execution reverted"),
            |r| error(r, StatusCode::OK, "nonce too low"),
            // Even from an overloaded node, and with -32603.
            |r| {
                let e = json!({"code":-32603,"message":"internal error: execution reverted"});
                let v = json!({"jsonrpc":"2.0","id":r["id"],"error":e});
                (StatusCode::SERVICE_UNAVAILABLE, v)
            },
        ];
        for reply in replies {
            let (first, _) = stub(reply).await;
            let (second, second_calls) = stub(block).await;
            let (f, p) = provider(&[first, second]);
            let err = p.get_block_number().await.unwrap_err();
            assert!(err.as_error_resp().is_some(), "{err}");
            assert_eq!(f.current(), 0);
            assert_eq!(n(&second_calls), 0);
        }
    }

    #[tokio::test]
    async fn http_403_fails_over() {
        let replies: [Reply; 3] = [
            |_| (StatusCode::FORBIDDEN, json!("bad key")),
            // Even a 403 shaped like JSON-RPC: it is not this call's answer.
            |r| error(r, StatusCode::FORBIDDEN, "forbidden"),
            |_| (StatusCode::OK, json!("<html>proxy</html>")),
        ];
        for (i, reply) in replies.into_iter().enumerate() {
            let (bad, _) = stub(reply).await;
            let (good, _) = stub(block).await;
            let (f, p) = provider(&[bad, good]);
            assert_eq!(p.get_block_number().await.unwrap(), 16, "case {i}");
            assert_eq!(f.current(), 1, "case {i}");
        }
    }

    #[tokio::test]
    async fn range_limits_pass_through() {
        let (first, _) = stub(|r| error(r, StatusCode::OK, "block range limit exceeded")).await;
        let (second, second_calls) = stub(block).await;
        let (f, p) = provider(&[first, second]);
        assert!(p.get_block_number().await.is_err());
        assert_eq!((f.current(), n(&second_calls)), (0, 0));
    }

    #[tokio::test]
    async fn every_endpoint_once_then_the_last_error() {
        let (a, a_calls) = stub(|_| (StatusCode::BAD_GATEWAY, json!("a"))).await;
        let (b, b_calls) = stub(|r| error(r, StatusCode::OK, "header not found")).await;
        let (f, p) = provider(&[a, b]);
        let err = p.get_block_number().await.unwrap_err();
        assert!(err.to_string().contains("header not found"), "{err}");
        assert_eq!((n(&a_calls), n(&b_calls)), (1, 1));
        // Wrapped back to the first.
        assert_eq!(f.current(), 0);
        // One endpoint: the answer as it came.
        let (a, _) = stub(|_| (StatusCode::BAD_GATEWAY, json!("a"))).await;
        let (_, p) = provider(&[a]);
        assert!(p.get_block_number().await.is_err());
    }

    type Limit = fn(&Value) -> (StatusCode, Option<&'static str>, Value);

    // A stub answering its first `limited` calls with `limit` (status,
    // Retry-After, body), then `block`; counts calls.
    async fn limiting(limited: usize, limit: Limit) -> (Url, Arc<AtomicUsize>) {
        use axum::response::IntoResponse;
        let calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();
        let app = axum::Router::new().route(
            "/",
            axum::routing::post(move |axum::Json(req): axum::Json<Value>| async move {
                if c.fetch_add(1, Ordering::SeqCst) >= limited {
                    return (StatusCode::OK, axum::Json(block(&req).1)).into_response();
                }
                let (st, after, v) = limit(&req);
                let mut r = (st, axum::Json(v)).into_response();
                if let Some(a) = after {
                    let h = axum::http::HeaderValue::from_static(a);
                    r.headers_mut().insert(axum::http::header::RETRY_AFTER, h);
                }
                r
            }),
        );
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/", l.local_addr().unwrap())
            .parse()
            .unwrap();
        tokio::spawn(async move { axum::serve(l, app).await });
        (url, calls)
    }

    fn slow_down(_: &Value) -> (StatusCode, Option<&'static str>, Value) {
        (StatusCode::TOO_MANY_REQUESTS, Some("1"), json!("slow down"))
    }

    fn slow_down_long(_: &Value) -> (StatusCode, Option<&'static str>, Value) {
        (
            StatusCode::TOO_MANY_REQUESTS,
            Some("120"),
            json!("slow down"),
        )
    }

    fn rpc_limit(r: &Value) -> (StatusCode, Option<&'static str>, Value) {
        let e = json!({"code":-32005,"message":"Too Many Requests"});
        let v = json!({"jsonrpc":"2.0","id":r["id"],"error":e});
        (StatusCode::OK, None, v)
    }

    // Every endpoint answers 429 once: the request waits out the
    // Retry-After instead of failing, then gets its answer.
    #[tokio::test]
    async fn rate_limited_endpoints_rest_then_serve() {
        let (a, a_calls) = limiting(1, slow_down).await;
        let (b, b_calls) = limiting(1, slow_down).await;
        let (_, p) = provider(&[a, b]);
        let t = Instant::now();
        assert_eq!(p.get_block_number().await.unwrap(), 16);
        assert!(
            t.elapsed() >= Duration::from_millis(950),
            "{:?}",
            t.elapsed()
        );
        assert_eq!((n(&a_calls), n(&b_calls)), (2, 1));
    }

    // A Retry-After beyond what a request waits fails it, and the requests
    // after it fail at once without asking the resting endpoint again.
    #[tokio::test]
    async fn a_long_retry_after_is_not_asked_again() {
        let (a, calls) = limiting(usize::MAX, slow_down_long).await;
        let (_, p) = provider(&[a]);
        let e = p.get_block_number().await.unwrap_err();
        assert!(e.to_string().contains("429"), "{e}");
        let t = Instant::now();
        for _ in 0..3 {
            assert!(p.get_block_number().await.is_err());
        }
        assert!(t.elapsed() < Duration::from_secs(1));
        assert_eq!(n(&calls), 1);
    }

    // A JSON-RPC rate-limit error rests the endpoint too; without a
    // Retry-After the rest doubles on each limit in a row (1 s, then 2 s).
    #[tokio::test]
    async fn rpc_rate_limits_back_off() {
        let (a, calls) = limiting(2, rpc_limit).await;
        let (_, p) = provider(&[a]);
        let t = Instant::now();
        assert_eq!(p.get_block_number().await.unwrap(), 16);
        assert!(
            t.elapsed() >= Duration::from_millis(2950),
            "{:?}",
            t.elapsed()
        );
        assert_eq!(n(&calls), 3);
    }

    // While an endpoint rests, other requests wait too instead of
    // hammering it; they all go through once it is back.
    #[tokio::test]
    async fn concurrent_requests_wait_out_the_rest() {
        let (a, calls) = limiting(1, slow_down).await;
        let (_, p) = provider(&[a]);
        let first = tokio::spawn({
            let p = p.clone();
            async move { p.get_block_number().await }
        });
        while n(&calls) == 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
        let others: Vec<_> = (0..5)
            .map(|_| {
                let p = p.clone();
                tokio::spawn(async move { p.get_block_number().await })
            })
            .collect();
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert_eq!(n(&calls), 1, "asked a resting endpoint");
        assert_eq!(first.await.unwrap().unwrap(), 16);
        for o in others {
            assert_eq!(o.await.unwrap().unwrap(), 16);
        }
        assert_eq!(n(&calls), 7);
    }

    // An answer to a request sent before a rate limit does not end the
    // rest; one sent after the rest does, and the backoff starts over.
    #[tokio::test(start_paused = true)]
    async fn only_newer_answers_end_a_rest() {
        use tokio::time::Instant;
        let u: Url = "http://127.0.0.1:1/".parse().unwrap();
        let ep = Endpoint {
            host: host(&u),
            url: u,
            rest: Mutex::default(),
        };
        let old = Instant::now();
        tokio::time::advance(Duration::from_millis(10)).await;
        ep.limit(None);
        ep.answered(old);
        assert!(ep.resting(Instant::now()).is_some());
        tokio::time::advance(Duration::from_secs(1)).await;
        assert!(ep.resting(Instant::now()).is_none());
        ep.limit(None);
        let rest = ep.resting(Instant::now()).unwrap() - Instant::now();
        assert_eq!(rest, Duration::from_secs(2), "doubled on a limit in a row");
        tokio::time::advance(rest).await;
        ep.answered(Instant::now());
        ep.limit(None);
        let rest = ep.resting(Instant::now()).unwrap() - Instant::now();
        assert_eq!(rest, Duration::from_secs(1), "started over after an answer");
    }

    #[test]
    fn host_hides_path_and_query() {
        let u: Url = "https://user:pw@rpc.example.org/v3/secret?key=k"
            .parse()
            .unwrap();
        assert_eq!(host(&u), "rpc.example.org");
        let u: Url = "http://127.0.0.1:8545/k".parse().unwrap();
        assert_eq!(host(&u), "127.0.0.1:8545");
    }
}
