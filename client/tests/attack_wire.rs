//! Hostile decoding of the wire types: what `POST /votes` and
//! `POST /processes/keys` bodies may and may not look like.

use davinci_client::api::{
    CensusProofWire, CspWire, EncryptionKeyRequest, EncryptionKeyResponse, MerkleProof, ProcessId,
    VoteRequest, parse_vote_id,
};
use davinci_zkvm_sdk::ballot::Ballot;
use davinci_zkvm_sdk::census::{LeanImt, census_leaf};
use davinci_zkvm_sdk::crypto::babyjubjub::Point;
use davinci_zkvm_sdk::crypto::elgamal::encrypt;
use davinci_zkvm_sdk::crypto::field::{Fr, U256};
use davinci_zkvm_sdk::types::SnarkJsProof;
use serde_json::{Value, json};

const P: &str = "21888242871839275222246405745257275088548364400416034343698204186575808495617";
const P_MINUS_1: &str =
    "21888242871839275222246405745257275088548364400416034343698204186575808495616";
const P_PLUS_1: &str =
    "21888242871839275222246405745257275088548364400416034343698204186575808495618";

fn good_vote(census: CensusProofWire) -> VoteRequest {
    let pk = Point::generator().mul(&U256::from(12345u64));
    let mut b = Ballot::identity();
    b.0[0] = encrypt(&pk, 3, &U256::from(77u64));
    VoteRequest {
        process_id: ProcessId([0xab; 31]),
        address: [0x11; 20],
        vote_id: 0x8000_0000_0000_1234,
        ballot: b,
        ballot_proof: SnarkJsProof {
            pi_a: ["1".into(), "2".into(), "1".into()],
            pi_b: [
                ["1".into(), "2".into()],
                ["3".into(), "4".into()],
                ["1".into(), "0".into()],
            ],
            pi_c: ["5".into(), "6".into(), "1".into()],
            protocol: "groth16".into(),
            curve: "bn128".into(),
        },
        signature: [7u8; 65],
        weight: 42,
        census_proof: Some(census),
    }
}

fn merkle() -> CensusProofWire {
    let mut t = LeanImt::new();
    for i in 0..5u8 {
        t.insert(census_leaf(&[i; 20], 1).unwrap());
    }
    CensusProofWire::Merkle(MerkleProof::from(&t.proof(3).unwrap()))
}

fn csp() -> CensusProofWire {
    CensusProofWire::Csp(CspWire {
        r: [1; 32],
        s: [2; 32],
        recid: 1,
        index: 9,
    })
}

fn good_json(c: CensusProofWire) -> Value {
    serde_json::to_value(good_vote(c)).unwrap()
}

fn decodes(s: &str) -> bool {
    serde_json::from_str::<VoteRequest>(s).is_ok()
}

fn mutated(f: impl Fn(&mut Value)) -> bool {
    let mut j = good_json(merkle());
    f(&mut j);
    serde_json::from_value::<VoteRequest>(j).is_ok()
}

// A decimal field element on its own (the key response's x has no curve check).
fn fr_decodes(v: &Value) -> bool {
    serde_json::from_value::<EncryptionKeyResponse>(json!({"x": v, "y": "1"})).is_ok()
}

#[test]
fn the_baseline_decodes() {
    assert!(mutated(|_| {}));
    let j = good_json(csp());
    assert!(serde_json::from_value::<VoteRequest>(j).is_ok());
}

// A duplicated key must not be last-wins or first-wins: it is refused, at
// every nesting level.
#[test]
fn duplicate_keys_are_refused() {
    let top = serde_json::to_string(&good_json(merkle())).unwrap();
    for (k, v) in [
        ("weight", "\"1\""),
        ("address", "\"0x1111111111111111111111111111111111111111\""),
        ("voteId", "\"0x8000000000001234\""),
        (
            "processId",
            "\"0xabababababababababababababababababababababababababababababab\"",
        ),
        ("censusProof", "null"),
        ("ballot", "[]"),
    ] {
        let dup = top.replacen('{', &format!("{{\"{k}\":{v},"), 1);
        assert!(!decodes(&dup), "duplicate top-level {k} accepted");
    }
    // Inside a point, a ciphertext, the proof and the census proof.
    let dup_point = top.replacen("\"x\":", "\"x\":\"0\",\"x\":", 1);
    assert!(!decodes(&dup_point), "duplicate point x");
    let dup_ct = top.replacen("\"c1\":", "\"c1\":{\"x\":\"0\",\"y\":\"1\"},\"c1\":", 1);
    assert!(!decodes(&dup_ct), "duplicate c1");
    let dup_leaf = top.replacen("\"leaf\":", "\"leaf\":\"1\",\"leaf\":", 1);
    assert!(!decodes(&dup_leaf), "duplicate merkle leaf");
    let dup_type = top.replacen(
        "\"type\":\"merkle\"",
        "\"type\":\"merkle\",\"type\":\"csp\"",
        1,
    );
    assert!(!decodes(&dup_type), "duplicate census type tag");
    let c = serde_json::to_string(&good_json(csp())).unwrap();
    let dup_recid = c.replacen("\"recid\":1", "\"recid\":1,\"recid\":0", 1);
    assert!(!decodes(&dup_recid), "duplicate csp recid");
    // A camelCase key and its snake_case twin are two different keys.
    assert!(!mutated(|j| {
        j["weight_x"] = json!("1");
    }));
}

