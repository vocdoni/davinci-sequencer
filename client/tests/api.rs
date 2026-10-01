//! Wire types: JSON shape, strict decoding, tracker proofs.

use arbo::{MemoryStorage, Sha256, Tree};
use davinci_client::Error;
use davinci_client::api::{
    self, CensusFile, CensusParticipant, CensusProofWire, CspWire, EncryptionKeyResponse, Info,
    MerkleProof, ProcessId, ProcessStatus, ProcessView, TrackerProof, VoteRequest, VoteStatus,
    VoteStatusResponse,
};
use davinci_client::organizer::OnchainProcess;
use davinci_zkvm_sdk::ballot::{Ballot, BallotMode};
use davinci_zkvm_sdk::census::{CensusWitness, LeanImt, census_leaf};
use davinci_zkvm_sdk::crypto::babyjubjub::Point;
use davinci_zkvm_sdk::crypto::elgamal::{Ciphertext, encrypt};
use davinci_zkvm_sdk::crypto::field::{Fr, U256, fr_to_dec};
use davinci_zkvm_sdk::types::SnarkJsProof;
use serde_json::{Value, json};

const P: &str = "21888242871839275222246405745257275088548364400416034343698204186575808495617";

fn mode(nf: u8) -> BallotMode {
    BallotMode {
        num_fields: nf,
        group_size: 1,
        unique_values: false,
        cost_exponent: 1,
        max_value: 5,
        min_value: 0,
        max_value_sum: 1 << 62,
        min_value_sum: 0,
    }
}

fn pk() -> Point {
    Point::generator().mul(&U256::from(12345u64))
}

fn sample_ballot() -> Ballot {
    let mut b = Ballot::identity();
    b.0[0] = encrypt(&pk(), 3, &U256::from(77u64));
    b.0[1] = encrypt(&pk(), 1, &U256::from(78u64));
    b
}

fn sample_proof() -> SnarkJsProof {
    SnarkJsProof {
        pi_a: ["1".into(), "2".into(), "1".into()],
        pi_b: [
            ["1".into(), "2".into()],
            ["3".into(), "4".into()],
            ["1".into(), "0".into()],
        ],
        pi_c: ["5".into(), "6".into(), "1".into()],
        protocol: "groth16".into(),
        curve: "bn128".into(),
    }
}

fn sample_vote(census: CensusProofWire) -> VoteRequest {
    VoteRequest {
        process_id: ProcessId([0xab; 31]),
        address: [0x11; 20],
        vote_id: 0x8000_0000_0000_1234,
        ballot: sample_ballot(),
        ballot_proof: sample_proof(),
        ballot_inputs_hash: Fr::from(999u64),
        signature: [7u8; 65],
        weight: 42,
        census_proof: Some(census),
    }
}

fn merkle_wire() -> CensusProofWire {
    let mut t = LeanImt::new();
    for i in 0..5u8 {
        t.insert(census_leaf(&[i; 20], 1).unwrap());
    }
    CensusProofWire::Merkle(MerkleProof::from(&t.proof(3).unwrap()))
}

