//! Reproduce Go arbo's circomproofs_test.go vectors with the SDK Poseidon:
//! a 4-level Poseidon tree over leaves (1,11)..(4,44) must emit exactly the
//! CircomVerifierProof JSONs checked into `testdata/circom/`.
//!
//! Gated behind the `sdk-poseidon` feature, which pulls in davinci-zkvm-sdk's
//! Poseidon; arbo itself does not depend on the SDK.
#![cfg(feature = "sdk-poseidon")]

use arbo::{Hash, HashFunction, MemoryStorage, Tree};
use ark_ff::{BigInteger, PrimeField};
use num_bigint::BigUint;
use serde_json::{Value, json};

/// Go arbo `HashFunctionPoseidon`: each part is a LE bigint, hashed as a
/// Poseidon input list, output rendered LE in 32 bytes.
#[derive(Clone)]
struct Poseidon;

impl HashFunction for Poseidon {
    fn id(&self) -> u8 {
        2
    }

    fn hash(&self, parts: &[&[u8]]) -> Hash {
        let frs: Vec<ark_bn254::Fr> = parts
            .iter()
            .map(|p| ark_bn254::Fr::from_le_bytes_mod_order(p))
            .collect();
        let h = davinci_zkvm_sdk::crypto::poseidon::poseidon(&frs).expect("poseidon hash");
        let bytes = h.into_bigint().to_bytes_le();
        let mut out = [0u8; 32];
        out[..bytes.len()].copy_from_slice(&bytes);
        out
    }
}

fn dec(b: &[u8]) -> String {
    BigUint::from_bytes_le(b).to_string()
}

fn proof_json(t: &Tree<MemoryStorage, Poseidon>, key: &[u8]) -> Value {
    let p = t.circom_verifier_proof(key).unwrap();
    json!({
        "root": dec(&p.root),
        "siblings": p.siblings.iter().map(|s| dec(s)).collect::<Vec<_>>(),
        "oldKey": dec(&p.old_key),
        "oldValue": dec(&p.old_value),
        "isOld0": if p.is_old0 { "1" } else { "0" },
        "key": dec(&p.key),
        "value": dec(&p.value),
        "fnc": p.fnc,
    })
}

fn expected(name: &str) -> Value {
    let path = format!("{}/testdata/circom/{}", env!("CARGO_MANIFEST_DIR"), name);
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

#[test]
fn circom_poseidon_vectors() {
    // Go test: maxLevels=4, bLen=1, leaves (1,11)(2,22)(3,33)(4,44).
    let mut t = Tree::new(MemoryStorage::new(), 4, Poseidon).unwrap();
    for i in 1u8..=4 {
        t.add(&[i], &[i * 11]).unwrap();
    }

    // Existence proof for key 2.
    assert_eq!(
        proof_json(&t, &[2]),
        expected("go-smt-verifier-inputs.json")
    );
    // Non-existence proof for key 5 (finds leaf 1).
    assert_eq!(
        proof_json(&t, &[5]),
        expected("go-smt-verifier-non-existence-inputs.json")
    );
}