// "vote bodies may carry no unknown fields" (docs/api.md), anywhere.
#[test]
fn unknown_fields_are_refused_everywhere() {
    assert!(!mutated(|j| j["extra"] = json!(1)));
    assert!(!mutated(|j| j["ballot"][0]["extra"] = json!(1)));
    assert!(!mutated(|j| j["ballot"][0]["c1"]["z"] = json!("1")));
    assert!(!mutated(|j| j["censusProof"]["extra"] = json!(1)));
    let mut c = good_json(csp());
    c["censusProof"]["extra"] = json!(1);
    assert!(serde_json::from_value::<VoteRequest>(c).is_err());
    // BUG: ballotProof (a davinci-zkvm-sdk type) takes unknown fields.
    assert!(
        !mutated(|j| j["ballotProof"]["extra"] = json!(1)),
        "unknown field inside ballotProof accepted"
    );
    // Case variants of a key are not the key.
    assert!(!mutated(|j| {
        let v = j["weight"].take();
        j.as_object_mut().unwrap().remove("weight");
        j["Weight"] = v;
    }));
}

#[test]
fn missing_and_null_fields_are_refused() {
    for k in [
        "processId",
        "address",
        "voteId",
        "ballot",
        "ballotProof",
        "signature",
        "weight",
    ] {
        assert!(
            !mutated(|j| {
                j.as_object_mut().unwrap().remove(k);
            }),
            "{k} missing accepted"
        );
        assert!(!mutated(|j| j[k] = Value::Null), "{k}: null accepted");
    }
    // An old client's ballotInputsHash is an unknown field now.
    assert!(!mutated(|j| j["ballotInputsHash"] = json!("999")));
    // censusProof is the one optional field.
    assert!(mutated(|j| {
        j.as_object_mut().unwrap().remove("censusProof");
    }));
    assert!(mutated(|j| j["censusProof"] = Value::Null));
    for bad in [
        json!({}),
        json!([]),
        json!("merkle"),
        json!({"type": null}),
        json!({"type": "Merkle"}),
    ] {
        assert!(!mutated(|j| j["censusProof"] = bad.clone()), "{bad}");
    }
}

#[test]
fn decimal_strings_are_strict() {
    // Field element: a key coordinate. u128: weight.
    let bad = [
        "", " ", "+1", "-0", "-1", " 1", "1 ", "1\n", "0x1", "0X1", "1_0", "1e3", "1.0", "1,0",
        "٣", "１２", "NaN", "inf", "\u{0}", "1\u{0}",
    ];
    for s in bad {
        assert!(!fr_decodes(&json!(s)), "field element {s:?}");
        assert!(!mutated(|j| j["weight"] = json!(s)), "weight {s:?}");
    }
    // Field: the modulus boundary and 80+ digit strings.
    assert!(fr_decodes(&json!(P_MINUS_1)));
    assert!(!fr_decodes(&json!(P)));
    assert!(!fr_decodes(&json!(P_PLUS_1)));
    assert!(!fr_decodes(&json!("9".repeat(78))));
    assert!(!fr_decodes(&json!("9".repeat(80))));
    assert!(!fr_decodes(&json!("9".repeat(81))));
    assert!(!fr_decodes(&json!("1".repeat(100_000))));
    assert!(!fr_decodes(&json!("0".repeat(100_000))));
    // Weight: u128 boundary.
    assert!(mutated(|j| j["weight"] = json!(u128::MAX.to_string())));
    assert!(!mutated(
        |j| j["weight"] = json!("340282366920938463463374607431768211456")
    ));
    assert!(!mutated(|j| j["weight"] = json!("9".repeat(39))));
    assert!(!mutated(|j| j["weight"] = json!("0".repeat(40))));
    // Not strings.
    for v in [
        json!(1),
        json!(1.5),
        json!(-1),
        json!(1e30),
        json!(true),
        json!([]),
        json!({}),
        json!(["1"]),
    ] {
        assert!(!mutated(|j| j["weight"] = v.clone()), "weight {v}");
        assert!(!fr_decodes(&v), "field element {v}");
    }
}

