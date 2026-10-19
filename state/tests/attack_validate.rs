//! Adversarial per-vote and per-batch checks that need no emulator:
//! `validate_vote`'s verdict on every targeted mutation of an honest
//! package, the batch rules the guest applies across votes, wire coordinates
//! at and past the modulus, and the byte order of every request field.
//! `attack_matrix.rs` runs the same mutations through the guest.

mod common;
mod emu;

use std::collections::BTreeSet;

use davinci_state::{
    CensusOrigin, ProcessConfig, ProcessState, VerifiedVote, VoteError, VotePackage,
    ballot_from_be_coords, validate_vote,
};
use davinci_zkvm_sdk::ballot::{BallotMode, ballot_leaf_hash, enc_key_hash, leaf_value_bytes};
use davinci_zkvm_sdk::census::{
    CensusWitness, LeanImt, census_leaf, csp_sign, eth_address, slot_key_address,
};
use davinci_zkvm_sdk::crypto::babyjubjub::Point;
use davinci_zkvm_sdk::crypto::elgamal::{Ciphertext, keygen};
use davinci_zkvm_sdk::crypto::field::{Fr, fr_from_be, fr_to_be, fr_to_le};
use davinci_zkvm_sdk::groth16::BallotVerifier;
use davinci_zkvm_sdk::limits::{BALLOT_COORDS, BALLOT_MAX, BALLOT_MIN, KEY_BALLOT_MODE};
use davinci_zkvm_sdk::release;
use k256::ecdsa::SigningKey;
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};

fn verifier() -> BallotVerifier {
    BallotVerifier::from_snarkjs_json(release::ballot_vk_json()).unwrap()
}

fn none() -> BTreeSet<u64> {
    BTreeSet::new()
}

fn csp_cfg(cfg: &ProcessConfig, csp: &SigningKey) -> ProcessConfig {
    let mut be = [0u8; 32];
    be[12..].copy_from_slice(&eth_address(csp.verifying_key()));
    ProcessConfig {
        census_origin: CensusOrigin::Csp,
        census_root: be,
        ..cfg.clone()
    }
}

/// A real-proof vote of `voter` under `c` with a CSP attestation.
fn csp_vote(
    c: &ProcessConfig,
    csp: &SigningKey,
    voter: &SigningKey,
    fields: &[u64],
    k: u64,
    weight: u128,
    index: u64,
) -> VotePackage {
    let addr = eth_address(voter.verifying_key());
    let att = csp_sign(csp, &c.process_id, &addr, weight, index);
    let (v, proved) = emu::vote(c, voter, fields, k, weight, CensusWitness::Csp(att));
    assert!(proved, "the circuit refused {fields:?}");
    v.pkg
}

/// `s -> n - s` with the recovery id flipped: the same signature, high-S.
fn high_s(s: &mut [u8; 32], v: &mut u8) {
    use k256::Scalar;
    use k256::elliptic_curve::PrimeField;
    let x = Scalar::from_repr((*s).into()).unwrap();
    *s = (-x).to_bytes().into();
    *v ^= 1;
}

// ----------------------------------------------------------------- bugs

// Fixed BUG: two CSP credentials for one address (two indexes) are two
// slots, and the guest's 3B.3 fails a batch carrying both (FAIL_CSP,
// attack_matrix `csp_one_address_two_indexes_fails_the_batch`), erroring
// every vote in it. The selection leaves the second for a later batch.
#[test]
fn csp_batch_takes_one_vote_per_address() {
    let env = common::env(2, 4);
    let csp = emu::key(4242);
    let c = csp_cfg(&env.cfg, &csp);
    let voter = emu::key(4243);
    let other = emu::key(4244);
    let st = ProcessState::create(c.clone(), arbo::MemoryStorage::new()).unwrap();
    let pending: Vec<VerifiedVote> = [
        csp_vote(&c, &csp, &voter, &[1, 2], 51, 1, 0),
        csp_vote(&c, &csp, &other, &[0, 1], 52, 1, 1),
        csp_vote(&c, &csp, &voter, &[2, 2], 53, 1, 7),
    ]
    .into_iter()
    .map(|p| validate_vote(&c, &verifier(), p).unwrap())
    .collect();
    let (sel, _) = st
        .select_batch(&pending, u64::MAX, &none(), usize::MAX)
        .unwrap();
    let addrs: Vec<[u8; 20]> = sel.iter().map(|v| v.pkg.address).collect();
    let distinct: BTreeSet<[u8; 20]> = addrs.iter().copied().collect();
    assert_eq!(
        distinct.len(),
        addrs.len(),
        "a CSP batch carries one address twice (the guest fails it with FAIL_CSP)"
    );
}

