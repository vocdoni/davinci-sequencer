//! Finalization waits that must not be silent: a DKG committee that never
//! reaches its threshold leaves the results unpublished, and the node's log
//! is where an operator sees why.

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::*;
use davinci_sequencer::storage::Db;
use davinci_sequencer::web3::{KeyMode, ProcessStatus};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;
use tracing::Level;
use tracing_subscriber::layer::SubscriberExt;

type Lines = Arc<Mutex<Vec<(Level, String)>>>;

/// Every event the node logs on this thread, as `message k=v ...`.
struct Capture(Lines);

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Capture {
    fn on_event(&self, e: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
        struct V(String);
        impl tracing::field::Visit for V {
            fn record_debug(&mut self, f: &tracing::field::Field, v: &dyn std::fmt::Debug) {
                if f.name() == "message" {
                    self.0.insert_str(0, &format!("{v:?} "));
                } else {
                    self.0.push_str(&format!("{}={v:?} ", f.name()));
                }
            }
        }
        let mut v = V(String::new());
        e.record(&mut v);
        self.0
            .lock()
            .unwrap()
            .push((*e.metadata().level(), v.0.replace('"', "")));
    }
}

fn lines(l: &Lines, level: Level, needle: &str) -> Vec<String> {
    l.lock()
        .unwrap()
        .iter()
        .filter(|(lv, m)| *lv == level && m.contains(needle))
        .map(|(_, m)| m.clone())
        .collect()
}

// Two votes, the end, the grace: the decryption is requested and then the
// committee combines one of the two ciphertexts and stops (a threshold it
// never reaches). The first wait is said once at info; while it lasts, a
// warning every ten minutes says how many of the ciphertexts are combined.
#[tokio::test(start_paused = true)]
async fn a_stuck_dkg_committee_is_visible_in_the_log() {
    let logs: Lines = Arc::default();
    let _sub = tracing::subscriber::set_default(
        tracing_subscriber::registry().with(Capture(logs.clone())),
    );
    let s = setup(2, 8, None);
    s.chain.set_key_mode(KeyMode::DkgAutomatic);
    let dir = TempDir::new().unwrap();
    let shutdown = CancellationToken::new();
    let node = start_node(
        Db::open_in(dir.path()).unwrap(),
        dir.path(),
        &s,
        s.chain.clone(),
        FakeProver::open(),
        2,
        "0s",
        shutdown.clone(),
    )
    .await;
    let h = handle(&node).await;
    let votes = [
        fake_vote(&s.env, 0, &[1, 2], 40),
        fake_vote(&s.env, 1, &[2, 3], 41),
    ];
    let vids: Vec<u64> = votes.iter().map(|v| v.pkg.vote_id).collect();
    for v in &votes {
        h.submit(v.clone()).await.unwrap();
    }
    wait_until("settled", async || all_settled(&h, &vids).await).await;
    s.chain.set_status(ProcessStatus::Ended);
    s.chain.pass_grace();
    wait_until("decryption requested", async || {
        s.chain.dkg_calls().0.0 == 1
    })
    .await;
    s.chain.dkg_combine_some(1);
    tokio::time::sleep(Duration::from_secs(25 * 60)).await;
    assert!(s.chain.results().is_empty());

    // BUG: the wait is logged at debug only, with no progress
    let first = lines(&logs, Level::INFO, "results wait for the DKG committee");
    assert_eq!(first.len(), 1, "one info line on the first wait: {first:?}");
    let warns = lines(&logs, Level::WARN, "DKG results still waiting");
    assert!(
        (2..=3).contains(&warns.len()),
        "a warning every ten minutes: {warns:?}"
    );
    for w in &warns {
        assert!(w.contains("combined=1") && w.contains("count=2"), "{w}");
    }

    // Once combined, the results go out and the warnings stop.
    s.chain.dkg_combine(vec![3, 5]);
    tokio::time::sleep(Duration::from_secs(60)).await;
    assert_eq!(s.chain.results(), vec![3, 5]);
    let n = lines(&logs, Level::WARN, "DKG results still waiting").len();
    tokio::time::sleep(Duration::from_secs(30 * 60)).await;
    assert_eq!(
        lines(&logs, Level::WARN, "DKG results still waiting").len(),
        n
    );
    shutdown.cancel();
}