#[test]
fn vote_request_json_shape_and_roundtrip() {
    let v = sample_vote(merkle_wire());
    let j = serde_json::to_value(&v).unwrap();
    let keys: Vec<&str> = j.as_object().unwrap().keys().map(String::as_str).collect();
    for k in [
        "processId",
        "address",
        "voteId",
        "ballot",
        "ballotProof",
        "ballotInputsHash",
        "signature",
        "weight",
        "censusProof",
    ] {
        assert!(keys.contains(&k), "missing {k}: {keys:?}");
    }
    assert_eq!(keys.len(), 9);
    assert_eq!(j["processId"], json!(format!("0x{}", "ab".repeat(31))));
    assert_eq!(j["address"], json!(format!("0x{}", "11".repeat(20))));
    assert_eq!(j["voteId"], json!("0x8000000000001234"));
    assert_eq!(j["ballotInputsHash"], json!("999"));
    assert_eq!(j["weight"], json!("42"));
    assert_eq!(j["signature"].as_str().unwrap().len(), 2 + 130);
    // 16 ciphertexts of decimal TE coordinates.
    let ballot = j["ballot"].as_array().unwrap();
    assert_eq!(ballot.len(), 16);
    assert_eq!(
        ballot[0]["c1"]["x"],
        json!(fr_to_dec(&sample_ballot().0[0].c1.x))
    );
    assert_eq!(
        ballot[15],
        json!({"c1": {"x": "0", "y": "1"}, "c2": {"x": "0", "y": "1"}})
    );
    assert_eq!(j["ballotProof"]["protocol"], json!("groth16"));
    assert_eq!(j["censusProof"]["type"], json!("merkle"));
    assert!(j["censusProof"]["siblings"].is_array());

    let back: VoteRequest = serde_json::from_value(j).unwrap();
    assert_eq!(back, v);
}

#[test]
fn vote_request_csp_roundtrip_and_witness() {
    let csp = CensusProofWire::Csp(CspWire {
        r: [1; 32],
        s: [2; 32],
        recid: 1,
        index: 9,
    });
    let v = sample_vote(csp);
    let j = serde_json::to_value(&v).unwrap();
    assert_eq!(j["censusProof"]["type"], json!("csp"));
    assert_eq!(j["censusProof"]["index"], json!(9));
    let back: VoteRequest = serde_json::from_value(j).unwrap();
    assert_eq!(back, v);
    // The CSP attestation covers the vote's own address and weight.
    match v.census_witness().unwrap() {
        CensusWitness::Csp(p) => {
            assert_eq!(p.address, v.address);
            assert_eq!(p.weight, 42);
            assert_eq!(p.index, 9);
            assert_eq!(p.recid, 1);
        }
        CensusWitness::Merkle(_) => panic!("want csp"),
    }
    let sig = v.signature();
    assert_eq!((sig.r, sig.s, sig.v), ([7; 32], [7; 32], 7));
}

// Decoding must reject anything that is not the canonical encoding.
#[test]
fn vote_request_rejects_bad_encodings() {
    let good = serde_json::to_value(sample_vote(merkle_wire())).unwrap();
    let bad = |f: &dyn Fn(&mut Value)| {
        let mut j = good.clone();
        f(&mut j);
        serde_json::from_value::<VoteRequest>(j).is_err()
    };
    assert!(!bad(&|_| {}));
    assert!(bad(&|j| j["ballotInputsHash"] = json!(P)), "Fr = p");
    assert!(bad(&|j| j["ballotInputsHash"] = json!("0x10")), "hex Fr");
    assert!(bad(&|j| j["ballotInputsHash"] = json!("-1")));
    assert!(bad(&|j| j["ballotInputsHash"] = json!(5)), "number Fr");
    assert!(
        bad(&|j| j["ballot"][0]["c1"]["x"] = json!("5")),
        "off curve"
    );
    assert!(bad(&|j| j["ballot"][3]["c2"]["y"] = json!(P)), "coord = p");
    assert!(bad(&|j| {
        j["ballot"].as_array_mut().unwrap().pop();
    }));
    assert!(bad(&|j| {
        let c = j["ballot"][0].clone();
        j["ballot"].as_array_mut().unwrap().push(c);
    }));
    assert!(bad(&|j| j["voteId"] = json!("0x80000000000012")), "7 bytes");
    assert!(
        bad(&|j| j["voteId"] = json!("0x7fffffffffffffff")),
        "below 2^63"
    );
    assert!(
        bad(&|j| j["voteId"] = json!("0x0000000000001234")),
        "below 2^63"
    );
    assert!(bad(&|j| j["voteId"] = json!(9223372036854780468u64)));
    assert!(bad(
        &|j| j["processId"] = json!(format!("0x{}", "ab".repeat(32)))
    ));
    assert!(bad(
        &|j| j["address"] = json!(format!("0x{}", "11".repeat(19)))
    ));
    assert!(bad(
        &|j| j["address"] = json!(format!("0x{}", "zz".repeat(20)))
    ));
    assert!(bad(
        &|j| j["signature"] = json!(format!("0x{}", "07".repeat(64)))
    ));
    assert!(bad(
        &|j| j["weight"] = json!("340282366920938463463374607431768211456")
    ));
    assert!(bad(&|j| j["weight"] = json!(42)), "number weight");
    assert!(bad(&|j| j["censusProof"]["type"] = json!("dynamic")));
    assert!(bad(&|j| j["censusProof"]["root"] = json!(P)));
    assert!(bad(&|j| j["extra"] = json!(1)), "unknown field");
}

