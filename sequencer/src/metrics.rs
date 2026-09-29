//! Node counters. Cheap atomics, read by the API and the tests.

use std::collections::HashMap;
use std::sync::Mutex;
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
    /// Proving-time model the batch budget sizes against.
    pub prove: ProveModel,
}

/// Proving seconds per vote by ballot field count: an EWMA over every
/// finished batch proof of the node, seeded high. In memory only; a restart
/// re-seeds.
#[derive(Debug, Default)]
pub struct ProveModel(Mutex<HashMap<u8, f64>>);

impl ProveModel {
    /// Floor of one observation, seconds per vote.
    pub const FLOOR: f64 = 0.05;

    /// Seed before the first proof of `nf` fields: about twice an RTX 5090.
    pub fn seed(nf: u8) -> f64 {
        (0.05 * f64::from(nf)).max(0.2)
    }

    pub fn spv(&self, nf: u8) -> f64 {
        let m = self.0.lock().unwrap_or_else(|e| e.into_inner());
        m.get(&nf).copied().unwrap_or_else(|| Self::seed(nf))
    }

    /// `base + n × spv[nf]`, seconds.
    pub fn estimate(&self, nf: u8, n: usize, base: f64) -> f64 {
        base + n as f64 * self.spv(nf)
    }

    /// A proof of `n` votes took `secs`:
    /// `spv ← 0.7·spv + 0.3·max(FLOOR, (secs − base)/n)`.
    pub fn observe(&self, nf: u8, n: usize, secs: f64, base: f64) {
        if n == 0 {
            return;
        }
        let sample = ((secs - base) / n as f64).max(Self::FLOOR);
        let mut m = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let spv = m.entry(nf).or_insert_with(|| Self::seed(nf));
        *spv = 0.7 * *spv + 0.3 * sample;
    }
}