#[test]
fn leading_zeros_do_not_change_the_value() {
    let a: VoteRequest = serde_json::from_value({
        let mut j = good_json(merkle());
        j["weight"] = json!("0000000000000000000000000000042");
        j
    })
    .unwrap();
    assert_eq!(a.weight, 42);
    let k: EncryptionKeyResponse =
        serde_json::from_value(json!({"x": "0000999", "y": "1"})).unwrap();
    assert_eq!(k.x, Fr::from(999u64));
}

#[test]
fn hex_fields_are_strict() {
    let addr = "11".repeat(20);
    // Wrong lengths and digits.
    for s in [
        String::new(),
        "0x".into(),
        format!("0x{}", &addr[..39]),
        format!("0x{}", &addr[..38]),
        format!("0x{addr}00"),
        format!("0x{addr}0"),
        format!("0x{}g", &addr[..39]),
        format!("0x {}", &addr[..38]),
        format!(" 0x{addr}"),
        format!("0x{addr} "),
        format!("0x{addr}\n"),
        format!("0X{addr}"),
        format!("00x{addr}"),
        format!("0x0x{}", &addr[..38]),
        format!("0x{}é", &addr[..38]),
        format!("0x{}ée", &addr[..36]),
        "0x\u{1F600}\u{1F600}\u{1F600}\u{1F600}\u{1F600}".into(),
    ] {
        assert!(!mutated(|j| j["address"] = json!(s)), "address {s:?}");
    }
    for (k, n) in [("signature", 65usize), ("processId", 31)] {
        let ok = format!("0x{}", "ab".repeat(n));
        for s in [
            format!("{ok}00"),
            ok[..ok.len() - 1].to_string(),
            ok[..ok.len() - 2].to_string(),
            ok.replace("0x", "0X"),
            format!("{ok}\0"),
        ] {
            assert!(!mutated(|j| j[k] = json!(s)), "{k} {s:?}");
        }
        // Upper case digits are the same bytes, not a different value.
        let up = format!("0x{}", "AB".repeat(n));
        let a = good_vote(merkle());
        let mut j = serde_json::to_value(&a).unwrap();
        j[k] = json!(up);
        let b: VoteRequest = serde_json::from_value(j).unwrap();
        assert_eq!(serde_json::to_value(&b).unwrap()[k], json!(ok));
    }
    // Every byte string needs its `0x`.
    let good = serde_json::to_value(good_vote(merkle())).unwrap();
    for k in ["processId", "address", "voteId", "signature"] {
        let bare = good[k]
            .as_str()
            .unwrap()
            .strip_prefix("0x")
            .unwrap()
            .to_string();
        assert!(!mutated(|j| j[k] = json!(bare)), "{k} without 0x");
    }
    // Not strings.
    for v in [json!(1), json!([1, 2]), json!(null), json!({"0x": 1})] {
        assert!(!mutated(|j| j["address"] = v.clone()));
        assert!(!mutated(|j| j["signature"] = v.clone()));
        assert!(!mutated(|j| j["voteId"] = v.clone()));
    }
}

#[test]
fn vote_id_boundaries() {
    assert_eq!(parse_vote_id("0x8000000000000000").unwrap(), 1 << 63);
    assert_eq!(parse_vote_id("0xffffffffffffffff").unwrap(), u64::MAX);
    for s in [
        "0x7fffffffffffffff",
        "0x0000000000000000",
        "0x00000000000000000",
        "0x800000000000000",
        "0x80000000000000000",
        "8000000000000000 ",
        "0x+800000000000000",
        "0x-800000000000000",
        "0x 800000000000000",
        "",
        "0x",
    ] {
        assert!(parse_vote_id(s).is_err(), "{s:?}");
    }
}

