//! Sticky RPC failover. Every request goes to the current endpoint; a
//! node-side failure (transport error, any non-2xx status such as 429/5xx or
//! 401/403/413, an unparsable 2xx body, a [`NODE_SIDE`] JSON-RPC error) moves
//! `current` to the next one, retries there (each endpoint at most once per
//! request) and stays: a bad key or a proxy error page on one provider says
//! nothing about the next. A [`DETERMINISTIC`] error never does. No
//! per-call rotation: mixing providers at different heads could make a
//! lagging one's `eth_getLogs` up to our confirmed head silently miss events.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::task::{Context, Poll};

use alloy::rpc::json_rpc::{RequestPacket, ResponsePacket};
use alloy::transports::http::reqwest;
use alloy::transports::{TransportError, TransportErrorKind, TransportFut};
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
];

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

/// Why a JSON-RPC answer should fail over, if it should.
fn node_side(p: &ResponsePacket) -> Option<String> {
    p.iter_errors()
        .find(|e| {
            let m = e.message.to_ascii_lowercase();
            !is_deterministic(&m) && (e.code == -32603 || NODE_SIDE.iter().any(|s| m.contains(s)))
        })
        .map(|e| format!("{} {}", e.code, e.message))
}

struct Endpoint {
    url: Url,
    host: String,
}

/// The failover transport; clones share `current`.
#[derive(Clone)]
pub struct Failover {
    client: reqwest::Client,
    endpoints: Arc<Vec<Endpoint>>,
    current: Arc<AtomicUsize>,
    epoch: Arc<AtomicU64>,
}

// One POST, as alloy's `Http` does it, keeping the status: a non-2xx that
// carries a JSON-RPC error body is still `Ok` but must fail over.
async fn post(
    client: &reqwest::Client,
    url: &Url,
    req: &RequestPacket,
) -> (u16, Result<ResponsePacket, TransportError>) {
    let resp = match client.post(url.clone()).json(req).send().await {
        Ok(r) => r,
        Err(e) => {
            return (
                0,
                Err(TransportErrorKind::custom_str(&describe(&e.without_url()))),
            );
        }
    };
    let status = resp.status().as_u16();
    let body = match resp.bytes().await {
        Ok(b) => b,
        Err(e) => {
            return (
                status,
                Err(TransportErrorKind::custom_str(&describe(&e.without_url()))),
            );
        }
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
    (status, res)
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

    async fn send(self, req: RequestPacket) -> Result<ResponsePacket, TransportError> {
        let n = self.endpoints.len();
        let start = self.current();
        let mut last = Err(TransportErrorKind::custom_str("no RPC endpoint configured"));
        for k in 0..n {
            let i = (start + k) % n;
            let ep = &self.endpoints[i];
            let (status, res) = post(&self.client, &ep.url, &req).await;
            let why = match &res {
                // Any non-2xx fails over, but an overloaded node's revert is
                // still a revert.
                Ok(p) if !(200..300).contains(&status) => match deterministic(p) {
                    true => return res,
                    false => format!("HTTP {status}"),
                },
                Ok(p) => match node_side(p) {
                    Some(why) => why,
                    None => return res,
                },
                Err(e) => e.to_string(),
            };
            if n > 1 {
                let next = (i + 1) % n;
                if self
                    .current
                    .compare_exchange(i, next, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
                {
                    self.epoch.fetch_add(1, Ordering::SeqCst);
                    tracing::warn!(
                        from = %ep.host,
                        to = %self.endpoints[next].host,
                        error = %why,
                        "RPC failover"
                    );
                }
            }
            last = res;
        }
        last
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
        let replies: [Reply; 4] = [
            |r| error(r, StatusCode::OK, "historical state is not available"),
            |r| error(r, StatusCode::OK, "Rate limit exceeded"),
            |r| {
                let e = json!({"code":-32002,"message":"No state available for block 0x1f2e"});
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