#[test]
fn ids_parse_and_print() {
    let pid = ProcessId([0x01; 31]);
    let s = pid.to_string();
    assert_eq!(s, format!("0x{}", "01".repeat(31)));
    assert_eq!(s.parse::<ProcessId>().unwrap(), pid);
    assert_eq!(s[2..].parse::<ProcessId>().unwrap(), pid, "0x optional");
    assert!("0x01".parse::<ProcessId>().is_err());
    // The pid as a field element is its big-endian integer.
    let mut be = [0u8; 32];
    be[1..].copy_from_slice(&pid.0);
    assert_eq!(pid.to_fr(), Fr::from_be_bytes_mod_order(&be));

    assert_eq!(
        api::vote_id_hex(0x8000_0000_0000_00ff),
        "0x80000000000000ff"
    );
    assert_eq!(
        api::parse_vote_id("0x80000000000000ff").unwrap(),
        0x8000_0000_0000_00ff
    );
    assert_eq!(
        api::parse_vote_id("80000000000000ff").unwrap(),
        0x8000_0000_0000_00ff
    );
    assert!(api::parse_vote_id("0x800000000000000").is_err());
    // Vote ids live at or above 2^63.
    assert_eq!(api::parse_vote_id("0x8000000000000000").unwrap(), 1 << 63);
    assert!(matches!(
        api::parse_vote_id("0x7fffffffffffffff"),
        Err(Error::Invalid(_))
    ));
    assert!(matches!(
        api::parse_vote_id("0x0000000000000010"),
        Err(Error::Invalid(_))
    ));
    assert!(api::parse_vote_id("0x80000000000000ff00").is_err());
}

use ark_ff::PrimeField;

#[test]
fn vote_status_strings() {
    for (s, v) in [
        ("pending", VoteStatus::Pending),
        ("aggregated", VoteStatus::Aggregated),
        ("processed", VoteStatus::Processed),
        ("settled", VoteStatus::Settled),
        ("error", VoteStatus::Error),
    ] {
        assert_eq!(v.to_string(), s);
        assert_eq!(s.parse::<VoteStatus>().unwrap(), v);
        assert_eq!(serde_json::to_value(v).unwrap(), json!(s));
    }
    assert!("verified".parse::<VoteStatus>().is_err());
    let r: VoteStatusResponse =
        serde_json::from_value(json!({"status": "error", "error": "process closed"})).unwrap();
    assert_eq!(r.status, VoteStatus::Error);
    assert_eq!(r.error.as_deref(), Some("process closed"));
    let ok = serde_json::to_value(VoteStatusResponse {
        status: VoteStatus::Settled,
        error: None,
    })
    .unwrap();
    assert_eq!(ok, json!({"status": "settled"}));
    assert!(serde_json::from_value::<VoteStatusResponse>(json!({"status": "done"})).is_err());
}

fn view(m: BallotMode) -> ProcessView {
    ProcessView {
        id: ProcessId([3; 31]),
        status: ProcessStatus::Ready,
        is_accepting_votes: true,
        organization_id: [9; 20],
        encryption_key: pk(),
        ballot_mode: m,
        census: api::CensusView {
            census_origin: 1,
            census_root: Fr::from(5u64),
            census_uri: "file:///tmp/census.json".into(),
        },
        state_root: [4; 32],
        local_state_root: None,
        synced: false,
        pending_votes: None,
        next_seal_not_before: None,
        voters_count: 2,
        overwritten_votes_count: 1,
        max_voters: 100,
        start_time: 1_700_000_000,
        duration: 7200,
        result: None,
        ignored: false,
        note: None,
    }
}

