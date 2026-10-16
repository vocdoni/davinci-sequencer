# Compact DA stream and 512-vote batches

Status: design approved in conversation by the owner on 2026-10-13 ("apply that compression and
also let's increase the vote batch to 512 from now on"); written spec audited by Fable 5.1 (APPROVE WITH CHANGES, all folded in), pending the owner's review.
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
| 1 | header: the integer `n_vids · 2^64 + n_updates` (both below 2^12; bits 128..255 are a version field pinned to zero, and every other unused bit is zero) |
| `ceil((V + U) / 4)` | the integer section, below |
| `U · 2nf` | ballots, exactly as today |
| rest | zero |

**Integer section.** The list `I = [vid_0 & (2^63 − 1), …, vid_{V−1} & (2^63 − 1), key_0, …,
key_{U−1}]`, every element below `2^63`, is packed four per cell: cell `k` is the integer
`I[4k]·2^189 + I[4k+1]·2^126 + I[4k+2]·2^63 + I[4k+3]` (first element in the most significant
position; missing elements of the last cell are zero), built with shifts. A cell is below
`2^252 < r_bls`. The vote id's top bit is implied: every vote id is in `[2^63, 2^64)`
(`consistency.rs:55-66`, `VOTE_ID_MIN = 2^63`) and a key is below `2^63` (`consistency.rs:105`,
`smt.rs:472`). The guest derives the packed id with the mask, never `vid − 2^63`, so an
out-of-namespace id (already `FAIL_CONSISTENCY`) cannot panic it in the KZG phase.

**No accumulator.** A follower holds the old Results leaf and the old slot ballots, so the new
Results ballot is `old_results + Σ_u (new_ballot_u − old_ballot_{key_u})`, with the identity as the
old ballot of an empty slot. The guest computes `net = OldResults + Σ voter_ballots −
Σ overwritten + refresh_delta` (`circuit-primitives/src/results.rs:478-484`, `refresh_delta =
Σ_j (new_j − old_j)` at `babyjubjub.rs:382-391`), and the two agree case by case:

- new vote: `new − identity` on both sides;
- overwrite: `overwritten_ballots[j]` is pinned to the tree's old leaf (CIRCUIT.md 4.4.3,
  `results.rs:446-453`), which is the ballot the follower stored under the same root;
- refresh: `new_j − old_j` with `old_j` pinned the same way (4.5.5, 4.5.6, `results.rs:459-472`);
- a slot refreshed and written in one batch is impossible (4.5.3, `smt.rs:712-714`), and the
  decoder's strictly ascending keys exclude it anyway;
- a revote of a slot refreshed in an earlier batch: the follower persisted that refresh's write
  (`state/src/sync.rs:87`);
- first batch: the genesis Results leaf is the identity ballot on both sides (`results.rs:24-26`,
  `state/src/process.rs:171`); an empty batch is not a valid Results transition (4.4.8).

The group is abelian, so order does not matter. Both sides add with the complete twisted-Edwards
law on canonical coordinates (`rust-sdk/src/crypto/babyjubjub.rs:66`; the guest's `BjjAccumulator`,
`circuit-primitives/src/babyjubjub.rs:517-531`; CIRCUIT.md 13.2), so no subgroup check is needed
on either side, and the follower's existing check of the rebuilt tree against `rootAfter`
(`sync.rs:68-74`) binds the result. A corrupted local slot ballot now fails closed at that root
check instead of surfacing at finalization. The guest still computes `net` for the Results leaf; it
only stops emitting it.

**Deniability and versioning.** `n_vids` and `n_updates` were already public (cells 0 and `V + 1`
today), and the overwrite count is a public register (`main.rs:474`, register 19); the stream still
hides which updates are overwrites, and the header changes nothing. The stream carries no format
marker beyond the version field: a deployment's batch vk identifies its format, old-format blobs
exist only under the retired registries, and each deployment has its own database, so formats
never mix.

`T = 1 + ceil((V + U) / 4) + 2nf·U`. Points stay one per cell: a packed point uses 255 bits (a
y below the BN254 modulus plus the x-parity bit) and fits a cell only because
`p_bn254 + 2^254 < r_bls`. Re-chunking the whole stream into 31-byte cells (the 2026-10-11 draft)
would make each point cost 32/31 cells and keep only 116 of the 1,185 cells saved at 16 fields
(34,222 against 33,153 for 512 steady votes).

