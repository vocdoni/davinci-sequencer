//! Seeded combinatorial matrix through the real batch guest on ziskemu: every
//! batch the node builds must pass with all 64 registers equal to the node's
//! own expectation, and every per-vote verdict of `validate_vote` must be
//! the guest's.
//!
//! Gated: `ZISKEMU=1 cargo test -p davinci-state --test attack_matrix --
//! --test-threads=1`. `MATRIX_CASES=N` widens the seeded matrices,
//! `MATRIX_SEED` reseeds them. Needs `ziskemu`, the circuit ELF and the
//! circom artifacts (`CIRCOM_ARTIFACTS`) for proofs not yet in `tests/cache`.

mod common;
mod emu;

use std::collections::BTreeSet;

use davinci_state::{
    CensusOrigin, ProcessConfig, ProcessState, VerifiedVote, VoteError, validate_vote,
};
use davinci_zkvm_sdk::ballot::BallotMode;
use davinci_zkvm_sdk::census::{
    CensusProof, CensusWitness, LeanImt, census_leaf, csp_sign, eth_address, hash2,
    slot_key_address,
};
use davinci_zkvm_sdk::crypto::babyjubjub::Point;
use davinci_zkvm_sdk::crypto::elgamal::keygen;
use davinci_zkvm_sdk::crypto::field::{Fr, fr_from_be, fr_to_be};
use davinci_zkvm_sdk::groth16::BallotVerifier;
use davinci_zkvm_sdk::limits::{BALLOT_MAX, BALLOT_MIN, MAX_CENSUS_DEPTH, required_refresh};
use davinci_zkvm_sdk::release;
use davinci_zkvm_sdk::types::{CensusProofJson, enc};
use emu::Verdict;
use k256::ecdsa::SigningKey;
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};

fn verifier() -> &'static BallotVerifier {
    static V: std::sync::OnceLock<BallotVerifier> = std::sync::OnceLock::new();
    V.get_or_init(|| BallotVerifier::from_snarkjs_json(release::ballot_vk_json()).unwrap())
}

fn none() -> BTreeSet<u64> {
    BTreeSet::new()
}

/// A 31-byte process id read big-endian, top byte `top` (0xff puts the
/// value just under 2^248), the rest asymmetric.
fn pid(rng: &mut StdRng, top: u8) -> Fr {
    let mut b = [0u8; 32];
    rng.fill(&mut b[2..]);
    b[1] = top;
    b[31] = 0x01;
    fr_from_be(&b).unwrap()
}

/// Election key from a seeded secret; `big` keeps drawing until both
/// coordinates are at or above 2^248 (a BE/LE slip changes them).
fn enc_key(rng: &mut StdRng, big: bool) -> Point {
    loop {
        let (_, pk) = keygen(rng);
        let hi = |x: &Fr| fr_to_be(x)[0] != 0;
        if !big || (hi(&pk.x) && hi(&pk.y)) {
            return pk;
        }
    }
}

fn cfg(
    pid: Fr,
    mode: BallotMode,
    pk: Point,
    origin: CensusOrigin,
    root: [u8; 32],
) -> ProcessConfig {
    ProcessConfig {
        process_id: pid,
        ballot_mode: mode,
        enc_key: pk,
        census_origin: origin,
        census_root: root,
        ballot_vk_hash: common::embedded_vk_hash(),
    }
}

fn state(c: &ProcessConfig) -> ProcessState<arbo::MemoryStorage> {
    ProcessState::create(c.clone(), arbo::MemoryStorage::new()).unwrap()
}

/// A Merkle election: voter keys, weights, census.
struct Census {
    keys: Vec<SigningKey>,
    weights: Vec<u128>,
    imt: LeanImt,
}

impl Census {
    fn new(base: u64, weights: &[u128]) -> Census {
        let keys: Vec<SigningKey> = (0..weights.len() as u64)
            .map(|i| emu::key(base + i))
            .collect();
        let leaves = keys
            .iter()
            .zip(weights)
            .map(|(k, w)| census_leaf(&eth_address(k.verifying_key()), *w).unwrap())
            .collect();
        Census {
            keys,
            weights: weights.to_vec(),
            imt: LeanImt::from_leaves(leaves),
        }
    }

    fn witness(&self, i: usize) -> CensusWitness {
        CensusWitness::Merkle(self.imt.proof(i).unwrap())
    }

    /// Voter `i`'s vote, validated: it must pass `validate_vote`.
    fn vote(&self, c: &ProcessConfig, i: usize, fields: &[u64], k: u64) -> VerifiedVote {
        let (v, proved) = emu::vote(
            c,
            &self.keys[i],
            fields,
            k,
            self.weights[i],
            self.witness(i),
        );
        assert!(
            proved,
            "voter {i}: circuit refused {fields:?} under {:?}",
            c.ballot_mode
        );
        let got = validate_vote(c, verifier(), v.pkg.clone()).unwrap();
        assert_eq!(got.slot, v.slot);
        got
    }
}

// ------------------------------------------------------------- ballot modes

/// `sum(v^e)` the way the circom `SumPow` computes it (exponent 0 is 1).
fn cost(fields: &[u64], e: u8) -> u128 {
    fields.iter().map(|v| (*v as u128).pow(e as u32)).sum()
}