#[test]
fn process_id_forms() {
    let ok = format!("0x{}", "00".repeat(31));
    assert!(ok.parse::<ProcessId>().is_ok());
    for s in [
        "".to_string(),
        "0x".into(),
        format!("0x{}", "0".repeat(61)),
        format!("0x{}", "0".repeat(63)),
        format!("0x{}", "0".repeat(64)),
        format!("0x{}", "g".repeat(62)),
        format!("0x{}", "é".repeat(31)),
        format!("0x{}\0", "0".repeat(62)),
    ] {
        assert!(s.parse::<ProcessId>().is_err(), "{s:?}");
    }
    // Every 31-byte id is below the field modulus: to_fr never reduces.
    let f = ProcessId([0xff; 31]).to_fr();
    let mut be = [0u8; 32];
    be[1..].copy_from_slice(&[0xff; 31]);
    assert_eq!(
        davinci_zkvm_sdk::crypto::field::fr_to_be(&f).to_vec(),
        be.to_vec()
    );
}

#[test]
fn key_request_is_strict() {
    let ok = |s: &str| serde_json::from_str::<EncryptionKeyRequest>(s).is_ok();
    let pid = format!("0x{}", "ab".repeat(31));
    assert!(ok(&format!("{{\"processId\":\"{pid}\"}}")));
    assert!(!ok("{}"));
    assert!(!ok("null"));
    assert!(!ok("[]"));
    assert!(!ok(&format!("{{\"processId\":\"{pid}\",\"x\":1}}")));
    assert!(!ok(&format!(
        "{{\"processId\":\"{pid}\",\"processId\":\"{pid}\"}}"
    )));
    assert!(!ok(&format!("{{\"process_id\":\"{pid}\"}}")));
    assert!(!ok(&format!(
        "{{\"processId\":\"{}\"}}",
        &pid[..pid.len() - 2]
    )));
}

// Ballot shape and the points inside.
#[test]
fn ballot_points_are_canonical_and_on_curve() {
    let set = |idx: usize, side: &str, coord: &str, v: &str| {
        mutated(|j| j["ballot"][idx][side][coord] = json!(v))
    };
    // Non-canonical identity (x + p, y) and (x, y + p).
    assert!(!set(15, "c1", "x", P));
    assert!(!set(15, "c1", "y", P_PLUS_1));
    // (0, p+1) would be the identity under reduction.
    assert!(!set(
        15,
        "c2",
        "y",
        "21888242871839275222246405745257275088548364400416034343698204186575808495618"
    ));
    // The identity, canonically, and the order-2 point (0, -1) are on the curve.
    assert!(mutated(|j| {
        j["ballot"][5] = json!({"c1": {"x": "0", "y": "1"}, "c2": {"x": "0", "y": "1"}});
    }));
    assert!(mutated(|j| {
        j["ballot"][5] = json!({"c1": {"x": "0", "y": P_MINUS_1}, "c2": {"x": "0", "y": "1"}});
    }));
    // Off the curve: the all-zero point, (1,1), and a swap of x and y.
    for (x, y) in [("0", "0"), ("1", "1"), ("1", "0"), ("2", "3")] {
        assert!(
            !mutated(|j| {
                j["ballot"][5] = json!({"c1": {"x": x, "y": y}, "c2": {"x": "0", "y": "1"}});
            }),
            "({x},{y})"
        );
        assert!(!mutated(|j| {
            j["ballot"][5] = json!({"c1": {"x": "0", "y": "1"}, "c2": {"x": x, "y": y}});
        }));
    }
    // 15 and 17 ciphertexts, a non-array, a map, null entries, missing sides.
    assert!(!mutated(|j| {
        j["ballot"].as_array_mut().unwrap().pop();
    }));
    assert!(!mutated(|j| {
        let c = j["ballot"][0].clone();
        j["ballot"].as_array_mut().unwrap().push(c);
    }));
    assert!(!mutated(|j| j["ballot"] = json!({})));
    assert!(!mutated(|j| j["ballot"][3] = Value::Null));
    assert!(!mutated(|j| {
        j["ballot"][3].as_object_mut().unwrap().remove("c2");
    }));
    assert!(!mutated(|j| {
        j["ballot"][3]["c1"].as_object_mut().unwrap().remove("y");
    }));
    assert!(!mutated(|j| j["ballot"][3]["c1"]["x"] = json!(0)));
}

