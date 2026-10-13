# Compact DA stream and 512-vote batches

Status: design approved in conversation by the owner on 2026-10-13 ("apply that compression and
also let's increase the vote batch to 512 from now on"); written spec pending the owner's review.
First sub-project of the OP-stack rollout (order: 1 compact stream, 2 core redeploy on OP,
3 DKG and Council on OP, 4 SDK and apps, 5 on-chain census on OP; Gnosis is retired). Every
sub-project ends with a strict Fable 5.1 audit, then merges to `main` with releases.

## 1. Goal

Shrink the data-availability stream the batch guest commits to, without changing what it proves or
what a follower can rebuild, and make 512 votes the node's batch size. The stream is mirrored by the
guest, the Rust SDK, the Go SDK and their vectors, and decoded by every follower, so all of them
change together and the batch program vk changes (`ROOT_C_VADCOP_FINAL` does not).

Non-goals (later sub-projects or rejected): delta coding of ids and keys (under 1 % at 16 fields
for a more complex parser on the follower's trust boundary); shared ElGamal randomness (rejected by
the owner); several Ethereum blob transactions per transition and the higher grace floor (core
sub-project); any contract change (contracts read only commitments, the pair digest and the
`0x0A` openings, never cell contents).

## 2. Today's stream (davinci-zkvm `main` 18f1639)

One BLS12-381 Fr element per 32-byte cell, 4096 cells per blob (`circuit/CIRCUIT.md` §8,
`circuit-primitives/src/da_blob.rs`, `rust-sdk/src/blob.rs`, `go-sdk/blob.go`):

| Cells | Content |
|---|---|
| 1 | `n_vids` (u64, big-endian in the low 8 bytes) |
| `n_vids` | vote ids, strictly ascending, each in `[2^63, 2^64)` |
| 1 | `n_updates` |
| `n_updates` | slot keys, strictly ascending, in `[BALLOT_MIN, BALLOT_MAX] = [0x10, 2^63 − 1]` |
| `n_updates · 2nf` | ballots, per update in key order: `c1_0, c2_0, …, c1_{nf−1}, c2_{nf−1}`, each point packed as `y + ((x & 1) << 254)` |
| `2nf` | the new Results accumulator (the plaintext of leaf `0x04`) |
| rest | zero |

`T = 2 + V + U(1 + 2nf) + 2nf` cells, `n_blobs = ceil(T / 4096)` (V votes, U = V + R updates).

## 3. The compact stream

Same 4096-cell blobs, same field, same point packing, same order and deniability (one merged,
key-sorted, unmarked update list; a slot's first write still marks a new vote):

| Cells | Content |
|---|---|
| 1 | header: the integer `n_vids · 2^64 + n_updates` (both below 2^12; every other bit zero) |
| `ceil((V + U) / 4)` | the integer section, below |
| `U · 2nf` | ballots, exactly as today |
| rest | zero |

**Integer section.** The list `I = [vid_0 − 2^63, …, vid_{V−1} − 2^63, key_0, …, key_{U−1}]`,
every element below `2^63`, is packed four per cell: cell `k` is the integer
`I[4k]·2^189 + I[4k+1]·2^126 + I[4k+2]·2^63 + I[4k+3]` (first element in the most significant
position; missing elements of the last cell are zero). A cell is below `2^252 < r_bls`. The vote
id's top bit is implied (`VOTE_ID_MIN = 2^63`); a key already fits 63 bits.

**No accumulator.** A follower holds the old Results leaf and the old slot ballots, so the new
Results ballot is `old_results + Σ_u (new_ballot_u − old_ballot_{key_u})`, with the identity as the
old ballot of an empty slot. This covers new votes, overwrites and silent refreshes alike, and the
follower's existing check of the rebuilt tree against `rootAfter` verifies it. The guest still
computes `net` for the Results leaf; it only stops emitting it.

`T = 1 + ceil((V + U) / 4) + 2nf·U`. Points stay one per cell: a packed point uses 255 bits (a
y below the BN254 modulus plus the x-parity bit) and fits a cell only because
`p_bn254 + 2^254 < r_bls`; re-chunking the stream into 31-byte cells (the 2026-10-11 draft) would
make each point cost 32/31 cells and lose the gain at 16 fields.

| 512 votes | Cells today | Blobs | Compact | Blobs |
|---|---:|---:|---:|---:|
| nf 2, first batch | 3,078 | 1 | 2,305 | 1 |
| nf 2, steady, all revotes (1,024 refreshes) | 8,198 | 3 | 6,657 | 2 |
| nf 8, steady (512 refreshes) | 17,938 | 5 | 16,769 | 5 |
| nf 16, steady (512 refreshes) | 34,338 | 9 | 33,153 | 9 |

Saving: 19-25 % at 2 fields, 3-5 % at 16. Single-transaction (6 blobs) capacity at 16 fields
rises from 366 to 379 votes.

## 4. Decoder: the follower's trust boundary

`decode_blobs` (Rust) and its Go mirror reject, as today: a blob count other than
`ceil(T / 4096)`, non-zero padding, a cell not below `r_bls`, ids not strictly ascending, keys not
strictly ascending or outside `[BALLOT_MIN, BALLOT_MAX]`, points not on the curve. New checks: the
header's unused bits are zero and `n_vids ≤ n_updates ≤ MAX_BATCH_SIZE + MAX_REFRESH` with
`n_vids ≤ MAX_BATCH_SIZE`; every integer-section cell is below `2^252`; the unused slots of the
last integer cell are zero. `TransitionData` loses its `accumulator` field; a separate helper
computes the new Results ballot from the old state for followers (`davinci-state` `sync.rs`), and
the guest-side builder keeps no copy of the old field.

## 5. Changes by repository

- **davinci-zkvm:** `circuit-primitives/src/da_blob.rs` (`total_cells`, `build_cells`),
  `circuit/src/kzg.rs` (cell producer, blob count), `circuit/CIRCUIT.md` §8 (layout, counts, cost
  note), `rust-sdk/src/blob.rs` (`cells`, `total_cells`, `blob_count`, `max_votes_for_cap`,
  `build_blobs`, `decode_blobs`, `TransitionData`), `go-sdk/blob.go` and its tests,
  `go-sdk/cmd/sdk-vectors/blob.go` and the regenerated `rust-sdk/testdata/blob.json`, docs that
  quote cell counts (`BENCHMARK.md`, `docs/architecture.md`, `CLAUDE.md`), the rebuilt
  `circuit/elf/circuit.elf`, and the new `BATCH_PROGRAM_VK` in `rust-sdk/src/release.rs` and
  `go-sdk/chain/release.go` (the results vk only if its ELF changes).
- **davinci-sequencer:** `state` (transition build without the accumulator, `sync.rs` recomputing
  it, blob caps), the node's `decode_blobs` users, the release pin check, tests (including the
  `ZISKEMU=1` dry runs against the new ELF), and `--batch-max` default **512** (was 1024; the
  guest limit `MAX_BATCH_SIZE` stays 1024). Until the core sub-project adds several blob
  transactions per transition, an L1-blob node still caps a batch at one transaction's 6 blobs
  (379 votes at 16 fields).
- **davinci-contracts:** only test vectors built with the Go SDK (`test/vectors/main.go`)
  regenerate; no Solidity change.
- **Other consumers:** `davinci-sdk` and `davinci-fold` pin the batch vk; they move with the core
  and SDK sub-projects. `davinci-node` (the old Go sequencer) has its own format and is untouched.

## 6. Tests

- Go vectors regenerated; Rust cells, blobs, KZG and decode checked against them byte for byte.
- Round trip and boundary cases: V or U at 0, 1, 3, 4, 5 (integer-cell boundaries), the 4096-cell
  blob boundaries, maximum batch and refresh counts, nf 1 and 16.
- Decoder negatives: each rejection in §4, including a non-zero unused integer slot, header high
  bits, `n_vids > n_updates`, an integer cell at `2^252`.
- Cheat tests in the guest (the repository's `circuit-attacker` practice): a host that feeds a
  stream with a wrong count, a packed id without its top bit, or an omitted update must not get a
  digest the guest commits to.
- Follower: the recomputed accumulator equals the guest's Results leaf on new votes, overwrites,
  refreshes, an empty first batch and a batch that only refreshes; sync rejects a stream whose
  ballots do not rebuild `rootAfter`.
- `ZISKEMU=1` dry runs in davinci-sequencer against the new ELF (the byte-exactness oracle), and
  emulator step counts before and after (the `perf-profiler` practice) for 256 and 512 votes at 2
  and 16 fields.

## 7. Release

davinci-zkvm `v0.2.0` (a wire and vk change): ELF, pins, Go SDK and docs in the same change, then
the prover image. Tagging publishes the image as `latest`, which the watchtower on the prover host
follows: the owner confirms before the tag, since the production Gnosis node on that host pins the
old vk (Gnosis is being retired). The sequencer and contracts merge with the core sub-project.