#[test]
fn process_view_roundtrip_and_ballot_mode_checked() {
    let v = view(mode(4));
    let j = serde_json::to_value(&v).unwrap();
    assert_eq!(j["status"], json!("ready"));
    assert_eq!(j["isAcceptingVotes"], json!(true));
    assert_eq!(j["ballotMode"]["numFields"], json!(4));
    assert_eq!(
        j["ballotMode"]["maxValueSum"],
        json!((1u64 << 62).to_string())
    );
    assert_eq!(j["census"]["censusOrigin"], json!(1));
    assert_eq!(j["census"]["censusURI"], json!("file:///tmp/census.json"));
    assert_eq!(j["encryptionKey"]["x"], json!(fr_to_dec(&pk().x)));
    assert_eq!(j["stateRoot"], json!(format!("0x{}", "04".repeat(32))));
    assert!(j.get("result").is_none());
    assert_eq!(serde_json::from_value::<ProcessView>(j.clone()).unwrap(), v);

    let mut bad = j.clone();
    bad["ballotMode"]["groupSize"] = json!(5);
    assert!(
        serde_json::from_value::<ProcessView>(bad).is_err(),
        "groupSize > numFields"
    );
    let mut bad = j.clone();
    bad["ballotMode"]["maxValue"] = json!((1u64 << 48).to_string());
    assert!(
        serde_json::from_value::<ProcessView>(bad).is_err(),
        "maxValue >= 2^48"
    );
    let mut bad = j.clone();
    bad["encryptionKey"]["y"] = json!("3");
    assert!(
        serde_json::from_value::<ProcessView>(bad).is_err(),
        "key off curve"
    );
    let mut bad = j;
    bad["status"] = json!("finished");
    assert!(serde_json::from_value::<ProcessView>(bad).is_err());

    let mut r = view(mode(2));
    r.status = ProcessStatus::Results;
    r.result = Some(vec![3, 0]);
    let j = serde_json::to_value(&r).unwrap();
    assert_eq!(j["result"], json!([3, 0]));
    assert_eq!(serde_json::from_value::<ProcessView>(j).unwrap(), r);
}

#[test]
fn info_and_census_file_roundtrip() {
    let i = Info {
        sequencer_address: Some([1; 20]),
        chain_id: 31337,
        process_registry: [2; 20],
        ballot_vk_hash: [3; 32],
        batch_program_vk: [4; 32],
        results_program_vk: [5; 32],
        observer: false,
        settled_by_self: 7,
        synced_from_others: 8,
        lost_races: 3,
    };
    let j = serde_json::to_value(&i).unwrap();
    assert_eq!(j["chainId"], json!(31337));
    assert_eq!(j["ballotVkHash"], json!(format!("0x{}", "03".repeat(32))));
    assert_eq!(serde_json::from_value::<Info>(j).unwrap(), i);
    let obs = Info {
        sequencer_address: None,
        observer: true,
        ..i
    };
    let j = serde_json::to_value(&obs).unwrap();
    assert_eq!(j["sequencerAddress"], Value::Null);
    assert_eq!(serde_json::from_value::<Info>(j).unwrap(), obs);

    // davinci-node census format: {participants:[{key, weight}]}.
    let c = CensusFile {
        participants: vec![CensusParticipant {
            key: [0xaa; 20],
            weight: 3,
        }],
    };
    let j = serde_json::to_value(&c).unwrap();
    assert_eq!(
        j,
        json!({"participants": [{"key": format!("0x{}", "aa".repeat(20)), "weight": "3"}]})
    );
    assert_eq!(serde_json::from_value::<CensusFile>(j).unwrap(), c);
}

