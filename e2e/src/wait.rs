//! Bounded polling.

use std::future::Future;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};

/// Consecutive transient errors `until` tolerates before giving up.
const MAX_TRANSIENT: u32 = 10;

/// Network trouble or a 5xx: worth another try.
pub fn is_transient(e: &anyhow::Error) -> bool {
    e.chain().any(|c| {
        if let Some(e) = c.downcast_ref::<davinci_client::Error>() {
            return match e {
                davinci_client::Error::Http(_) => true,
                davinci_client::Error::Api { status, .. } => *status >= 500,
                _ => false,
            };
        }
        if let Some(e) = c.downcast_ref::<reqwest::Error>() {
            return e.is_connect()
                || e.is_timeout()
                || e.status().is_some_and(|s| s.is_server_error());
        }
        false
    })
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
}
