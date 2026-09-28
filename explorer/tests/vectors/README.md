# Test vectors

Copied verbatim from davinci-zkvm `rust-sdk/testdata/`, where the Rust SDK
checks the same values against the Go reference code:

| File | What | Rust test |
|---|---|---|
| `publics_batch.bin` | 256-byte u32 publics of a recorded batch job | `tests/wire.rs::batch_publics_from_recorded_job` |
| `snark_batch.json` | the same job's PLONK: `program_vk`, `root_c_vadcop_final`, the 512-byte `public_values`, `proof_bytes` | same |
| `blob.json` | three transitions (nf 2, 6, 16): cells, commitments, evaluation points, openings, versioned hashes, digest | `tests/blob.rs::blobs_match_go_kzg4844` |

`blob.json` carries vote ids above 2^53 as bare JSON numbers; the tests parse
it with `loadJsonBig` so they keep full precision.

Refresh them by copying the files again after a change on the Rust side.

`gnosis_transition.json` is recorded from the live Gnosis deployment
(ProcessRegistry `0x3CDE68c39E26ecf94bD029b6ED3b9F945441daf3`): one
`submitStateTransition` transaction with its calldata, versioned hashes, the
`ProcessStateTransitioned` event, the process's `numFields` and the beacon
sidecar of its blob (slot 30315748). The tests tie the four together.
