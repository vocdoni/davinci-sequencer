//! Every named rejection of `validate_vote`, plus one accepted vote per
//! census origin with a real circom proof under the protocol VK.

mod common;

use common::*;
use davinci_state::{
    CensusOrigin, ProcessConfig, VoteError, ballot_from_be_coords, validate_vote, vote_still_valid,
};
use davinci_zkvm_sdk::census::{
    CensusProof, CensusWitness, LeanImt, census_leaf, csp_sign, hash2, slot_key_address,
};
use davinci_zkvm_sdk::crypto::babyjubjub::Point;
use davinci_zkvm_sdk::crypto::elgamal::encrypt;
use davinci_zkvm_sdk::crypto::elgamal::keygen;
use davinci_zkvm_sdk::crypto::field::{Fr, fr_from_be, fr_to_be, u256_from_be};
use davinci_zkvm_sdk::groth16::BallotVerifier;
use davinci_zkvm_sdk::limits::{BALLOT_COORDS, BALLOT_MIN};
use davinci_zkvm_sdk::release;
use k256::Scalar;
use k256::elliptic_curve::PrimeField;

fn verifier() -> BallotVerifier {
    BallotVerifier::from_snarkjs_json(release::ballot_vk_json()).unwrap()
}

// CSP flavour of `env`'s config: the census root is the CSP address.
fn csp_cfg(cfg: &ProcessConfig, csp_address: &[u8; 20]) -> ProcessConfig {
    let mut be = [0u8; 32];
    be[12..].copy_from_slice(csp_address);
    let mut c = cfg.clone();
    c.census_origin = CensusOrigin::Csp;
    c.census_root = be;
    c
}

#[test]
fn accepts_a_valid_merkle_vote() {
    let env = env(2, 4);
    let v = real_vote(&env, 0, &[1, 2], 5);
    let got = validate_vote(&env.cfg, &verifier(), v.pkg.clone()).unwrap();
    assert_eq!(got.slot, v.slot);
}

// Keccak roots are uniform 256-bit values, most above the BN254 modulus;
// the node carries them as bytes, never as field elements.
#[test]
fn accepts_a_vote_under_a_root_above_the_modulus() {
    let env = env(2, 4);
    assert!(
        fr_from_be(&env.cfg.census_root).is_err(),
        "the 4-member root must be above the modulus"
    );
    let v = real_vote(&env, 2, &[0, 3], 8);
    validate_vote(&env.cfg, &verifier(), v.pkg).unwrap();
}

// A single-member census has root = leaf (no hashing), as before.
#[test]
fn accepts_a_vote_in_a_single_member_census() {
    let env = env(2, 1);
    assert_eq!(
        env.cfg.census_root,
        census_leaf(&voter_address(0), 1).unwrap()
    );
    let v = real_vote(&env, 0, &[1, 2], 5);
    let got = validate_vote(&env.cfg, &verifier(), v.pkg.clone()).unwrap();
    assert!(got.pkg.census == v.pkg.census);
}

// Review Focus 1, node side: the proof is checked against the 71 inputs the
// guest derives, so changing any of them (with every earlier check still
// passing) fails the proof.
#[test]
fn the_proof_binds_every_public_input() {
    let env = env(2, 4);
    let vf = verifier();
    let v = real_vote(&env, 0, &[1, 2], 5);
    validate_vote(&env.cfg, &vf, v.pkg.clone()).unwrap();
    let bad = |cfg: &ProcessConfig, p| {
        assert_eq!(validate_vote(cfg, &vf, p).unwrap_err(), VoteError::BadProof);
    };
    // 1 vote id, signed again by the voter.
    let mut p = v.pkg.clone();
    p.vote_id += 1;
    p.signature = davinci_zkvm_sdk::census::vote_id_sign(&voter_key(0), p.vote_id);
    bad(&env.cfg, p);
    // 3 packed ballot mode and 4, 5 the encryption key: the process's.
    let mut c = env.cfg.clone();
    c.ballot_mode.max_value += 1;
    bad(&c, v.pkg.clone());
    let mut c = env.cfg.clone();
    c.enc_key = keygen(&mut <rand::rngs::StdRng as rand::SeedableRng>::seed_from_u64(8)).1;
    bad(&c, v.pkg.clone());
    // 6 weight: a CSP attestation for weight 2 (a Merkle leaf pins it).
    let csp = voter_key(99);
    let cfg = csp_cfg(&env.cfg, &voter_address(99));
    let mut p = v.pkg.clone();
    p.weight = 2;
    p.census = CensusWitness::Csp(csp_sign(&csp, &cfg.process_id, &p.address, 2, 3));
    bad(&cfg, p);
    // 7.. ciphertexts: two live fields swapped, and one field encrypted again.
    let mut p = v.pkg.clone();
    p.ballot.0.swap(0, 1);
    bad(&env.cfg, p);
    let mut p = v.pkg.clone();
    p.ballot.0[1] = encrypt(
        &env.cfg.enc_key,
        2,
        &u256_from_be(&fr_to_be(&Fr::from(7u64))),
    );
    bad(&env.cfg, p);
}

