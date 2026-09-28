//! A loopback HTTP/1.1 forwarder that can be taken down, for outage tests,
//! or tapped, to see what a node asks its prover. One request per
//! connection, `Content-Length` bodies only; https upstreams go through
//! reqwest.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

/// Largest request or response head accepted.
const MAX_HEAD: usize = 64 * 1024;

/// One forwarded request and the upstream's answer.
pub struct Exchange<'a> {
    /// When the connection was accepted.
    pub at: Instant,
    pub method: &'a str,
    pub path: &'a str,
    pub request: &'a [u8],
    pub status: u16,
    pub response: &'a [u8],
}

/// Called with every exchange after its response went back to the client.
pub type Tap = Arc<dyn Fn(&Exchange<'_>) + Send + Sync>;

/// A running proxy; the listener stops when this drops.
pub struct Proxy {
    /// `http://127.0.0.1:<port>`: give this to the client instead of `upstream`.
    pub url: String,
    pub upstream: String,
    down: Arc<AtomicBool>,
    refused: Arc<AtomicU64>,
    task: JoinHandle<()>,
}

impl Drop for Proxy {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Proxy {
    pub async fn start(upstream: &str) -> Result<Proxy> {
        Proxy::spawn(upstream, None).await
    }

    /// [`Proxy::start`], handing every exchange to `tap`.
    pub async fn tapped(upstream: &str, tap: Tap) -> Result<Proxy> {
        Proxy::spawn(upstream, Some(tap)).await
    }

    async fn spawn(upstream: &str, tap: Option<Tap>) -> Result<Proxy> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let url = format!("http://{}", listener.local_addr()?);
        let base = upstream.trim_end_matches('/').to_string();
        let client = reqwest::Client::builder()
            .user_agent("davinci-e2e-proxy")
            .timeout(Duration::from_secs(300))
            .build()?;
        let down = Arc::new(AtomicBool::new(false));
        let refused = Arc::new(AtomicU64::new(0));
        let (d, r) = (down.clone(), refused.clone());
        let task = tokio::spawn(async move {
            while let Ok((sock, _)) = listener.accept().await {
                if d.load(Ordering::SeqCst) {
                    r.fetch_add(1, Ordering::SeqCst);
                    drop(sock); // the client sees a reset connection
                    continue;
                }
                let (client, base, tap) = (client.clone(), base.clone(), tap.clone());
                tokio::spawn(async move {
                    let _ = serve(sock, &client, &base, tap.as_ref()).await;
                });
            }
        });
        Ok(Proxy {
            url,
            upstream: upstream.to_string(),
            down,
            refused,
            task,
        })
    }

    pub fn set_down(&self, down: bool) {
        self.down.store(down, Ordering::SeqCst);
    }

    /// Connections dropped while down, since the start.
    pub fn refused(&self) -> u64 {
        self.refused.load(Ordering::SeqCst)
    }

    /// Takes the proxy down now and back up after `d`, in the background.
    pub fn outage(&self, d: Duration) -> JoinHandle<()> {
        self.set_down(true);
        let down = self.down.clone();
        tokio::spawn(async move {
            tokio::time::sleep(d).await;
            down.store(false, Ordering::SeqCst);
        })
    }
}

async fn serve(
    mut sock: TcpStream,
    client: &reqwest::Client,
    base: &str,
    tap: Option<&Tap>,
) -> Result<()> {
    let at = Instant::now();
    let mut buf = Vec::new();
    let head_end = loop {
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
        if buf.len() > MAX_HEAD {
            bail!("request head too large");
        }
        let mut chunk = [0u8; 8192];
        let n = sock.read(&mut chunk).await?;
        if n == 0 {
            bail!("closed before the request head");
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    let head = std::str::from_utf8(&buf[..head_end]).context("request head")?;
    let mut lines = head.split("\r\n");
    let mut first = lines.next().unwrap_or_default().split(' ');
    let (method, path) = (
        first.next().unwrap_or_default(),
        first.next().unwrap_or("/"),
    );
    let mut len = 0usize;
    let mut headers = Vec::new();
    for l in lines.filter(|l| !l.is_empty()) {
        let Some((k, v)) = l.split_once(':') else {
            continue;
        };
        let (k, v) = (k.trim().to_ascii_lowercase(), v.trim().to_string());
        match k.as_str() {
            "content-length" => len = v.parse()?,
            "transfer-encoding" => {
                sock.write_all(b"HTTP/1.1 501 Not Implemented\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                    .await?;
                bail!("chunked request");
            }
            "content-type" | "accept" | "authorization" => headers.push((k, v)),
            _ => {}
        }
    }
    let mut body = buf[head_end..].to_vec();
    while body.len() < len {
        let mut chunk = vec![0u8; (len - body.len()).min(1 << 20)];
        let n = sock.read(&mut chunk).await?;
        if n == 0 {
            bail!("closed inside the request body");
        }
        body.extend_from_slice(&chunk[..n]);
    }
    body.truncate(len);

    let mut req = client.request(method.parse()?, format!("{base}{path}"));
    for (k, v) in headers {
        req = req.header(k, v);
    }
    // A copy for the tap only.
    let sent = tap.map(|_| body.clone()).unwrap_or_default();
    let resp = match req.body(body).send().await {
        Ok(r) => r,
        Err(_) => {
            sock.write_all(
                b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            )
            .await?;
            return Ok(());
        }
    };
    let status = resp.status();
    let ctype = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let body = resp.bytes().await?;
    let mut out = format!(
        "HTTP/1.1 {} {}\r\nContent-Length: {}\r\nConnection: close\r\n",
        status.as_u16(),
        status.canonical_reason().unwrap_or(""),
        body.len()
    );
    if let Some(c) = ctype {
        out.push_str(&format!("Content-Type: {c}\r\n"));
    }
    out.push_str("\r\n");
    sock.write_all(out.as_bytes()).await?;
    sock.write_all(&body).await?;
    sock.shutdown().await?;
    if let Some(t) = tap {
        t(&Exchange {
            at,
            method,
            path,
            request: &sent,
            status: status.as_u16(),
            response: &body,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // An upstream that answers every request with its method, path and body.
    async fn echo() -> Result<String> {
        let l = TcpListener::bind("127.0.0.1:0").await?;
        let url = format!("http://{}", l.local_addr()?);
        tokio::spawn(async move {
            while let Ok((mut s, _)) = l.accept().await {
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 65536];
                    let mut got = Vec::new();
                    loop {
                        let n = s.read(&mut buf).await.unwrap_or(0);
                        got.extend_from_slice(&buf[..n]);
                        let text = String::from_utf8_lossy(&got).to_string();
                        if let Some((h, b)) = text.split_once("\r\n\r\n") {
                            let len = h
                                .lines()
                                .find_map(|l| l.strip_prefix("content-length: "))
                                .and_then(|v| v.parse::<usize>().ok())
                                .unwrap_or(0);
                            if b.len() >= len || n == 0 {
                                let line = h.lines().next().unwrap_or_default();
                                let reply = format!("{line}|{b}");
                                let resp = format!(
                                    "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\n\r\n{reply}",
                                    reply.len()
                                );
                                let _ = s.write_all(resp.as_bytes()).await;
                                return;
                            }
                        }
                        if n == 0 {
                            return;
                        }
                    }
                });
            }
        });
        Ok(url)
    }

    #[tokio::test]
    async fn forwards_and_drops() -> Result<()> {
        let p = Proxy::start(&echo().await?).await?;
        let c = reqwest::Client::new();
        let big = "x".repeat(200_000);
        let r = c
            .post(format!("{}/rpc?a=1", p.url))
            .body(big.clone())
            .send()
            .await?;
        assert_eq!(r.status(), 200);
        assert_eq!(r.text().await?, format!("POST /rpc?a=1 HTTP/1.1|{big}"));

        let back = p.outage(Duration::from_millis(300));
        assert!(c.get(format!("{}/x", p.url)).send().await.is_err());
        assert_eq!(p.refused(), 1);
        back.await?;
        let r = c.get(format!("{}/x", p.url)).send().await?;
        assert_eq!(r.text().await?, "GET /x HTTP/1.1|");
        Ok(())
    }

    #[tokio::test]
    async fn taps_every_exchange() -> Result<()> {
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let s = seen.clone();
        let tap: Tap = Arc::new(move |x: &Exchange<'_>| {
            s.lock().unwrap().push((
                x.method.to_string(),
                x.path.to_string(),
                x.request.to_vec(),
                x.status,
                x.response.to_vec(),
            ));
        });
        let p = Proxy::tapped(&echo().await?, tap).await?;
        let r = reqwest::Client::new()
            .post(format!("{}/prove", p.url))
            .body("{}")
            .send()
            .await?;
        assert_eq!(r.text().await?, "POST /prove HTTP/1.1|{}");
        // The tap runs after the response is written.
        for _ in 0..50 {
            if !seen.lock().unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let got = seen.lock().unwrap().clone();
        assert_eq!(
            got,
            [(
                "POST".to_string(),
                "/prove".to_string(),
                b"{}".to_vec(),
                200,
                b"POST /prove HTTP/1.1|{}".to_vec()
            )]
        );
        Ok(())
    }
}