// Fixed BUG: the guest recovers high-S vote signatures (zisklib's recover
// checks only r, s in [1, n-1]; attack_matrix
// `per_vote_mutations_agree_with_the_guest` runs it), but validate_vote
// refused them, turning away a vote the circuit accepts.
#[test]
fn high_s_vote_signature_accepted_like_the_guest() {
    let census = MerkleElection::new(5000);
    let mut p = census.vote(0, &[1, 2, 3], 61);
    high_s(&mut p.signature.s, &mut p.signature.v);
    assert_eq!(
        validate_vote(&census.cfg, &verifier(), p.clone()).map(|_| ()),
        Ok(())
    );
    // Also in the 27/28 form the wire maps down.
    p.signature.v += 27;
    assert_eq!(
        validate_vote(&census.cfg, &verifier(), p).map(|_| ()),
        Ok(())
    );
}

// Fixed BUG: CSP attestations the guest takes were refused: a recovery id
// of 27/28 (transition.rs maps it to 0/1 for the wire, but csp_recover
// refused it first) and a high-S signature. attack_matrix
// `csp_mutations_agree_with_the_guest` shows the guest accepting each.
#[test]
fn csp_attestations_accepted_like_the_guest() {
    let env = common::env(2, 4);
    let csp = emu::key(4300);
    let c = csp_cfg(&env.cfg, &csp);
    let voter = emu::key(4301);
    let base = csp_vote(&c, &csp, &voter, &[1, 1], 71, 1, 3);
    let mut refused = Vec::new();
    let mut check = |what: &str, p: VotePackage| {
        if let Err(e) = validate_vote(&c, &verifier(), p) {
            refused.push(format!("{what}: {e}"));
        }
    };
    let mut p = base.clone();
    if let CensusWitness::Csp(a) = &mut p.census {
        a.recid += 27;
    }
    check("recid + 27", p);
    let mut p = base.clone();
    if let CensusWitness::Csp(a) = &mut p.census {
        high_s(&mut a.s, &mut a.recid);
    }
    check("high-S", p.clone());
    if let CensusWitness::Csp(a) = &mut p.census {
        a.recid += 27;
    }
    check("high-S, recid + 27", p);
    assert!(refused.is_empty(), "refused: {refused:#?}");
    // The guest refuses a CSP weight of 2^88 (3B.2c), and so does the node.
    let p = csp_vote(&c, &csp, &voter, &[1, 1], 72, 1 << 88, 3);
    assert_eq!(
        validate_vote(&c, &verifier(), p).map(|_| ()),
        Err(VoteError::WeightRange)
    );
}

// --------------------------------------------------------- held: per vote

/// A Merkle election at three fields with real proofs.
struct MerkleElection {
    cfg: ProcessConfig,
    keys: Vec<SigningKey>,
    imt: LeanImt,
}

impl MerkleElection {
    fn new(base: u64) -> Self {
        let keys: Vec<SigningKey> = (0..3).map(|i| emu::key(base + i)).collect();
        let imt = LeanImt::from_leaves(
            keys.iter()
                .map(|k| census_leaf(&eth_address(k.verifying_key()), 1).unwrap())
                .collect(),
        );
        let env = common::env(3, 1);
        let cfg = ProcessConfig {
            census_root: imt.root(),
            ..env.cfg
        };
        MerkleElection { cfg, keys, imt }
    }

    fn vote(&self, i: usize, fields: &[u64], k: u64) -> VotePackage {
        let w = CensusWitness::Merkle(self.imt.proof(i).unwrap());
        let (v, proved) = emu::vote(&self.cfg, &self.keys[i], fields, k, 1, w);
        assert!(proved);
        v.pkg
    }
}

