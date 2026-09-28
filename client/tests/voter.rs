//! Voter checks that run before any proving (no artifacts needed).

mod common;

use common::{csp_root, election, merkle_census, voters};
use davinci_client::Error;
use davinci_client::api::ProcessId;
use davinci_client::voter::{PreparedVote, random_k};
use davinci_zkvm_sdk::census::{CensusWitness, csp_sign};
use davinci_zkvm_sdk::crypto::babyjubjub::Point;
use davinci_zkvm_sdk::crypto::field::{Fr, fr_to_dec};
use k256::ecdsa::SigningKey;
use rand::SeedableRng;
use rand::rngs::StdRng;

#[track_caller]
fn invalid<T: std::fmt::Debug>(r: Result<T, Error>) {
    match r {
        Err(Error::Invalid(_)) => {}
        other => panic!("want Error::Invalid, got {other:?}"),
    }
}

// The client refuses to build votes the sequencer would reject.
#[test]
fn inconsistent_inputs_are_rejected() {
    let mut rng = StdRng::seed_from_u64(5);
    let vs = voters(3, &mut rng);
    let census = merkle_census(&vs, 10);
    let e = election(2, 1, census.root(), &mut rng);
    let k = || Fr::from(77u64);
    let own = || CensusWitness::Merkle(census.proof(0).unwrap());
    vs[0].prepare_vote(&e.chain, &[1], own(), 10, k()).unwrap();

    // Someone else's census proof.
    let other = CensusWitness::Merkle(census.proof(1).unwrap());
    invalid(vs[0].prepare_vote(&e.chain, &[1], other, 10, k()));
    // Wrong weight for the leaf.
    invalid(vs[0].prepare_vote(&e.chain, &[1], own(), 11, k()));
    // Census from another root.
    let mut e2 = election(2, 1, Fr::from(1u64), &mut rng);
    e2.chain.encryption_key = e.chain.encryption_key;
    invalid(vs[0].prepare_vote(&e2.chain, &[1], own(), 10, k()));
    // More values than fields; no fields at all.
    invalid(vs[0].prepare_vote(&e.chain, &[1, 1, 1], own(), 10, k()));
    let mut e0 = election(2, 1, census.root(), &mut rng);
    e0.chain.ballot_mode.num_fields = 0;
    e0.chain.ballot_mode.group_size = 0;
    invalid(vs[0].prepare_vote(&e0.chain, &[], own(), 10, k()));
    // Identity and off-subgroup encryption keys.
    let mut e3 = election(2, 1, census.root(), &mut rng);
    e3.chain.encryption_key = Point::IDENTITY;
    invalid(vs[0].prepare_vote(&e3.chain, &[1], own(), 10, k()));
    // (0, -1) is on the curve with order 2.
    e3.chain.encryption_key = Point {
        x: Fr::from(0u64),
        y: -Fr::from(1u64),
    };
    assert!(e3.chain.encryption_key.is_on_curve());
    invalid(vs[0].prepare_vote(&e3.chain, &[1], own(), 10, k()));

    // CSP.
    let csp = SigningKey::random(&mut rng);
    let mut e4 = election(2, 4, csp_root(&csp), &mut rng);
    e4.chain.encryption_key = e.chain.encryption_key;
    let pid = ProcessId(e4.chain.id).to_fr();
    let good = csp_sign(&csp, &pid, &vs[0].address(), 10, 0);
    vs[0]
        .prepare_vote(&e4.chain, &[1], CensusWitness::Csp(good.clone()), 10, k())
        .unwrap();
    // Attestation for another voter.
    let att = csp_sign(&csp, &pid, &vs[1].address(), 10, 0);
    invalid(vs[0].prepare_vote(&e4.chain, &[1], CensusWitness::Csp(att), 10, k()));
    // Signed by a key other than the census root.
    let rogue = SigningKey::random(&mut rng);
    let att = csp_sign(&rogue, &pid, &vs[0].address(), 10, 0);
    invalid(vs[0].prepare_vote(&e4.chain, &[1], CensusWitness::Csp(att), 10, k()));
    // Weight other than the attested one.
    invalid(vs[0].prepare_vote(&e4.chain, &[1], CensusWitness::Csp(good), 9, k()));
    // Witness type that does not match the census origin.
    invalid(vs[0].prepare_vote(&e4.chain, &[1], own(), 10, k()));
    invalid(vs[0].prepare_vote(
        &e.chain,
        &[1],
        CensusWitness::Csp(csp_sign(&csp, &pid, &vs[0].address(), 10, 0)),
        10,
        k(),
    ));
}

// k opens the ballot; Debug output must never carry it.
#[test]
fn debug_output_redacts_the_secret() {
    let mut rng = StdRng::seed_from_u64(6);
    let vs = voters(2, &mut rng);
    let census = merkle_census(&vs, 10);
    let e = election(2, 1, census.root(), &mut rng);
    let k = random_k(&mut rng);
    let secret = fr_to_dec(&k);
    let prep: PreparedVote = vs[0]
        .prepare_vote(
            &e.chain,
            &[1, 2],
            CensusWitness::Merkle(census.proof(0).unwrap()),
            10,
            k,
        )
        .unwrap();
    assert_eq!(prep.k, k);
    // Fr's own Debug prints limbs, so check both renderings.
    let limbs = format!("{k:?}");
    for (what, s) in [
        ("PreparedVote", format!("{prep:?}")),
        ("PreparedVote alt", format!("{prep:#?}")),
        ("CircomInputs", format!("{:?}", prep.inputs)),
        ("CircomInputs alt", format!("{:#?}", prep.inputs)),
        ("Voter", format!("{:?}", vs[0])),
    ] {
        assert!(!s.contains(&secret), "{what} leaks k");
        assert!(!s.contains(&limbs), "{what} leaks k");
        assert!(s.contains("redacted") || what == "Voter", "{what}: {s}");
    }
    // The voter key is never printed either.
    let sk_hex = hex::encode(vs[0].key.to_bytes());
    assert!(!format!("{:?}", vs[0]).contains(&sk_hex));
}
