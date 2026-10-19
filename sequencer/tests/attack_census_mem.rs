//! `file://` censuses obey the byte budget before they are read (own binary:
//! the check is the peak of live heap bytes, counted by a global allocator:
//! RSS would include allocator caching and other threads).

use std::time::Duration;

use davinci_sequencer::census::{CensusOptions, CensusStore};
use davinci_sequencer::storage::Db;

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

struct Counting;

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(l) };
        if !p.is_null() {
            let now = LIVE.fetch_add(l.size(), Relaxed) + l.size();
            PEAK.fetch_max(now, Relaxed);
        }
        p
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        LIVE.fetch_sub(l.size(), Relaxed);
        unsafe { System.dealloc(p, l) }
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, new: usize) -> *mut u8 {
        let q = unsafe { System.realloc(p, l, new) };
        if !q.is_null() {
            LIVE.fetch_sub(l.size(), Relaxed);
            let now = LIVE.fetch_add(new, Relaxed) + new;
            PEAK.fetch_max(now, Relaxed);
        }
        q
    }
}

#[global_allocator]
static A: Counting = Counting;

#[tokio::test]
async fn file_reads_wait_for_the_byte_budget_before_allocating() {
    let base = tempfile::tempdir().unwrap();
    let inside = base.path().join("c");
    std::fs::create_dir(&inside).unwrap();
    // Valid JSON padded with blanks: 16 MiB each, 8 files.
    let mut body = String::from(r#"{"participants":[]}"#);
    body.push_str(&" ".repeat((16 << 20) - body.len()));
    for i in 0..8 {
        std::fs::write(inside.join(format!("c{i}.json")), &body).unwrap();
    }
    drop(body);
    let dbdir = tempfile::tempdir().unwrap();
    let opts = CensusOptions {
        dir: Some(inside.clone()),
        ..CensusOptions::default()
    };
    // Budget for one document and a bit.
    let cs = std::sync::Arc::new(
        CensusStore::with_options(Db::open_in(dbdir.path()).unwrap(), opts)
            .unwrap()
            .with_body_budget_kib(20 << 10),
    );
    // Peak live heap from here on, above what is live now.
    let before = LIVE.load(Relaxed);
    PEAK.store(before, Relaxed);
    let tasks: Vec<_> = (0..8)
        .map(|i| {
            let (cs, uri) = (cs.clone(), format!("file://{}/c{i}.json", inside.display()));
            tokio::spawn(async move { cs.fetch(&uri, &[1u8; 32]).await })
        })
        .collect();
    for t in tasks {
        let _ = tokio::time::timeout(Duration::from_secs(60), t)
            .await
            .unwrap();
    }
    let grew = (PEAK.load(Relaxed) - before) >> 20;
    eprintln!("peak above baseline: {grew} MiB");
    // BUG: every file is read whole before it asks for budget
    assert!(
        grew < 70,
        "live heap peaked {grew} MiB above baseline with a 20 MiB budget"
    );
}