#[test]
fn merkle_wire_converts_both_ways() {
    let mut t = LeanImt::new();
    for i in 0..7u8 {
        t.insert(census_leaf(&[i; 20], i as u128 + 1).unwrap());
    }
    let p = t.proof(6).unwrap();
    let w = MerkleProof::from(&p);
    assert_eq!(w.to_census_proof(), p);
    let j = serde_json::to_value(&w).unwrap();
    assert_eq!(j["pathBits"], json!(p.path_bits));
    assert_eq!(serde_json::from_value::<MerkleProof>(j).unwrap(), w);
}

fn vid_key(vid: u64) -> [u8; 8] {
    vid.to_le_bytes()
}

// A tracker proof is an arbo inclusion proof of the vote-id leaf (value 0).
#[test]
fn tracker_proofs_verify_against_the_root() {
    let mut tree = Tree::new(MemoryStorage::new(), 64, Sha256).unwrap();
    // Config and ballot leaves share the tree with the vote ids.
    for k in [0u64, 2, 3, 4, 6, 7, 0x10, 0x11, 0x25] {
        tree.add(&k.to_le_bytes(), &[k as u8 + 1; 32]).unwrap();
    }
    let vids: Vec<u64> = (0..40u64)
        .map(|i| (1 << 63) | i.wrapping_mul(0x9e37_79b9_7f4a_7c15) >> 1)
        .collect();
    for v in &vids {
        tree.add(&vid_key(*v), &[0u8; 32]).unwrap();
    }
    let root = tree.root();
    let pid = ProcessId([1; 31]);
    for v in &vids {
        let p = tree.gen_proof(&vid_key(*v)).unwrap();
        assert!(p.exists);
        let tp = TrackerProof {
            process_id: pid,
            vote_id: *v,
            root,
            siblings: p.siblings.clone(),
        };
        assert!(api::verify_tracker(&tp, &root), "vid {v:x}");
        let j = serde_json::to_value(&tp).unwrap();
        assert_eq!(serde_json::from_value::<TrackerProof>(j).unwrap(), tp);

        // Every tamper must fail.
        let mut other = root;
        other[0] ^= 1;
        assert!(!api::verify_tracker(&tp, &other), "other on-chain root");
        let mut t2 = tp.clone();
        t2.root = other;
        assert!(!api::verify_tracker(&t2, &other), "proof for another root");
        let mut t3 = tp.clone();
        t3.vote_id ^= 1;
        assert!(!api::verify_tracker(&t3, &root), "other vote id");
        if let Some(i) = tp.siblings.iter().position(|s| *s != [0u8; 32]) {
            let mut t4 = tp.clone();
            t4.siblings[i][5] ^= 0x40;
            assert!(!api::verify_tracker(&t4, &root), "tampered sibling");
        }
        let mut t5 = tp.clone();
        t5.siblings.push([0u8; 32]);
        assert!(!api::verify_tracker(&t5, &root), "extra level");
        let mut t6 = tp.clone();
        t6.siblings = vec![[0u8; 32]; 65];
        assert!(!api::verify_tracker(&t6, &root), "65 levels");
    }
    // A ballot leaf (non-zero value) is not a recorded vote id.
    let p = tree.gen_proof(&0x10u64.to_le_bytes()).unwrap();
    let tp = TrackerProof {
        process_id: pid,
        vote_id: 0x10,
        root,
        siblings: p.siblings,
    };
    assert!(!api::verify_tracker(&tp, &root));
    // An absent vote id has no inclusion proof.
    let absent = (1u64 << 63) | 12345;
    let p = tree.gen_proof(&vid_key(absent)).unwrap();
    assert!(!p.exists);
    let tp = TrackerProof {
        process_id: pid,
        vote_id: absent,
        root,
        siblings: p.siblings,
    };
    assert!(!api::verify_tracker(&tp, &root));
}

