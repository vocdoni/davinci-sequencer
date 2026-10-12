//! Known DAVINCI deployments. The node's `--network` presets and the
//! organizer and voter defaults come from this table.

use alloy::primitives::{Address, address};

/// A deployment known by name: the chain, its `ProcessRegistry` and what a
/// node needs to follow it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Network {
    pub name: &'static str,
    pub chain_id: u64,
    pub registry: Address,
    /// The registry's deployment block: where a fresh node starts scanning.
    pub start_block: u64,
    /// Execution-layer JSON-RPCs, in order of preference.
    pub rpc_urls: &'static [&'static str],
    /// Blob source in the node's `--blob-source` form.
    pub blob_source: &'static str,
    /// Blocks behind head a node treats as final.
    pub confirmations: u64,
    /// Where the deployment publishes transition data.
    pub da: NetworkDa,
}

/// How a deployment publishes transition data.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NetworkDa {
    /// Blob transactions on the registry's own chain.
    Blobs,
    /// Ethereum blob transactions, proven on an OP Stack chain through the
    /// registry's `L1BlobVerifier`. `blob_source` then names Ethereum beacon APIs.
    L1Blobs {
        /// Ethereum's chain id.
        l1_chain_id: u64,
        /// The `L1Block` predeploy the verifier reads OP's origin from.
        l1_block: Address,
        /// Ethereum execution JSON-RPCs, in order of preference.
        l1_rpc_urls: &'static [&'static str],
    },
}

/// OP Stack's `L1Block` predeploy.
pub const OP_L1_BLOCK: Address = address!("4200000000000000000000000000000000000015");

impl Network {
    /// The beacon APIs of a `beacon:` blob source.
    pub fn beacon_urls(&self) -> Vec<&'static str> {
        self.blob_source
            .strip_prefix("beacon:")
            .map(|s| s.split(',').collect())
            .unwrap_or_default()
    }
}

/// Gnosis Chain, registry R2 (the production beta): DKG and Council key
/// modes.
pub const GNOSIS: Network = Network {
    name: "gnosis",
    chain_id: 100,
    registry: address!("20b96e465CA7C3536B9C733571ec1eCf42b2eA21"),
    start_block: 48_633_301,
    rpc_urls: &[
        "https://gnosis-rpc.publicnode.com",
        "https://gnosis-rpc.blockreq.com/v1/rpc/public",
        "https://rpc.gnosischain.com",
        "https://gnosis.drpc.org",
    ],
    blob_source: "beacon:https://rpc-gbc.gnosischain.com",
    confirmations: 3,
    da: NetworkDa::Blobs,
};

/// OP Mainnet in L1-blob mode: transitions settle on OP, their blobs live on
/// Ethereum (deployed 2026-10-12, see docs/deployments.md). Blob sends need an
/// Ethereum RPC that takes 1.5 MB bodies: publicnode refuses them, dRPC takes
/// them, and the node sends to every L1 endpoint at once.
pub const OPTIMISM: Network = Network {
    name: "optimism",
    chain_id: 10,
    registry: address!("8b63fAE96948D69CF92Eb40e4EE0fCE40962267d"),
    start_block: 158_125_136,
    rpc_urls: &[
        "https://optimism-rpc.publicnode.com",
        "https://mainnet.optimism.io",
    ],
    // Ethereum beacon APIs: they must keep full blobs (supernodes, archives).
    blob_source: "beacon:https://ethereum-beacon-api.publicnode.com",
    confirmations: 1,
    da: NetworkDa::L1Blobs {
        l1_chain_id: 1,
        l1_block: OP_L1_BLOCK,
        l1_rpc_urls: &[
            "https://ethereum-rpc.publicnode.com",
            "https://eth.drpc.org",
        ],
    },
};

/// Every known deployment.
pub const NETWORKS: &[Network] = &[GNOSIS, OPTIMISM];

/// The deployment a node and the tooling use unless told otherwise.
pub const DEFAULT: &Network = &GNOSIS;

/// The known deployment called `name` (case-insensitive).
pub fn by_name(name: &str) -> Option<&'static Network> {
    let name = name.trim();
    NETWORKS.iter().find(|n| n.name.eq_ignore_ascii_case(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lookup() {
        assert_eq!(by_name(" Gnosis "), Some(&GNOSIS));
        assert_eq!(by_name("custom"), None);
        assert_eq!(by_name(DEFAULT.name), Some(DEFAULT));
        assert_eq!(
            GNOSIS.beacon_urls(),
            ["https://rpc-gbc.gnosischain.com"].to_vec()
        );
    }

    #[test]
    fn gnosis_preset_is_deployed() {
        assert_ne!(GNOSIS.registry, Address::ZERO, "gnosis registry unset");
        assert_ne!(GNOSIS.start_block, 0, "gnosis start block unset");
    }

    #[test]
    fn names_are_unique_and_lowercase() {
        for (i, n) in NETWORKS.iter().enumerate() {
            assert_eq!(n.name, n.name.to_ascii_lowercase());
            assert!(!n.rpc_urls.is_empty());
            assert!(NETWORKS[i + 1..].iter().all(|m| m.name != n.name));
        }
    }

    #[test]
    fn optimism_preset_is_deployed() {
        assert_ne!(OPTIMISM.registry, Address::ZERO, "optimism registry unset");
        assert_ne!(OPTIMISM.start_block, 0, "optimism start block unset");
    }

    #[test]
    fn optimism_posts_its_blobs_on_ethereum() {
        assert_eq!(by_name(" Optimism "), Some(&OPTIMISM));
        assert_eq!(OPTIMISM.chain_id, 10);
        assert_eq!(OPTIMISM.confirmations, 1);
        let NetworkDa::L1Blobs {
            l1_chain_id,
            l1_block,
            l1_rpc_urls,
        } = OPTIMISM.da
        else {
            panic!("optimism must be an L1-blob preset");
        };
        assert_eq!((l1_chain_id, l1_block), (1, OP_L1_BLOCK));
        assert!(!l1_rpc_urls.is_empty());
        // Followers read the blobs from Ethereum beacon APIs.
        assert!(!OPTIMISM.beacon_urls().is_empty());
        assert_eq!(GNOSIS.da, NetworkDa::Blobs);
    }
}
