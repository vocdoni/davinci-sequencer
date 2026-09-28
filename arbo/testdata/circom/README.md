# circom test vectors

Copied from `github.com/vocdoni/arbo@v0.0.0-20260501121933-158dce698e7a/testvectors/circom`.

- `go-smt-verifier-inputs.json`, `go-smt-verifier-non-existence-inputs.json`: the
  `GenerateCircomVerifierProof` outputs (Poseidon, maxLevels=4, leaves (1,11) (2,22)
  (3,33) (4,44), 1-byte LE keys/values) that upstream verified against the circomlib
  `SMTVerifier` circuit (`smt.test.ts`). The Rust test `tests/circom_poseidon.rs`
  reproduces them byte for byte.
- `smt.test.ts`, `smt-proof-verifier_test.circom`, `smt-proof-processor_test.circom`:
  the upstream circom harness, kept for provenance only.
