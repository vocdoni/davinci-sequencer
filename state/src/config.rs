//! Process configuration and the genesis state tree (must equal the
//! Solidity `genesisRoot` and go-sdk `chain.NewState`).

use arbo::{MemoryStorage, Sha256, Tree};
use davinci_zkvm_sdk::ballot::{
    Ballot, BallotMode, ballot_leaf_hash, enc_key_hash, leaf_value_bytes,
};
use davinci_zkvm_sdk::crypto::babyjubjub::Point;
use davinci_zkvm_sdk::crypto::field::{Fr, fr_to_le};
use davinci_zkvm_sdk::limits::{
    KEY_BALLOT_MODE, KEY_BALLOT_VK, KEY_CENSUS_ORIGIN, KEY_ENC_KEY, KEY_PROCESS_ID, KEY_RESULTS,
    SMT_LEVELS,
};

use crate::error::Error;

/// Census origin, as stored in the 0x06 leaf. Origins 1 to 3 are lean-IMT
/// censuses and validate the same way; they differ only in how the batch
/// census root is chosen.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CensusOrigin {
    MerkleStatic = 1,
    /// Organizer-updated root (`setProcessCensus`).
    MerkleOffchainDynamic = 2,
    /// Root of an on-chain census contract.
    MerkleOnchainDynamic = 3,
    Csp = 4,
}

impl CensusOrigin {
    /// True for the lean-IMT origins (1, 2 and 3).
    pub fn is_merkle(self) -> bool {
        self != CensusOrigin::Csp
    }
}

/// Everything that pins a process: the six genesis leaves.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ProcessConfig {
    /// Big-endian integer of the 31-byte on-chain process id.
    pub process_id: Fr,
    pub ballot_mode: BallotMode,
    pub enc_key: Point,
    pub census_origin: CensusOrigin,
    /// Merkle census root, or the CSP address as uint160.
    pub census_root: Fr,
    /// `sha256` of the ballot VK wire bytes (the 0x07 leaf digest).
    pub ballot_vk_hash: [u8; 32],
}

/// Arbo SMT key bytes: the 8-byte little-endian key.
pub(crate) fn key_bytes(k: u64) -> [u8; 8] {
    k.to_le_bytes()
}

/// The six genesis leaves `(key, value_le32)`.
pub(crate) fn genesis_leaves(cfg: &ProcessConfig) -> Result<Vec<(u64, [u8; 32])>, Error> {
    let mut origin = [0u8; 32];
    origin[0] = cfg.census_origin as u8;
    Ok(vec![
        (KEY_PROCESS_ID, fr_to_le(&cfg.process_id)),
        (KEY_BALLOT_MODE, fr_to_le(&cfg.ballot_mode.pack()?)),
        (KEY_ENC_KEY, leaf_value_bytes(&enc_key_hash(&cfg.enc_key))),
        (
            KEY_RESULTS,
            leaf_value_bytes(&ballot_leaf_hash(&Ballot::identity())),
        ),
        (KEY_CENSUS_ORIGIN, origin),
        (KEY_BALLOT_VK, leaf_value_bytes(&cfg.ballot_vk_hash)),
    ])
}

/// The genesis state root of `cfg` (raw arbo bytes).
pub fn genesis_root(cfg: &ProcessConfig) -> Result<[u8; 32], Error> {
    let mut tree = Tree::new(MemoryStorage::new(), SMT_LEVELS, Sha256)?;
    for (k, v) in genesis_leaves(cfg)? {
        tree.add(&key_bytes(k), &v)?;
    }
    Ok(tree.root())
}
