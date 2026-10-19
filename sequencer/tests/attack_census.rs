//! Attacks on census acquisition: the URI, the document and (origin 3) the
//! contract all come from on-chain data an attacker controls. Everything here
//! talks to servers spawned on localhost; nothing leaves the machine.
//! The anvil section is gated by `ANVIL=1` (see `common/anvil.rs`).

mod common;

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
use std::time::{Duration, Instant};

use davinci_sequencer::census::{CensusError, CensusOptions, CensusStore};
use davinci_sequencer::storage::Db;
use davinci_zkvm_sdk::census::{LeanImt, census_leaf};
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

fn db(dir: &TempDir) -> Db {
    Db::open_in(dir.path()).unwrap()
}

fn addr_of(i: u32) -> [u8; 20] {
    let mut a = [0x77; 20];
    a[16..].copy_from_slice(&i.to_be_bytes());
    a
}

/// `root` with its last word changed by `k`: another (wrong) census root.
fn other_root(root: &[u8; 32], k: u64) -> [u8; 32] {
    let mut r = *root;
    for (b, x) in r[24..].iter_mut().zip(k.to_be_bytes()) {
        *b ^= x;
    }
    r
}

/// A census document and its lean-IMT root.
fn doc_of(parts: &[([u8; 20], u128)]) -> (String, [u8; 32]) {
    let leaves = parts.iter().map(|(a, w)| census_leaf(a, *w).unwrap());
    let root = LeanImt::from_leaves(leaves.collect()).root();
    let ps: Vec<_> = parts
        .iter()
        .map(|(a, w)| serde_json::json!({"key": format!("0x{}", hex::encode(a)), "weight": w.to_string()}))
        .collect();
    (serde_json::json!({ "participants": ps }).to_string(), root)
}

fn small_doc() -> (String, [u8; 32]) {
    doc_of(
        &(0..5)
            .map(|i| (addr_of(i), u128::from(i) + 1))
            .collect::<Vec<_>>(),
    )
}

fn private(dir: &TempDir, max_bytes: u64) -> CensusStore {
    let opts = CensusOptions {
        allow_private: true,
        max_bytes,
        ..CensusOptions::default()
    };
    CensusStore::with_options(db(dir), opts).unwrap()
}

fn with_dir(dir: &TempDir, census_dir: &Path, max_bytes: u64) -> CensusStore {
    let opts = CensusOptions {
        dir: Some(census_dir.to_path_buf()),
        max_bytes,
        ..CensusOptions::default()
    };
    CensusStore::with_options(db(dir), opts).unwrap()
}

/// A raw TCP server: `f` gets each connection after its request was read.
async fn raw<F, Fut>(f: F) -> (SocketAddr, Arc<AtomicUsize>)
where
    F: Fn(tokio::net::TcpStream) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    let hits = Arc::new(AtomicUsize::new(0));
    let h = hits.clone();
    let f = Arc::new(f);
    tokio::spawn(async move {
        while let Ok((mut s, _)) = l.accept().await {
            h.fetch_add(1, SeqCst);
            let f = f.clone();
            tokio::spawn(async move {
                let mut req = [0u8; 4096];
                let _ = s.read(&mut req).await;
                f(s).await;
            });
        }
    });
    (addr, hits)
}

/// A server answering every request with `head` (status line and headers,
/// ending in a blank line) and then `body`.
async fn canned(head: String, body: Vec<u8>) -> (SocketAddr, Arc<AtomicUsize>) {
    let head = Arc::new(head);
    let body = Arc::new(body);
    raw(move |mut s| {
        let (head, body) = (head.clone(), body.clone());
        async move {
            let _ = s.write_all(head.as_bytes()).await;
            let _ = s.write_all(&body).await;
            let _ = s.shutdown().await;
        }
    })
    .await
}

fn ok_head(len: usize) -> String {
    format!(
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {len}\r\nconnection: close\r\n\r\n"
    )
}

// ---------------------------------------------------------------- SSRF

// Every spelling of a loopback host must be refused before any connection.
#[tokio::test]
async fn ssrf_loopback_spellings_never_connect() {
    let (_, root) = small_doc();
    let (addr, hits) = canned(ok_head(2), b"{}".to_vec()).await;
    let port = addr.port();
    let hosts = [
        "127.0.0.1",
        "127.1",
        "127.0.1",
        "2130706433",
        "0x7f000001",
        "0x7f.0.0.1",
        "0177.0.0.1",
        "017700000001",
        "127.0.0.1.",
        "0",
        "0.0.0.0",
        "[::1]",
        "[::ffff:127.0.0.1]",
        "[::ffff:7f00:1]",
        "[::127.0.0.1]",
        "[::7f00:1]",
        "[0:0:0:0:0:ffff:7f00:1]",
        "[64:ff9b::7f00:1]",
        "[::ffff:0:7f00:1]",
        "localhost",
        "localhost.",
        "LOCALHOST",
        "ip6-localhost",
        "ip6-loopback",
        // fullwidth and ideographic-dot spellings normalise to 127.0.0.1
        "\u{ff11}\u{ff12}\u{ff17}.0.0.1",
        "127\u{3002}0\u{3002}0\u{3002}1",
        "\u{2460}\u{2461}\u{2466}.0.0.1",
        // userinfo tricks: the host is what follows the last `@`
        "1.1.1.1@127.0.0.1",
        "user:pw@127.0.0.1",
        "127.0.0.1%09",
        "[fe80::1%25lo]",
        "[::1%25lo]",
    ];
    let dir = TempDir::new().unwrap();
    let cs = CensusStore::new(db(&dir)).unwrap();
    for host in hosts {
        for scheme in ["http", "https"] {
            let uri = format!("{scheme}://{host}:{port}/c.json");
            let r = tokio::time::timeout(Duration::from_secs(20), cs.fetch(&uri, &root)).await;
            match r {
                Ok(Err(_)) => {}
                other => panic!("{uri}: {other:?}"),
            }
        }
    }
    assert_eq!(hits.load(SeqCst), 0, "a census fetch reached localhost");
}

// Private and special ranges as literals: refused for good, never connected.
#[tokio::test]
async fn ssrf_special_ranges_are_refused_as_permanent() {
    let (_, root) = small_doc();
    let hosts = [
        "10.1.2.3",
        "172.16.0.1",
        "172.31.255.255",
        "192.168.0.1",
        "169.254.169.254",
        "100.64.0.1",
        "100.127.255.254",
        "192.0.2.1",
        "198.51.100.1",
        "203.0.113.1",
        "224.0.0.1",
        "239.255.255.255",
        "240.0.0.1",
        "255.255.255.255",
        "[::]",
        "[fc00::1]",
        "[fd12:3456::1]",
        "[fe80::1]",
        "[ff02::1]",
        "[::ffff:10.0.0.1]",
        "[::ffff:169.254.169.254]",
        "[::10.0.0.1]",
        "[64:ff9b::a00:1]",
        "[64:ff9b:1::a00:1]",
        "[2002:a00:1::]",
        "[2001:db8::1]",
        "[2001::1]",
    ];
    let dir = TempDir::new().unwrap();
    let cs = CensusStore::new(db(&dir)).unwrap();
    for host in hosts {
        let e = cs
            .fetch(&format!("http://{host}/c.json"), &root)
            .await
            .unwrap_err();
        assert!(
            matches!(&e, CensusError::Refused(m) if m.contains("not a public address")),
            "{host}: {e}"
        );
    }
}

// A redirect of any 3xx kind is not followed (even with private hosts
// allowed), and the target is never contacted.
#[tokio::test]
async fn every_redirect_code_is_refused() {
    let (doc, root) = small_doc();
    let (target, thits) = canned(ok_head(doc.len()), doc.clone().into_bytes()).await;
    for code in [300, 301, 302, 303, 305, 307, 308] {
        let head = format!(
            "HTTP/1.1 {code} X\r\nlocation: http://{target}/c.json\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
        );
        let (redir, _) = canned(head, vec![]).await;
        let dir = TempDir::new().unwrap();
        let cs = private(&dir, 1 << 20);
        let e = cs
            .fetch(&format!("http://{redir}/c.json"), &root)
            .await
            .unwrap_err();
        assert!(e.to_string().contains(&code.to_string()), "{code}: {e}");
        assert!(!cs.has(&root).unwrap(), "{code}");
    }
    // Redirects to file:// and to a metadata address, too.
    for loc in ["file:///etc/hostname", "http://169.254.169.254/latest/"] {
        let head = format!(
            "HTTP/1.1 302 Found\r\nlocation: {loc}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
        );
        let (redir, _) = canned(head, vec![]).await;
        let dir = TempDir::new().unwrap();
        let cs = private(&dir, 1 << 20);
        assert!(cs.fetch(&format!("http://{redir}/"), &root).await.is_err());
    }
    assert_eq!(thits.load(SeqCst), 0, "a redirect was followed");
}