#[test]
fn census_proof_wire_is_strict() {
    let mut m = good_json(merkle());
    let p = m["censusProof"].clone();
    // Extra siblings are fine for the decoder (the node ignores them for a
    // Merkle census); each must still be `0x` and 64 hex digits.
    for bad in [
        json!(P),
        json!("5"),
        json!(-1),
        json!(1),
        json!("x"),
        json!(format!("0x{}", "1".repeat(63))),
        Value::Null,
    ] {
        let mut v = p.clone();
        v["siblings"][0] = bad.clone();
        m["censusProof"] = v;
        assert!(
            serde_json::from_value::<VoteRequest>(m.clone()).is_err(),
            "{bad}"
        );
    }
    // pathBits must be a u64: no float, no negative, no beyond u64.
    for bad in [json!(-1), json!(1.5), json!(1e30), json!("1")] {
        let mut v = p.clone();
        v["pathBits"] = bad.clone();
        m["censusProof"] = v;
        assert!(
            serde_json::from_value::<VoteRequest>(m.clone()).is_err(),
            "pathBits {bad}"
        );
    }
    // CSP: recid is a u8, index a u64, r and s exactly 32 bytes.
    let c = good_json(csp());
    for (k, bad) in [
        ("recid", json!(256)),
        ("recid", json!(-1)),
        ("recid", json!("1")),
        ("recid", json!(1.0)),
        ("index", json!(-1)),
        ("index", json!(1.5)),
        ("index", json!("9")),
        ("r", json!("0x01")),
        ("s", json!(format!("0x{}", "00".repeat(33)))),
        ("r", json!(format!("0x{}", "zz".repeat(32)))),
    ] {
        let mut v = c.clone();
        v["censusProof"][k] = bad.clone();
        assert!(
            serde_json::from_value::<VoteRequest>(v).is_err(),
            "{k} {bad}"
        );
    }
    // Past u64, as raw text (a `Value` cannot hold it).
    let raw = serde_json::to_string(&m).unwrap();
    assert!(!decodes(&raw.replacen(
        "\"pathBits\":",
        "\"pathBits\":18446744073709551616,\"x\":",
        1
    )));
    let raw = serde_json::to_string(&c).unwrap();
    assert!(!decodes(&raw.replacen(
        "\"index\":9",
        "\"index\":18446744073709551616",
        1
    )));
    // recid 0..=255 and index u64::MAX decode (the node judges them).
    let mut v = c.clone();
    v["censusProof"]["recid"] = json!(255);
    v["censusProof"]["index"] = json!(u64::MAX);
    assert!(serde_json::from_value::<VoteRequest>(v).is_ok());
}

// Resource shape: depth and size must not blow the stack or the heap.
#[test]
fn deep_nesting_and_huge_arrays_are_bounded() {
    // serde_json caps recursion at 128: a 100k-deep body is an error, not a
    // stack overflow.
    let deep = format!("{}{}", "[".repeat(100_000), "]".repeat(100_000));
    assert!(serde_json::from_str::<VoteRequest>(&deep).is_err());
    let top = serde_json::to_string(&good_json(merkle())).unwrap();
    let nested = top.replacen("\"weight\":\"42\"", &format!("\"weight\":{deep}"), 1);
    assert!(!decodes(&nested));
    let unknown = top.replacen('{', &format!("{{\"zzz\":{deep},"), 1);
    assert!(!decodes(&unknown));
    let objs = format!("{}1{}", "{\"a\":".repeat(50_000), "}".repeat(50_000));
    assert!(!decodes(&top.replacen(
        '{',
        &format!("{{\"zzz\":{objs},"),
        1
    )));
    // A 250 KiB sibling list (the most a body can carry) decodes without
    // trouble in the tag-buffered enum.
    let mut j = good_json(merkle());
    let sib = json!(format!("0x{}", "11".repeat(32)));
    j["censusProof"]["siblings"] = Value::Array(vec![sib; 3_800]);
    let t0 = std::time::Instant::now();
    let _ = serde_json::from_value::<VoteRequest>(j);
    assert!(t0.elapsed().as_secs() < 5);
    // Thousands of ciphertexts are refused by length.
    let mut j = good_json(merkle());
    j["ballot"] = Value::Array(vec![j["ballot"][15].clone(); 30_000]);
    assert!(serde_json::from_value::<VoteRequest>(j).is_err());
}

#[test]
fn bad_utf8_and_bom_are_refused() {
    let top = serde_json::to_vec(&good_json(merkle())).unwrap();
    assert!(serde_json::from_slice::<VoteRequest>(&top).is_ok());
    let mut bom = vec![0xef, 0xbb, 0xbf];
    bom.extend_from_slice(&top);
    assert!(serde_json::from_slice::<VoteRequest>(&bom).is_err());
    let mut bad = top.clone();
    bad.insert(1, 0xff);
    assert!(serde_json::from_slice::<VoteRequest>(&bad).is_err());
    let mut trailing = top.clone();
    trailing.extend_from_slice(b" {}");
    assert!(serde_json::from_slice::<VoteRequest>(&trailing).is_err());
}
