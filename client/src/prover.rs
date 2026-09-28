//! Groth16 ballot prover over the davinci-circom artifacts: wasmer witness
//! calculator plus the ark-circom zkey reader and prover (arkworks 0.6).
//! arkworks 0.6 stays inside this module; values cross as integers.

use std::fs::File;
use std::io::BufReader;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;

use ark_bn254_06::{Bn254, Fq, Fr as Fr06, G1Affine, G2Affine};
use ark_circom::{CircomReduction, WitnessCalculator, read_zkey};
use ark_ec_06::AffineRepr;
use ark_ff_06::{BigInteger, PrimeField};
use ark_groth16_06::{
    Groth16, PreparedVerifyingKey, ProvingKey, VerifyingKey, prepare_verifying_key,
};
use davinci_zkvm_sdk::crypto::field::{fr_from_le, fr_to_le};
use davinci_zkvm_sdk::groth16::{BallotVerifier, N_PUBLIC};
use davinci_zkvm_sdk::release;
use davinci_zkvm_sdk::types::{SnarkJsProof, SnarkJsVk};
use num_bigint::{BigInt, BigUint, Sign};
use rand::RngCore;
use rand::rngs::OsRng;
use wasmer::{Engine, Module, Store};

use crate::api::Fr;
use crate::voter::CircomInputs;
use crate::{Error, Result};

type Matrix = Vec<Vec<(Fr06, usize)>>;

/// Loaded circuit: compiled witness wasm, proving key and R1CS matrices.
/// `prove` builds a fresh wasm instance per call, so it can run on many
/// threads at once.
pub struct BallotProver {
    engine: Engine,
    module: Module,
    pk: ProvingKey<Bn254>,
    pvk: PreparedVerifyingKey<Bn254>,
    matrices: [Matrix; 3],
    num_inputs: usize,
    num_constraints: usize,
    num_witness: usize,
}

impl std::fmt::Debug for BallotProver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "BallotProver(constraints={}, inputs={})",
            self.num_constraints, self.num_inputs
        )
    }
}

fn perr(what: &str, e: impl std::fmt::Display) -> Error {
    Error::Prover(format!("{what}: {e}"))
}

// ark-circom panics on some malformed artifacts; report those as errors.
fn guarded<T>(what: &str, f: impl FnOnce() -> Result<T>) -> Result<T> {
    catch_unwind(AssertUnwindSafe(f)).unwrap_or_else(|_| Err(perr(what, "panicked")))
}

fn to_bigint(x: &Fr) -> BigInt {
    BigInt::from_bytes_le(Sign::Plus, &fr_to_le(x))
}

fn le32<B: BigInteger>(b: B) -> Result<[u8; 32]> {
    b.to_bytes_le()
        .try_into()
        .map_err(|_| Error::Prover("field element is not 32 bytes".into()))
}

fn from06(x: &Fr06) -> Result<Fr> {
    Ok(fr_from_le(&le32(x.into_bigint())?)?)
}

fn fq_dec(x: &Fq) -> String {
    BigUint::from_bytes_le(&x.into_bigint().to_bytes_le()).to_string()
}

fn random06() -> Fr06 {
    let mut b = [0u8; 64];
    OsRng.fill_bytes(&mut b);
    Fr06::from_le_bytes_mod_order(&b)
}

fn g1_json(p: &G1Affine) -> [String; 3] {
    if p.is_zero() {
        return ["0".into(), "1".into(), "0".into()];
    }
    [fq_dec(&p.x), fq_dec(&p.y), "1".into()]
}

fn g2_json(p: &G2Affine) -> [[String; 2]; 3] {
    if p.is_zero() {
        return [
            ["0".into(), "0".into()],
            ["1".into(), "0".into()],
            ["0".into(), "0".into()],
        ];
    }
    [
        [fq_dec(&p.x.c0), fq_dec(&p.x.c1)],
        [fq_dec(&p.y.c0), fq_dec(&p.y.c1)],
        ["1".into(), "0".into()],
    ]
}

// The zkey's VK hashed like the guest and the 0x07 leaf (SDK `vk_hash`).
fn vk_hash(vk: &VerifyingKey<Bn254>) -> Result<[u8; 32]> {
    let json = SnarkJsVk {
        protocol: "groth16".into(),
        curve: "bn128".into(),
        n_public: Some(N_PUBLIC as u64),
        vk_alpha_1: g1_json(&vk.alpha_g1),
        vk_beta_2: g2_json(&vk.beta_g2),
        vk_gamma_2: g2_json(&vk.gamma_g2),
        vk_delta_2: g2_json(&vk.delta_g2),
        vk_alphabeta_12: None,
        ic: vk.gamma_abc_g1.iter().map(g1_json).collect(),
    };
    Ok(BallotVerifier::from_snarkjs(&json)?.vk_hash())
}