// ------------------------------------------------------------- file://

#[tokio::test]
async fn file_uri_escapes_are_refused() {
    let (doc, root) = small_doc();
    let base = TempDir::new().unwrap();
    let inside = base.path().join("census");
    let outside = base.path().join("secret");
    std::fs::create_dir(&inside).unwrap();
    std::fs::create_dir(&outside).unwrap();
    // A sibling whose name starts with the census directory's name.
    let evil = base.path().join("census-evil");
    std::fs::create_dir(&evil).unwrap();
    std::fs::write(outside.join("c.json"), &doc).unwrap();
    std::fs::write(evil.join("c.json"), &doc).unwrap();
    std::fs::write(inside.join("ok.json"), &doc).unwrap();
    let out = |p: &Path| p.display().to_string();
    std::os::unix::fs::symlink(outside.join("c.json"), inside.join("link.json")).unwrap();
    std::os::unix::fs::symlink(&outside, inside.join("dirlink")).unwrap();
    std::os::unix::fs::symlink("../secret/c.json", inside.join("rel.json")).unwrap();
    std::os::unix::fs::symlink("/dev/zero", inside.join("zero.json")).unwrap();
    // A symlink loop.
    std::os::unix::fs::symlink(inside.join("loop2"), inside.join("loop1")).unwrap();
    std::os::unix::fs::symlink(inside.join("loop1"), inside.join("loop2")).unwrap();

    let dbdir = TempDir::new().unwrap();
    let cs = with_dir(&dbdir, &inside, 1 << 20);
    let i = out(&inside);
    let uris = vec![
        format!("file://{i}/../secret/c.json"),
        format!("file://{i}/%2e%2e/secret/c.json"),
        format!("file://{i}/%2E%2E/secret/c.json"),
        format!("file://{i}/..%2fsecret/c.json"),
        format!("file://{i}/dirlink/c.json"),
        format!("file://{i}/link.json"),
        format!("file://{i}/rel.json"),
        format!("file://{i}/zero.json"),
        format!("file://{i}/loop1"),
        format!("file://{i}/missing.json"),
        format!("file://{i}"),
        format!("file://{}/c.json", out(&evil)),
        format!("file://{}/c.json", out(&outside)),
        format!("file://{i}-evil/c.json"),
        "file:///etc/hostname".into(),
        "file:///etc/passwd".into(),
        "file:///proc/self/environ".into(),
        "file:///dev/zero".into(),
        "file:///".into(),
        // NUL, and a non-local host
        format!("file://{i}/ok.json%00.json"),
        format!("file://evil.example{i}/ok.json"),
        // relative and scheme confusion
        "file:c.json".into(),
        "file:/c.json".into(),
        "FILE:///etc/hostname".into(),
    ];
    for uri in &uris {
        let r = tokio::time::timeout(Duration::from_secs(10), cs.fetch(uri, &root))
            .await
            .unwrap_or_else(|_| panic!("{uri} hung"));
        assert!(r.is_err(), "{uri} was accepted");
        assert!(!cs.has(&root).unwrap(), "{uri} stored a census");
    }
    // The honest file still works, also spelled with a redundant `./` and
    // `%6f` (percent-encoded `o`).
    let good = TempDir::new().unwrap();
    let cs = with_dir(&good, &inside, 1 << 20);
    cs.fetch(&format!("file://{i}/./ok.json"), &root)
        .await
        .unwrap();
    let good2 = TempDir::new().unwrap();
    let cs = with_dir(&good2, &inside, 1 << 20);
    cs.fetch(&format!("file://{i}/%6fk.json"), &root)
        .await
        .unwrap();
}

#[tokio::test]
async fn file_fifo_and_huge_files_do_not_hang_or_fill_memory() {
    let (doc, root) = small_doc();
    let base = TempDir::new().unwrap();
    let inside = base.path().join("c");
    std::fs::create_dir(&inside).unwrap();
    // FIFO with no writer: opening it for read would block forever.
    let fifo = inside.join("fifo.json");
    let st = std::process::Command::new("mkfifo").arg(&fifo).status();
    let have_fifo = st.is_ok_and(|s| s.success());
    // A sparse 64 GiB file: the cap, not the size, bounds the read.
    let big = inside.join("big.json");
    let f = std::fs::File::create(&big).unwrap();
    f.set_len(64 << 30).unwrap();
    drop(f);
    // A file exactly at the cap with a valid document is fine; one byte over
    // is refused.
    let cap = doc.len() as u64;
    std::fs::write(inside.join("edge.json"), &doc).unwrap();
    let mut over = doc.clone().into_bytes();
    over.push(b' ');
    std::fs::write(inside.join("over.json"), &over).unwrap();

    let dbdir = TempDir::new().unwrap();
    let cs = with_dir(&dbdir, &inside, cap);
    let i = inside.display().to_string();
    if have_fifo {
        let t = Instant::now();
        let r = tokio::time::timeout(
            Duration::from_secs(10),
            cs.fetch(&format!("file://{i}/fifo.json"), &root),
        )
        .await;
        assert!(
            matches!(r, Ok(Err(_))),
            "fifo: {r:?} after {:?}",
            t.elapsed()
        );
    }
    let r = cs.fetch(&format!("file://{i}/big.json"), &root).await;
    assert!(r.is_err());
    let r = cs.fetch(&format!("file://{i}/over.json"), &root).await;
    assert!(r.is_err(), "one byte over the cap");
    cs.fetch(&format!("file://{i}/edge.json"), &root)
        .await
        .unwrap();
}

// ------------------------------------------------------- hostile servers

#[tokio::test]
async fn http_body_caps_hold_for_every_framing() {
    let (doc, root) = small_doc();
    let cap = doc.len();
    let mut padded = doc.clone().into_bytes();
    padded.extend_from_slice(&vec![b' '; 4096]);

    // Exactly at the cap: accepted, content-length framing.
    let (a, _) = canned(ok_head(cap), doc.clone().into_bytes()).await;
    let dir = TempDir::new().unwrap();
    private(&dir, cap as u64)
        .fetch(&format!("http://{a}/c"), &root)
        .await
        .unwrap();

    // One byte over, honest content-length.
    let mut one = doc.clone().into_bytes();
    one.push(b' ');
    let (a, _) = canned(ok_head(cap + 1), one).await;
    let dir = TempDir::new().unwrap();
    let e = private(&dir, cap as u64)
        .fetch(&format!("http://{a}/c"), &root)
        .await
        .unwrap_err();
    assert!(matches!(e, CensusError::Refused(_)), "{e}");

    // Chunked with no length, far over the cap.
    let mut chunked = Vec::new();
    for c in padded.chunks(100) {
        chunked.extend_from_slice(format!("{:x}\r\n", c.len()).as_bytes());
        chunked.extend_from_slice(c);
        chunked.extend_from_slice(b"\r\n");
    }
    chunked.extend_from_slice(b"0\r\n\r\n");
    let head = "HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n";
    let (a, _) = canned(head.into(), chunked).await;
    let dir = TempDir::new().unwrap();
    let e = private(&dir, cap as u64)
        .fetch(&format!("http://{a}/c"), &root)
        .await
        .unwrap_err();
    assert!(matches!(e, CensusError::Refused(_)), "{e}");

    // A content-length that lies low: the client must not read past it, and a
    // truncated document is a format error, never a stored census.
    let (a, _) = canned(ok_head(10), padded.clone()).await;
    let dir = TempDir::new().unwrap();
    let e = private(&dir, 1 << 20)
        .fetch(&format!("http://{a}/c"), &root)
        .await
        .unwrap_err();
    assert!(!matches!(e, CensusError::Storage(_)), "{e}");

    // A content-length that lies high and then closes early.
    let (a, _) = canned(ok_head(1 << 30), doc.clone().into_bytes()).await;
    let dir = TempDir::new().unwrap();
    let cs = private(&dir, 1 << 20);
    assert!(cs.fetch(&format!("http://{a}/c"), &root).await.is_err());
    assert!(!cs.has(&root).unwrap());
}

