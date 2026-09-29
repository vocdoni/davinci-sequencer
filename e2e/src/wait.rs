//! Bounded polling.

use std::future::Future;
use std::time::{Duration, Instant};

use alloy::transports::{RpcError, TransportError};
use anyhow::{Result, bail};

/// Consecutive transient errors `until` tolerates before giving up.
const MAX_TRANSIENT: u32 = 10;

/// Network trouble, a 5xx or a rate-limited RPC: worth another try.
pub fn is_transient(e: &anyhow::Error) -> bool {
    e.chain().any(|c| {
        if let Some(e) = c.downcast_ref::<davinci_client::Error>() {
            return match e {
                davinci_client::Error::Http(_) => true,
                davinci_client::Error::Api { status, .. } => *status >= 500,
                // The organizer and registry reader flatten RPC errors to text.
                davinci_client::Error::Chain(m) => rpc_gave_up(m),
                _ => false,
            };
        }
        if let Some(e) = c.downcast_ref::<reqwest::Error>() {
            return e.is_connect()
                || e.is_timeout()
                || e.status().is_some_and(|s| s.is_server_error());
        }
        if let Some(e) = c.downcast_ref::<TransportError>() {
            return rpc_retryable(e);
        }
        if let Some(alloy::contract::Error::TransportError(e)) =
            c.downcast_ref::<alloy::contract::Error>()
        {
            return rpc_retryable(e);
        }
        false
    })
}

/// An RPC error alloy retries (a rate limit, a 503), or what its retry
/// layer returns once it gives up on one.
fn rpc_retryable(e: &TransportError) -> bool {
    match e {
        RpcError::Transport(k) => k.is_retry_err() || rpc_gave_up(&k.to_string()),
        RpcError::ErrorResp(p) => p.is_retry_err(),
        _ => false,
    }
}

/// alloy's retry layer gave up on an RPC that kept refusing (a rate limit
/// that outlasted its retries, a 503).
fn rpc_gave_up(msg: &str) -> bool {
    msg.contains("Max retries exceeded")
}

/// Calls `f` every `every` until it returns `Some`, a non-transient error,
/// more than `MAX_TRANSIENT` transient errors in a row, or `timeout` passes.
/// `what` names the wait in the timeout error.
pub async fn until<T, F, Fut>(what: &str, timeout: Duration, every: Duration, mut f: F) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<Option<T>>>,
{
    let start = Instant::now();
    let mut transient = 0u32;
    let mut last_err: Option<anyhow::Error> = None;
    loop {
        match f().await {
            Ok(Some(v)) => return Ok(v),
            Ok(None) => transient = 0,
            Err(e) if is_transient(&e) && transient < MAX_TRANSIENT => {
                transient += 1;
                last_err = Some(e);
            }
            Err(e) => return Err(e.context(format!("waiting for {what}"))),
        }
        if start.elapsed() > timeout {
            match last_err {
                Some(e) => {
                    bail!("timed out after {timeout:?} waiting for {what}; last error: {e:#}")
                }
                None => bail!("timed out after {timeout:?} waiting for {what}"),
            }
        }
        tokio::time::sleep(every).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn retries_transient_errors_only() {
        let mut n = 0;
        let v = until(
            "x",
            Duration::from_secs(5),
            Duration::from_millis(1),
            || {
                n += 1;
                let k = n;
                async move {
                    match k {
                        1 | 2 => Err(davinci_client::Error::Http("refused".into()).into()),
                        3 => Ok(None),
                        _ => Ok(Some(k)),
                    }
                }
            },
        )
        .await
        .unwrap();
        assert_eq!(v, 4);

        // A 4xx is an answer, not a hiccup: no retry.
        let mut calls = 0;
        let e = until::<(), _, _>(
            "y",
            Duration::from_secs(5),
            Duration::from_millis(1),
            || {
                calls += 1;
                async {
                    Err(davinci_client::Error::Api {
                        status: 400,
                        code: Some(40001),
                        message: "bad".into(),
                    }
                    .into())
                }
            },
        )
        .await
        .unwrap_err();
        assert_eq!(calls, 1);
        assert!(format!("{e:#}").contains("waiting for y"));

        let mut calls = 0;
        let e = until::<(), _, _>(
            "z",
            Duration::from_secs(5),
            Duration::from_millis(1),
            || {
                calls += 1;
                async { Err(davinci_client::Error::Http("down".into()).into()) }
            },
        )
        .await
        .unwrap_err();
        assert_eq!(calls, MAX_TRANSIENT + 1);
        assert!(format!("{e:#}").contains("down"));
    }

    #[tokio::test]
    async fn rate_limited_rpc_is_transient() {
        use alloy::providers::{Provider, ProviderBuilder};
        use alloy::rpc::client::ClientBuilder;
        use alloy::transports::layers::RetryBackoffLayer;
        use alloy::transports::{HttpError, TransportErrorKind};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        // An RPC that refuses every request, behind alloy's retry layer: the
        // error the layer returns once it gives up.
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", l.local_addr().unwrap());
        tokio::spawn(async move {
            while let Ok((mut s, _)) = l.accept().await {
                let _ = s.read(&mut [0u8; 4096]).await;
                let _ = s
                    .write_all(b"HTTP/1.1 429 Too Many Requests\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")
                    .await;
            }
        });
        let client = ClientBuilder::default()
            .layer(RetryBackoffLayer::new(2, 1, 10_000))
            .http(url.parse().unwrap());
        let gave_up = ProviderBuilder::new()
            .connect_client(client)
            .get_block_number()
            .await
            .unwrap_err();
        let msg = gave_up.to_string();
        assert!(is_transient(&anyhow::Error::from(gave_up)), "{msg}");
        // The same through the client, which keeps only the text.
        let flat = davinci_client::Error::Chain(msg.clone());
        assert!(is_transient(&anyhow::Error::from(flat).context("results")));
        let wrapped = alloy::contract::Error::TransportError(TransportErrorKind::custom_str(&msg));
        assert!(is_transient(&anyhow::Error::from(wrapped).context("call")));

        let payload = |code: i64, message: &str| {
            let j = serde_json::json!({ "code": code, "message": message });
            RpcError::ErrorResp(serde_json::from_value(j).unwrap())
        };
        for e in [
            RpcError::Transport(TransportErrorKind::HttpError(HttpError {
                status: 503,
                body: String::new(),
            })),
            payload(-32005, "limit exceeded"),
        ] {
            assert!(is_transient(&anyhow::Error::from(e)));
        }
        for e in [
            payload(3, "execution reverted"),
            TransportErrorKind::custom_str("invalid response"),
        ] {
            assert!(!is_transient(&anyhow::Error::from(e)));
        }
        let e = davinci_client::Error::Chain("process not found".into());
        assert!(!is_transient(&anyhow::Error::from(e)));

        // until rides out the refusals.
        let mut n = 0;
        let v = until(
            "x",
            Duration::from_secs(5),
            Duration::from_millis(1),
            || {
                n += 1;
                let k = n;
                let e = davinci_client::Error::Chain(msg.clone());
                async move {
                    match k {
                        1 | 2 => Err(anyhow::Error::from(e).context("eth_getLogs")),
                        _ => Ok(Some(k)),
                    }
                }
            },
        )
        .await
        .unwrap();
        assert_eq!(v, 3);
    }
}