impl BallotProver {
    /// Loads `ballot_proof.wasm` and `ballot_proof_pkey.zkey`. The zkey must
    /// carry the protocol's ballot VK (the SDK's embedded
    /// `ballot_proof_vkey.json`), so every proof is one the sequencer accepts.
    pub fn load(wasm: &Path, zkey: &Path) -> Result<Self> {
        Self::load_with_vk(wasm, zkey, release::ballot_vk_json())
    }

    /// [`BallotProver::load`] against another snarkjs VK.
    pub fn load_with_vk(wasm: &Path, zkey: &Path, vk_json: &str) -> Result<Self> {
        let expected = BallotVerifier::from_snarkjs_json(vk_json)?.vk_hash();
        guarded("load", || {
            let f = File::open(zkey).map_err(|e| perr("open zkey", e))?;
            let (pk, idx) = read_zkey(&mut BufReader::new(f)).map_err(|e| perr("read zkey", e))?;
            if idx.num_instance_variables != N_PUBLIC + 1
                || pk.vk.gamma_abc_g1.len() != N_PUBLIC + 1
            {
                return Err(Error::Prover(format!(
                    "zkey has {} instance variables, want {}",
                    idx.num_instance_variables,
                    N_PUBLIC + 1
                )));
            }
            if vk_hash(&pk.vk)? != expected {
                return Err(Error::Prover(
                    "zkey VK does not match the expected ballot VK".into(),
                ));
            }
            let engine = Engine::default();
            let module = Module::from_file(&engine, wasm).map_err(|e| perr("compile wasm", e))?;
            let pvk = prepare_verifying_key(&pk.vk);
            let num_witness = pk.a_query.len();
            Ok(BallotProver {
                engine,
                module,
                pvk,
                pk,
                num_inputs: idx.num_instance_variables,
                num_constraints: idx.num_constraints,
                // One value per zkey variable (NPIndex counts one too many).
                num_witness,
                matrices: [idx.a, idx.b, idx.c],
            })
        })
    }

    fn witness(&self, inputs: &CircomInputs) -> Result<Vec<Fr06>> {
        let signals: Vec<(String, Vec<BigInt>)> = inputs
            .signals()
            .into_iter()
            .map(|(n, v)| (n.to_string(), v.iter().map(to_bigint).collect()))
            .collect();
        guarded("witness", || {
            let mut store = Store::new(self.engine.clone());
            let mut wc = WitnessCalculator::from_module(&mut store, self.module.clone())
                .map_err(|e| perr("instantiate wasm", e))?;
            wc.calculate_witness_element::<Fr06, _>(&mut store, signals, true)
                .map_err(|e| perr("witness", e))
        })
    }

    /// Proves `inputs`; returns the snarkjs proof and the public signals
    /// `[address, vote_id, inputs_hash]`. The proof is verified before it is
    /// returned.
    pub fn prove(&self, inputs: &CircomInputs) -> Result<(SnarkJsProof, [Fr; N_PUBLIC])> {
        let w = self.witness(inputs)?;
        if w.len() != self.num_witness || w.first() != Some(&Fr06::from(1u64)) {
            return Err(Error::Prover(format!(
                "witness has {} values, want {}",
                w.len(),
                self.num_witness
            )));
        }
        let proof = guarded("prove", || {
            Groth16::<Bn254, CircomReduction>::create_proof_with_reduction_and_matrices(
                &self.pk,
                random06(),
                random06(),
                &self.matrices,
                self.num_inputs,
                self.num_constraints,
                &w,
            )
            .map_err(|e| perr("prove", e))
        })?;
        let pubs06 = &w[1..self.num_inputs];
        let ok = Groth16::<Bn254>::verify_proof(&self.pvk, &proof, pubs06).unwrap_or(false);
        if !ok || proof.a.is_zero() || proof.b.is_zero() || proof.c.is_zero() {
            // The witness wasm does not trap on failed `===`; this is where an
            // out-of-mode ballot is caught.
            return Err(Error::Prover(
                "generated proof does not verify: inputs rejected by the circuit".into(),
            ));
        }
        let mut pubs = [Fr::from(0u64); N_PUBLIC];
        for (o, x) in pubs.iter_mut().zip(pubs06) {
            *o = from06(x)?;
        }
        let snark = SnarkJsProof {
            pi_a: [fq_dec(&proof.a.x), fq_dec(&proof.a.y), "1".into()],
            pi_b: [
                [fq_dec(&proof.b.x.c0), fq_dec(&proof.b.x.c1)],
                [fq_dec(&proof.b.y.c0), fq_dec(&proof.b.y.c1)],
                ["1".into(), "0".into()],
            ],
            pi_c: [fq_dec(&proof.c.x), fq_dec(&proof.c.y), "1".into()],
            protocol: "groth16".into(),
            curve: "bn128".into(),
        };
        Ok((snark, pubs))
    }
}
