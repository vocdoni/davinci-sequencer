//! Harness for the end-to-end acceptance test (`tests/e2e.rs`): anvil with
//! the zkVM contracts and the origin-3 census contract, sequencer nodes as
//! subprocesses, the seeded voter fixture and the polling helpers. `demo`
//! holds the demo elections of `tests/demo.rs`.
#![forbid(unsafe_code)]

pub mod census;
pub mod chain;
pub mod cost;
pub mod demo;
pub mod dkg;
pub mod fixture;
pub mod negative;
pub mod net;
pub mod node;
pub mod proxy;
pub mod wait;