/// One ballot-mode case: a mode at `nf` fields and two ballots sitting
/// exactly on its edges (every field at `min`, resp. at `max`, and the sums
/// equal to `min_value_sum`, resp. `max_value_sum`), plus ballots one step
/// past an edge that the circom circuit must refuse.
struct ModeCase {
    mode: BallotMode,
    low: Vec<u64>,
    high: Vec<u64>,
    /// `(what, ballot)` that must not prove.
    bad: Vec<(&'static str, Vec<u64>)>,
}

fn mode_case(rng: &mut StdRng, nf: u8) -> ModeCase {
    let n = nf as usize;
    let exps = [0u8, 1, 1, 1, 2, 3, 255];
    let e = exps[rng.gen_range(0..exps.len())];
    // Value range: the sum of n powers stays below 2^63.
    let max_cap: u64 = match e {
        0 | 1 => (1u64 << 48) - 1,
        2 => 1 << 28,
        3 => 1 << 18,
        _ => 1,
    };
    // n distinct values need n of them in range.
    let unique = max_cap >= n as u64 - 1 && rng.gen_bool(0.4);
    let span_min = if unique { n as u64 - 1 } else { 0 };
    let (min, max) = loop {
        let hi = if rng.gen_bool(0.3) {
            max_cap
        } else {
            rng.gen_range(0..=max_cap.min(1 << 20))
        };
        let lo = if rng.gen_bool(0.3) {
            0
        } else {
            rng.gen_range(0..=hi)
        };
        if hi - lo >= span_min && hi >= lo {
            break (lo, hi);
        }
    };
    let ballot = |start: u64, step_down: bool| -> Vec<u64> {
        (0..n as u64)
            .map(|i| {
                if !unique {
                    start
                } else if step_down {
                    start - i
                } else {
                    start + i
                }
            })
            .collect()
    };
    let low = ballot(min, false);
    let high = ballot(max, true);
    let s_low = cost(&low, e);
    let s_high = cost(&high, e);
    assert!(s_high < 1 << 63 && s_low <= s_high);
    let mode = BallotMode {
        num_fields: nf,
        group_size: rng.gen_range(0..=nf),
        unique_values: unique,
        cost_exponent: e,
        max_value: max,
        min_value: min,
        // 0 would mean "bounded by the weight": keep the edge exact.
        max_value_sum: s_high as u64,
        min_value_sum: s_low as u64,
    };
    let mut bad = Vec::new();
    if max < (1 << 48) - 1 && !unique {
        let mut b = high.clone();
        b[n - 1] = max + 1;
        bad.push(("a field above maxValue", b));
    }
    if min > 0 && !unique {
        let mut b = low.clone();
        b[0] = min - 1;
        bad.push(("a field below minValue", b));
    }
    if unique && n >= 2 {
        let mut b = low.clone();
        b[1] = b[0];
        bad.push(("a repeated value under uniqueValues", b));
    }
    ModeCase {
        mode: mode.validated(),
        low,
        high,
        bad,
    }
}

trait Validated {
    fn validated(self) -> Self;
}

impl Validated for BallotMode {
    // The contract's own bounds (ProcessRegistry._validateNewProcess).
    fn validated(self) -> Self {
        assert!(self.num_fields >= 1 && self.num_fields <= 16);
        assert!(self.group_size <= self.num_fields);
        assert!(self.min_value <= self.max_value);
        assert!(self.min_value_sum <= self.max_value_sum);
        self.pack().unwrap();
        self
    }
}

/// Every numFields 1..=16 (one per case, MATRIX_CASES widens it), a seeded
/// mode with both ballots on its edges, a fresh election key and process id.
/// Batch 1: two new votes. Batch 2: a revote and a new voter, so one
/// overwrite and one refresh. Both pass the guest with the node's 64
/// registers, and every ballot one step past an edge is unprovable.
#[test]
fn modes_and_field_counts_through_the_guest() {
    if !emu::gated() {
        return;
    }
    let mut rng = StdRng::seed_from_u64(emu::seed(0x6d6f_6465));
    for case in 0..emu::cases(16) {
        let nf = (case % 16 + 1) as u8;
        let mc = mode_case(&mut rng, nf);
        let top = if case % 2 == 0 { 0xff } else { 0x00 };
        let c0 = cfg(
            pid(&mut rng, top),
            mc.mode,
            enc_key(&mut rng, case % 3 == 0),
            CensusOrigin::MerkleStatic,
            [0u8; 32],
        );
        // Weights: with a sum bound any 88-bit weight proves. maxValueSum 0
        // bounds the sum by the weight through a 63-bit comparator, which
        // no weight of 2^63 or more passes (circom, not the node).
        let wmax = if mc.mode.max_value_sum == 0 {
            1u128 << 63
        } else {
            1u128 << 88
        };
        let w = [1, wmax - 1, rng.gen_range(0..wmax)];
        let census = Census::new(1000 * case as u64, &w);
        let c = ProcessConfig {
            census_root: census.imt.root(),
            ..c0
        };
        let ctx = format!("case {case} nf {nf} {:?}", c.ballot_mode);
        let t = std::time::Instant::now();
        for (what, b) in &mc.bad {
            let (_, proved) = emu::vote(
                &c,
                &census.keys[0],
                b,
                7 + case as u64,
                w[0],
                census.witness(0),
            );
            assert!(!proved, "{ctx}: {what} proved: {b:?}");
        }
        let mut st = state(&c);
        let v = vec![
            census.vote(&c, 0, &mc.low, 11),
            census.vote(&c, 1, &mc.high, 12),
        ];
        let b = st
            .prepare(&v, &mut StdRng::seed_from_u64(case as u64), &none())
            .unwrap();
        assert!(b.refresh_slots().is_empty());
        emu::assert_accepted(&b, &format!("{ctx} batch 1"));
        st.commit(&b).unwrap();

        let v = vec![
            census.vote(&c, 0, &mc.high, 13),
            census.vote(&c, 2, &mc.low, 14),
        ];
        let b = st
            .prepare(&v, &mut StdRng::seed_from_u64(case as u64 + 1), &none())
            .unwrap();
        assert_eq!(b.expected.overwrites, 1, "{ctx}");
        assert_eq!(b.refresh_slots().len(), required_refresh(2, 1, 2), "{ctx}");
        emu::assert_accepted(&b, &format!("{ctx} batch 2"));
        eprintln!("{ctx}: ok in {:.1}s", t.elapsed().as_secs_f64());
    }
}

/// Hand-picked modes the seeded draw may miss: exponent 0 (every field
/// costs 1), a sum bounded by the weight (`maxValueSum` 0, sum exactly the
/// weight, and one unit over it unprovable), every field at 2^48 - 1, and
/// 16 unique values filling `[min, min + 15]` exactly.
#[test]
fn pinned_mode_edges_through_the_guest() {
    if !emu::gated() {
        return;
    }
    let mut rng = StdRng::seed_from_u64(0x9177);
    let m = |nf: u8, unique: bool, e: u8, min: u64, max: u64, smin: u64, smax: u64| BallotMode {
        num_fields: nf,
        group_size: nf,
        unique_values: unique,
        cost_exponent: e,
        max_value: max,
        min_value: min,
        max_value_sum: smax,
        min_value_sum: smin,
    };
    let top = (1u64 << 48) - 1;
    // (mode, weight, ballots that prove, ballots that must not)
    type Pinned = (BallotMode, u128, Vec<Vec<u64>>, Vec<Vec<u64>>);
    let cases: Vec<Pinned> = vec![
        (
            m(16, false, 0, 0, 9, 16, 16),
            1,
            vec![vec![0; 16], vec![9; 16]],
            vec![vec![10; 16]],
        ),
        (
            m(4, false, 1, 0, 100, 0, 0),
            250,
            vec![vec![100, 100, 50, 0], vec![0; 4]],
            vec![vec![100, 100, 51, 0]],
        ),
        (
            m(16, false, 1, top, top, 16 * top, 16 * top),
            (1 << 88) - 1,
            vec![vec![top; 16], vec![top; 16]],
            vec![],
        ),
        (
            m(16, true, 1, 7, 22, 0, 16 * 22),
            3,
            vec![(7..23).collect(), (7..23).rev().collect()],
            vec![(6..22).collect(), {
                let mut v: Vec<u64> = (7..23).collect();
                v[15] = 7;
                v
            }],
        ),
    ];
    for (case, (mode, w, good, bad)) in cases.into_iter().enumerate() {
        let census = Census::new(400_000 + 10 * case as u64, &[w, w]);
        let c = cfg(
            pid(&mut rng, 0x3c),
            mode.validated(),
            enc_key(&mut rng, false),
            CensusOrigin::MerkleStatic,
            census.imt.root(),
        );
        for b in &bad {
            let (_, proved) = emu::vote(&c, &census.keys[0], b, 5, w, census.witness(0));
            assert!(!proved, "{mode:?}: {b:?} proved");
        }
        let votes: Vec<VerifiedVote> = good
            .iter()
            .enumerate()
            .map(|(i, f)| census.vote(&c, i, f, 6 + i as u64))
            .collect();
        let mut st = state(&c);
        let b = st.prepare(&votes, &mut rng, &none()).unwrap();
        emu::assert_accepted(&b, &format!("{mode:?}"));
    }
}

// ------------------------------------------------------ per-vote differential

/// BN254 base field modulus q (snarkjs proof coordinates live below it).
const Q: &str = "21888242871839275222246405745257275088696311157297823662689037894645226208583";

// a - b for decimal strings, a >= b.
fn dec_sub(a: &str, b: &str) -> String {
    let a: Vec<u8> = a.bytes().rev().map(|c| c - b'0').collect();
    let b: Vec<u8> = b.bytes().rev().map(|c| c - b'0').collect();
    let mut out = Vec::with_capacity(a.len());
    let mut borrow = 0i16;
    for (i, ai) in a.iter().enumerate() {
        let mut d = *ai as i16 - borrow - *b.get(i).unwrap_or(&0) as i16;
        borrow = 0;
        if d < 0 {
            d += 10;
            borrow = 1;
        }
        out.push(d as u8);
    }
    assert_eq!(borrow, 0);
    while out.len() > 1 && *out.last().unwrap() == 0 {
        out.pop();
    }
    out.iter().rev().map(|d| (d + b'0') as char).collect()
}

fn neg_fq(x: &str) -> String {
    if x == "0" { "0".into() } else { dec_sub(Q, x) }
}

/// The slot the guest binds a vote to: from the census leaf's address
/// (Merkle) or `BallotMin + index` (CSP, unchecked here).
fn guest_slot(p: &davinci_state::VotePackage) -> u64 {
    match &p.census {
        CensusWitness::Merkle(m) => {
            slot_key_address(&davinci_zkvm_sdk::census::census_leaf_address(&m.leaf))
        }
        CensusWitness::Csp(c) => BALLOT_MIN.wrapping_add(c.index),
    }
}

/// The guest's verdict on a batch of `pkgs` as given (no node check), from
/// a fresh state: `Err` when the node's builder refuses it outright.
fn guest_on(c: &ProcessConfig, pkgs: &[davinci_state::VotePackage]) -> Result<Verdict, String> {
    let mut st = state(c);
    let votes: Vec<VerifiedVote> = pkgs
        .iter()
        .map(|p| VerifiedVote {
            slot: guest_slot(p),
            pkg: p.clone(),
        })
        .collect();
    let b = st
        .prepare(&votes, &mut StdRng::seed_from_u64(99), &none())
        .map_err(|e| e.to_string())?;
    Ok(emu::run(&b.request))
}

type Mutate = Box<dyn Fn(&mut davinci_state::VotePackage)>;

/// `(mutation, node verdict, guest verdict)`.
type Row = (&'static str, Result<(), VoteError>, Result<Verdict, String>);

fn high_s(sig_s: &mut [u8; 32], v: &mut u8) {
    use k256::Scalar;
    use k256::elliptic_curve::PrimeField;
    let s = Scalar::from_repr((*sig_s).into()).unwrap();
    *sig_s = (-s).to_bytes().into();
    *v ^= 1;
}

/// Runs `muts` on `base[0]` (`base[1..]` ride along honest) and returns
/// `(name, node verdict, guest verdict)` rows.
fn differential(
    c: &ProcessConfig,
    base: &[davinci_state::VotePackage],
    muts: Vec<(&'static str, Mutate)>,
) -> Vec<Row> {
    let mut rows = Vec::new();
    for (name, f) in muts {
        let mut p = base[0].clone();
        f(&mut p);
        let node = validate_vote(c, verifier(), p.clone()).map(|_| ());
        let mut batch = vec![p];
        batch.extend_from_slice(&base[1..]);
        let guest = guest_on(c, &batch);
        eprintln!(
            "{name:44} node {:<40} guest {}",
            format!("{:?}", node.as_ref().err()),
            match &guest {
                Ok(Verdict::Accepted(..)) => "accepted".to_string(),
                Ok(Verdict::Rejected(m)) => format!(
                    "rejected {m:#x} {:?}",
                    davinci_zkvm_sdk::publics::fail_bits(*m)
                ),
                Ok(Verdict::Refused(e)) => format!("service refused: {e:.60}"),
                Err(e) => format!("builder refused: {e:.60}"),
            }
        );
        rows.push((name, node, guest));
    }
    rows
}

fn merkle_mutations(
    c: &ProcessConfig,
    other: &davinci_state::VotePackage,
) -> Vec<(&'static str, Mutate)> {
    let other = other.clone();
    let (o_addr, o_census) = (other.address, other.census.clone());
    let pk = c.enc_key;
    let nf = c.ballot_mode.num_fields as usize;
    let mut m: Vec<(&'static str, Mutate)> = vec![
        ("honest", Box::new(|_| {})),
        ("vote id + 1", Box::new(|p| p.vote_id += 1)),
        (
            "vote id top bit cleared",
            Box::new(|p| p.vote_id &= !(1 << 63)),
        ),
        (
            "address of another member",
            Box::new(move |p| p.address = o_addr),
        ),
        (
            "census witness of another member",
            Box::new(move |p| p.census = o_census.clone()),
        ),
        ("weight + 1", Box::new(|p| p.weight += 1)),
        ("weight 0", Box::new(|p| p.weight = 0)),
        (
            "field 0 c1 + B8 (on curve)",
            Box::new(move |p| p.ballot.0[0].c1 = p.ballot.0[0].c1.add(&Point::generator())),
        ),
        (
            "field 0 set to identity",
            Box::new(|p| p.ballot.0[0] = davinci_zkvm_sdk::crypto::elgamal::Ciphertext::IDENTITY),
        ),
        (
            "field 0 c1.x + 1 (off curve)",
            Box::new(|p| p.ballot.0[0].c1.x += Fr::from(1u64)),
        ),
        (
            "field 0 c1/c2 swapped",
            Box::new(|p| {
                let ct = &mut p.ballot.0[0];
                std::mem::swap(&mut ct.c1, &mut ct.c2)
            }),
        ),
        (
            "fields 0 and 1 swapped",
            Box::new(|p| p.ballot.0.swap(0, 1)),
        ),
        (
            "ballot re-encrypted (c + Enc(0))",
            Box::new(move |p| {
                let z = davinci_zkvm_sdk::crypto::elgamal::encrypt(
                    &pk,
                    0,
                    &davinci_zkvm_sdk::crypto::field::fr_to_u256(&Fr::from(5u64)),
                );
                p.ballot.0[0] = p.ballot.0[0].add(&z)
            }),
        ),
        (
            "signature r byte flip",
            Box::new(|p| p.signature.r[31] ^= 1),
        ),
        (
            "signature s byte flip",
            Box::new(|p| p.signature.s[31] ^= 1),
        ),
        ("signature v ^ 1", Box::new(|p| p.signature.v ^= 1)),
        ("signature v + 27", Box::new(|p| p.signature.v += 27)),
        ("signature v = 2", Box::new(|p| p.signature.v = 2)),
        ("signature v = 29", Box::new(|p| p.signature.v = 29)),
        (
            "signature high-S (s -> n - s, v ^ 1)",
            Box::new(|p| high_s(&mut p.signature.s, &mut p.signature.v)),
        ),
        ("signature r = 0", Box::new(|p| p.signature.r = [0; 32])),
        (
            "census sibling 0 low bit",
            Box::new(|p| {
                if let CensusWitness::Merkle(m) = &mut p.census {
                    m.siblings[0][31] ^= 1;
                }
            }),
        ),
        (
            "census leaf at 2^248",
            Box::new(|p| {
                if let CensusWitness::Merkle(m) = &mut p.census {
                    m.leaf[0] = 1;
                }
            }),
        ),
        (
            "census path bit 0 flipped",
            Box::new(|p| {
                if let CensusWitness::Merkle(m) = &mut p.census {
                    m.path_bits ^= 1;
                }
            }),
        ),
        (
            "census path bit above depth",
            Box::new(|p| {
                if let CensusWitness::Merkle(m) = &mut p.census {
                    m.path_bits |= 1 << m.siblings.len();
                }
            }),
        ),
        (
            "census root low bit",
            Box::new(|p| {
                if let CensusWitness::Merkle(m) = &mut p.census {
                    m.root[31] ^= 1;
                }
            }),
        ),
        (
            "census sibling appended (zero)",
            Box::new(|p| {
                if let CensusWitness::Merkle(m) = &mut p.census {
                    m.siblings.push([0u8; 32]);
                }
            }),
        ),
        (
            "proof a.x + 1",
            Box::new(|p| {
                p.proof.pi_a[0] = (p.proof.pi_a[0].parse::<u128>().map(|x| (x + 1).to_string()))
                    .unwrap_or_else(|_| {
                        format!("{}1", &p.proof.pi_a[0][..p.proof.pi_a[0].len() - 1])
                    })
            }),
        ),
        (
            "proof (-A, -B, C) malleation",
            Box::new(|p| {
                p.proof.pi_a[1] = neg_fq(&p.proof.pi_a[1]);
                p.proof.pi_b[1][0] = neg_fq(&p.proof.pi_b[1][0]);
                p.proof.pi_b[1][1] = neg_fq(&p.proof.pi_b[1][1]);
            }),
        ),
        (
            "proof a = identity (z = 0)",
            Box::new(|p| p.proof.pi_a[2] = "0".into()),
        ),
        (
            "proof a z = 2 (ignored)",
            Box::new(|p| p.proof.pi_a[2] = "2".into()),
        ),
        (
            "proof a.x with leading zeros",
            Box::new(|p| p.proof.pi_a[0] = format!("00{}", p.proof.pi_a[0])),
        ),
        (
            "proof a.x + q (non-canonical)",
            Box::new(|p| {
                // q + x has 78 digits at most; the node and input-gen reject >= q.
                let x = &p.proof.pi_a[0];
                p.proof.pi_a[0] = dec_add(Q, x);
            }),
        ),
        (
            "proof b c0/c1 swapped",
            Box::new(|p| {
                p.proof.pi_b[0].swap(0, 1);
                p.proof.pi_b[1].swap(0, 1);
            }),
        ),
        (
            "proof a and c swapped",
            Box::new(|p| std::mem::swap(&mut p.proof.pi_a, &mut p.proof.pi_c)),
        ),
        (
            "proof protocol label",
            Box::new(|p| p.proof.protocol = "plonk".into()),
        ),
        (
            "process id + 1",
            Box::new(|p| p.process_id += Fr::from(1u64)),
        ),
    ];
    if nf < 16 {
        m.push((
            "padded field set to Enc(0)",
            Box::new(move |p| {
                p.ballot.0[nf] = davinci_zkvm_sdk::crypto::elgamal::encrypt(
                    &pk,
                    0,
                    &davinci_zkvm_sdk::crypto::field::fr_to_u256(&Fr::from(9u64)),
                )
            }),
        ));
        m.push((
            "padded field c1 = B8",
            Box::new(move |p| p.ballot.0[nf].c1 = Point::generator()),
        ));
    }
    m
}

// a + b for decimal strings.
fn dec_add(a: &str, b: &str) -> String {
    let a: Vec<u8> = a.bytes().rev().map(|c| c - b'0').collect();
    let b: Vec<u8> = b.bytes().rev().map(|c| c - b'0').collect();
    let mut out = Vec::new();
    let mut carry = 0u8;
    for i in 0..a.len().max(b.len()) {
        let d = a.get(i).unwrap_or(&0) + b.get(i).unwrap_or(&0) + carry;
        out.push(d % 10);
        carry = d / 10;
    }
    if carry > 0 {
        out.push(carry);
    }
    out.iter().rev().map(|d| (d + b'0') as char).collect()
}

/// Rows where `validate_vote` and the guest may disagree, and why.
fn excused(name: &str) -> Option<&'static str> {
    match name {
        // The declared weight and address are not on the wire: the guest
        // derives both public inputs from the leaf, the node binds its own
        // copies to the leaf (stricter, never a sunk batch).
        "weight + 1" | "weight 0" | "address of another member" => Some("not on the wire"),
        // input-gen ignores the label; snarkjs always writes "groth16".
        "proof protocol label" => Some("not on the wire"),
        _ => None,
    }
}

fn agree(rows: &[Row]) {
    for (name, node, guest) in rows {
        let g = matches!(guest, Ok(Verdict::Accepted(..)));
        match excused(name) {
            Some("bug") => assert!(g && node.is_err(), "{name}: the divergence moved: {node:?}"),
            Some(_) => {}
            // Node accepts what the guest (or the service) refuses: one such
            // vote errors its whole batch.
            None => assert_eq!(node.is_ok(), g, "{name}: node {node:?}, guest {guest:?}"),
        }
    }
}

/// Every targeted mutation of an honest vote (first in a two-vote batch),
/// then seeded byte flips (second in the batch): `validate_vote` accepts
/// exactly what the guest accepts. A mutation the node lets through and the
/// guest (or the service's input generation) refuses would sink a batch.
#[test]
fn per_vote_mutations_agree_with_the_guest() {
    if !emu::gated() {
        return;
    }
    let mut rng = StdRng::seed_from_u64(emu::seed(0xd1ff));
    let mode = common::ballot_mode(3);
    let census = Census::new(50_000, &[1, 1, 1]);
    let c = cfg(
        pid(&mut rng, 0xff),
        mode,
        enc_key(&mut rng, true),
        CensusOrigin::MerkleStatic,
        census.imt.root(),
    );
    let a = census.vote(&c, 0, &[1, 2, 3], 21).pkg;
    let b = census.vote(&c, 1, &[0, 5, 0], 22).pkg;
    agree(&differential(
        &c,
        &[a.clone(), b.clone()],
        merkle_mutations(&c, &b),
    ));

    // Seeded flips of one byte or digit of a wire field of the second vote.
    let mut muts: Vec<(&'static str, Mutate)> = Vec::new();
    for _ in 0..emu::cases(12) {
        let which = rng.gen_range(0..6);
        let byte = rng.gen_range(0..32usize);
        let bit = 1u8 << rng.gen_range(0..8);
        let digit = rng.gen_range(0..70usize);
        let (name, f): (&'static str, Mutate) = match which {
            0 => (
                "flip: signature r",
                Box::new(move |p| p.signature.r[byte] ^= bit),
            ),
            1 => (
                "flip: signature s",
                Box::new(move |p| p.signature.s[byte] ^= bit),
            ),
            2 => (
                "flip: vote id bit",
                Box::new(move |p| p.vote_id ^= 1 << (byte * 2)),
            ),
            3 => (
                "flip: ciphertext byte",
                Box::new(move |p| {
                    let mut x = fr_to_be(&p.ballot.0[0].c2.x);
                    x[byte] ^= bit;
                    p.ballot.0[0].c2.x = davinci_zkvm_sdk::crypto::field::fr_from_be_mod_order(&x);
                }),
            ),
            4 => (
                "flip: census sibling byte",
                Box::new(move |p| {
                    if let CensusWitness::Merkle(m) = &mut p.census {
                        let i = byte % m.siblings.len();
                        m.siblings[i][byte] ^= bit;
                    }
                }),
            ),
            _ => (
                "flip: proof digit",
                Box::new(move |p| {
                    let s = &mut p.proof.pi_c[0];
                    let i = digit % s.len();
                    let d = s.as_bytes()[i];
                    let nd = if d == b'9' { b'0' } else { d + 1 };
                    s.replace_range(i..i + 1, &(nd as char).to_string());
                }),
            ),
        };
        muts.push((name, f));
    }
    let rows = differential(&c, &[b, a], muts);
    for (name, node, guest) in &rows {
        let g = matches!(guest, Ok(Verdict::Accepted(..)));
        assert_eq!(node.is_ok(), g, "{name}: node {node:?}, guest {guest:?}");
    }
}

// Register 20's little-endian bytes of a census root.
fn le(root: &[u8; 32]) -> [u8; 32] {
    let mut r = *root;
    r.reverse();
    r
}

fn csp_root(csp: &SigningKey) -> [u8; 32] {
    let mut be = [0u8; 32];
    be[12..].copy_from_slice(&eth_address(csp.verifying_key()));
    be
}

/// A CSP vote by `voter` with `weight` and CSP `index`, validated or not.
fn csp_vote(
    c: &ProcessConfig,
    csp: &SigningKey,
    voter: &SigningKey,
    fields: &[u64],
    k: u64,
    weight: u128,
    index: u64,
) -> davinci_state::VotePackage {
    let addr = eth_address(voter.verifying_key());
    let att = csp_sign(csp, &c.process_id, &addr, weight, index);
    let (v, proved) = emu::vote(c, voter, fields, k, weight, CensusWitness::Csp(att));
    assert!(proved);
    v.pkg
}

fn resign(c: &ProcessConfig, csp: &SigningKey, p: &mut davinci_state::VotePackage) {
    if let CensusWitness::Csp(a) = &mut p.census {
        *a = csp_sign(csp, &c.process_id, &a.address, a.weight, a.index);
    }
}

/// CSP attestation mutations through `validate_vote` and the guest, then
/// the weight edges (each with its own proof), then one address with two
/// indexes. The one divergence is the node's 88-bit weight cap (policy).
#[test]
fn csp_mutations_agree_with_the_guest() {
    if !emu::gated() {
        return;
    }
    let mut rng = StdRng::seed_from_u64(0xc5b);
    let csp = emu::key(77_777);
    let c = cfg(
        pid(&mut rng, 0x80),
        common::ballot_mode(2),
        enc_key(&mut rng, false),
        CensusOrigin::Csp,
        csp_root(&csp),
    );
    let (va, vb) = (emu::key(60_000), emu::key(60_001));
    let a = csp_vote(&c, &csp, &va, &[1, 2], 31, 1, 0);
    let b = csp_vote(&c, &csp, &vb, &[3, 0], 32, 1, 1);
    let csp2 = csp.clone();
    let csp3 = csp.clone();
    let csp4 = csp.clone();
    let c2 = c.clone();
    let c3 = c.clone();
    let c4 = c.clone();
    let b_addr = b.address;
    let muts: Vec<(&'static str, Mutate)> = vec![
        ("honest", Box::new(|_| {})),
        (
            "csp recid + 27",
            Box::new(|p| {
                if let CensusWitness::Csp(a) = &mut p.census {
                    a.recid += 27;
                }
            }),
        ),
        (
            "csp recid ^ 1",
            Box::new(|p| {
                if let CensusWitness::Csp(a) = &mut p.census {
                    a.recid ^= 1;
                }
            }),
        ),
        (
            "csp high-S",
            Box::new(|p| {
                if let CensusWitness::Csp(a) = &mut p.census {
                    high_s(&mut a.s, &mut a.recid);
                }
            }),
        ),
        (
            "csp high-S, recid + 27",
            Box::new(|p| {
                if let CensusWitness::Csp(a) = &mut p.census {
                    high_s(&mut a.s, &mut a.recid);
                    a.recid += 27;
                }
            }),
        ),
        (
            "csp signed by another key",
            Box::new(|p| {
                if let CensusWitness::Csp(a) = &mut p.census {
                    *a = csp_sign(&emu::key(1), &p.process_id, &a.address, a.weight, a.index);
                }
            }),
        ),
        (
            "csp index max (re-signed)",
            Box::new(move |p| {
                if let CensusWitness::Csp(a) = &mut p.census {
                    a.index = BALLOT_MAX - BALLOT_MIN;
                }
                resign(&c2, &csp2, p);
            }),
        ),
        (
            "csp index max + 1 (re-signed)",
            Box::new(move |p| {
                if let CensusWitness::Csp(a) = &mut p.census {
                    a.index = BALLOT_MAX - BALLOT_MIN + 1;
                }
                resign(&c3, &csp3, p);
            }),
        ),
        (
            "csp index u64::MAX (re-signed)",
            Box::new(move |p| {
                if let CensusWitness::Csp(a) = &mut p.census {
                    a.index = u64::MAX;
                }
                resign(&c4, &csp4, p);
            }),
        ),
        (
            "csp attestation of another address",
            Box::new(move |p| {
                if let CensusWitness::Csp(a) = &mut p.census {
                    a.address = b_addr;
                }
            }),
        ),
        (
            "csp index of the other voter",
            Box::new(|p| {
                if let CensusWitness::Csp(a) = &mut p.census {
                    a.index = 1;
                }
            }),
        ),
    ];
    let rows = differential(&c, &[a.clone(), b.clone()], muts);
    for (name, node, guest) in &rows {
        let g = matches!(guest, Ok(Verdict::Accepted(..)));
        assert_eq!(node.is_ok(), g, "{name}: node {node:?}, guest {guest:?}");
    }

    // Weight edges need their own proofs (the weight is a public input).
    // From 2^88 up the guest fails the batch (3B.2c, FAIL_CENSUS) and the
    // node refuses the vote (`WeightRange`).
    for w in [0u128, 1, (1 << 88) - 1, 1 << 88, u128::MAX >> 1] {
        let p = csp_vote(&c, &csp, &va, &[1, 2], 33, w, 0);
        let node = validate_vote(&c, verifier(), p.clone()).map(|_| ());
        let g = guest_on(&c, &[p, b.clone()]).unwrap();
        if w >> 88 == 0 {
            assert!(g.accepted(), "weight {w:#x}: guest {g:?}");
            assert_eq!(node, Ok(()), "weight {w:#x}");
        } else {
            assert_eq!(
                g.mask(),
                Some(emu::FAIL_CENSUS),
                "weight {w:#x}: guest {g:?}"
            );
            assert_eq!(node, Err(VoteError::WeightRange), "weight {w:#x}");
        }
    }
}

/// Evidence for attack_validate `csp_batch_takes_one_vote_per_address`: two
/// valid credentials of one address in one batch fail the guest's 3B.3
/// (FAIL_CSP alone). Across two batches the guest takes both, so that
/// voter then holds two counted ballots, and an index credited to a second
/// address overwrites the first holder's ballot: only the CSP's
/// one-index-per-voter rule stands between them.
#[test]
fn csp_one_address_two_indexes_fails_the_batch() {
    if !emu::gated() {
        return;
    }
    let mut rng = StdRng::seed_from_u64(0xc5c);
    let csp = emu::key(77_778);
    let c = cfg(
        pid(&mut rng, 0x80),
        common::ballot_mode(2),
        enc_key(&mut rng, false),
        CensusOrigin::Csp,
        csp_root(&csp),
    );
    let voter = emu::key(60_010);
    let honest = emu::key(60_011);
    let a = csp_vote(&c, &csp, &voter, &[1, 2], 35, 1, 0);
    let a2 = csp_vote(&c, &csp, &voter, &[2, 2], 36, 1, 5);
    let h = csp_vote(&c, &csp, &honest, &[0, 1], 37, 1, 1);
    let vv: Vec<VerifiedVote> = [a, h, a2]
        .into_iter()
        .map(|p| validate_vote(&c, verifier(), p).unwrap())
        .collect();
    // Sealed together, the honest vote fails with both.
    let mut st = state(&c);
    let b = st.prepare(&vv, &mut rng, &none()).unwrap();
    let got = emu::run(&b.request);
    assert_eq!(got.mask(), Some(emu::FAIL_CSP), "{got:?}");
    // So the selection leaves the second credential for a later batch.
    let st = state(&c);
    let (sel, _) = st.select_batch(&vv, u64::MAX, &none(), usize::MAX).unwrap();
    let sel: Vec<VerifiedVote> = sel.into_iter().cloned().collect();
    let ids: Vec<u64> = sel.iter().map(|v| v.pkg.vote_id).collect();
    assert_eq!(ids, [vv[0].pkg.vote_id, vv[1].pkg.vote_id]);
    let mut st = state(&c);
    let b = st.prepare(&sel, &mut rng, &none()).unwrap();
    emu::assert_accepted(&b, "deduplicated CSP batch");

    // Two batches: both credentials land.
    let mut st = state(&c);
    let b = st.prepare(&vv[..2], &mut rng, &none()).unwrap();
    emu::assert_accepted(&b, "first credential");
    st.commit(&b).unwrap();
    let b = st.prepare(&vv[2..], &mut rng, &none()).unwrap();
    emu::assert_accepted(&b, "second credential, next batch");
    st.commit(&b).unwrap();
    assert_eq!(st.occupied(), 3, "one address holds two slots");

    // One index credited to another address: the guest takes it as an
    // overwrite of the first holder's ballot.
    let thief = emu::key(60_012);
    let t = csp_vote(&c, &csp, &thief, &[0, 0], 38, 1, 0);
    let t = validate_vote(&c, verifier(), t).unwrap();
    let b = st.prepare(&[t], &mut rng, &none()).unwrap();
    assert_eq!(b.expected.overwrites, 1);
    emu::assert_accepted(&b, "index reused by another address");
}

/// A census proof of `leaf` with `depth` seeded siblings and path bits
/// `bits`, and the root it folds to (a "census" only this path defines).
fn crafted_path(rng: &mut StdRng, leaf: [u8; 32], depth: usize, bits: u64) -> CensusProof {
    // Full 256-bit siblings, like Keccak nodes.
    let siblings: Vec<[u8; 32]> = (0..depth).map(|_| rng.r#gen()).collect();
    let mut p = CensusProof {
        root: [0u8; 32],
        leaf,
        path_bits: bits,
        siblings,
    };
    // Fold with the SDK's walk (verify_census_proof's), whatever the shape.
    let mut node = leaf;
    for (i, s) in p.siblings.iter().enumerate() {
        node = if (bits >> i) & 1 == 1 {
            hash2(s, &node)
        } else {
            hash2(&node, s)
        };
    }
    p.root = node;
    p
}

/// Census proof shapes at the guest's bounds (3A.4): no sibling (a one-leaf
/// census), one, 61 with every path bit set, then 62 and 64 siblings,
/// at both ends of the 88-bit weight. `validate_vote` and the guest agree.
#[test]
fn census_shapes_at_the_bounds_agree_with_the_guest() {
    if !emu::gated() {
        return;
    }
    let mut rng = StdRng::seed_from_u64(0xce);
    let voter = emu::key(70_000);
    let addr = eth_address(voter.verifying_key());
    for (depth, bits) in [
        (0usize, 0u64),
        (1, 1),
        (61, (1u64 << 61) - 1),
        (61, 0x0a5a_5a5a_5a5a_5a5a & ((1 << 61) - 1)),
        (62, 0),
        (64, 0),
    ] {
        for w in [1u128, (1 << 88) - 1] {
            let leaf = census_leaf(&addr, w).unwrap();
            let p = crafted_path(&mut rng, leaf, depth, bits);
            let c = cfg(
                pid(&mut rng, 0x11),
                common::ballot_mode(1),
                enc_key(&mut rng, false),
                CensusOrigin::MerkleStatic,
                p.root,
            );
            let (v, proved) = emu::vote(&c, &voter, &[4], 41, w, CensusWitness::Merkle(p));
            assert!(proved);
            let node = validate_vote(&c, verifier(), v.pkg.clone()).map(|_| ());
            let g = guest_on(&c, &[v.pkg]).unwrap();
            let ctx = format!("depth {depth} bits {bits:#x} weight {w:#x}");
            if depth <= MAX_CENSUS_DEPTH {
                assert_eq!(node, Ok(()), "{ctx}");
                assert!(g.accepted(), "{ctx}: {g:?}");
            } else {
                assert_eq!(node, Err(VoteError::CensusDepth), "{ctx}");
                assert_eq!(g.mask(), Some(emu::FAIL_CENSUS), "{ctx}");
            }
        }
    }
}

// ------------------------------------------------------------- byte orders

fn reverse_hex(h: &mut String) {
    let mut b = hex::decode(h.trim_start_matches("0x")).unwrap();
    let before = b.clone();
    b.reverse();
    assert_ne!(b, before, "a palindromic value proves nothing");
    *h = format!("0x{}", hex::encode(b));
}

/// The emulator side of attack_validate's byte-order checks: every
/// order-sensitive request field, byte-reversed on its own, makes the guest
/// (or the service) refuse an otherwise accepted batch, so none of the
/// values the matrix uses is symmetric enough to hide a wrong order.
#[test]
fn byte_reversed_fields_are_refused() {
    if !emu::gated() {
        return;
    }
    let mut rng = StdRng::seed_from_u64(0xbe1e);
    for csp_mode in [false, true] {
        let census = Census::new(500_000, &[0x0102_0304, 7]);
        let csp = emu::key(500_100);
        let (origin, root) = if csp_mode {
            (CensusOrigin::Csp, csp_root(&csp))
        } else {
            (CensusOrigin::MerkleStatic, census.imt.root())
        };
        let c = cfg(
            pid(&mut rng, 0xff),
            common::ballot_mode(2),
            enc_key(&mut rng, true),
            origin,
            root,
        );
        let votes: Vec<VerifiedVote> = (0..2)
            .map(|i| {
                if csp_mode {
                    let p = csp_vote(
                        &c,
                        &csp,
                        &census.keys[i],
                        &[1, 2],
                        120 + i as u64,
                        census.weights[i],
                        (1 << 33) + i as u64,
                    );
                    validate_vote(&c, verifier(), p).unwrap()
                } else {
                    census.vote(&c, i, &[1, 2], 120 + i as u64)
                }
            })
            .collect();
        let mut st = state(&c);
        let b = st.prepare(&votes, &mut rng, &none()).unwrap();
        emu::assert_accepted(&b, "untampered");
        type Tamper = fn(&mut davinci_zkvm_sdk::types::ProveRequest);
        let mut tampers: Vec<(&str, Tamper)> = vec![
            ("state.process_id", |r| {
                reverse_hex(&mut r.state.as_mut().unwrap().process_id)
            }),
            ("state.old_state_root", |r| {
                reverse_hex(&mut r.state.as_mut().unwrap().old_state_root)
            }),
            ("state.new_state_root", |r| {
                reverse_hex(&mut r.state.as_mut().unwrap().new_state_root)
            }),
            ("process_smt[0].new_value", |r| {
                reverse_hex(&mut r.state.as_mut().unwrap().process_smt[0].new_value)
            }),
            ("process_smt[2].new_value", |r| {
                reverse_hex(&mut r.state.as_mut().unwrap().process_smt[2].new_value)
            }),
            ("ballot_smt[0].new_value", |r| {
                reverse_hex(&mut r.state.as_mut().unwrap().ballot_smt[0].new_value)
            }),
            ("vote_id_smt[1], first non-zero sibling", |r| {
                let e = &mut r.state.as_mut().unwrap().vote_id_smt[1];
                let i = e
                    .siblings
                    .iter()
                    .position(|s| s.trim_start_matches("0x").bytes().any(|b| b != b'0'))
                    .unwrap();
                reverse_hex(&mut e.siblings[i]);
            }),
            ("voter_ballots[0][0]", |r| {
                reverse_hex(
                    &mut r
                        .state
                        .as_mut()
                        .unwrap()
                        .ballot_proofs
                        .as_mut()
                        .unwrap()
                        .voter_ballots[0][0],
                )
            }),
            ("reencryption.encryption_key_x", |r| {
                reverse_hex(&mut r.reencryption.as_mut().unwrap().encryption_key_x)
            }),
            ("reencryption.seed", |r| {
                reverse_hex(&mut r.reencryption.as_mut().unwrap().seed)
            }),
            ("reencryption original c1.x", |r| {
                reverse_hex(&mut r.reencryption.as_mut().unwrap().entries[0].original[0].c1.x)
            }),
            ("kzg.process_id", |r| {
                reverse_hex(&mut r.kzg.as_mut().unwrap().process_id)
            }),
            ("kzg.root_hash_before", |r| {
                reverse_hex(&mut r.kzg.as_mut().unwrap().root_hash_before)
            }),
            ("sigs[0].signature_r", |r| {
                reverse_hex(&mut r.sigs[0].signature_r)
            }),
        ];
        if csp_mode {
            tampers.push(("csp.voter_address", |r| {
                reverse_hex(&mut r.csp_data.as_mut().unwrap().proofs[0].voter_address)
            }));
            tampers.push(("csp.weight", |r| {
                reverse_hex(&mut r.csp_data.as_mut().unwrap().proofs[0].weight)
            }));
            tampers.push(("csp.r", |r| {
                reverse_hex(&mut r.csp_data.as_mut().unwrap().proofs[1].r)
            }));
        } else {
            tampers.push(("census.root", |r| {
                reverse_hex(&mut r.census_proofs.as_mut().unwrap()[0].root)
            }));
            tampers.push(("census.leaf", |r| {
                reverse_hex(&mut r.census_proofs.as_mut().unwrap()[0].leaf)
            }));
            tampers.push(("census.sibling", |r| {
                reverse_hex(&mut r.census_proofs.as_mut().unwrap()[1].siblings[0])
            }));
        }
        for (what, t) in tampers {
            let mut req = (*b.request).clone();
            t(&mut req);
            let v = emu::run(&req);
            assert!(!v.accepted(), "csp {csp_mode}: reversed {what} accepted");
        }
    }
}

// ------------------------------------------------------------- batch shapes

/// Batch sizes around every `log_n` step (register 45 is
/// `floor(log2 n)`), from a fresh state at 16 fields. The default runs
/// the first 14 sizes; `MATRIX_CASES` takes more of the list (512 votes
/// need about 75 s of emulator).
#[test]
fn batch_sizes_through_the_guest() {
    if !emu::gated() {
        return;
    }
    let sizes = [
        1usize, 2, 3, 4, 5, 7, 8, 9, 15, 16, 17, 31, 32, 33, 63, 64, 65, 127, 128, 129, 255, 256,
        257, 511, 512,
    ];
    let env = common::env(16, 512);
    let fields: Vec<u64> = (0..16).map(|i| i % 6).collect();
    // The proofs `dryrun.rs` caches for its 16-field batches.
    let all: Vec<VerifiedVote> = (0..*sizes.last().unwrap())
        .take(
            sizes[..emu::cases(14).min(sizes.len())]
                .iter()
                .copied()
                .max()
                .unwrap(),
        )
        .map(|i| common::real_vote(&env, i, &fields, 60 + i as u64))
        .collect();
    for &n in sizes.iter().take(emu::cases(14)) {
        let mut st = common::new_state(&env);
        st.set_blob_cap(davinci_zkvm_sdk::limits::MAX_BLOBS)
            .unwrap();
        let b = st
            .prepare(&all[..n], &mut StdRng::seed_from_u64(n as u64), &none())
            .unwrap();
        assert_eq!(b.expected.log_n, n.ilog2(), "n {n}");
        let t = std::time::Instant::now();
        emu::assert_accepted(&b, &format!("{n} votes"));
        eprintln!(
            "{n} votes, {} blobs: ok in {:.1}s",
            b.expected.n_blobs,
            t.elapsed().as_secs_f64()
        );
    }
}

/// `occupied` committed voters (fake proofs: that batch never reaches the
/// guest) at two fields.
fn occupied_state(env: &common::Env, occupied: usize) -> ProcessState<arbo::MemoryStorage> {
    let mut st = common::new_state(env);
    let mut rng = StdRng::seed_from_u64(occupied as u64);
    for chunk in (0..occupied).collect::<Vec<_>>().chunks(1024) {
        let votes: Vec<VerifiedVote> = chunk
            .iter()
            .map(|&i| common::fake_vote(env, i, &[1, 0], 5000 + i as u64))
            .collect();
        let b = st.prepare(&votes, &mut rng, &none()).unwrap();
        st.commit(&b).unwrap();
    }
    st
}

fn slot_of(i: usize) -> u64 {
    slot_key_address(&common::voter_address(i))
}

/// Refresh selections of every size the guest's rule (4.5.2) can ask for:
/// none (first batch, a lone revote), `occupied - w` below the target, the
/// target itself (`REFRESH_MIN`, `TAU * w`), a large exposed set above it,
/// and exactly `MAX_REFRESH`.
#[test]
fn refresh_selections_through_the_guest() {
    if !emu::gated() {
        return;
    }
    let env = common::env(2, 2100);
    // Real voters sit above the fake ones; revoters are fake-committed ones
    // re-proved for real.
    let new = |i: usize, k: u64| common::real_vote(&env, 2060 + i, &[2, 1], k);
    let re = |i: usize, k: u64| common::real_vote(&env, i, &[0, 2], k);
    // (occupied, new votes, revotes, exposed live slots, expected refreshes)
    let cases: Vec<(usize, usize, usize, usize, usize)> = vec![
        (0, 1, 0, 0, 0),
        (1, 0, 1, 0, 0),
        (1, 1, 0, 0, 1),
        (15, 1, 0, 0, 15),
        (16, 1, 0, 0, 16),
        (17, 1, 0, 0, 16),
        (17, 1, 1, 0, 16),
        (20, 2, 10, 0, 10), // occupied - w = 10 < target 20
        (40, 0, 10, 0, 20), // TAU * w = 20
        (40, 3, 10, 0, 20),
        (100, 1, 0, 60, 60),
        (2049, 1, 0, 2048, 2048),
    ];
    for (case, &(occ, n_new, n_re, exposed, want)) in cases.iter().enumerate() {
        if case >= emu::cases(cases.len()) {
            break;
        }
        let mut st = occupied_state(&env, occ);
        let mut votes: Vec<VerifiedVote> = (0..n_re).map(|i| re(i, 700 + i as u64)).collect();
        votes.extend((0..n_new).map(|i| new(i, 800 + i as u64)));
        // Exposed: live slots not written by the batch.
        let must: BTreeSet<u64> = (n_re..occ).map(slot_of).take(exposed).collect();
        if exposed > 0 {
            st.set_blob_cap(davinci_zkvm_sdk::limits::MAX_BLOBS)
                .unwrap();
        }
        let b = st
            .prepare(&votes, &mut StdRng::seed_from_u64(case as u64), &must)
            .unwrap();
        let ctx = format!("occ {occ} new {n_new} re {n_re} exposed {exposed}");
        assert_eq!(b.refresh_slots().len(), want, "{ctx}");
        assert_eq!(b.expected.overwrites as usize, n_re, "{ctx}");
        assert_eq!(b.expected.occupied_before as usize, occ, "{ctx}");
        let t = std::time::Instant::now();
        emu::assert_accepted(&b, &ctx);
        eprintln!(
            "{ctx}: {want} refreshes, {} blobs, ok in {:.1}s",
            b.expected.n_blobs,
            t.elapsed().as_secs_f64()
        );
    }
}

/// Revotes inside one queue: `select_batch` seals one vote per slot (the
/// guest's 4.1.7), the later revote waits and lands as an overwrite next
/// batch, and a third revote after that as another. `prepare` refuses a
/// batch writing one slot twice outright.
#[test]
fn revotes_inside_one_queue_through_the_guest() {
    if !emu::gated() {
        return;
    }
    let mut rng = StdRng::seed_from_u64(0x7e7);
    let census = Census::new(600_000, &[1, 1, 1]);
    let c = cfg(
        pid(&mut rng, 0x42),
        common::ballot_mode(5),
        enc_key(&mut rng, false),
        CensusOrigin::MerkleStatic,
        census.imt.root(),
    );
    let queue = vec![
        census.vote(&c, 0, &[1, 0, 0, 0, 0], 1),
        census.vote(&c, 1, &[0, 1, 0, 0, 0], 2),
        census.vote(&c, 0, &[0, 0, 1, 0, 0], 3),
        census.vote(&c, 0, &[0, 0, 0, 1, 0], 4),
        census.vote(&c, 2, &[0, 0, 0, 0, 1], 5),
    ];
    let mut st = state(&c);
    assert!(st.prepare(&queue[..3], &mut rng, &none()).is_err());
    let mut pending = queue.clone();
    let mut overwrites = Vec::new();
    while !pending.is_empty() {
        let (sel, _) = st
            .select_batch(&pending, u64::MAX, &none(), usize::MAX)
            .unwrap();
        let sel: Vec<VerifiedVote> = sel.into_iter().cloned().collect();
        let slots: BTreeSet<u64> = sel.iter().map(|v| v.slot).collect();
        assert_eq!(slots.len(), sel.len());
        let b = st.prepare(&sel, &mut rng, &none()).unwrap();
        emu::assert_accepted(&b, &format!("batch {}", overwrites.len()));
        overwrites.push(b.expected.overwrites);
        st.commit(&b).unwrap();
        let done: BTreeSet<u64> = sel.iter().map(|v| v.pkg.vote_id).collect();
        pending.retain(|v| !done.contains(&v.pkg.vote_id));
    }
    assert_eq!(overwrites, vec![0, 1, 1]);
    assert_eq!(st.occupied(), 3);
}

// ---------------------------------------------------------------- origins

/// Every census origin, two batches each with a revote across them, at an
/// asymmetric process id just under 2^248 and an election key whose
/// coordinates are both at or above 2^248. Dynamic origins (2, 3) re-root
/// between the batches: the census grows and the second batch's witnesses
/// are re-proved at the new root, which register 20 must then carry.
#[test]
fn census_origins_through_the_guest() {
    if !emu::gated() {
        return;
    }
    let mut rng = StdRng::seed_from_u64(emu::seed(0x0619));
    let csp = emu::key(88_888);
    for origin in [
        CensusOrigin::MerkleStatic,
        CensusOrigin::MerkleOffchainDynamic,
        CensusOrigin::MerkleOnchainDynamic,
        CensusOrigin::Csp,
    ] {
        let mode = common::ballot_mode(4);
        let small = Census::new(90_000, &[1, 2, 3, 4]);
        let grown = Census::new(90_000, &[1, 2, 3, 4, 5, 6]);
        let root = if origin == CensusOrigin::Csp {
            csp_root(&csp)
        } else {
            small.imt.root()
        };
        let c = cfg(
            pid(&mut rng, 0xff),
            mode,
            enc_key(&mut rng, true),
            origin,
            root,
        );
        let mut st = state(&c);
        let vote = |c: &ProcessConfig, census: &Census, i: usize, f: &[u64], k: u64| {
            if origin == CensusOrigin::Csp {
                let p = csp_vote(
                    c,
                    &csp,
                    &census.keys[i],
                    f,
                    k,
                    census.weights[i],
                    3 * i as u64,
                );
                let v = validate_vote(c, verifier(), p).unwrap();
                assert_eq!(v.slot, BALLOT_MIN + 3 * i as u64);
                v
            } else {
                census.vote(c, i, f, k)
            }
        };
        let v1: Vec<VerifiedVote> = (0..3)
            .map(|i| vote(&c, &small, i, &[1, 2, 3, 4], 100 + i as u64))
            .collect();
        let b = st.prepare(&v1, &mut rng, &none()).unwrap();
        let reg20 = emu::assert_accepted(&b, &format!("{origin:?} batch 1")).census_root;
        assert_eq!(
            reg20,
            le(&root),
            "{origin:?}: register 20 is the LE census root"
        );
        st.commit(&b).unwrap();

        let dynamic = matches!(
            origin,
            CensusOrigin::MerkleOffchainDynamic | CensusOrigin::MerkleOnchainDynamic
        );
        let (c2, census2) = if dynamic {
            st.set_census_root(grown.imt.root());
            (
                ProcessConfig {
                    census_root: grown.imt.root(),
                    ..c.clone()
                },
                &grown,
            )
        } else {
            (c.clone(), &small)
        };
        let fresh = if dynamic { 5 } else { 3 };
        let v2 = vec![
            vote(&c2, census2, 0, &[4, 3, 2, 1], 110),
            vote(&c2, census2, fresh, &[0, 0, 0, 5], 111),
        ];
        let b = st.prepare(&v2, &mut rng, &none()).unwrap();
        assert_eq!(b.expected.overwrites, 1);
        let reg20 = emu::assert_accepted(&b, &format!("{origin:?} batch 2")).census_root;
        assert_eq!(reg20, le(&c2.census_root), "{origin:?} batch 2 register 20");

        if dynamic {
            // A witness left at the old root: the guest refuses the mix
            // (patched into the request), so `prepare` refuses it first.
            st.rollback(&b).unwrap();
            let mut stale = v2.clone();
            stale[0].pkg.census = small.witness(0);
            assert!(st.prepare(&stale, &mut rng, &none()).is_err());
            let b = st.prepare(&v2, &mut rng, &none()).unwrap();
            let mut req = (*b.request).clone();
            let p = small.imt.proof(0).unwrap();
            req.census_proofs.as_mut().unwrap()[0] = CensusProofJson {
                root: enc::be_hex(&p.root),
                leaf: enc::be_hex(&p.leaf),
                index: p.path_bits,
                siblings: p.siblings.iter().map(enc::be_hex).collect(),
            };
            let v = emu::run(&req);
            assert_eq!(
                v.mask(),
                Some(emu::FAIL_CENSUS),
                "{origin:?} mixed roots: {v:?}"
            );
        }
        eprintln!("{origin:?}: ok");
    }
}

// ---------------------------------------------------------- batch sequences

/// Seeded sequences of batches over a small electorate: random field
/// counts and census origin, 1..=4 votes a batch drawn with revotes, and
/// now and then a seal that is rolled back (its blobs may be public) and
/// re-sealed with its slots as the exposed set, as the actor does after a
/// lost race. Every batch must pass the guest with the node's registers,
/// and a follower replaying only the blobs must reach the same record.
#[test]
fn batch_sequences_through_the_guest() {
    if !emu::gated() {
        return;
    }
    let mut rng = StdRng::seed_from_u64(emu::seed(0x5e9));
    for case in 0..emu::cases(4) {
        let nf = rng.gen_range(1..=16u8);
        let mode = common::ballot_mode(nf);
        let csp_mode = case % 2 == 1;
        let census = Census::new(200_000 + 100 * case as u64, &[1, 1, 2, 3, 5, 8, 13, 21]);
        let csp = emu::key(300_000 + case as u64);
        let (origin, root) = if csp_mode {
            (CensusOrigin::Csp, csp_root(&csp))
        } else {
            (CensusOrigin::MerkleStatic, census.imt.root())
        };
        let top: u8 = rng.r#gen();
        let c = cfg(
            pid(&mut rng, top),
            mode,
            enc_key(&mut rng, false),
            origin,
            root,
        );
        let mut st = state(&c);
        // A follower that only sees the blobs (another sequencer's batches).
        let mut follower = state(&c);
        let mut k = 1000 * case as u64;
        for batch in 0..4 {
            let n = rng.gen_range(1..=4usize);
            let mut who: Vec<usize> = (0..8).collect();
            for i in 0..n {
                let j = rng.gen_range(i..8);
                who.swap(i, j);
            }
            let votes: Vec<VerifiedVote> = who[..n]
                .iter()
                .map(|&i| {
                    k += 1;
                    let f: Vec<u64> = (0..nf).map(|_| rng.gen_range(0..=5)).collect();
                    // ballot_mode caps the sum at 5 * nf: always in mode.
                    if csp_mode {
                        let p = csp_vote(
                            &c,
                            &csp,
                            &census.keys[i],
                            &f,
                            k,
                            census.weights[i],
                            i as u64,
                        );
                        validate_vote(&c, verifier(), p).unwrap()
                    } else {
                        census.vote(&c, i, &f, k)
                    }
                })
                .collect();
            let ctx = format!("case {case} nf {nf} csp {csp_mode} batch {batch} n {n}");
            let mut b = st.prepare(&votes, &mut rng, &none()).unwrap();
            if rng.gen_bool(0.3) {
                // Lost the race after the blobs went out: re-seal keeping
                // every slot those blobs listed.
                let exposed: BTreeSet<u64> = b.updated_slots().into_iter().collect();
                st.rollback(&b).unwrap();
                b = st.prepare(&votes, &mut rng, &exposed).unwrap();
            }
            emu::assert_accepted(&b, &ctx);
            st.commit(&b).unwrap();
            let t = davinci_zkvm_sdk::blob::decode_blobs(&b.blobs.blobs, nf).unwrap();
            follower.apply_synced(&t, b.new_root).unwrap();
            assert_eq!(follower.committed(), st.committed(), "{ctx}: follower");
        }
        eprintln!("case {case} nf {nf} csp {csp_mode}: ok");
    }
}

/// The most blobs a transition reaches with the cached 16-field proofs:
/// 512 new votes next to 2048 refreshes, T = 1 + 640 + 32 * 2560 = 82,561
/// cells in 21 blobs. Several minutes of emulator: only with
/// `MATRIX_CASES` of 32 or more.
#[test]
fn twenty_one_blob_transition_through_the_guest() {
    if !emu::gated() || emu::cases(0) < 32 {
        eprintln!("skipped: MATRIX_CASES>=32 runs the 21-blob transition");
        return;
    }
    let env = common::env(16, 2600);
    let fields: Vec<u64> = (0..16).map(|i| i % 6).collect();
    let mut st = common::new_state(&env);
    let mut rng = StdRng::seed_from_u64(0x21);
    for chunk in (512..2560).collect::<Vec<_>>().chunks(1024) {
        let votes: Vec<VerifiedVote> = chunk
            .iter()
            .map(|&i| common::fake_vote(&env, i, &fields, 9000 + i as u64))
            .collect();
        let b = st.prepare(&votes, &mut rng, &none()).unwrap();
        st.commit(&b).unwrap();
    }
    st.set_blob_cap(davinci_zkvm_sdk::limits::MAX_BLOBS)
        .unwrap();
    let votes: Vec<VerifiedVote> = (0..512)
        .map(|i| common::real_vote(&env, i, &fields, 60 + i as u64))
        .collect();
    let must: BTreeSet<u64> = (512..2560)
        .map(|i| slot_key_address(&common::voter_address(i)))
        .collect();
    let b = st.prepare(&votes, &mut rng, &must).unwrap();
    assert_eq!(b.refresh_slots().len(), 2048);
    assert_eq!(b.expected.n_blobs, 21);
    // `EMU_INPUT_OUT=<file>` keeps the framed input for a `ziskemu -X` cost run.
    if let Some(path) = std::env::var_os("EMU_INPUT_OUT") {
        emu::write_framed(
            std::path::Path::new(&path),
            &emu::input_bytes(&b.request).unwrap(),
        );
    }
    let t = std::time::Instant::now();
    emu::assert_accepted(&b, "512 votes + 2048 refreshes");
    eprintln!("21 blobs: ok in {:.1}s", t.elapsed().as_secs_f64());
}