type Mutate = Box<dyn Fn(&mut VotePackage)>;

/// `(mutation, apply, the expected error)`.
type Named = (&'static str, Mutate, fn(&VoteError) -> bool);

fn merkle_sibling(p: &mut VotePackage, f: impl Fn(&mut davinci_zkvm_sdk::census::CensusProof)) {
    if let CensusWitness::Merkle(m) = &mut p.census {
        f(m)
    }
}

/// Every named rejection, on one honest package: the error must be the
/// guest rule that the mutation breaks (attack_matrix checks the guest
/// refuses the same ones).
#[test]
fn per_vote_mutations_named() {
    let e = MerkleElection::new(5100);
    let ok = e.vote(0, &[1, 2, 3], 62);
    let other = e.vote(1, &[0, 0, 4], 63);
    let pk = e.cfg.enc_key;
    let (o_addr, o_census) = (other.address, other.census.clone());
    use VoteError::*;
    let sig = |e: &VoteError| matches!(e, SignatureInvalid(_) | SignatureMismatch);
    let rows: Vec<Named> = vec![
        ("vote id + 1", Box::new(|p| p.vote_id += 1), |e| {
            *e == SignatureMismatch
        }),
        (
            "vote id below 2^63",
            Box::new(|p| p.vote_id &= !(1 << 63)),
            |e| *e == VoteIdRange,
        ),
        (
            "another member's address",
            Box::new(move |p| p.address = o_addr),
            |e| *e == CensusLeafMismatch,
        ),
        (
            "another member's witness",
            Box::new(move |p| p.census = o_census.clone()),
            |e| *e == CensusLeafMismatch,
        ),
        ("weight 2^88", Box::new(|p| p.weight = 1 << 88), |e| {
            *e == WeightRange
        }),
        // The ciphertexts are public inputs: a changed one fails the proof.
        (
            "field 0 c1 + B8",
            Box::new(|p| p.ballot.0[0].c1 = p.ballot.0[0].c1.add(&Point::generator())),
            |e| *e == BadProof,
        ),
        (
            "field 0 identity",
            Box::new(|p| p.ballot.0[0] = Ciphertext::IDENTITY),
            |e| *e == BadProof,
        ),
        (
            "fields 0 and 1 swapped",
            Box::new(|p| p.ballot.0.swap(0, 1)),
            |e| *e == BadProof,
        ),
        (
            "field 0 off curve",
            Box::new(|p| p.ballot.0[0].c2.y += Fr::from(1u64)),
            |e| *e == OffCurve,
        ),
        (
            "padded field c2 = B8",
            Box::new(|p| p.ballot.0[3].c2 = Point::generator()),
            |e| *e == PaddingNotIdentity,
        ),
        (
            "padded field off curve",
            Box::new(|p| p.ballot.0[15].c1.x = Fr::from(1u64)),
            |e| *e == OffCurve,
        ),
        (
            "re-encrypted field",
            Box::new(move |p| {
                let z = davinci_zkvm_sdk::crypto::elgamal::encrypt(
                    &pk,
                    0,
                    &davinci_zkvm_sdk::crypto::field::fr_to_u256(&Fr::from(3u64)),
                );
                p.ballot.0[1] = p.ballot.0[1].add(&z)
            }),
            |e| *e == BadProof,
        ),
        (
            "signature r flip",
            Box::new(|p| p.signature.r[0] ^= 0x10),
            sig,
        ),
        (
            "signature s flip",
            Box::new(|p| p.signature.s[9] ^= 0x01),
            sig,
        ),
        ("signature v ^ 1", Box::new(|p| p.signature.v ^= 1), |e| {
            *e == SignatureMismatch
        }),
        ("signature v = 2", Box::new(|p| p.signature.v = 2), |e| {
            matches!(e, SignatureInvalid(_))
        }),
        (
            "signature s = 0",
            Box::new(|p| p.signature.s = [0; 32]),
            |e| matches!(e, SignatureInvalid(_)),
        ),
        (
            "signature r = n",
            Box::new(|p| {
                p.signature.r =
                    hex::decode("fffffffffffffffffffffffffffffffebaaedce6af48a03bbfd25e8cd0364141")
                        .unwrap()
                        .try_into()
                        .unwrap()
            }),
            |e| matches!(e, SignatureInvalid(_)),
        ),
        (
            "census sibling low bit",
            Box::new(|p| merkle_sibling(p, |m| m.siblings[0][31] ^= 1)),
            |e| *e == CensusProofInvalid,
        ),
        (
            "census sibling top bit",
            Box::new(|p| merkle_sibling(p, |m| m.siblings[0][0] ^= 0x80)),
            |e| *e == CensusProofInvalid,
        ),
        (
            "census leaf at 2^248",
            Box::new(|p| merkle_sibling(p, |m| m.leaf[0] = 1)),
            |e| *e == CensusLeafRange,
        ),
        (
            "census path bit 0",
            Box::new(|p| merkle_sibling(p, |m| m.path_bits ^= 1)),
            |e| *e == CensusProofInvalid,
        ),
        (
            "census path bit above depth",
            Box::new(|p| merkle_sibling(p, |m| m.path_bits |= 1 << m.siblings.len())),
            |e| *e == CensusPathBits,
        ),
        (
            "census 62 siblings",
            Box::new(|p| merkle_sibling(p, |m| m.siblings.resize(62, [0u8; 32]))),
            |e| *e == CensusDepth,
        ),
        (
            "census 64 siblings, path bit 63",
            Box::new(|p| {
                merkle_sibling(p, |m| {
                    m.siblings.resize(64, [0u8; 32]);
                    m.path_bits = 1 << 63;
                })
            }),
            |e| *e == CensusDepth,
        ),
        (
            "census root of another tree",
            Box::new(|p| merkle_sibling(p, |m| m.root[31] ^= 1)),
            |e| *e == CensusProofInvalid,
        ),
        (
            "proof a.x digit",
            Box::new(|p| {
                let s = &mut p.proof.pi_a[0];
                let last = s.pop().unwrap();
                s.push(if last == '9' { '8' } else { '9' });
            }),
            |e| *e == BadProof,
        ),
        (
            "proof a identity",
            Box::new(|p| p.proof.pi_a[2] = "0".into()),
            |e| *e == BadProof,
        ),
        (
            "proof b identity",
            Box::new(|p| p.proof.pi_b[2] = ["0".into(), "0".into()]),
            |e| *e == BadProof,
        ),
        (
            "proof hex coordinate",
            Box::new(|p| p.proof.pi_c[0] = format!("0x{}", p.proof.pi_c[0])),
            |e| *e == BadProof,
        ),
        (
            "proof 81-digit coordinate",
            Box::new(|p| p.proof.pi_c[1] = format!("{:0>81}", p.proof.pi_c[1])),
            |e| *e == BadProof,
        ),
        (
            "proof b c0/c1",
            Box::new(|p| p.proof.pi_b[0].swap(0, 1)),
            |e| *e == BadProof,
        ),
        (
            "process id + 1",
            Box::new(|p| p.process_id += Fr::from(1u64)),
            |e| *e == ProcessMismatch,
        ),
        (
            "process id byte-reversed",
            Box::new(|p| {
                let mut b = fr_to_be(&p.process_id);
                b.reverse();
                p.process_id = davinci_zkvm_sdk::crypto::field::fr_from_be_mod_order(&b);
            }),
            |e| *e == ProcessMismatch,
        ),
        (
            "Merkle witness, CSP shape",
            Box::new(|p| {
                p.census =
                    CensusWitness::Csp(csp_sign(&emu::key(1), &p.process_id, &p.address, 1, 0))
            }),
            |e| *e == OriginMismatch,
        ),
    ];
    assert_eq!(
        validate_vote(&e.cfg, &verifier(), ok.clone()).map(|_| ()),
        Ok(())
    );
    for (name, f, want) in rows {
        let mut p = ok.clone();
        f(&mut p);
        match validate_vote(&e.cfg, &verifier(), p) {
            Ok(_) => panic!("{name}: accepted"),
            Err(err) => assert!(want(&err), "{name}: {err:?}"),
        }
    }
    // Shapes the guest takes as the same vote: accepted.
    let same: Vec<(&str, Mutate)> = vec![
        ("v + 27", Box::new(|p| p.signature.v += 27)),
        (
            "proof a z = 2 (ignored)",
            Box::new(|p| p.proof.pi_a[2] = "2".into()),
        ),
        (
            "proof a.x with leading zeros",
            Box::new(|p| p.proof.pi_a[0] = format!("00{}", p.proof.pi_a[0])),
        ),
    ];
    for (name, f) in same {
        let mut p = ok.clone();
        f(&mut p);
        assert!(validate_vote(&e.cfg, &verifier(), p).is_ok(), "{name}");
    }
}

/// Wire coordinates: canonical below p only; p, p + 1, 2^256 - 1 and a
/// valid point's x + p are not field elements, p - 1 is one but off the
/// curve. The identity and real points pass, top bits set or not.
#[test]
fn ballot_coordinates_at_and_past_the_modulus() {
    let env = common::env(16, 1);
    let v = common::fake_vote(&env, 0, &[1; 16], 9);
    let good: Vec<[u8; 32]> = v.pkg.ballot.coords().iter().map(fr_to_be).collect();
    let arr = |c: &[[u8; 32]]| -> [[u8; 32]; BALLOT_COORDS] { c.to_vec().try_into().unwrap() };
    assert_eq!(ballot_from_be_coords(&arr(&good)).unwrap(), v.pkg.ballot);
    // Some coordinate of a 16-field ballot sits at or above 2^248.
    assert!(good.iter().any(|c| c[0] != 0));

    let p_be: [u8; 32] =
        hex::decode("30644e72e131a029b85045b68181585d2833e84879b9709143e1f593f0000001")
            .unwrap()
            .try_into()
            .unwrap();
    let add_be = |a: &[u8; 32], b: &[u8; 32]| -> [u8; 32] {
        let mut out = [0u8; 32];
        let mut carry = 0u16;
        for i in (0..32).rev() {
            let s = a[i] as u16 + b[i] as u16 + carry;
            out[i] = s as u8;
            carry = s >> 8;
        }
        out
    };
    let one = {
        let mut b = [0u8; 32];
        b[31] = 1;
        b
    };
    let mut p_minus_1 = p_be;
    p_minus_1[31] -= 1;
    for (what, val, want) in [
        ("p", p_be, VoteError::BadCoordinate),
        ("p + 1", add_be(&p_be, &one), VoteError::BadCoordinate),
        ("2^256 - 1", [0xff; 32], VoteError::BadCoordinate),
        ("x + p", add_be(&good[0], &p_be), VoteError::BadCoordinate),
        ("p - 1", p_minus_1, VoteError::OffCurve),
    ] {
        for at in [0usize, 5, 63] {
            let mut c = good.clone();
            c[at] = val;
            assert_eq!(
                ballot_from_be_coords(&arr(&c)).unwrap_err(),
                want,
                "{what} at {at}"
            );
        }
    }
    // The byte-reversed coordinates are another (off-curve) ballot.
    let rev: Vec<[u8; 32]> = good
        .iter()
        .map(|c| {
            let mut r = *c;
            r.reverse();
            r
        })
        .collect();
    assert!(ballot_from_be_coords(&arr(&rev)).is_err());
}

// ------------------------------------------------------- held: byte orders

fn le_hex(b: &[u8; 32]) -> String {
    format!("0x{}", hex::encode(b))
}

fn rev(b: &[u8; 32]) -> [u8; 32] {
    let mut r = *b;
    r.reverse();
    r
}

/// Every byte-order rule of CONTRIBUTING.md ("Byte orders"), checked field by
/// field on a built request with values whose reversal is another value:
/// an asymmetric process id with its top byte set (just under 2^248), an
/// election key with both coordinates at or above 2^248 and a census root
/// above the BN254 modulus.
#[test]
fn request_fields_follow_the_byte_order_rules() {
    let mut rng = StdRng::seed_from_u64(0xb0);
    let mut pid_be = [0u8; 32];
    rng.fill(&mut pid_be[1..]);
    pid_be[1] = 0xff;
    pid_be[31] = 0x02;
    let pid = fr_from_be(&pid_be).unwrap();
    let pk = loop {
        let (_, pk) = keygen(&mut rng);
        if fr_to_be(&pk.x)[0] != 0 && fr_to_be(&pk.y)[0] != 0 {
            break pk;
        }
    };
    let keys: Vec<SigningKey> = (0..3).map(|i| emu::key(5200 + i)).collect();
    let imt = LeanImt::from_leaves(
        keys.iter()
            .map(|k| census_leaf(&eth_address(k.verifying_key()), 0x0102_0304).unwrap())
            .collect(),
    );
    let root = imt.root();
    assert!(
        fr_from_be(&root).is_err(),
        "pick a seed whose census root is above the modulus"
    );
    let mode = BallotMode {
        num_fields: 3,
        group_size: 2,
        unique_values: true,
        cost_exponent: 2,
        max_value: 7,
        min_value: 1,
        max_value_sum: 300,
        min_value_sum: 3,
    };
    let c = ProcessConfig {
        process_id: pid,
        ballot_mode: mode,
        enc_key: pk,
        census_origin: CensusOrigin::MerkleStatic,
        census_root: root,
        ballot_vk_hash: common::embedded_vk_hash(),
    };
    let votes: Vec<VerifiedVote> = (0..2)
        .map(|i| {
            let w = CensusWitness::Merkle(imt.proof(i).unwrap());
            let (v, ok) = emu::vote(&c, &keys[i], &[1, 2, 3], 80 + i as u64, 0x0102_0304, w);
            assert!(ok);
            validate_vote(&c, &verifier(), v.pkg).unwrap()
        })
        .collect();
    let mut st = ProcessState::create(c.clone(), arbo::MemoryStorage::new()).unwrap();
    let old_root = st.root();
    let b = st
        .prepare(&votes, &mut StdRng::seed_from_u64(1), &none())
        .unwrap();
    let req = &b.request;
    let s = req.state.as_ref().unwrap();

    // Transition process id: arbo LE; KZG process id: BE.
    assert_eq!(s.process_id, le_hex(&rev(&pid_be)));
    assert_eq!(req.kzg.as_ref().unwrap().process_id, le_hex(&pid_be));
    // Roots: the raw arbo digest; the KZG root is its reversal (BE of the
    // LE integer); public outputs carry the raw digest.
    assert_eq!(s.old_state_root, le_hex(&old_root));
    assert_eq!(s.new_state_root, le_hex(&b.new_root));
    assert_eq!(
        req.kzg.as_ref().unwrap().root_hash_before,
        le_hex(&rev(&old_root))
    );
    assert_eq!(b.expected.root_before, old_root);
    // Config read proofs: keys LE8, values as their leaves hold them.
    let key = |k: u64| {
        let mut b = [0u8; 32];
        b[..8].copy_from_slice(&k.to_le_bytes());
        le_hex(&b)
    };
    let p = &s.process_smt;
    assert_eq!(
        p.iter().map(|e| e.new_key.clone()).collect::<Vec<_>>(),
        [0u64, 2, 3, 6, 7].map(key).to_vec()
    );
    assert_eq!(p[0].new_value, le_hex(&rev(&pid_be)));
    assert_eq!(p[1].new_value, le_hex(&fr_to_le(&mode.pack().unwrap())));
    // Hashed leaves: int_be(sha256) sent LE, i.e. the reversed digest.
    assert_eq!(p[2].new_value, le_hex(&rev(&enc_key_hash(&pk))));
    let mut origin = [0u8; 32];
    origin[0] = 1;
    assert_eq!(p[3].new_value, le_hex(&origin));
    assert_eq!(p[4].new_value, le_hex(&rev(&common::embedded_vk_hash())));
    assert_eq!(key(KEY_BALLOT_MODE), key(2));
    // Ballot leaves: the reversed sha256 of the re-encrypted ballot.
    let reenc0 = &s.ballot_proofs.as_ref().unwrap().voter_ballots[0];
    assert_eq!(reenc0[0].len(), 66);
    // Ballot coordinates and the election key: BE hex.
    let r = req.reencryption.as_ref().unwrap();
    assert_eq!(r.encryption_key_x, le_hex(&fr_to_be(&pk.x)));
    assert_eq!(r.encryption_key_y, le_hex(&fr_to_be(&pk.y)));
    for (e, v) in r.entries.iter().zip(&votes) {
        let c0 = &v.pkg.ballot.0[0];
        assert_eq!(e.original[0].c1.x, le_hex(&fr_to_be(&c0.c1.x)));
        assert_eq!(e.original[0].c2.y, le_hex(&fr_to_be(&c0.c2.y)));
    }
    let leaf0 = s.ballot_smt[0].new_value.clone();
    let reenc_ballot = b
        .request
        .state
        .as_ref()
        .unwrap()
        .ballot_proofs
        .as_ref()
        .unwrap()
        .voter_ballots[0]
        .iter()
        .map(|h| fr_from_be(&hex::decode(&h[2..]).unwrap().try_into().unwrap()).unwrap())
        .collect::<Vec<Fr>>();
    let rb =
        davinci_zkvm_sdk::ballot::Ballot::from_coords(&reenc_ballot.try_into().unwrap()).unwrap();
    assert_eq!(leaf0, le_hex(&leaf_value_bytes(&ballot_leaf_hash(&rb))));
    // Census values: BE hex, `0x` and 64 lowercase digits, never reduced;
    // the index is the path bits.
    let cp = &req.census_proofs.as_ref().unwrap()[1];
    assert_eq!(cp.root, le_hex(&root));
    let proof1 = imt.proof(1).unwrap();
    assert_eq!(cp.leaf, le_hex(&proof1.leaf));
    assert_eq!(
        cp.siblings,
        proof1.siblings.iter().map(le_hex).collect::<Vec<_>>()
    );
    assert_eq!(
        proof1.leaf,
        census_leaf(&votes[1].pkg.address, 0x0102_0304).unwrap()
    );
    assert_eq!(cp.index, proof1.path_bits);
    // No public inputs: the guest derives them.
    assert_eq!(
        serde_json::to_value(&**req).unwrap().get("public_inputs"),
        None
    );
    // Signatures: r, s BE as signed, v a recovery id.
    let sg = &req.sigs[0];
    assert_eq!(
        sg.signature_r,
        format!("0x{}", hex::encode(votes[0].pkg.signature.r))
    );
    assert!(sg.signature_v <= 1);
    // Register 20 is the census root's LE bytes; the contract reverses it
    // (PublicsLib.reverse32) and compares with `censusRoot`, the BE bytes
    // the node keeps.
    assert_eq!(b.expected.census_root, rev(&root));
}

/// CSP flavour: the voter address keeps its byte order, the weight is a
/// 32-byte BE integer, register 20 is the CSP address as uint160 LE.
#[test]
fn csp_request_fields_follow_the_byte_order_rules() {
    let env = common::env(2, 1);
    let csp = emu::key(5300);
    let c = csp_cfg(&env.cfg, &csp);
    let voter = emu::key(5301);
    let w: u128 = 0x00ff_0102_0304_0506_0708_090a;
    let v = validate_vote(
        &c,
        &verifier(),
        csp_vote(&c, &csp, &voter, &[2, 3], 91, w, (1 << 40) + 5),
    )
    .unwrap();
    assert_eq!(v.slot, BALLOT_MIN + (1 << 40) + 5);
    let mut st = ProcessState::create(c.clone(), arbo::MemoryStorage::new()).unwrap();
    let b = st
        .prepare(
            std::slice::from_ref(&v),
            &mut StdRng::seed_from_u64(2),
            &none(),
        )
        .unwrap();
    let cp = &b.request.csp_data.as_ref().unwrap().proofs[0];
    assert_eq!(
        cp.voter_address,
        format!("0x{}", hex::encode(v.pkg.address))
    );
    let mut wbe = [0u8; 32];
    wbe[16..].copy_from_slice(&w.to_be_bytes());
    assert_eq!(cp.weight, le_hex(&wbe));
    assert_eq!(cp.index, (1 << 40) + 5);
    assert!(cp.recid <= 1);
    let mut addr_le = [0u8; 32];
    for (i, b) in eth_address(csp.verifying_key()).iter().rev().enumerate() {
        addr_le[i] = *b;
    }
    assert_eq!(b.expected.census_root, addr_le);
    assert!(b.request.census_proofs.is_none());
}

/// The CSP index range: `BallotMax - BallotMin` is the last slot, one more
/// leaves the ballot namespace (the guest fails it with FAIL_BALLOT_NS).
#[test]
fn csp_index_bounds() {
    let env = common::env(2, 1);
    let csp = emu::key(5400);
    let c = csp_cfg(&env.cfg, &csp);
    let voter = emu::key(5401);
    let base = csp_vote(&c, &csp, &voter, &[1, 0], 95, 1, 0);
    for (index, want) in [
        (0u64, Ok(BALLOT_MIN)),
        (BALLOT_MAX - BALLOT_MIN, Ok(BALLOT_MAX)),
        (BALLOT_MAX - BALLOT_MIN + 1, Err(VoteError::SlotRange)),
        (u64::MAX - BALLOT_MIN + 1, Err(VoteError::SlotRange)),
        (u64::MAX, Err(VoteError::SlotRange)),
    ] {
        let mut p = base.clone();
        p.census = CensusWitness::Csp(csp_sign(&csp, &c.process_id, &p.address, 1, index));
        assert_eq!(
            validate_vote(&c, &verifier(), p).map(|v| v.slot),
            want,
            "index {index:#x}"
        );
    }
}

/// Merkle weights at both ends of the 88-bit range validate; the leaf binds
/// address and weight, and the slot follows the address only.
#[test]
fn merkle_weight_edges() {
    let keys: Vec<SigningKey> = (0..3).map(|i| emu::key(5500 + i)).collect();
    let ws = [0u128, 1, (1 << 88) - 1];
    let imt = LeanImt::from_leaves(
        keys.iter()
            .zip(ws)
            .map(|(k, w)| census_leaf(&eth_address(k.verifying_key()), w).unwrap())
            .collect(),
    );
    let env = common::env(2, 1);
    let c = ProcessConfig {
        census_root: imt.root(),
        ..env.cfg
    };
    for (i, w) in ws.into_iter().enumerate() {
        let wit = CensusWitness::Merkle(imt.proof(i).unwrap());
        let (v, ok) = emu::vote(&c, &keys[i], &[0, 0], 97, w, wit);
        assert!(ok, "weight {w:#x}");
        let got = validate_vote(&c, &verifier(), v.pkg.clone()).unwrap();
        assert_eq!(got.slot, slot_key_address(&v.pkg.address));
    }
}

/// Register 45 is input-gen's `(nproofs as f64).log2() as u64`; the node
/// expects `ilog2`. Equal for every batch size the guest takes.
#[test]
fn log_n_matches_input_gen_for_every_batch_size() {
    for n in 1..=davinci_zkvm_sdk::limits::MAX_BATCH_SIZE {
        assert_eq!((n as f64).log2() as u64, n.ilog2() as u64, "n {n}");
    }
}

/// The node's batch builder refuses what the guest's batch rules refuse:
/// an empty batch, 1025 votes, one slot twice, one vote id twice.
#[test]
fn batch_rules_refused_at_build() {
    let env = common::env(2, 1100);
    let mut st = common::new_state(&env);
    let mut rng = StdRng::seed_from_u64(3);
    assert!(st.prepare(&[], &mut rng, &none()).is_err());
    let big: Vec<VerifiedVote> = (0..1025)
        .map(|i| common::fake_vote(&env, i, &[1, 0], 10_000 + i as u64))
        .collect();
    assert!(st.prepare(&big, &mut rng, &none()).is_err());
    assert_eq!(st.root(), st.committed().root);
    let a = common::fake_vote(&env, 0, &[1, 0], 1);
    let a2 = common::fake_vote(&env, 0, &[0, 1], 2);
    assert!(st.prepare(&[a.clone(), a2], &mut rng, &none()).is_err());
    let mut twin = common::fake_vote(&env, 1, &[1, 0], 3);
    twin.pkg.vote_id = a.pkg.vote_id;
    assert!(st.prepare(&[a, twin], &mut rng, &none()).is_err());
    assert_eq!(st.root(), st.committed().root);
}
