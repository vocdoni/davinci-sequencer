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
    /// Stalls of the landing-time read (`STALE_WARN_TICKS` consecutive
    /// ticks each): close-out and results wait until it clears.
    pub stalled_reads: AtomicU64,
    /// Results requests built for elections holding this node's key.
    pub finalize_attempts: AtomicU64,
    /// Batches the prover failed for good, requeued for bisection.
    pub bisections: AtomicU64,
    /// Votes errored alone by a bisection.
    pub isolated_votes: AtomicU64,
    /// Batches held pending after a node-side check failed.
    pub held_batches: AtomicU64,
    /// Batches the guest rejected (fail mask); their votes are errored.
    pub guest_rejections: AtomicU64,
    /// Reorgs seen at the monitor's event cursor (events re-routed).
    pub reorgs: AtomicU64,
    /// Event pages routed without a block hash (failing hash reads): the
    /// anchor stays behind and actors make no anchored decision past it.
    pub anchor_stalls: AtomicU64,
    /// Times a process's reads by block hash failed for 90 s with degraded
    /// reads off: it decides nothing from the chain until they answer.
    pub hash_read_stalls: AtomicU64,
    /// Processes rebuilt from the chain after a reorg took transitions
    /// they had committed.
    pub rebuilds: AtomicU64,
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
    /// Ceiling of one observation, in seeds: a stalled job must not size
    /// the rest of the window's batches to nothing.
    pub const CEIL_SEEDS: f64 = 60.0;

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
    /// `spv ← 0.7·spv + 0.3·clamp((secs − base)/n, FLOOR, CEIL_SEEDS·seed)`.
    pub fn observe(&self, nf: u8, n: usize, secs: f64, base: f64) {
        if n == 0 {
            return;
        }
        let sample =
            ((secs - base) / n as f64).clamp(Self::FLOOR, Self::CEIL_SEEDS * Self::seed(nf));
        let mut m = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let spv = m.entry(nf).or_insert_with(|| Self::seed(nf));
        *spv = 0.7 * *spv + 0.3 * sample;
    }
}
