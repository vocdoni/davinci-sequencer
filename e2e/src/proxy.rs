//! A loopback HTTP/1.1 forwarder that can be taken down, for outage tests,
//! or tapped, to see what a node asks its prover. One request per
//! connection, `Content-Length` bodies only; https upstreams go through
//! reqwest.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
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

/// The URL a request for `path` (with its query) is forwarded to: `base` may
/// carry a path of its own (`https://h/optimism`), which a root request keeps.
pub fn target(base: &str, path: &str) -> String {
    let base = base.trim_end_matches('/');
    if path == "/" {
        base.to_string()
    } else {
        format!("{base}{path}")
    }
}

/// Called with every exchange after its response went back to the client.
pub type Tap = Arc<dyn Fn(&Exchange<'_>) + Send + Sync>;

/// Called with the method and path of a request as it arrives, before it is
/// forwarded: sees calls the client abandons on a timeout.
pub type Arrival = Arc<dyn Fn(Instant, &str, &str) + Send + Sync>;

/// A running proxy; the listener stops when this drops.
pub struct Proxy {
    /// `http://127.0.0.1:<port>`: give this to the client instead of `upstream`.
    pub url: String,
    pub upstream: String,
    down: Arc<AtomicBool>,
    refused: Arc<AtomicU64>,
    modes: Arc<Modes>,
    task: JoinHandle<()>,
}

/// Delay and fail settings, read per request.
#[derive(Default)]
struct Modes {
    /// Path prefix and how long matching requests are held.
    delay: Mutex<(String, Duration)>,
    /// Status answered instead of forwarding; 0 forwards.
    fail: AtomicU32,
    delayed: AtomicU64,
    failed: AtomicU64,
    dropped: AtomicU64,
}

impl Drop for Proxy {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Proxy {
    pub async fn start(upstream: &str) -> Result<Proxy> {
        Proxy::spawn(upstream, None, None).await
    }

    /// [`Proxy::start`], handing every exchange to `tap`.
    pub async fn tapped(upstream: &str, tap: Tap) -> Result<Proxy> {
        Proxy::spawn(upstream, Some(tap), None).await
    }

    /// [`Proxy::tapped`], also telling `arrival` of each request when it comes in.
    pub async fn watched(upstream: &str, arrival: Arrival, tap: Tap) -> Result<Proxy> {
        Proxy::spawn(upstream, Some(tap), Some(arrival)).await
    }

    async fn spawn(upstream: &str, tap: Option<Tap>, arrival: Option<Arrival>) -> Result<Proxy> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let url = format!("http://{}", listener.local_addr()?);
        let base = upstream.trim_end_matches('/').to_string();
        let client = reqwest::Client::builder()
            .user_agent("davinci-e2e-proxy")
            .timeout(Duration::from_secs(300))
            .build()?;
        let down = Arc::new(AtomicBool::new(false));
        let refused = Arc::new(AtomicU64::new(0));
        let modes = Arc::new(Modes::default());
        let (d, r, m) = (down.clone(), refused.clone(), modes.clone());
        let task = tokio::spawn(async move {
            while let Ok((sock, _)) = listener.accept().await {
                if d.load(Ordering::SeqCst) {
                    r.fetch_add(1, Ordering::SeqCst);
                    drop(sock); // the client sees a reset connection
                    continue;
                }
                let (client, base, tap, arrival, m) = (
                    client.clone(),
                    base.clone(),
                    tap.clone(),
                    arrival.clone(),
                    m.clone(),
                );
                tokio::spawn(async move {
                    let _ = serve(sock, &client, &base, tap.as_ref(), arrival.as_ref(), &m).await;
                });
            }
        });
        Ok(Proxy {
            url,
            upstream: upstream.to_string(),
            down,
            refused,
            modes,
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

    /// Holds requests whose path starts with `path_prefix` for `d` before
    /// forwarding them; `Duration::ZERO` clears. A request whose client
    /// closes during the hold is dropped, not forwarded.
    pub fn set_delay(&self, path_prefix: &str, d: Duration) {
        if let Ok(mut g) = self.modes.delay.lock() {
            *g = (path_prefix.to_string(), d);
        }
    }

    /// Answers `status` with an empty body instead of forwarding; `None` forwards again.
    pub fn set_fail(&self, status: Option<u16>) {
        self.modes
            .fail
            .store(status.map_or(0, u32::from), Ordering::SeqCst);
    }

    /// Held requests dropped because their client closed.
    pub fn dropped(&self) -> u64 {
        self.modes.dropped.load(Ordering::SeqCst)
    }

    /// Requests held by a delay, since the start.
    pub fn delayed(&self) -> u64 {
        self.modes.delayed.load(Ordering::SeqCst)
    }

    /// Requests answered by a fail mode, since the start.
    pub fn failed(&self) -> u64 {
        self.modes.failed.load(Ordering::SeqCst)
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
    arrival: Option<&Arrival>,
    modes: &Modes,
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
    if let Some(a) = arrival {
        a(at, method, path);
    }
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

    let fail = modes.fail.load(Ordering::SeqCst);
    if fail != 0 {
        modes.failed.fetch_add(1, Ordering::SeqCst);
        sock.write_all(
            format!("HTTP/1.1 {fail} Injected\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                .as_bytes(),
        )
        .await?;
        return Ok(());
    }
    let hold = modes
        .delay
        .lock()
        .map(|g| g.clone())
        .ok()
        .filter(|(p, d)| !d.is_zero() && path.starts_with(p.as_str()));
    if let Some((_, d)) = hold {
        modes.delayed.fetch_add(1, Ordering::SeqCst);
        let sleep = tokio::time::sleep(d);
        tokio::pin!(sleep);
        let mut scratch = [0u8; 1024];
        loop {
            tokio::select! {
                _ = &mut sleep => break,
                n = sock.read(&mut scratch) => {
                    // Bytes after the request are ignored; EOF or an error is a close.
                    if !matches!(n, Ok(k) if k > 0) {
                        modes.dropped.fetch_add(1, Ordering::SeqCst);
                        return Ok(());
                    }
                }
            }
        }
    }

    let mut req = client.request(method.parse()?, target(base, path));
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

    #[test]
    fn target_keeps_a_path_upstream() {
        assert_eq!(target("https://h/optimism", "/"), "https://h/optimism");
        assert_eq!(
            target("https://h", "/eth/v1/beacon/blobs/1?x=y"),
            "https://h/eth/v1/beacon/blobs/1?x=y"
        );
        assert_eq!(target("http://h:8080/", "/prove"), "http://h:8080/prove");
    }

    #[tokio::test]
    async fn forwards_root_to_a_path_upstream() -> Result<()> {
        let p = Proxy::start(&format!("{}/optimism", echo().await?)).await?;
        let r = reqwest::Client::new()
            .post(format!("{}/", p.url))
            .body("{}")
            .send()
            .await?;
        assert_eq!(r.text().await?, "POST /optimism HTTP/1.1|{}");
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

    // `echo` behind a proxy that counts the requests reaching it.
    async fn counted() -> Result<(Proxy, Arc<AtomicU64>)> {
        let n = Arc::new(AtomicU64::new(0));
        let c = n.clone();
        let arrival: Arrival = Arc::new(move |_, _, _| {
            c.fetch_add(1, Ordering::SeqCst);
        });
        let tap: Tap = Arc::new(|_: &Exchange<'_>| {});
        Ok((Proxy::watched(&echo().await?, arrival, tap).await?, n))
    }

    #[tokio::test]
    async fn delays_matching_paths() -> Result<()> {
        let p = Proxy::start(&echo().await?).await?;
        p.set_delay("/prove", Duration::from_millis(300));
        let c = reqwest::Client::new();
        let t = Instant::now();
        c.post(format!("{}/prove", p.url)).body("{}").send().await?;
        assert!(t.elapsed() >= Duration::from_millis(300));
        let t = Instant::now();
        c.get(format!("{}/health", p.url)).send().await?;
        // Undelayed: well under the 300 ms hold (a tight bound flakes under host load).
        assert!(t.elapsed() < Duration::from_millis(250));
        assert_eq!(p.delayed(), 1);
        Ok(())
    }

    #[tokio::test]
    async fn fails_with_a_status() -> Result<()> {
        let (up, n) = counted().await?;
        let p = Proxy::start(&up.url).await?;
        let c = reqwest::Client::new();
        p.set_fail(Some(503));
        let r = c.post(format!("{}/x", p.url)).body("{}").send().await?;
        assert_eq!(r.status(), 503);
        assert!(r.bytes().await?.is_empty());
        assert_eq!((n.load(Ordering::SeqCst), p.failed()), (0, 1));
        p.set_fail(None);
        let r = c.post(format!("{}/x", p.url)).body("{}").send().await?;
        assert_eq!(r.status(), 200);
        assert_eq!(n.load(Ordering::SeqCst), 1);
        Ok(())
    }

    #[tokio::test]
    async fn delay_drops_a_closed_client() -> Result<()> {
        let (up, n) = counted().await?;
        let p = Proxy::start(&up.url).await?;
        p.set_delay("/prove", Duration::from_millis(300));
        let addr = p.url.trim_start_matches("http://").to_string();
        let mut s = TcpStream::connect(addr).await?;
        s.write_all(b"POST /prove HTTP/1.1\r\nContent-Length: 2\r\n\r\n{}")
            .await?;
        tokio::time::sleep(Duration::from_millis(50)).await;
        drop(s);
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(n.load(Ordering::SeqCst), 0);
        assert_eq!(p.dropped(), 1);
        Ok(())
    }
}