#[test]
fn merkle_slot_follows_the_address_not_the_leaf() {
    let env = env(2, 4);
    let v = real_vote(&env, 0, &[1, 2], 5);
    let want = slot_key_address(&v.pkg.address);
    let got = validate_vote(&env.cfg, &verifier(), v.pkg.clone()).unwrap();
    assert_eq!(got.slot, want);

    // Same voter at leaf 3 of a reordered census: same slot.
    let rev = LeanImt::from_leaves(
        (0..4)
            .rev()
            .map(|i| census_leaf(&voter_address(i), 1).unwrap())
            .collect(),
    );
    let mut cfg = env.cfg.clone();
    cfg.census_root = rev.root();
    let mut p = v.pkg.clone();
    p.census = CensusWitness::Merkle(rev.proof(3).unwrap());
    assert_eq!(validate_vote(&cfg, &verifier(), p).unwrap().slot, want);
}

#[test]
fn accepts_a_valid_csp_vote() {
    let env = env(2, 4);
    // Same voter, key, ballot and weight: the public signals (and so the
    // cached proof) are identical, only the census witness changes.
    let mut v = real_vote(&env, 0, &[1, 2], 5);
    let csp = voter_key(99);
    let cfg = csp_cfg(&env.cfg, &voter_address(99));
    v.pkg.census = CensusWitness::Csp(csp_sign(&csp, &cfg.process_id, &v.pkg.address, 1, 3));
    let got = validate_vote(&cfg, &verifier(), v.pkg).unwrap();
    assert_eq!(got.slot, BALLOT_MIN + 3);
}

#[test]
fn dynamic_merkle_origins_validate_as_merkle() {
    let env = env(2, 4);
    let v = real_vote(&env, 0, &[1, 2], 5);
    let fake = fake_vote(&env, 0, &[1, 2], 6);
    let vf = verifier();
    for o in [
        CensusOrigin::MerkleOffchainDynamic,
        CensusOrigin::MerkleOnchainDynamic,
    ] {
        let cfg = ProcessConfig {
            census_origin: o,
            ..env.cfg.clone()
        };
        let got = validate_vote(&cfg, &vf, v.pkg.clone()).unwrap();
        assert_eq!(got.slot, slot_key_address(&v.pkg.address), "{o:?}");

        // The census root is still pinned.
        let mut c = cfg.clone();
        c.census_root[31] ^= 1;
        assert_eq!(
            validate_vote(&c, &vf, fake.pkg.clone()).unwrap_err(),
            VoteError::CensusRootMismatch,
            "{o:?}"
        );
        // Another member's leaf.
        let mut p = fake.pkg.clone();
        p.census = CensusWitness::Merkle(env.imt.proof(1).unwrap());
        assert_eq!(
            validate_vote(&cfg, &vf, p).unwrap_err(),
            VoteError::CensusLeafMismatch,
            "{o:?}"
        );
        // A CSP witness is not a Merkle one.
        let mut p = fake.pkg.clone();
        p.census = CensusWitness::Csp(csp_sign(&voter_key(99), &cfg.process_id, &p.address, 1, 0));
        assert_eq!(
            validate_vote(&cfg, &vf, p).unwrap_err(),
            VoteError::OriginMismatch,
            "{o:?}"
        );
    }
}

#[test]
fn rejects_non_canonical_coordinate() {
    let env = env(2, 4);
    let v = fake_vote(&env, 0, &[1], 6);
    let mut coords: Vec<[u8; 32]> = v.pkg.ballot.coords().iter().map(fr_to_be).collect();
    // BN254 r: one above the largest canonical element.
    coords[0] = hex::decode("30644e72e131a029b85045b68181585d2833e84879b9709143e1f593f0000001")
        .unwrap()
        .try_into()
        .unwrap();
    let arr: [[u8; 32]; BALLOT_COORDS] = coords.try_into().unwrap();
    assert_eq!(
        ballot_from_be_coords(&arr).unwrap_err(),
        VoteError::BadCoordinate
    );
}

