//! L1-blob mode (OP Stack registries): a batch's blobs are posted on
//! Ethereum while it is proved, and the settlement on OP carries a proof that
//! the blob transaction is in Ethereum's canonical chain. The actor calls
//! into this module only when `Deps.l1` is set; blob mode never does.

mod post;

use std::sync::Arc;
use std::time::Duration;

use crate::storage::{Db, StorageError};
use crate::web3::L1Chain;

pub use post::{PostError, Posted, Poster, REPLACE_AFTER};

/// The Ethereum side of a node in L1-blob mode.
pub struct L1Da {
    pub l1: Arc<dyn L1Chain>,
    pub poster: Poster,
    /// Pace of the settle loop and the poster (the chain poll interval).
    pub poll: Duration,
}

impl L1Da {
    /// `fee_cap`: the most one post may cost at the fees it offers, in wei.
    pub fn new(
        l1: Arc<dyn L1Chain>,
        db: Db,
        fee_cap: u128,
        poll: Duration,
    ) -> Result<Self, StorageError> {
        Ok(L1Da {
            poster: Poster::new(l1.clone(), db, fee_cap, poll)?,
            l1,
            poll,
        })
    }
}
