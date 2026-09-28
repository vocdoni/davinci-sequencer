//! Node counters. Cheap atomics, read by the API and the tests.

use std::sync::atomic::AtomicU64;

#[derive(Debug, Default)]
pub struct Metrics {
    /// Transitions this node proved and settled on-chain.
    pub settled_by_self: AtomicU64,
    /// Transitions applied from other sequencers' blobs.
    pub synced_from_others: AtomicU64,
    /// Batches lost to another sequencer settling first.
    pub lost_races: AtomicU64,
    /// Results requests built for elections holding this node's key.
    pub finalize_attempts: AtomicU64,
}