// Header floods, absurd lengths and endless 1xx answers end in an error
// within seconds, without reading the whole stream.
#[tokio::test]
async fn hostile_http_framing_ends_quickly() {
    let (_, root) = small_doc();
    let mut cases: Vec<(&str, Vec<u8>)> = Vec::new();
    // 8 MiB of header
    let big = format!(
        "HTTP/1.1 200 OK\r\nx-junk: {}\r\ncontent-length: 2\r\n\r\n{{}}",
        "a".repeat(8 << 20)
    );
    cases.push(("one huge header", big.into_bytes()));
    let many: String = (0..50_000).map(|i| format!("x-{i}: 1\r\n")).collect();
    let many = format!("HTTP/1.1 200 OK\r\n{many}content-length: 2\r\n\r\n{{}}");
    cases.push(("50k headers", many.into_bytes()));
    cases.push((
        "absurd length",
        b"HTTP/1.1 200 OK\r\ncontent-length: 99999999999999999999999\r\n\r\n{}".to_vec(),
    ));
    cases.push((
        "two lengths",
        b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\ncontent-length: 5\r\n\r\n{}".to_vec(),
    ));
    cases.push((
        "chunk size overflow",
        b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\nffffffffffffffffff\r\n".to_vec(),
    ));
    let ext = format!(
        "HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\n2;{}\r\n{{}}\r\n0\r\n\r\n",
        "x=y;".repeat(4 << 20)
    );
    cases.push(("chunk extension flood", ext.into_bytes()));
    cases.push(("garbage", b"\x00\x01\x02 not http at all\r\n\r\n".to_vec()));
    cases.push(("http/0.9", b"{}".to_vec()));
    for (name, bytes) in cases {
        let bytes = Arc::new(bytes);
        let (a, _) = raw(move |mut s| {
            let bytes = bytes.clone();
            async move {
                let _ = s.write_all(&bytes).await;
                let _ = s.shutdown().await;
            }
        })
        .await;
        let dir = TempDir::new().unwrap();
        let cs = private(&dir, 1 << 20);
        let t = Instant::now();
        let r = tokio::time::timeout(
            Duration::from_secs(20),
            cs.fetch(&format!("http://{a}/c.json"), &root),
        )
        .await;
        assert!(
            matches!(r, Ok(Err(_))),
            "{name}: {r:?} after {:?}",
            t.elapsed()
        );
        assert!(!cs.has(&root).unwrap(), "{name}");
    }
}

// A server that claims gzip must not be decoded (no decompression bomb).
#[tokio::test]
async fn content_encoding_is_not_decoded() {
    let (_, root) = small_doc();
    // 2 KiB that would be a gigabyte if a decoder were enabled is
    // indistinguishable here; assert on the behaviour: bytes are used raw.
    let gz = [
        0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 3, 0x4b, 0x4c, 0x4a, 0x06, 0x00,
    ];
    for enc in ["gzip", "br", "deflate", "zstd"] {
        let head = format!(
            "HTTP/1.1 200 OK\r\ncontent-encoding: {enc}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
            gz.len()
        );
        let (a, _) = canned(head, gz.to_vec()).await;
        let dir = TempDir::new().unwrap();
        let e = private(&dir, 1 << 20)
            .fetch(&format!("http://{a}/c"), &root)
            .await
            .unwrap_err();
        assert!(matches!(e, CensusError::Format(_)), "{enc}: {e}");
    }
}