#[test]
fn padded_identity_is_visible_on_the_wire() {
    let b = sample_ballot();
    assert!(b.is_padded_ok(2));
    assert_eq!(b.0[2], Ciphertext::IDENTITY);
}

// Merkle proofs are optional: the sequencer re-derives them from its tree.
#[test]
fn census_proof_is_optional() {
    let mut v = sample_vote(merkle_wire());
    v.census_proof = None;
    let j = serde_json::to_value(&v).unwrap();
    assert!(j.get("censusProof").is_none(), "omitted when absent");
    assert_eq!(j.as_object().unwrap().len(), 8);
    let back: VoteRequest = serde_json::from_value(j).unwrap();
    assert_eq!(back, v);
    assert_eq!(back.census_witness(), None);
    let mut j = serde_json::to_value(&v).unwrap();
    j["censusProof"] = Value::Null;
    assert_eq!(serde_json::from_value::<VoteRequest>(j).unwrap(), v);
}

#[test]
fn encryption_key_response() {
    let r = EncryptionKeyResponse::from_point(&pk());
    let j = serde_json::to_value(&r).unwrap();
    assert_eq!(j, json!({"x": fr_to_dec(&pk().x), "y": fr_to_dec(&pk().y)}));
    let back: EncryptionKeyResponse = serde_json::from_value(j).unwrap();
    assert_eq!(back.point().unwrap(), pk());
    let off: EncryptionKeyResponse = serde_json::from_value(json!({"x": "1", "y": "2"})).unwrap();
    assert!(matches!(off.point(), Err(Error::Decode(_))));
    assert!(serde_json::from_value::<EncryptionKeyResponse>(json!({"x": P, "y": "1"})).is_err());
}

fn onchain(v: &ProcessView) -> OnchainProcess {
    OnchainProcess {
        id: v.id.0,
        status: v.status,
        organization_id: v.organization_id,
        encryption_key: v.encryption_key,
        state_root: v.state_root,
        result: vec![],
        start_time: v.start_time,
        duration: v.duration,
        max_voters: v.max_voters,
        voters_count: v.voters_count,
        overwritten_votes_count: v.overwritten_votes_count,
        ballot_mode: v.ballot_mode,
        census_origin: v.census.census_origin,
        census_root: v.census.census_root,
        census_contract: [0; 20],
        census_uri: v.census.census_uri.clone(),
        metadata_uri: String::new(),
        metadata_hash: [0; 32],
        dkg: None,
        grace: 0,
        last_vote_at: 0,
    }
}

// A sequencer's view is only trusted where it matches the registry.
#[test]
fn process_view_is_checked_against_the_registry() {
    let v = view(mode(4));
    let chain = onchain(&v);
    v.check_against(&chain).unwrap();

    // Counters, status and the root may lag the chain; that is not a lie.
    let mut lagging = v.clone();
    lagging.state_root = [0; 32];
    lagging.voters_count = 0;
    lagging.status = ProcessStatus::Ended;
    lagging.check_against(&chain).unwrap();

    type Tamper = Box<dyn Fn(&mut ProcessView)>;
    let tampered: Vec<(&str, Tamper)> = vec![
        ("pid", Box::new(|v| v.id.0[30] ^= 1)),
        (
            "key",
            Box::new(|v| v.encryption_key = Point::generator().mul(&U256::from(7u64))),
        ),
        ("mode", Box::new(|v| v.ballot_mode.max_value += 1)),
        ("num fields", Box::new(|v| v.ballot_mode.num_fields = 3)),
        (
            "census root",
            Box::new(|v| v.census.census_root += Fr::from(1u64)),
        ),
        ("census origin", Box::new(|v| v.census.census_origin = 4)),
    ];
    for (what, f) in tampered {
        let mut bad = v.clone();
        f(&mut bad);
        match bad.check_against(&chain) {
            Err(Error::Decode(m)) => assert!(m.contains("registry"), "{what}: {m}"),
            other => panic!("{what}: want a mismatch, got {other:?}"),
        }
    }
}
