//! Shared test elections and voters.
#![allow(dead_code)]

use davinci_client::api::ProcessStatus;
use davinci_client::organizer::OnchainProcess;
use davinci_client::voter::Voter;
use davinci_zkvm_sdk::ballot::BallotMode;
use davinci_zkvm_sdk::census::{LeanImt, census_leaf, eth_address};
use davinci_zkvm_sdk::crypto::elgamal::keygen;
use davinci_zkvm_sdk::crypto::field::{Fr, U256, fr_from_be};
use k256::ecdsa::SigningKey;
use rand::rngs::StdRng;

pub fn mode(nf: u8) -> BallotMode {
    BallotMode {
        num_fields: nf,
        group_size: 1,
        unique_values: false,
        cost_exponent: 1,
        max_value: 7,
        min_value: 0,
        max_value_sum: 0, // bounded by the weight
        min_value_sum: 0,
    }
}

/// A process as the registry holds it, plus its secret key.
pub struct Election {
    pub chain: OnchainProcess,
    pub sk: U256,
}

pub fn election(nf: u8, origin: u8, census_root: Fr, rng: &mut StdRng) -> Election {
    let (sk, pk) = keygen(rng);
    let mut pid = [0u8; 31];
    pid[..20].copy_from_slice(&[0x42; 20]);
    pid[30] = nf;
    Election {
        chain: OnchainProcess {
            id: pid,
            status: ProcessStatus::Ready,
            organization_id: [0x42; 20],
            encryption_key: pk,
            state_root: [1; 32],
            result: vec![],
            start_time: 0,
            duration: 3600,
            max_voters: 1000,
            voters_count: 0,
            overwritten_votes_count: 0,
            ballot_mode: mode(nf),
            census_origin: origin,
            census_root,
            census_contract: [0; 20],
            census_uri: String::new(),
            metadata_uri: String::new(),
            metadata_hash: [0; 32],
            dkg: None,
            grace: 0,
            last_vote_at: 0,
        },
        sk,
    }
}

pub fn voters(n: usize, rng: &mut StdRng) -> Vec<Voter> {
    (0..n).map(|_| Voter::random(rng)).collect()
}

/// CSP census root: the CSP address as an integer.
pub fn csp_root(csp: &SigningKey) -> Fr {
    let mut be = [0u8; 32];
    be[12..].copy_from_slice(&eth_address(csp.verifying_key()));
    fr_from_be(&be).unwrap()
}

pub fn merkle_census(vs: &[Voter], weight: u128) -> LeanImt {
    let mut t = LeanImt::new();
    for v in vs {
        t.insert(census_leaf(&v.address(), weight).unwrap());
    }
    t
}