| 512 votes | V | U | Cells today | Blobs | Compact | Blobs | Saving |
|---|---:|---:|---:|---:|---:|---:|---:|
| nf 2, first (no refresh) | 512 | 512 | 3,078 | 1 | 2,305 | 1 | 25.1 % |
| nf 2, steady, all revotes (1,024 refreshes) | 512 | 1,536 | 8,198 | 3 | 6,657 | 2 | 18.8 % |
| nf 8, steady (512 refreshes) | 512 | 1,024 | 17,938 | 5 | 16,769 | 5 | 6.5 % |
| nf 16, first | 512 | 512 | 17,442 | 5 | 16,641 | 5 | 4.6 % |
| nf 16, steady (512 refreshes) | 512 | 1,024 | 34,338 | 9 | 33,153 | 9 | 3.5 % |
| largest valid (1,024 + 2,048, nf 16) | 1,024 | 3,072 | 102,434 | 26 | 99,329 | 25 | 3.0 % |

Single-transaction (6 blobs) capacity, `max_votes_for_cap` with no overwrites / all overwrites:

| nf | today | compact |
|---:|---|---|
| 1, 2 | 1024 / 1024 | 1024 / 1024 |
| 5 | 1024 / 722 | 1024 / 792 |
| 6 | 909 / 614 | 992 / 664 |
| 8 | 701 / 472 | 750 / 501 |
| 12 | 481 / 323 | 504 / 336 |
| 16 | 366 / 245 | 379 / 253 |

**Guest cost.** Packing is shifts and sorting is unchanged. In `evaluate_barycentric`
(`kzg.rs:97-131`) a non-zero cell costs 2 `arith256_mod` and a blob about 16-17k (the omega table,
4,096 multiplications, and the batch inverse, about 3 × 4,096). 512 steady votes at 2 fields save
one blob plus 1,541 cells (about 20k calls); at 16 fields 1,185 cells (about 2.4k calls) and no
blob. Both are small against the Groth16 phase. While in `kzg.rs`, the omega table, rebuilt per
blob today (`:100`), is built once per batch.

## 4. Decoder: the follower's trust boundary

`decode_blobs` (Rust) rejects, as today: a blob count other than `ceil(T / 4096)`, non-zero
padding, ids not strictly ascending, keys not strictly ascending or outside
`[BALLOT_MIN, BALLOT_MAX]`, and points that fail decompression (bit 255 set, `y ≥ p_bn254`, off the
curve, or a non-canonical zero x: stronger than "below `r_bls`"). New checks: the header's version
field and unused bits are zero, `n_vids ≤ MAX_BATCH_SIZE` and `n_vids ≤ n_updates ≤ MAX_BATCH_SIZE
+ MAX_REFRESH`; every integer-section cell is below `2^252`; the unused slots of the last integer
cell are zero. The Go SDK has no decoder: its builder (`TransitionCells`, `BuildTransitionBlobs`)
and tests mirror the layout. `TransitionData` loses its `accumulator` field; a separate helper in
`davinci-state` computes the new Results ballot from the old state for followers (`sync.rs`). The
helper adds with the complete twisted-Edwards law on canonical coordinates and does no subgroup
check (none is needed, see §3); an occupied slot with no stored ballot is an error, not the
identity.

## 5. Changes by repository

- **davinci-zkvm:**
  - guest: `circuit-primitives/src/da_blob.rs` (`total_cells`, `build_cells`),
    `circuit/src/kzg.rs` (cell producer, blob count, module docs at `:1-9`, the omega table once
    per batch);
  - spec and docs: `circuit/CIRCUIT.md` §2 (KZGBLK text), §8 (preamble, layout, counts,
    performance note: the largest transition 26 → 25 blobs; 1024 + 1024 at 16 fields stays 17),
    §12.8 and §13.5 (all mention the "NEW net accumulator"); `BENCHMARK.md:108` (366 → 379);
    `docs/architecture.md:160-163`; `CLAUDE.md:504`; `go-sdk/README.md:100-106`;
  - Rust SDK: `rust-sdk/src/blob.rs` (`cells`, `total_cells`, `blob_count`, `max_votes_for_cap`,
    `build_blobs`, `decode_blobs`, `TransitionData`), `rust-sdk/tests/blob.rs` (including
    `six_blob_cap_table` with the values above and the `blob_count` boundary cases), the
    regenerated `rust-sdk/testdata/blob.json`;
  - Go SDK: `go-sdk/blob.go`, `blob_test.go`, `blobcap_test.go` (`MaxSingleTxBatch` table; the two
    `TransitionBlobCount` pins stay 9 and 3), `go-sdk/cmd/sdk-vectors/blob.go`,
    `go-sdk/tests/integration/election.go:904` (builder signature);
  - release: `scripts/build-guests.sh` for all three guests; the new `BATCH_PROGRAM_VK` in
    `rust-sdk/src/release.rs` and `go-sdk/chain/release.go`; the results and aggregator vks are
    refrozen only if their ELFs moved (expected: neither, since `circuit-results` and
    `circuit-aggregator` do not import `da_blob`). `input-gen` and `service` do not compute `T`.