#[test]
fn rejects_everything_the_guest_rejects() {
    let env = env(2, 4);
    let cfg = &env.cfg;
    let vf = verifier();
    let ok = fake_vote(&env, 0, &[1, 2], 6);
    let check = |pkg, want: VoteError| {
        assert_eq!(validate_vote(cfg, &vf, pkg).unwrap_err(), want);
    };

    // Wrong process.
    let mut p = ok.pkg.clone();
    p.process_id = Fr::from(9u64);
    check(p, VoteError::ProcessMismatch);

    // Config pinned to another ballot VK.
    let mut c = cfg.clone();
    c.ballot_vk_hash = [0u8; 32];
    assert_eq!(
        validate_vote(&c, &vf, ok.pkg.clone()).unwrap_err(),
        VoteError::VkMismatch
    );

    // Vote id below 2^63.
    let mut p = ok.pkg.clone();
    p.vote_id = 41;
    check(p, VoteError::VoteIdRange);

    // Off-curve ballot point.
    let mut p = ok.pkg.clone();
    p.ballot.0[0].c1 = Point {
        x: Fr::from(1u64),
        y: Fr::from(1u64),
    };
    check(p, VoteError::OffCurve);

    // Padded slot carries a real ciphertext (encryption of zero, even).
    let mut p = ok.pkg.clone();
    p.ballot.0[3] = encrypt(&cfg.enc_key, 0, &u256_from_be(&fr_to_be(&Fr::from(7u64))));
    check(p, VoteError::PaddingNotIdentity);

    // Weight above 88 bits.
    let mut p = ok.pkg.clone();
    p.weight = 1u128 << 88;
    check(p, VoteError::WeightRange);

    // Signature by another key.
    let mut p = ok.pkg.clone();
    p.signature = davinci_zkvm_sdk::census::vote_id_sign(&voter_key(1), p.vote_id);
    check(p, VoteError::SignatureMismatch);

    // s negated alone: another key's signature.
    let mut p = ok.pkg.clone();
    let s = Scalar::from_repr(p.signature.s.into()).unwrap();
    p.signature.s = (-s).to_bytes().into();
    assert!(matches!(
        validate_vote(cfg, &vf, p.clone()).unwrap_err(),
        VoteError::SignatureMismatch | VoteError::SignatureInvalid(_)
    ));
    // High-S with the recid flipped is the same signature, which the guest
    // takes: the next check (the proof) fails.
    p.signature.v ^= 1;
    check(p, VoteError::BadProof);

    // Flipped recovery id: some other key signed this.
    let mut p = ok.pkg.clone();
    p.signature.v ^= 1;
    assert!(matches!(
        validate_vote(cfg, &vf, p).unwrap_err(),
        VoteError::SignatureMismatch | VoteError::SignatureInvalid(_)
    ));

    // Census proof for another root.
    let mut other = LeanImt::from_leaves(
        (0..4)
            .map(|i| census_leaf(&voter_address(i), 1).unwrap())
            .collect(),
    );
    other.insert(census_leaf(&voter_address(9), 1).unwrap());
    let mut p = ok.pkg.clone();
    p.census = CensusWitness::Merkle(other.proof(0).unwrap());
    check(p, VoteError::CensusRootMismatch);

    // Deeper than 61 levels.
    let mut p = ok.pkg.clone();
    let mut one = [0u8; 32];
    one[31] = 1;
    p.census = CensusWitness::Merkle(CensusProof {
        root: cfg.census_root,
        leaf: one,
        path_bits: 0,
        siblings: vec![[0u8; 32]; 62],
    });
    check(p, VoteError::CensusDepth);

    // Path bits above the proof depth.
    let (root, leaf, sibs) = match &ok.pkg.census {
        CensusWitness::Merkle(m) => (m.root, m.leaf, m.siblings.clone()),
        _ => unreachable!(),
    };
    let mut p = ok.pkg.clone();
    p.census = CensusWitness::Merkle(CensusProof {
        root,
        leaf,
        path_bits: 1 << sibs.len(),
        siblings: sibs.clone(),
    });
    check(p, VoteError::CensusPathBits);

    // Leaf at or above 2^248 (guest 3A.6): the internal node over leaves 0
    // and 1 presented as a leaf one level up, on a valid path to the root.
    let n0 = hash2(&leaf, &sibs[0]);
    assert_ne!(n0[0], 0, "this level-1 Keccak node is above 2^248");
    let mut p = ok.pkg.clone();
    p.census = CensusWitness::Merkle(CensusProof {
        root,
        leaf: n0,
        path_bits: 0,
        siblings: vec![sibs[1]],
    });
    assert_eq!(hash2(&n0, &sibs[1]), root);
    check(p, VoteError::CensusLeafRange);

    // Broken sibling.
    let mut bad = sibs.clone();
    bad[0][31] ^= 1;
    let mut p = ok.pkg.clone();
    p.census = CensusWitness::Merkle(CensusProof {
        root,
        leaf,
        path_bits: 0,
        siblings: bad,
    });
    check(p, VoteError::CensusProofInvalid);

    // Another member's proof: right root, wrong leaf for this address.
    let mut p = ok.pkg.clone();
    p.census = CensusWitness::Merkle(env.imt.proof(1).unwrap());
    check(p, VoteError::CensusLeafMismatch);

    // Groth16 proof does not verify (everything before it passes).
    check(ok.pkg.clone(), VoteError::BadProof);
}