// Documents that must be refused, however they arrive. Each is served from a
// file so the matrix runs in milliseconds.
#[tokio::test]
async fn hostile_documents_are_refused() {
    let base = TempDir::new().unwrap();
    let inside = base.path().join("c");
    std::fs::create_dir(&inside).unwrap();
    let a = hex::encode(addr_of(1));
    let b = hex::encode(addr_of(2));
    let any = [0x39; 32];
    let deep = |n: usize| format!("{}{}", "[".repeat(n), "]".repeat(n));
    let docs: Vec<(&str, Vec<u8>)> = vec![
        ("invalid utf8 in key", b"{\"participants\":[{\"key\":\"0x\xff\xfe\",\"weight\":1}]}".to_vec()),
        ("invalid utf8 outside", b"\xff\xfe{\"participants\":[]}".to_vec()),
        ("bom", b"\xef\xbb\xbf{\"participants\":[{\"key\":\"0x01\",\"weight\":1}]}".to_vec()),
        ("nul bytes", vec![0u8; 4096]),
        ("empty", vec![]),
        ("null", b"null".to_vec()),
        ("array top", b"[]".to_vec()),
        (
            "duplicate key field",
            format!(r#"{{"participants":[{{"key":"0x{a}","key":"0x{b}","weight":1}}]}}"#).into_bytes(),
        ),
        (
            "key and address both",
            format!(r#"{{"participants":[{{"key":"0x{a}","address":"0x{b}","weight":1}}]}}"#).into_bytes(),
        ),
        (
            "duplicate participants key",
            format!(r#"{{"participants":[],"participants":[{{"key":"0x{a}","weight":1}}]}}"#).into_bytes(),
        ),
        (
            "same address different case",
            format!(
                r#"{{"participants":[{{"key":"0x{a}","weight":1}},{{"key":"0x{}","weight":2}}]}}"#,
                a.to_uppercase()
            )
            .into_bytes(),
        ),
        (
            "same address with and without prefix",
            format!(r#"{{"participants":[{{"key":"0x{a}","weight":1}},{{"key":"{a}","weight":2}}]}}"#).into_bytes(),
        ),
        (
            "weight 2^88",
            format!(r#"{{"participants":[{{"key":"0x{a}","weight":"309485009821345068724781056"}}]}}"#).into_bytes(),
        ),
        (
            "weight huge number",
            format!(r#"{{"participants":[{{"key":"0x{a}","weight":{}}}]}}"#, "9".repeat(5000)).into_bytes(),
        ),
        (
            "weight unicode digits",
            format!(r#"{{"participants":[{{"key":"0x{a}","weight":"١"}}]}}"#).into_bytes(),
        ),
        (
            "weight object",
            format!(r#"{{"participants":[{{"key":"0x{a}","weight":{{"x":1}}}}]}}"#).into_bytes(),
        ),
        (
            "nested unknown field",
            format!(r#"{{"x":{},"participants":[{{"key":"0x{a}","weight":1}}]}}"#, deep(100_000)).into_bytes(),
        ),
        (
            "nested weight",
            format!(r#"{{"participants":[{{"key":"0x{a}","weight":{}}}]}}"#, deep(100_000)).into_bytes(),
        ),
        ("deep jsonl line", format!("{{\"key\":\"0x{a}\",\"weight\":1,\"x\":{}}}\n", deep(100_000)).into_bytes()),
        (
            "index u64 max",
            format!(r#"{{"participants":[{{"addressIndex":18446744073709551615,"address":"0x{a}","weight":1}}]}}"#).into_bytes(),
        ),
        (
            "index negative",
            format!(r#"{{"participants":[{{"addressIndex":-1,"address":"0x{a}","weight":1}}]}}"#).into_bytes(),
        ),
        (
            "index float",
            format!(r#"{{"participants":[{{"addressIndex":1.5,"address":"0x{a}","weight":1}}]}}"#).into_bytes(),
        ),
        (
            "root not a number",
            format!(r#"{{"root":"abc","participants":[{{"key":"0x{a}","weight":1}}]}}"#).into_bytes(),
        ),
        (
            "root above the field",
            format!(r#"{{"root":"{}","participants":[{{"key":"0x{a}","weight":1}}]}}"#, "9".repeat(100)).into_bytes(),
        ),
        ("only blank jsonl", b"\n\n  \n".to_vec()),
        ("jsonl with garbage tail", format!("{{\"key\":\"0x{a}\",\"weight\":1}}\nnot json\n").into_bytes()),
        ("trailing garbage", format!(r#"{{"participants":[{{"key":"0x{a}","weight":1}}]}} trailing"#).into_bytes()),
    ];
    let dbdir = TempDir::new().unwrap();
    let cs = with_dir(&dbdir, &inside, 64 << 20);
    for (i, (name, body)) in docs.iter().enumerate() {
        let p = inside.join(format!("d{i}.json"));
        std::fs::write(&p, body).unwrap();
        let uri = format!("file://{}", p.display());
        let r = tokio::time::timeout(Duration::from_secs(20), cs.fetch(&uri, &any)).await;
        match r {
            Ok(Err(
                CensusError::Format(_) | CensusError::RootMismatch { .. } | CensusError::Fetch(_),
            )) => {}
            other => panic!("{name}: {other:?}"),
        }
        assert!(!cs.has(&any).unwrap(), "{name} stored a census");
    }
}

// Weights at the extremes with the right root are accepted and proofs verify;
// a document whose own `root` field is honest but whose leaves differ is not.
#[tokio::test]
async fn weight_extremes_and_root_binding() {
    let max = (1u128 << 88) - 1;
    let parts: Vec<_> = vec![
        (addr_of(1), max),
        (addr_of(2), max),
        (addr_of(3), 1),
        (addr_of(4), 0),
    ];
    let (doc, root) = doc_of(&parts);
    let base = TempDir::new().unwrap();
    let inside = base.path().join("c");
    std::fs::create_dir(&inside).unwrap();
    std::fs::write(inside.join("c.json"), &doc).unwrap();
    let dbdir = TempDir::new().unwrap();
    let cs = with_dir(&dbdir, &inside, 1 << 20);
    cs.fetch(&format!("file://{}/c.json", inside.display()), &root)
        .await
        .unwrap();
    for (a, w) in &parts {
        let (p, got) = cs.proof_async(&root, a).await.unwrap().expect("member");
        assert_eq!(got, *w);
        assert!(davinci_zkvm_sdk::census::verify_census_proof(&p));
    }
    // Total weight can't overflow u128 with 2^22 voters of 2^88 - 1.
    assert!((1u128 << 22).checked_mul(max).is_some());

    // A dump whose `root` field matches the chain but whose participants are
    // a different set: the rebuilt tree decides.
    let other = doc_of(&[(addr_of(9), 1)]).0;
    let mut v: serde_json::Value = serde_json::from_str(&other).unwrap();
    v["root"] = serde_json::json!(format!("0x{}", hex::encode(root)));
    std::fs::write(inside.join("liar.json"), v.to_string()).unwrap();
    let dbdir = TempDir::new().unwrap();
    let cs = with_dir(&dbdir, &inside, 1 << 20);
    let e = cs
        .fetch(&format!("file://{}/liar.json", inside.display()), &root)
        .await
        .unwrap_err();
    assert!(matches!(e, CensusError::RootMismatch { .. }), "{e}");
    assert!(!cs.has(&root).unwrap());
}

#[tokio::test]
async fn participant_cap_is_enforced() {
    let base = TempDir::new().unwrap();
    let inside = base.path().join("c");
    std::fs::create_dir(&inside).unwrap();
    let parts: Vec<_> = (0..50).map(|i| (addr_of(i), 1u128)).collect();
    let (doc, root) = doc_of(&parts);
    std::fs::write(inside.join("c.json"), doc).unwrap();
    let uri = format!("file://{}/c.json", inside.display());
    for (max, ok) in [(49, false), (50, true)] {
        let opts = CensusOptions {
            dir: Some(inside.clone()),
            max_participants: max,
            ..CensusOptions::default()
        };
        let dbdir = TempDir::new().unwrap();
        let cs = CensusStore::with_options(db(&dbdir), opts).unwrap();
        assert_eq!(cs.fetch(&uri, &root).await.is_ok(), ok, "max {max}");
    }
    // JSONL with a million tiny lines against a tiny cap is refused quickly.
    let line = format!(
        "{{\"key\":\"0x{}\",\"weight\":1}}\n",
        hex::encode(addr_of(1))
    );
    std::fs::write(inside.join("many.jsonl"), line.repeat(200_000)).unwrap();
    let opts = CensusOptions {
        dir: Some(inside.clone()),
        max_participants: 100,
        ..CensusOptions::default()
    };
    let dbdir = TempDir::new().unwrap();
    let cs = CensusStore::with_options(db(&dbdir), opts).unwrap();
    let t = Instant::now();
    let e = cs
        .fetch(
            &format!("file://{}/many.jsonl", inside.display()),
            &[1u8; 32],
        )
        .await
        .unwrap_err();
    assert!(matches!(e, CensusError::Format(_)), "{e}");
    assert!(t.elapsed() < Duration::from_secs(10), "{:?}", t.elapsed());
}

// Many bootstraps of one URI: every one ends Ok or backing off, the census is
// stored once and intact.
#[tokio::test]
async fn concurrent_fetches_of_one_uri() {
    let (doc, root) = small_doc();
    let (a, hits) = canned(ok_head(doc.len()), doc.clone().into_bytes()).await;
    let dir = TempDir::new().unwrap();
    let cs = Arc::new(private(&dir, 1 << 20));
    let uri = format!("http://{a}/c.json");
    let mut js = tokio::task::JoinSet::new();
    for _ in 0..24 {
        let (cs, uri) = (cs.clone(), uri.clone());
        js.spawn(async move { cs.fetch(&uri, &root).await });
    }
    while let Some(r) = js.join_next().await {
        r.unwrap().unwrap();
    }
    assert!(hits.load(SeqCst) <= 24);
    let (p, w) = cs.proof_async(&root, &addr_of(2)).await.unwrap().unwrap();
    assert_eq!(w, 3);
    assert!(davinci_zkvm_sdk::census::verify_census_proof(&p));

    // The same URI under several wrong roots, concurrently, all fail and none
    // stores anything.
    let dir = TempDir::new().unwrap();
    let cs = Arc::new(private(&dir, 1 << 20));
    let mut js = tokio::task::JoinSet::new();
    for k in 1..9u64 {
        let (cs, uri) = (cs.clone(), uri.clone());
        js.spawn(async move { (k, cs.fetch(&uri, &other_root(&root, k)).await) });
    }
    while let Some(r) = js.join_next().await {
        let (k, r) = r.unwrap();
        assert!(
            matches!(r, Err(CensusError::RootMismatch { .. })),
            "{k}: {r:?}"
        );
        assert!(!cs.has(&other_root(&root, k)).unwrap());
    }
}

// A server that accepts and never answers must not hold a download slot for
// minutes: four of them would stall every other census bootstrap.
#[tokio::test]
#[ignore = "takes 30 s (the read timeout); a silent server holds a download slot that long"]
async fn silent_server_releases_its_download_slot_quickly() {
    let (_, root) = small_doc();
    let (a, _) = raw(|s| async move {
        let _keep = s;
        tokio::time::sleep(Duration::from_secs(3600)).await;
    })
    .await;
    let dir = TempDir::new().unwrap();
    let t = Instant::now();
    let r = tokio::time::timeout(
        Duration::from_secs(60),
        private(&dir, 1 << 20).fetch(&format!("http://{a}/c.json"), &root),
    )
    .await;
    eprintln!("silent server: {r:?} after {:?}", t.elapsed());
    assert!(matches!(r, Ok(Err(_))), "{r:?}");
    assert!(t.elapsed() < Duration::from_secs(45), "{:?}", t.elapsed());
}

fn peak_rss_mb() -> u64 {
    let s = std::fs::read_to_string("/proc/self/status").unwrap();
    let l = s.lines().find(|l| l.starts_with("VmHWM:")).unwrap();
    l.split_whitespace().nth(1).unwrap().parse::<u64>().unwrap() / 1024
}

// A document of empty entries is refused only after serde built every
// participant: memory should stay near the document size, not a multiple.
#[tokio::test]
async fn tiny_entries_do_not_multiply_memory() {
    let base = TempDir::new().unwrap();
    let inside = base.path().join("c");
    std::fs::create_dir(&inside).unwrap();
    let entry = "{\"key\":\"\",\"weight\":1},";
    let n = 9_000_000usize;
    let mut body = String::from("{\"participants\":[");
    body.push_str(&entry.repeat(n));
    body.push_str("{\"key\":\"\",\"weight\":1}]}");
    std::fs::write(inside.join("c.json"), &body).unwrap();
    let size_mb = body.len() as u64 >> 20;
    drop(body);
    let before = peak_rss_mb();
    let dbdir = TempDir::new().unwrap();
    let cs = with_dir(&dbdir, &inside, 256 << 20);
    let e = cs
        .fetch(&format!("file://{}/c.json", inside.display()), &[1u8; 32])
        .await
        .unwrap_err();
    let after = peak_rss_mb();
    eprintln!("doc {size_mb} MiB, peak {before} -> {after} MiB, {e}");
    assert!(
        after - before < 2 * size_mb,
        "peak grew {} MiB for a {size_mb} MiB document",
        after - before
    );
}

// ------------------------------------------------- origin 3, hostile contract

mod hostile {
    use super::common::anvil::RpcProxy;
    use super::*;
    use alloy::network::{EthereumWallet, TransactionBuilder};
    use alloy::node_bindings::{Anvil, AnvilInstance};
    use alloy::primitives::{Address, B256, Bytes, U256};
    use alloy::providers::{DynProvider, Provider, ProviderBuilder};
    use alloy::rpc::types::TransactionRequest;
    use alloy::signers::local::PrivateKeySigner;
    use alloy::sol_types::{SolCall, SolEvent};
    use davinci_sequencer::census::onchain::{OnchainIndex, UsableError};

    alloy::sol!(
        #[allow(missing_docs)]
        Census,
        "abi/OnchainCensus.json"
    );

    pub fn enabled() -> bool {
        super::common::anvil::enabled()
    }

    pub struct Chain {
        pub anvil: AnvilInstance,
        pub owner: DynProvider,
        pub reader: DynProvider,
    }

    impl Chain {
        pub async fn start() -> Self {
            let bin = std::path::PathBuf::from(std::env::var("HOME").unwrap_or_default())
                .join(".foundry/bin/anvil");
            let anvil = if bin.exists() {
                Anvil::at(bin).spawn()
            } else {
                Anvil::new().spawn()
            };
            let signer: PrivateKeySigner = anvil.keys()[0].clone().into();
            let owner = ProviderBuilder::new()
                .with_simple_nonce_management()
                .wallet(EthereumWallet::from(signer))
                .connect_http(anvil.endpoint_url())
                .erased();
            let reader = ProviderBuilder::new()
                .disable_recommended_fillers()
                .connect_http(anvil.endpoint_url())
                .erased();
            Chain {
                anvil,
                owner,
                reader,
            }
        }

        pub async fn set_code(&self, at: Address, code: Vec<u8>) {
            let () = self
                .reader
                .raw_request("anvil_setCode".into(), (at, Bytes::from(code)))
                .await
                .unwrap();
        }

        pub async fn set_slot(&self, at: Address, slot: U256, v: U256) {
            let _: serde_json::Value = self
                .reader
                .raw_request(
                    "anvil_setStorageAt".into(),
                    (at, B256::from(slot), B256::from(v)),
                )
                .await
                .unwrap();
        }

        pub async fn mine(&self, n: u64) {
            let () = self
                .reader
                .raw_request("anvil_mine".into(), (U256::from(n),))
                .await
                .unwrap();
        }

        pub async fn head(&self) -> u64 {
            self.reader.get_block_number().await.unwrap()
        }

        /// Sends `input` to `to` and waits for the receipt.
        pub async fn send(&self, to: Address, input: Vec<u8>) {
            let tx = TransactionRequest::default()
                .with_to(to)
                .with_input(Bytes::from(input));
            let r = self
                .owner
                .send_transaction(tx)
                .await
                .unwrap()
                .get_receipt()
                .await
                .unwrap();
            assert!(r.status());
        }

        pub fn index(&self, db: &Db, chain_id: u64, provider: DynProvider) -> OnchainIndex {
            OnchainIndex::new(
                db,
                chain_id,
                provider,
                Duration::from_millis(50),
                Duration::ZERO,
                0,
            )
            .unwrap()
        }
    }

    /// Runtime code of a contract that answers any 4-byte call with
    /// `SLOAD(selector)` and, for calldata >= 64 bytes, emits
    /// `LOG2(topic0 = word0, topic1 = word1, data = rest)`: a census contract
    /// whose logs and state are whatever the test says.
    pub fn puppet() -> Vec<u8> {
        let mut c = vec![
            0x36, // CALLDATASIZE
            0x60, 0x40, // PUSH1 0x40
            0x11, // GT: 0x40 > size
            0x60, 0x00, // PUSH1 <read> (patched)
            0x57, // JUMPI
            // write mode: CALLDATACOPY(0, 0x40, size - 0x40)
            0x60, 0x40, 0x36, 0x03, // len = size - 0x40
            0x60, 0x40, // offset
            0x60, 0x00, // dest
            0x37, // CALLDATACOPY
            0x60, 0x20, 0x35, // topic1
            0x60, 0x00, 0x35, // topic0
            0x60, 0x40, 0x36, 0x03, // len
            0x60, 0x00, // offset
            0xa2, // LOG2
            0x00, // STOP
        ];
        let read = c.len() as u8;
        c[5] = read;
        c.extend_from_slice(&[
            0x5b, // JUMPDEST
            0x60, 0x00, 0x35, // calldataload(0)
            0x60, 0xe0, 0x1c, // >> 224
            0x54, // SLOAD
            0x60, 0x00, 0x52, // MSTORE(0)
            0x60, 0x20, 0x60, 0x00, 0xf3, // RETURN(0, 32)
        ]);
        c
    }

    pub fn root_slot() -> U256 {
        U256::from(u32::from_be_bytes(Census::getCensusRootCall::SELECTOR))
    }

    pub fn size_slot() -> U256 {
        U256::from(u32::from_be_bytes(Census::treeSizeCall::SELECTOR))
    }

    fn word(v: U256) -> [u8; 32] {
        v.to_be_bytes()
    }

    fn user_topic(a: &[u8; 20]) -> [u8; 32] {
        let mut t = [0u8; 32];
        t[12..].copy_from_slice(a);
        t
    }

    /// A puppet census with an honest local copy of its tree.
    pub struct Puppet {
        pub at: Address,
        pub tree: LeanImt,
    }

    impl Puppet {
        pub async fn deploy(ch: &Chain, at: Address) -> Self {
            ch.set_code(at, puppet()).await;
            Puppet {
                at,
                tree: LeanImt::new(),
            }
        }

        pub async fn emit_weight_changed(&self, ch: &Chain, who: &[u8; 20], prev: u128, new: u128) {
            let mut input = Census::WeightChanged::SIGNATURE_HASH.to_vec();
            input.extend_from_slice(&user_topic(who));
            input.extend_from_slice(&word(U256::from(prev)));
            input.extend_from_slice(&word(U256::from(new)));
            ch.send(self.at, input).await;
        }

        pub async fn emit_added(
            &self,
            ch: &Chain,
            who: &[u8; 20],
            weight: U256,
            leaf: U256,
            new_root: U256,
        ) {
            let mut input = Census::CensusMemberAdded::SIGNATURE_HASH.to_vec();
            input.extend_from_slice(&user_topic(who));
            for w in [weight, leaf, new_root, U256::ZERO] {
                input.extend_from_slice(&word(w));
            }
            ch.send(self.at, input).await;
        }

        /// An honest add: both events, then the state follows.
        pub async fn add(&mut self, ch: &Chain, who: &[u8; 20], weight: u128) {
            let leaf = census_leaf(who, weight).unwrap();
            self.tree.insert(leaf);
            self.emit_weight_changed(ch, who, 0, weight).await;
            let be = |f: &[u8; 32]| U256::from_be_bytes(*f);
            self.emit_added(
                ch,
                who,
                U256::from(weight),
                be(&leaf),
                be(&self.tree.root()),
            )
            .await;
            self.sync_state(ch).await;
        }

        pub async fn sync_state_of(&self, ch: &Chain, tree: &LeanImt) {
            let r = U256::from_be_bytes(tree.root());
            ch.set_slot(self.at, root_slot(), r).await;
            ch.set_slot(self.at, size_slot(), U256::from(tree.len()))
                .await;
        }

        pub async fn sync_state(&self, ch: &Chain) {
            let r = U256::from_be_bytes(self.tree.root());
            ch.set_slot(self.at, root_slot(), r).await;
            ch.set_slot(self.at, size_slot(), U256::from(self.tree.len()))
                .await;
        }
    }

    pub fn a(n: u8) -> Address {
        Address::from([n; 20])
    }

    fn setup() -> (TempDir, Db) {
        let dir = TempDir::new().unwrap();
        let db = Db::open_in(dir.path()).unwrap();
        (dir, db)
    }

    // The control: a puppet that behaves like a census is indexed, so the
    // other cases below fail for the reason they claim.
    #[tokio::test]
    async fn puppet_census_control() {
        if !enabled() {
            return;
        }
        let ch = Chain::start().await;
        let mut p = Puppet::deploy(&ch, a(0xc1)).await;
        for i in 1..=5u8 {
            p.add(&ch, &[0x50 + i; 20], u128::from(i)).await;
        }
        let (_d, db) = setup();
        let ix = ch.index(&db, 31337, ch.reader.clone());
        let c = p.at.into_array();
        ix.register(c).await.unwrap();
        ix.sync(c, ch.head().await).await.unwrap();
        let (root, size, _) = ix.latest(&c).unwrap();
        assert_eq!((root, size), (p.tree.root(), 5));
        let (proof, w) = ix.proof(&c, &root, &[0x53; 20]).await.unwrap().unwrap();
        assert_eq!(w, 3);
        assert!(davinci_zkvm_sdk::census::verify_census_proof(&proof));
    }

    // Contracts that cannot be read: no code, always reverting, returning
    // short, long or out-of-range words. None may panic, none may be marked
    // synced, each must back off.
    #[tokio::test]
    async fn unreadable_contracts_fail_closed() {
        if !enabled() {
            return;
        }
        let ch = Chain::start().await;
        // PUSH1 0 DUP1 REVERT
        let revert = vec![0x60, 0x00, 0x80, 0xfd];
        // returns n bytes of 0xff: PUSH1 n PUSH1 0 MSTORE8-free: use CODECOPY-less RETURN of zero mem
        let ret_len = |n: u8| vec![0x60, n, 0x60, 0x00, 0xf3];
        // returns a 32-byte 0xff..ff word
        let ones = vec![
            0x7f, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
            0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
            0xff, 0xff, 0xff, 0xff, 0xff, 0x60, 0x00, 0x52, 0x60, 0x20, 0x60, 0x00, 0xf3,
        ];
        // INVALID opcode, and SELFDESTRUCT-style empty contract (STOP)
        let cases: Vec<(&str, Option<Vec<u8>>)> = vec![
            ("no code", None),
            ("revert", Some(revert)),
            ("empty return", Some(ret_len(0))),
            ("31 bytes", Some(ret_len(31))),
            ("96 bytes", Some(ret_len(96))),
            ("all ones", Some(ones)),
            ("invalid opcode", Some(vec![0xfe])),
            ("stop", Some(vec![0x00])),
            ("infinite loop", Some(vec![0x5b, 0x60, 0x00, 0x56])),
        ];
        let (_d, db) = setup();
        let ix = ch.index(&db, 31337, ch.reader.clone());
        let head0 = ch.head().await;
        for (i, (name, code)) in cases.iter().enumerate() {
            let at = a(0xd0 + i as u8);
            if let Some(code) = code {
                ch.set_code(at, code.clone()).await;
            }
            let c = at.into_array();
            ix.register(c).await.unwrap();
            let head = head0.max(ch.head().await);
            let r = tokio::time::timeout(Duration::from_secs(30), ix.sync(c, head))
                .await
                .unwrap_or_else(|_| panic!("{name}: sync hung"));
            // "all ones" and zero returns decode: only the root/size
            // consistency can refuse them, which it does via a mismatch.
            match r {
                Err(_) => {}
                Ok(()) => {
                    // Accepted only as an empty census (root 0, size 0).
                    let (root, size, _) = ix.latest(&c).unwrap();
                    assert_eq!(size, 0, "{name}");
                    assert_eq!(root, [0u8; 32], "{name}");
                }
            }
            assert!(
                ix.usable(&c).is_ok() || matches!(ix.usable(&c), Err(UsableError::Unusable(_)))
            );
        }
    }

    // Logs and state that disagree, every way, fail closed and never produce
    // a proof for a voter the contract state does not contain.
    #[tokio::test]
    async fn inconsistent_logs_are_never_trusted() {
        if !enabled() {
            return;
        }
        let ch = Chain::start().await;
        let be = |f: &[u8; 32]| U256::from_be_bytes(*f);
        let who = [0x61u8; 20];
        let leaf = census_leaf(&who, 5).unwrap();
        let tree = LeanImt::from_leaves(vec![leaf]);

        // (name, mutate puppet) -> sync must not mark synced.
        #[derive(Clone, Copy, Debug)]
        enum Lie {
            WrongNewRoot,
            WrongLeaf,
            WeightMismatch,
            ZeroAddress,
            ZeroWeight,
            StateAheadOfLogs,
            StateRootWrong,
            SizeBeyondLogs,
            NoLogsAtAll,
        }
        for (k, lie) in [
            Lie::WrongNewRoot,
            Lie::WrongLeaf,
            Lie::WeightMismatch,
            Lie::ZeroAddress,
            Lie::ZeroWeight,
            Lie::StateAheadOfLogs,
            Lie::StateRootWrong,
            Lie::SizeBeyondLogs,
            Lie::NoLogsAtAll,
        ]
        .into_iter()
        .enumerate()
        {
            let at = a(0xe0 + k as u8);
            let p = Puppet::deploy(&ch, at).await;
            let (mut root, mut size) = (be(&tree.root()), U256::from(1));
            let mut user = who;
            let (mut w, mut l, mut nr) = (U256::from(5), be(&leaf), be(&tree.root()));
            match lie {
                Lie::WrongNewRoot => nr += U256::from(1),
                Lie::WrongLeaf => l = be(&census_leaf(&who, 6).unwrap()),
                Lie::WeightMismatch => w = U256::from(6),
                Lie::ZeroAddress => user = [0; 20],
                Lie::ZeroWeight => w = U256::ZERO,
                Lie::StateAheadOfLogs => size = U256::from(2),
                Lie::StateRootWrong => root += U256::from(1),
                Lie::SizeBeyondLogs => size = U256::from(1) << 40,
                Lie::NoLogsAtAll => {}
            }
            if !matches!(lie, Lie::NoLogsAtAll) {
                p.emit_weight_changed(&ch, &who, 0, 5).await;
                p.emit_added(&ch, &user, w, l, nr).await;
            }
            ch.set_slot(at, root_slot(), root).await;
            ch.set_slot(at, size_slot(), size).await;
            let (_d, db) = setup();
            let ix = ch.index(&db, 31337, ch.reader.clone());
            let c = at.into_array();
            ix.register(c).await.unwrap();
            let r = tokio::time::timeout(Duration::from_secs(30), ix.sync(c, ch.head().await))
                .await
                .unwrap_or_else(|_| panic!("{lie:?}: hung"));
            assert!(r.is_err(), "{lie:?}: accepted a lying contract");
            assert!(ix.latest(&c).is_none(), "{lie:?}: marked synced");
            assert!(
                ix.proof(&c, &tree.root(), &who).await.is_err()
                    || ix.proof(&c, &tree.root(), &who).await.unwrap().is_none(),
                "{lie:?}: a proof was served"
            );
        }
    }

    // A hostile contract can emit a `uint88` word with dirty high bits; the
    // decoder truncates it. That must be equivalent to the truncated weight
    // (leaf and root still bind it), never a different weight.
    #[tokio::test]
    async fn dirty_uint88_words_cannot_change_a_weight() {
        if !enabled() {
            return;
        }
        let ch = Chain::start().await;
        let be = |f: &[u8; 32]| U256::from_be_bytes(*f);
        let who = [0x63u8; 20];
        let leaf = census_leaf(&who, 5).unwrap();
        let tree = LeanImt::from_leaves(vec![leaf]);
        for (k, w) in [
            (U256::from(1) << 88) | U256::from(5),
            (U256::from(0xdead) << 200) | U256::from(5),
            U256::MAX,
        ]
        .into_iter()
        .enumerate()
        {
            let at = a(0xb0 + k as u8);
            let p = Puppet::deploy(&ch, at).await;
            p.emit_weight_changed(&ch, &who, 0, 5).await;
            p.emit_added(&ch, &who, w, be(&leaf), be(&tree.root()))
                .await;
            p.sync_state_of(&ch, &tree).await;
            let (_d, db) = setup();
            let ix = ch.index(&db, 31337, ch.reader.clone());
            let c = at.into_array();
            ix.register(c).await.unwrap();
            if ix.sync(c, ch.head().await).await.is_ok() {
                let (proof, got) = ix.proof(&c, &tree.root(), &who).await.unwrap().unwrap();
                assert_eq!(got, 5, "{w}");
                assert!(davinci_zkvm_sdk::census::verify_census_proof(&proof));
            }
        }
    }

    // Re-adding a member with a new weight, or changing a weight, never
    // yields a proof with the second weight.
    #[tokio::test]
    async fn readded_members_and_weight_changes() {
        if !enabled() {
            return;
        }
        let ch = Chain::start().await;
        let mut p = Puppet::deploy(&ch, a(0xc2)).await;
        let who = [0x62u8; 20];
        p.add(&ch, &who, 5).await;
        let (_d, db) = setup();
        let ix = ch.index(&db, 31337, ch.reader.clone());
        let c = p.at.into_array();
        ix.register(c).await.unwrap();
        ix.sync(c, ch.head().await).await.unwrap();
        let first = ix.latest(&c).unwrap().0;

        // The same address added again with a bigger weight.
        p.add(&ch, &who, 500).await;
        ix.expire_backoff(&c);
        let r = ix.sync(c, ch.head().await).await;
        assert!(r.is_err(), "duplicate add accepted");
        // The index kept its earlier, consistent snapshot.
        assert_eq!(ix.latest(&c).unwrap().0, first);
        let (_, w) = ix.proof(&c, &first, &who).await.unwrap().unwrap();
        assert_eq!(w, 5);

        // A weight change is terminal.
        let ch2 = Chain::start().await;
        let mut p2 = Puppet::deploy(&ch2, a(0xc3)).await;
        p2.add(&ch2, &who, 5).await;
        let (_d2, db2) = setup();
        let ix2 = ch2.index(&db2, 31337, ch2.reader.clone());
        let c2 = p2.at.into_array();
        ix2.register(c2).await.unwrap();
        ix2.sync(c2, ch2.head().await).await.unwrap();
        p2.emit_weight_changed(&ch2, &who, 5, 1).await;
        assert!(ix2.sync(c2, ch2.head().await).await.is_err());
        assert!(matches!(ix2.usable(&c2), Err(UsableError::Unusable(_))));
        // Terminal: even a later honest state does not clear it.
        assert!(ix2.sync(c2, ch2.head().await).await.is_err());
        assert!(matches!(ix2.usable(&c2), Err(UsableError::Unusable(_))));
        // ... and survives a restart.
        drop(ix2);
        let ix3 = ch2.index(&db2, 31337, ch2.reader.clone());
        ix3.register(c2).await.unwrap();
        assert!(matches!(ix3.usable(&c2), Err(UsableError::Unusable(_))));
    }

    // Logs emitted by another contract (same event signatures) are ignored.
    #[tokio::test]
    async fn foreign_contract_logs_are_ignored() {
        if !enabled() {
            return;
        }
        let ch = Chain::start().await;
        let mut honest = Puppet::deploy(&ch, a(0xc4)).await;
        let mut other = Puppet::deploy(&ch, a(0xc5)).await;
        honest.add(&ch, &[0x71; 20], 1).await;
        other.add(&ch, &[0x72; 20], 2).await;
        other.add(&ch, &[0x73; 20], 3).await;
        let (_d, db) = setup();
        let ix = ch.index(&db, 31337, ch.reader.clone());
        let c = honest.at.into_array();
        ix.register(c).await.unwrap();
        ix.sync(c, ch.head().await).await.unwrap();
        let (root, size, _) = ix.latest(&c).unwrap();
        assert_eq!((root, size), (honest.tree.root(), 1));
        assert!(ix.proof(&c, &root, &[0x72; 20]).await.unwrap().is_none());
    }

    // A contract that vanishes after being indexed keeps serving its last
    // snapshot; syncing it fails and backs off instead of panicking.
    #[tokio::test]
    async fn contract_that_selfdestructs_after_indexing() {
        if !enabled() {
            return;
        }
        let ch = Chain::start().await;
        let mut p = Puppet::deploy(&ch, a(0xc6)).await;
        p.add(&ch, &[0x81; 20], 7).await;
        let (_d, db) = setup();
        let ix = ch.index(&db, 31337, ch.reader.clone());
        let c = p.at.into_array();
        ix.register(c).await.unwrap();
        ix.sync(c, ch.head().await).await.unwrap();
        let (root, _, _) = ix.latest(&c).unwrap();
        ch.set_code(p.at, vec![]).await;
        ch.mine(3).await;
        assert!(ix.sync(c, ch.head().await).await.is_err());
        assert!(ix.retry_in(&c).is_some());
        let (proof, w) = ix.proof(&c, &root, &[0x81; 20]).await.unwrap().unwrap();
        assert_eq!(w, 7);
        assert!(davinci_zkvm_sdk::census::verify_census_proof(&proof));
    }

    // A contract that lies about its size makes the first sync scan the whole
    // chain history. Bound the RPC work one hostile contract can cause.
    #[tokio::test]
    async fn lying_size_does_not_scan_the_whole_chain() {
        if !enabled() {
            return;
        }
        let ch = Chain::start().await;
        let at = a(0xc7);
        ch.set_code(at, puppet()).await;
        ch.set_slot(at, size_slot(), U256::from(1)).await;
        ch.set_slot(at, root_slot(), U256::from(1)).await;
        ch.mine(20_000).await;
        let proxy = RpcProxy::start(ch.anvil.endpoint()).await;
        let (_d, db) = setup();
        let ix = ch.index(&db, 31337, proxy.provider());
        let c = at.into_array();
        ix.register(c).await.unwrap();
        let head = ch.head().await;
        // BUG: every backoff cycle scans back to genesis again, forever
        for i in 0..3 {
            assert!(ix.sync(c, head).await.is_err());
            if i < 2 {
                ix.expire_backoff(&c);
            }
        }
        let calls = proxy.knobs.get_logs.load(SeqCst);
        // Suspect after the third full scan, with both counts named; not
        // terminal, and retried at the longest backoff.
        let e = ix.sync(c, head).await.unwrap_err();
        assert!(matches!(e, CensusError::Backoff(_)), "{e}");
        let why = match ix.usable(&c) {
            Err(UsableError::Suspect(w)) => w,
            other => panic!("{other:?}"),
        };
        assert!(
            why.contains("treeSize says 1") && why.contains("logs show 0"),
            "{why}"
        );
        assert!(ix.retry_in(&c).unwrap() > Duration::from_secs(3000));
        assert_eq!(
            proxy.knobs.get_logs.load(SeqCst),
            calls,
            "scanned in backoff"
        );
        // The contract turns out honest: a good sync clears the mark.
        let mut p = Puppet {
            at,
            tree: LeanImt::new(),
        };
        p.add(&ch, &[0x55; 20], 1).await;
        ix.expire_backoff(&c);
        ix.sync(c, ch.head().await).await.unwrap();
        assert!(ix.usable(&c).is_ok());
        assert_eq!(ix.latest(&c).unwrap().1, 1);
    }

    // `--census-max-participants` bounds origin 3 too: a contract with more
    // members than the cap is refused before any log is fetched or any
    // member kept in memory.
    #[tokio::test]
    async fn onchain_member_cap_is_enforced() {
        if !enabled() {
            return;
        }
        let ch = Chain::start().await;
        let at = a(0xc8);
        ch.set_code(at, puppet()).await;
        ch.set_slot(at, size_slot(), U256::from(1_000_000)).await;
        ch.set_slot(at, root_slot(), U256::from(1)).await;
        ch.mine(5).await;
        let proxy = RpcProxy::start(ch.anvil.endpoint()).await;
        let (_d, db) = setup();
        // BUG: no member cap on origin 3 (with_max_members does not exist yet)
        let ix = ch
            .index(&db, 31337, proxy.provider())
            .with_max_members(1000);
        let c = at.into_array();
        ix.register(c).await.unwrap();
        let e = ix.sync(c, ch.head().await).await.unwrap_err();
        assert!(e.to_string().contains("at most 1000"), "{e}");
        assert_eq!(proxy.knobs.get_logs.load(SeqCst), 0, "logs were scanned");
        assert!(ix.latest(&c).is_none());
    }

    // One contract with an endless scan must not starve the background sync
    // of every other contract (`sync_all` is one sequential pass).
    #[tokio::test]
    async fn slow_contract_does_not_starve_the_others() {
        if !enabled() {
            return;
        }
        let ch = Chain::start().await;
        let hostile = a(0xc9);
        ch.set_code(hostile, puppet()).await;
        ch.set_slot(hostile, size_slot(), U256::from(5)).await;
        ch.set_slot(hostile, root_slot(), U256::from(1)).await;
        ch.mine(30_000).await;
        // The provider only serves one-block getLogs ranges: the hostile
        // contract's scan back to genesis is 40k requests.
        let proxy = RpcProxy::start(ch.anvil.endpoint()).await;
        proxy.knobs.max_range.store(1, SeqCst);
        let mut honest = Vec::new();
        for i in 0..12u8 {
            let mut p = Puppet::deploy(&ch, a(0x20 + i)).await;
            p.add(&ch, &[0x90 + i; 20], 1).await;
            honest.push(p.at.into_array());
        }
        let (_d, db) = setup();
        let ix = std::sync::Arc::new(ch.index(&db, 31337, proxy.provider()));
        ix.register(hostile.into_array()).await.unwrap();
        for c in &honest {
            ix.register(*c).await.unwrap();
        }
        let head = ch.head().await;
        {
            let t = Instant::now();
            let r = ix.sync(honest[0], head).await;
            eprintln!("honest alone: {r:?} in {:?}", t.elapsed());
        }
        let task = {
            let ix = ix.clone();
            tokio::spawn(async move { ix.sync_all(head).await })
        };
        let t = Instant::now();
        while t.elapsed() < Duration::from_secs(12)
            && honest.iter().any(|c| ix.latest(c).is_none())
            && !task.is_finished()
        {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let done = honest.iter().filter(|c| ix.latest(c).is_some()).count();
        let still_running = !task.is_finished();
        task.abort();
        eprintln!("{done}/12 honest contracts synced, hostile scan running: {still_running}");
        // BUG: sync_all walks contracts one after another
        assert!(
            !still_running || done == 12,
            "{done}/12 honest contracts synced"
        );
    }

    // A backend behind the target block answers `eth_getLogs` short without
    // an error. Short answers from a lagging endpoint are not evidence of a
    // lying contract: it is never marked unusable.
    #[tokio::test]
    async fn lagging_backend_never_condemns_a_contract() {
        if !enabled() {
            return;
        }
        let ch = Chain::start().await;
        let mut p = Puppet::deploy(&ch, a(0xca)).await;
        for i in 1..=5u8 {
            p.add(&ch, &[0xa0 + i; 20], u128::from(i)).await;
        }
        ch.mine(5).await;
        let proxy = RpcProxy::start(ch.anvil.endpoint()).await;
        // BUG: three short scans from a lagging backend mark the contract unusable
        proxy.knobs.lag.store(8, SeqCst);
        let (_d, db) = setup();
        let ix = ch.index(&db, 31337, proxy.provider());
        let c = p.at.into_array();
        ix.register(c).await.unwrap();
        let head = ch.head().await;
        for _ in 0..6 {
            assert!(ix.sync(c, head).await.is_err());
            assert!(ix.usable(&c).is_ok(), "condemned by a lagging backend");
            ix.expire_backoff(&c);
        }
        proxy.knobs.lag.store(0, SeqCst);
        ix.sync(c, head).await.unwrap();
        assert_eq!(ix.latest(&c).unwrap().1, 5);
    }

    // One hostile scan in flight must not freeze later rounds: an honest
    // contract that gets a member after the first round still advances.
    #[tokio::test]
    async fn later_rounds_run_while_a_hostile_scan_is_in_flight() {
        if !enabled() {
            return;
        }
        let ch = Chain::start().await;
        let hostile = a(0xcc);
        ch.set_code(hostile, puppet()).await;
        ch.set_slot(hostile, size_slot(), U256::from(5)).await;
        ch.set_slot(hostile, root_slot(), U256::from(1)).await;
        ch.mine(30_000).await;
        let proxy = RpcProxy::start(ch.anvil.endpoint()).await;
        proxy.knobs.max_range.store(1, SeqCst);
        let mut p = Puppet::deploy(&ch, a(0xcd)).await;
        p.add(&ch, &[0xc1; 20], 1).await;
        let (_d, db) = setup();
        let ix = std::sync::Arc::new(ch.index(&db, 31337, proxy.provider()));
        let c = p.at.into_array();
        ix.register(c).await.unwrap();
        // Warm the shared getLogs span down to 1 so honest scans are quick.
        ix.sync(c, ch.head().await).await.unwrap();
        ix.register(hostile.into_array()).await.unwrap();
        let first = {
            let (ix, head) = (ix.clone(), ch.head().await);
            tokio::spawn(async move { ix.sync_all(head).await })
        };
        tokio::time::sleep(Duration::from_millis(500)).await;
        p.add(&ch, &[0xc2; 20], 2).await;
        let head = ch.head().await;
        let second = {
            let ix = ix.clone();
            tokio::spawn(async move { ix.sync_all(head).await })
        };
        let t = Instant::now();
        while t.elapsed() < Duration::from_secs(10)
            && ix.latest(&c).is_none_or(|(_, size, _)| size < 2)
        {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let hostile_running = !first.is_finished();
        first.abort();
        second.abort();
        // BUG: a global round gate drops every later sync_all while one runs
        assert!(hostile_running, "the hostile scan ended before the check");
        assert_eq!(ix.latest(&c).unwrap().1, 2, "the new member never showed");
    }

    // Hostile first scans that never end fill the full-scan slots; the
    // incremental sync of an already indexed contract must not queue behind.
    #[tokio::test]
    async fn updates_of_indexed_contracts_do_not_wait_behind_full_scans() {
        if !enabled() {
            return;
        }
        let ch = Chain::start().await;
        let mut hostile = Vec::new();
        for i in 0..4u8 {
            let h = a(0xd0 + i);
            ch.set_code(h, puppet()).await;
            ch.set_slot(h, size_slot(), U256::from(5)).await;
            ch.set_slot(h, root_slot(), U256::from(1)).await;
            hostile.push(h.into_array());
        }
        ch.mine(30_000).await;
        let proxy = RpcProxy::start(ch.anvil.endpoint()).await;
        proxy.knobs.max_range.store(1, SeqCst);
        let mut p = Puppet::deploy(&ch, a(0xd8)).await;
        p.add(&ch, &[0xd1; 20], 1).await;
        let (_d, db) = setup();
        let ix = std::sync::Arc::new(ch.index(&db, 31337, proxy.provider()));
        let c = p.at.into_array();
        ix.register(c).await.unwrap();
        ix.sync(c, ch.head().await).await.unwrap();
        for h in &hostile {
            ix.register(*h).await.unwrap();
        }
        let first = {
            let (ix, head) = (ix.clone(), ch.head().await);
            tokio::spawn(async move { ix.sync_all(head).await })
        };
        tokio::time::sleep(Duration::from_millis(500)).await;
        p.add(&ch, &[0xd2; 20], 2).await;
        let head = ch.head().await;
        let second = {
            let ix = ix.clone();
            tokio::spawn(async move { ix.sync_all(head).await })
        };
        let t = Instant::now();
        while t.elapsed() < Duration::from_secs(10)
            && ix.latest(&c).is_none_or(|(_, size, _)| size < 2)
        {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let running = !first.is_finished();
        first.abort();
        second.abort();
        assert!(running, "the hostile scans ended before the check");
        // BUG: four hostile first scans hold every slot
        assert_eq!(
            ix.latest(&c).unwrap().1,
            2,
            "the update waited behind full scans"
        );
    }
}