- **davinci-sequencer:**
  - `state`: transition build without the accumulator, `sync.rs` recomputing it,
    `state/tests/sync.rs` (the tamper test that substitutes the accumulator becomes a stored-ballot
    tamper test); `capacity_estimate` (`actor.rs:1594-1598`), `select_batch` (`process.rs:338`)
    and `refresh_selection` (`transition.rs:216`) need no edit, they go through `blob_count`;
  - node: the `decode_blobs` users, the release pin check, fixtures that build
    `TransitionData { accumulator, .. }` (`sequencer/tests/{stack,l1da,web3_anvil,actor}.rs`,
    `sequencer/src/web3/{blobs.rs, l1.rs, tx.rs}` tests);
  - e2e: `demo.rs` `the_large_election_spans_blobs` and `demo/wave2.rs` (the two-blob cap at 16
    fields moves 121 → 126; the 130-vote chunk is 4,226 cells, still 2 blobs; re-run it),
    `e2e/tests/bench*.rs` blob columns;
  - docs: `docs/lifecycle.md:96`, `docs/configuration.md:72`, `docs/benchmarks.md` where it quotes
    cell counts;
  - `ZISKEMU=1` dry runs against the new ELF;
  - **batch size (confirmed by the owner):** the `--batch-max` default becomes 512, down from 1024
    (`config.rs:343` and its test at `:811`); the guest limit `MAX_BATCH_SIZE` stays 1024. 512
    must settle at every field count: the core sub-project adds several Ethereum blob transactions
    per transition (a 512 × 16 steady batch needs 9 blobs, 2 transactions) and raises the OP
    registry's grace floor so the seal budget fits a 512-vote proof. Until then an L1-blob node
    still holds a batch to one transaction's 6 blobs (504 at 12 fields, 379 at 16).
- **davinci-contracts:** no Solidity change, but `ProcessRegistry.batchProgramVK` is immutable
  (`src/ProcessRegistry.sol:90`), so the new vk needs the registry redeploy the core sub-project
  plans. `test/vectors/main.go:643` passes the accumulator to `BuildTransitionBlobs` (signature
  change) and its vectors regenerate.
- **Other consumers:** `davinci-sdk` pins the vk (`src/protocol/release.ts:14`) and only relays raw
  blobs (no cell parsing); `davinci-fold` pins the vk and imports `go-sdk/tests/integration`
  (`go.mod:19`), so the integration helper's signature change reaches it when it bumps. Both move
  with the core and SDK sub-projects. `davinci-node` (the old Go sequencer) has its own format and
  is untouched.

## 6. Tests

- Go vectors regenerated, including `V + U ≡ 0, 1, 2, 3 (mod 4)`; Rust cells, blobs, KZG and decode
  checked against them byte for byte; the property test `decode ∘ build = id`
  (`rust-sdk/tests/blob.rs:320`) kept.
- Round trip and boundary cases: V or U at 0, 1, 3, 4, 5 (integer-cell boundaries), `T` at 4096 and
  4097, maximum batch and refresh counts, nf 1 and 16; `six_blob_cap_table` and `MaxSingleTxBatch`
  with the new values.
- Decoder negatives: each rejection in §4, including a non-zero unused integer slot, a non-zero
  version field or header bit, `n_vids > n_updates`, an integer cell at `2^252`.
- Guest negatives that exist (the guest never parses the stream: `KZGBLK` carries only commitments
  and the cells are built from verified state): a commitment count other than `ceil(T / 4096)` sets
  `FAIL_KZG` (`kzg.rs:203-209`), tested at `V + U ≡ 0..3 (mod 4)` and at `T = 4096, 4097`; a vote id
  below `2^63` or a key outside `[0x10, 2^63)` keeps failing in `consistency.rs:63,105` and
  `smt.rs:472` (existing cheat tests kept). The positive oracle that binds the packing: the
  emulator's digest equals `build_blobs` for the same transition (the `ZISKEMU=1` dry run).
- Follower: the recomputed accumulator equals the guest's Results leaf on new votes, overwrites,
  refreshes, the first batch, a batch that only refreshes, and a revote of a slot refreshed in an
  earlier batch; a corrupted stored slot ballot is rejected at the root check.
- Node: the `--batch-max` default test; `the_large_election_spans_blobs`.
- Emulator step counts before and after (the `perf-profiler` practice) for 256 and 512 votes at 2
  and 16 fields.

## 7. Release

davinci-zkvm `v0.2.0` (a wire and vk change): ELF, pins, Go SDK and docs in the same change, then
the prover image. Tagging publishes the image as `latest`, which the watchtower on the prover host
follows: the owner confirms before the tag, since the production Gnosis node on that host pins the
old vk (Gnosis is being retired). The sequencer and contracts merge with the core sub-project.