#[test]
fn rejects_csp_forgeries() {
    let env = env(2, 4);
    let csp = voter_key(99);
    let cfg = csp_cfg(&env.cfg, &voter_address(99));
    let vf = verifier();
    let base = fake_vote(&env, 0, &[1, 2], 6);
    let att = csp_sign(&csp, &cfg.process_id, &base.pkg.address, 1, 0);

    // Merkle witness in a CSP process.
    assert_eq!(
        validate_vote(&cfg, &vf, base.pkg.clone()).unwrap_err(),
        VoteError::OriginMismatch
    );

    // CSP witness in a Merkle process.
    let mut p = base.pkg.clone();
    p.census = CensusWitness::Csp(att.clone());
    assert_eq!(
        validate_vote(&env.cfg, &vf, p).unwrap_err(),
        VoteError::OriginMismatch
    );

    // CSP index outside the ballot namespace.
    let mut p = base.pkg.clone();
    p.census = CensusWitness::Csp(csp_sign(&csp, &cfg.process_id, &p.address, 1, u64::MAX));
    assert_eq!(
        validate_vote(&cfg, &vf, p).unwrap_err(),
        VoteError::SlotRange
    );

    // Attestation signed by another key.
    let mut p = base.pkg.clone();
    p.census = CensusWitness::Csp(csp_sign(&voter_key(98), &cfg.process_id, &p.address, 1, 0));
    assert_eq!(
        validate_vote(&cfg, &vf, p).unwrap_err(),
        VoteError::CspSignerMismatch
    );

    // Attestation for another address.
    let mut p = base.pkg.clone();
    let mut a = att.clone();
    a.address = voter_address(1);
    p.census = CensusWitness::Csp(a);
    assert_eq!(
        validate_vote(&cfg, &vf, p).unwrap_err(),
        VoteError::CspAddressMismatch
    );

    // Attestation for another weight.
    let mut p = base.pkg.clone();
    let mut a = att.clone();
    a.weight = 2;
    p.census = CensusWitness::Csp(a);
    assert_eq!(
        validate_vote(&cfg, &vf, p).unwrap_err(),
        VoteError::CspWeightMismatch
    );

    // A CSP weight of 2^88, signed by the CSP (guest 3B.2c).
    let mut p = base.pkg.clone();
    p.weight = 1u128 << 88;
    p.census = CensusWitness::Csp(csp_sign(&csp, &cfg.process_id, &p.address, 1 << 88, 0));
    assert_eq!(
        validate_vote(&cfg, &vf, p).unwrap_err(),
        VoteError::WeightRange
    );
}

#[test]
fn census_change_keeps_only_unchanged_leaves() {
    let env = env(2, 4);
    let v = fake_vote(&env, 0, &[1, 2], 6);
    let addr = v.pkg.address;
    for o in [
        CensusOrigin::MerkleStatic,
        CensusOrigin::MerkleOffchainDynamic,
        CensusOrigin::MerkleOnchainDynamic,
    ] {
        let cfg = ProcessConfig {
            census_origin: o,
            ..env.cfg.clone()
        };
        let still = |leaf| vote_still_valid(&cfg, &v.pkg, leaf);
        assert!(still(Some(census_leaf(&addr, 1).unwrap())), "{o:?}");
        // Removed, reweighted, or someone else's leaf.
        assert!(!still(None), "{o:?}");
        assert!(!still(Some(census_leaf(&addr, 2).unwrap())), "{o:?}");
        assert!(
            !still(Some(census_leaf(&voter_address(1), 1).unwrap())),
            "{o:?}"
        );
    }
    // A CSP census has no leaves to change.
    let cfg = csp_cfg(&env.cfg, &voter_address(99));
    assert!(vote_still_valid(&cfg, &v.pkg, None));
}
