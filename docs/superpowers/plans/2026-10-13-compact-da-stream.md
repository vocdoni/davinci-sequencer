# Compact DA Stream and 512-Vote Batches Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Pack the DA stream's vote ids and slot keys four per cell behind a one-cell header, drop the Results accumulator from the stream (followers recompute it), build the KZG omega table once per batch, and make 512 votes the node's default batch, in davinci-zkvm, davinci-sequencer and davinci-contracts.

**Architecture:** The guest lays out the new stream (`circuit-primitives/src/da_blob.rs`, `circuit/src/kzg.rs`); the Go builder (`go-sdk/blob.go`) and the Rust builder and decoder (`rust-sdk/src/blob.rs`) mirror it byte for byte, bound together by Go-generated vectors, a guest-versus-builder digest check in the cheat suite and the sequencer's ziskemu dry runs. A follower in `davinci-state` rebuilds the Results ballot as `old + Σ(new − stored)` and the existing root check binds it. All three guest ELFs and their program vks change, in the same commit as their pins.

**Tech Stack:** Rust 1.95 (davinci-zkvm crates edition 2021, davinci-sequencer edition 2024), ZisK 1.3.0-alpha (`cargo-zisk`, `ziskemu` in `~/.zisk/bin`, STARK proving key in `~/.zisk/provingKey`), Go 1.25.5 with go-ethereum `kzg4844`, c-kzg 2.1, Foundry 1.8.3 in `~/.foundry/bin`.

**Spec:** `docs/superpowers/specs/2026-10-13-compact-da-stream-design.md` (davinci-sequencer). Read §3 (layout and the accumulator argument) and §4 (decoder) before Task 1.

## Global Constraints

- Header cell, verbatim from the spec: the integer `n_vids · 2^64 + n_updates` (both below 2^12; bits 128..255 are a version field pinned to zero, and every other unused bit is zero).
- Integer section, `ceil((V + U) / 4)` cells: `I = [vid_0 & (2^63 − 1), …, vid_{V−1} & (2^63 − 1), key_0, …, key_{U−1}]`, every element below `2^63`, packed four per cell: cell `k` is `I[4k]·2^189 + I[4k+1]·2^126 + I[4k+2]·2^63 + I[4k+3]` (first element in the most significant position; missing elements of the last cell are zero), built with shifts. A cell is below `2^252 < r_bls`.
- Mask rule: the packed vote id is `vid & (2^63 − 1)`, never `vid − 2^63`, so an out-of-namespace id cannot panic the guest; a decoder restores `packed | 2^63`. Keys go in as they are.
- Ballot cells: `U · 2nf`, exactly as today (`y + ((x & 1) << 254)` per point, per update in key order: `c1_0, c2_0, …, c1_{nf−1}, c2_{nf−1}`); then zero to the end of the last blob.
- `T = 1 + ceil((V + U) / 4) + 2nf·U`, `n_blobs = ceil(T / 4096)`.
- No accumulator in the stream; `TransitionData` loses `accumulator`. A follower computes `old_results + Σ_u (new_ballot_u − old_ballot_{key_u})`, the identity as the old ballot of an empty slot, with the complete twisted-Edwards law on canonical coordinates and no subgroup check; an occupied slot with no stored ballot is an error, not the identity.
- `decode_blobs` rejects, as today: a blob count other than `ceil(T / 4096)`, non-zero padding, ids not strictly ascending, keys not strictly ascending or outside `[BALLOT_MIN, BALLOT_MAX]`, points that fail decompression. New: a non-zero header version field or unused bit, `n_vids > MAX_BATCH_SIZE`, `n_updates < n_vids`, `n_updates > MAX_BATCH_SIZE + MAX_REFRESH`, an integer cell at or above `2^252`, a non-zero unused slot of the last integer cell.
- `--batch-max` defaults to 512 (was 1024); the guest's `MAX_BATCH_SIZE` stays 1024.
- No contract source change: in davinci-contracts only `test/vectors` and the forge test literals that count the fixture's votes move.
- A guest source change is rebuilt with `scripts/build-guests.sh` (all three guests), the ELFs are committed with the source, and every moved vk is refrozen in the same commit: `rust-sdk/src/release.rs` (`BATCH_PROGRAM_VK`, `RESULTS_PROGRAM_VK`), `go-sdk/chain/release.go` (`AggVK`, `BatchVK`, `ResultsVK`), `rust-sdk/tests/wire.rs::release_pins`. `ROOT_C_VADCOP_FINAL` and `ZISK_VERIFIER_CODEHASH` stay.
- Branches: davinci-zkvm `feat/compact-da-stream` from `main`; davinci-sequencer and davinci-contracts `feat/compact-da-stream` from `feat/l1-blob-da`. The sequencer and the contracts vectors path-depend on `../davinci-zkvm`, which stays on its `feat/compact-da-stream` from Task 4 on.
- Commit titles: `type(scope): lowercase title`. No AI attribution and no `Co-Authored-By` (or any other) trailer in any commit. Never push: the pushes in the last section are the owner's, after the Fable audit.
- Build and test output never in `/tmp`: cargo targets stay in each repository's `target/`, every shell exports `TMPDIR=$HOME/.cache/compact-da/tmp`, and vk setups use a fresh `ZISK_CACHE_DIR` under `~/.cache/compact-da`.
- Never run the GPU prover. Vks come from `cargo-zisk setup`, guest checks from `ziskemu`.
- In `/home/p4u/davinci-zkvm`, never read, stage or touch the untracked `SILENT-REVOTING.md`, `paper.tex`, `token.overleaf`: stage files by name, never `git add -A` or `git add .`.

## Decisions beyond the spec

- **All three guest ELFs move, so all three vks are refrozen.** The spec expected only the batch vk. Any code change in `circuit-primitives` (shared by the three guests) changes how rustc splits that crate into codegen units, which reorders the linked code of `circuit-results` and `circuit-aggregator` too. Measured on scratch builds: a trailing comment leaves `results.elf` identical, the new `da_blob.rs` (or merely deleting the module) changes it and its vk. Keeping the layout in `circuit-primitives`, as the spec says, therefore refreezes `AggVK`, `BatchVK` and `ResultsVK`.
- **Builders do not validate.** `cells`/`TransitionCells` mask vote ids and pack keys as given (OR of fixed shifts, identical in the guest, Go and Rust); only `decode_blobs` rejects malformed streams.
- **The vector generator gets its own random stream** for `blob.json` (seed `20261013`), so future blob cases cannot shift `wire_prove.json`; `wire_prove.json` changes once in Task 1. Cases `(nf, V, U)`: `(2, 5, 21)`, `(6, 40, 80)`, `(4, 9, 12)`, `(1, 3, 4)`, `(16, 1, 127)`: residues 2, 0, 1, 3, 0 and one two-blob case at `T = 4097`.
- **The recorded GPU fixture keeps its vk.** `rust-sdk/testdata/snark_batch.json` was proved on the v0.1 batch ELF and cannot be re-proved without the GPU; `wire.rs` compares its `program_vk` against that literal and keeps checking its publics.
- **Two-blob fixtures grow to stay two blobs:** `sequencer/tests/web3_anvil.rs` second transition 460 → 500 votes at nf 4; the davinci-contracts fixture's second transition 820 → 1000 votes at nf 2, with the forge literals that count it (820 → 1000, 823 → 1003, 819 → 999).
- **Dry runs move to the new boundaries:** `multi_blob_batch_accepted` (125 votes, now one blob) becomes `blob_boundary_batches_accepted` (`T = 4096` and `T = 4097`), and `two_blob_cap_batch_accepted` uses 127 votes, of which two blobs hold 126.
- **The follower helper is `ProcessState::results_after(&TransitionData) -> Result<Ballot, Error>`** (public, in `state/src/sync.rs`), using `Ballot::add`/`sub` like the leader's net in `transition.rs`.
- **No storage `SCHEMA_VERSION` bump:** no record shape changes, stored blobs are never re-decoded, and a database belongs to one deployment whose registry pins one batch vk.
- **The emulator step-count tool is committed** (`TestEmuSteps`, gated by `EMU_STEPS_OUT`), so the before and after numbers in `BENCHMARK.md` can be reproduced.
- **davinci-zkvm lands on `main` as one commit** (`git merge --squash`), per CONTRIBUTING's "change every copy in one commit"; the feature branch keeps three reviewable commits.

## Review Focus

1. **Integer slots at their maxima** (a vote id of `u64::MAX`, a key of `BALLOT_MAX`): a packed slot must never bleed into its neighbour or push the cell to `2^252`. Tests: Task 1 `integer_cells_pack_four_per_cell` and `TestPack4MatchesBigInt`, Task 2 `pack4_layout`.
2. **Transitions of exactly 4096 and 4097 cells**: guest and both SDKs must agree on one versus two blobs, and the guest must reject a commitment count one off. Tests: Task 1 `layout_boundaries_round_trip` and `TestBuildTransitionBlobsMultipleBlobs`, Task 5 `blob_boundary_batches_accepted`.
3. **A follower whose stored ballot is wrong or missing** (a corrupted database, or a revote of a slot refreshed one batch earlier): it must stop at the root check or with an error, never apply the transition or treat the slot as empty. Tests: Task 4 `corrupted_stored_ballot_fails_the_root_check`, `occupied_slot_without_a_ballot_is_an_error`, `results_after_matches_the_guest_leaf`.
4. **Hostile blob content from another sequencer** (version bits, more ids than updates, counts above the limits, bits in unused slots, an integer cell at `2^252`, an extra blob): `decode_blobs` returns an error and never panics. Test: Task 1 `decode_rejects_malformed_blobs`.
5. **The 512 default on a one-transaction chain at 16 fields**: the node must seal what six blobs hold (379 steady votes) and keep the rest queued, never build an unsettleable transition. Test: Task 6 `a_512_vote_queue_at_sixteen_fields_seals_what_six_blobs_hold`.

---

## Shell setup (every task, every new shell)

```bash
mkdir -p $HOME/.cache/compact-da/tmp
export TMPDIR=$HOME/.cache/compact-da/tmp
export PATH=$HOME/.zisk/bin:$HOME/.foundry/bin:$PATH
```

Agents whose shell does not keep state prefix every command with these exports.

## File Structure

davinci-zkvm (`/home/p4u/davinci-zkvm`, branch `feat/compact-da-stream` from `main` at 18f1639):
- Modify `go-sdk/blob.go`: `TransitionCells` and `BuildTransitionBlobs` lose the accumulator; `headerCell`, `pack4`, `low63` replace `encU64`; `totalCells` takes the new formula.
- Modify `go-sdk/blob_test.go`, `go-sdk/blobcap_test.go`: layout, packing and capacity tests.
- Modify `go-sdk/tests/integration/election.go`: the harness stash and `BuildKZGBlock` without the accumulator.
- Modify `go-sdk/cmd/sdk-vectors/blob.go`: own random stream, new cases, no accumulator.
- Modify `rust-sdk/src/blob.rs`: layout, `TransitionData`, decoder.
- Replace `rust-sdk/tests/blob.rs`.
- Regenerate `rust-sdk/testdata/blob.json`, `rust-sdk/testdata/wire_prove.json`.
- Modify `circuit-primitives/src/da_blob.rs`: `header`, `pack4`, `LOW63`, `total_cells`, `build_cells`, tests.
- Modify `circuit/src/kzg.rs`: cell producer call, omega table once per batch, module docs.
- Modify `circuit/src/main.rs`: `verify_kzg` call and comments.
- Rebuild `circuit/elf/circuit.elf`, `circuit-aggregator/elf/aggregator.elf`, `circuit-results/elf/results.elf`.
- Modify `rust-sdk/src/release.rs`, `go-sdk/chain/release.go`, `rust-sdk/tests/wire.rs`: pins.
- Modify `go-sdk/tests/integration/cheat_test.go`: `TestCheatKZGBlobCount`.
- Create `go-sdk/tests/integration/emusteps_test.go`: `TestEmuSteps`.
- Modify `circuit/CIRCUIT.md` (§2 KZGBLK, §8, §12.8, §13.5), `BENCHMARK.md`, `docs/architecture.md`, `go-sdk/README.md`; `CLAUDE.md` is gitignored (edit it, it is not committed).

davinci-sequencer (`/home/p4u/davinci-sequencer`, branch `feat/compact-da-stream` from `feat/l1-blob-da` at cd7f2bb):
- Modify `state/src/sync.rs` (`results_after`, `apply`), `state/src/transition.rs` (`TransitionData` literal).
- Modify `state/tests/sync.rs`, `state/tests/transition.rs`, `state/tests/dryrun.rs`.
- Modify fixtures: `sequencer/tests/{stack,l1da,web3_anvil}.rs`, `sequencer/src/web3/{blobs,l1,tx}.rs`.
- Modify `sequencer/src/config.rs`, `.env.example`, `docs/configuration.md`, `docs/lifecycle.md`.
- Modify `e2e/src/demo.rs`, `e2e/src/demo/wave2.rs`.

davinci-contracts (`/home/p4u/davinci-contracts`, branch `feat/compact-da-stream` from `feat/l1-blob-da` at c72accc):
- Modify `test/vectors/main.go`; regenerate `test/vectors/transition.json`.
- Modify `test/Transition.t.sol`, `test/Grace.t.sol` (vote-count literals only).

Not changed, checked: `sequencer/src/actor.rs` (`capacity_estimate` and the `decode_blobs` caller go through `blob_count` and `apply_synced`), `state/src/process.rs` (`select_batch`), `state/src/transition.rs::refresh_selection`, `sequencer/src/web3/release.rs` (reads the SDK pins), `e2e/tests/bench*.rs` and `e2e/tests/throughput.rs` (blob columns are read from the transactions at run time; `bench.rs`'s "64 at nf=16, two blobs when steady" stays true: 4,145 cells), `docs/benchmarks.md` (quotes no cell counts, and every blob count it lists is the same under the compact stream), `solidity/DavinciSettlement.sol` and the registry (read only commitments, the pair digest and the openings).

---

### Task 1: Compact layout in the Go and Rust SDKs, with the shared vectors

The Go builder and the Rust builder/decoder change together, because the Rust tests replay the Go vectors and CI fails on stale vectors.

**Files:**
- Modify: `go-sdk/blob.go:99-191` (`TransitionCells`, `BuildTransitionBlobs`), `:343-353` (`encU64`, `totalCells`)
- Modify: `go-sdk/blob_test.go:3-11`, `:33-190`, `:200-210`, `:266-299`; `go-sdk/blobcap_test.go:5-9`
- Modify: `go-sdk/tests/integration/election.go:119-136`, `:678-682`, `:878-881`, `:904-910`
- Modify: `go-sdk/cmd/sdk-vectors/blob.go:3-18`, `:42-104`
- Modify: `rust-sdk/src/blob.rs:1-122`, `:276-284`, `:301-376`
- Replace: `rust-sdk/tests/blob.rs`
- Regenerate: `rust-sdk/testdata/blob.json`, `rust-sdk/testdata/wire_prove.json`
- Docs: `go-sdk/README.md:101`, `BENCHMARK.md:109` and `:118`, `docs/architecture.md:160-163`, `CLAUDE.md:505` (gitignored)

**Interfaces:**
- Produces (Go, package `davinci`): `func TransitionCells(numFields int, voteIDs []uint64, updates []SlotUpdate) ([][32]byte, error)`; `func BuildTransitionBlobs(numFields int, processIDBE32, rootBeforeBE32 [32]byte, voteIDs []uint64, updates []SlotUpdate) (*TransitionBlobs, error)`; `TransitionBlobCount` and `MaxSingleTxBatch` keep their signatures with new values (`MaxSingleTxBatch`: 1024 up to 5 fields, 992 at 6, 750 at 8, 504 at 12, 379 at 16).
- Produces (Rust, `davinci_zkvm_sdk::blob`): `pub struct TransitionData { pub vote_ids: Vec<u64>, pub updates: Vec<(u64, Ballot)>, pub num_fields: u8 }`; `cells`, `blob_count`, `max_votes_for_cap`, `blobs_from_cells`, `build_blobs`, `decode_blobs` keep their signatures.

- [ ] **Step 1: Create the branch**

```bash
cd /home/p4u/davinci-zkvm
git status --short          # expect only the three untracked files you must not touch
git checkout main && git checkout -b feat/compact-da-stream
```

- [ ] **Step 2: Write the failing Go tests**

In `go-sdk/blob_test.go`, add `"math/big"` to the imports (between `"encoding/hex"` and `"testing"`), then replace everything from `func TestTransitionCellsMirrorsGuestLayout` down to the end of `TestTotalCellsFormula` (lines 33-190) with:

```go
func TestTransitionCellsMirrorsGuestLayout(t *testing.T) {
	nf := 2
	vids := []uint64{1<<63 | 1}
	ballot := identityBallot()
	updates := []SlotUpdate{
		{Key: 0x11, Ballot: ballot},
		{Key: 0x10, Ballot: ballot},
	}
	cells, err := TransitionCells(nf, vids, updates)
	if err != nil {
		t.Fatalf("TransitionCells: %v", err)
	}
	// T = 1 + ceil((1 + 2)/4) + 2*2*2 = 10
	if len(cells) != 10 {
		t.Fatalf("len(cells) = %d, want 10", len(cells))
	}
	if got := hex.EncodeToString(cells[0][:]); got != "0000000000000000000000000000000000000000000000010000000000000002" {
		t.Errorf("header = %s", got)
	}
	// vid & (2^63-1) = 1 on top, then the keys ascending, one slot unused.
	if got := hex.EncodeToString(cells[1][:]); got != "0000000000000000200000000000000400000000000000088000000000000000" {
		t.Errorf("integer cell = %s", got)
	}
	// Each identity pack cell = enc_u64(1) (TE identity packs to 1).
	oneCell := encU64BE32(1)
	for i := 2; i < 10; i++ {
		if cells[i] != oneCell {
			t.Errorf("pack cell %d = %x", i, cells[i])
		}
	}
}

func TestPack4MatchesBigInt(t *testing.T) {
	const m = 1<<63 - 1
	for _, e := range [][4]uint64{{0, 0, 0, 0}, {1, 0x10, 0x11, m}, {m, m, m, m}, {m, 0, m, 0}, {0, m, 0, m}, {5, m - 2, 0x11, 0x30}} {
		want := new(big.Int)
		for _, x := range e {
			want.Lsh(want, 63)
			want.Or(want, new(big.Int).SetUint64(x))
		}
		var w [32]byte
		want.FillBytes(w[:])
		if got := pack4(e[0], e[1], e[2], e[3]); got != w {
			t.Errorf("pack4(%x) = %x, want %x", e, got, w)
		}
		if w[0]>>4 != 0 {
			t.Errorf("pack4(%x) at or above 2^252", e)
		}
	}
}

func TestTransitionCellsSortsInputs(t *testing.T) {
	nf := 1
	// Unsorted inputs: vids reverse, updates reverse.
	vids := []uint64{1<<63 | 9, 1<<63 | 5, 1<<63 | 3, 1<<63 | 1}
	ballot := identityBallot()
	updates := []SlotUpdate{
		{Key: 0x20, Ballot: ballot},
		{Key: 0x10, Ballot: ballot},
	}
	cells, err := TransitionCells(nf, vids, updates)
	if err != nil {
		t.Fatalf("TransitionCells: %v", err)
	}
	// Vote ids ascending fill the first integer cell, the keys the second.
	if cells[1] != pack4(1, 3, 5, 9) {
		t.Errorf("vote id cell = %x", cells[1])
	}
	if cells[2] != pack4(0x10, 0x20, 0, 0) {
		t.Errorf("key cell = %x", cells[2])
	}
	if len(cells) != 1+2+2*2 {
		t.Errorf("len(cells) = %d", len(cells))
	}
}

func TestTransitionCellsMasksVoteIDs(t *testing.T) {
	// The guest packs vid & (2^63-1), so an id without the top bit (which
	// it rejects elsewhere) lays out like the same id with it.
	updates := []SlotUpdate{{Key: 0x10, Ballot: identityBallot()}}
	low, err := TransitionCells(1, []uint64{5}, updates)
	if err != nil {
		t.Fatal(err)
	}
	high, err := TransitionCells(1, []uint64{1<<63 | 5}, updates)
	if err != nil {
		t.Fatal(err)
	}
	if !bytes.Equal(flat(low), flat(high)) {
		t.Errorf("masked vote id lays out differently")
	}
}

func flat(cells [][32]byte) []byte {
	var out []byte
	for _, c := range cells {
		out = append(out, c[:]...)
	}
	return out
}

func TestTransitionCellsStableByKey(t *testing.T) {
	nf := 1
	// Two updates with the same key: input order must be preserved (stable).
	ballot0 := identityBallot()
	ballot1 := identityBallot()
	// Make ballot1 distinguishable by using a non-identity y for field 0 c1.
	ballot1[1] = "0x0000000000000000000000000000000000000000000000000000000000000002"
	updates := []SlotUpdate{
		{Key: 0x42, Ballot: ballot0},
		{Key: 0x42, Ballot: ballot1},
	}
	cells, err := TransitionCells(nf, nil, updates)
	if err != nil {
		t.Fatalf("TransitionCells: %v", err)
	}
	// Layout: [header, keys, c1_0, c2_0, c1_1, c2_1].
	if got0 := cells[2]; got0 != encU64BE32(1) {
		t.Errorf("first (ballot0) c1 pack = %x, want enc(1)", got0)
	}
	if got1 := cells[4]; got1 != encU64BE32(2) {
		t.Errorf("second (ballot1) c1 pack = %x, want enc(2)", got1)
	}
}

func TestPackTeHexIdentityAndParity(t *testing.T) {
	// TE identity (0,1) packs to enc_u64(1).
	got, err := packTeHex("0x00", "0x01")
	if err != nil {
		t.Fatalf("packTeHex identity: %v", err)
	}
	if got != encU64BE32(1) {
		t.Errorf("identity pack = %x, want enc(1)", got)
	}

	// x odd → bit 254 set.
	got, err = packTeHex("0x01", "0x01")
	if err != nil {
		t.Fatalf("packTeHex x-odd: %v", err)
	}
	want := encU64BE32(1)
	want[0] |= 0x40
	if got != want {
		t.Errorf("x-odd pack = %x, want %x", got, want)
	}

	// x even → bit 254 clear.
	got, err = packTeHex("0x02", "0x01")
	if err != nil {
		t.Fatalf("packTeHex x-even: %v", err)
	}
	if got != encU64BE32(1) {
		t.Errorf("x-even pack = %x, want enc(1)", got)
	}
}

func TestTotalCellsFormula(t *testing.T) {
	cases := []struct {
		nVids, nUpd, nf int
		want            int
	}{
		{0, 0, 1, 1},
		{1, 2, 2, 1 + 1 + 2*2*2},
		{0, 4, 1, 1 + 1 + 2*4},
		{1, 4, 1, 1 + 2 + 2*4},
		{10, 128, 16, 1 + 35 + 2*16*128},
		{126, 126, 16, 4096},
		{1, 127, 16, 4097},
	}
	for _, c := range cases {
		if got := totalCells(c.nVids, c.nUpd, c.nf); got != c.want {
			t.Errorf("totalCells(%d,%d,%d) = %d, want %d", c.nVids, c.nUpd, c.nf, got, c.want)
		}
	}
}
```

In `TestBuildTransitionBlobsSingleBlob` (lines 199-210), use vote ids in the vote-id namespace and the new call:

```go
	nf := 2
	vids := []uint64{1<<63 | 1, 1<<63 | 2}
	ballot := identityBallot()
	updates := []SlotUpdate{
		{Key: 0x10, Ballot: ballot},
		{Key: 0x11, Ballot: ballot},
	}
	var pid, rhb [32]byte
	pid[31] = 0x42
	rhb[31] = 0x01

	tb, err := BuildTransitionBlobs(nf, pid, rhb, vids, updates)
```

Replace `TestBuildTransitionBlobsMultipleBlobs` and its comment (lines 266-299) with:

```go
// TestBuildTransitionBlobsMultipleBlobs crosses the blob boundary at 16
// fields: 126 votes fill one blob exactly (T = 4096), one vote with 126
// refreshes puts a single cell in a second blob (T = 4097).
func TestBuildTransitionBlobsMultipleBlobs(t *testing.T) {
	nf := 16
	ballot := identityBallot()
	for _, c := range []struct{ nVids, nUpd, blobs int }{{126, 126, 1}, {1, 127, 2}} {
		vids := make([]uint64, c.nVids)
		for i := range vids {
			vids[i] = 1<<63 | uint64(i)
		}
		updates := make([]SlotUpdate, c.nUpd)
		for i := range updates {
			updates[i] = SlotUpdate{Key: uint64(0x10 + i), Ballot: ballot}
		}
		var pid, rhb [32]byte
		rhb[31] = 0x01
		tb, err := BuildTransitionBlobs(nf, pid, rhb, vids, updates)
		if err != nil {
			t.Fatalf("BuildTransitionBlobs: %v", err)
		}
		if len(tb.Blobs) != c.blobs || TransitionBlobCount(c.nVids, c.nUpd, nf) != c.blobs {
			t.Fatalf("n_blobs = %d, want %d (T=%d)", len(tb.Blobs), c.blobs, totalCells(c.nVids, c.nUpd, nf))
		}
		for b := range tb.Blobs {
			var point kzg4844.Point
			copy(point[:], tb.Zs[b][:])
			var claim kzg4844.Claim
			copy(claim[:], tb.Ys[b][:])
			if err := kzg4844.VerifyProof(tb.Commitments[b], point, claim, tb.Proofs[b]); err != nil {
				t.Errorf("kzg VerifyProof blob %d: %v", b, err)
			}
		}
	}
}
```

In `go-sdk/blobcap_test.go`, replace the comment and the table (lines 5-9):

```go
// TestMaxSingleTxBatch pins the single-transaction capacity table: a steady
// batch of n votes carries n refreshes, each update is 2*nf point cells plus
// a quarter of an integer cell, six blobs hold 24576 cells.
func TestMaxSingleTxBatch(t *testing.T) {
	want := map[int]int{1: 1024, 2: 1024, 4: 1024, 5: 1024, 6: 992, 8: 750, 12: 504, 16: 379}
```

The two `TransitionBlobCount` pins below it (9 and 3) stay.

- [ ] **Step 3: Run the Go tests to see them fail**

Run: `cd /home/p4u/davinci-zkvm/go-sdk && go test . 2>&1 | head -5`
Expected: build failure, `too many arguments in call to TransitionCells` / `undefined: pack4`.

- [ ] **Step 4: Implement the Go layout**

In `go-sdk/blob.go`, replace `TransitionCells` with its doc comment (lines 99-176):

```go
// TransitionCells rebuilds the DA blob cell stream, byte-identical to the
// guest's circuit_primitives::da_blob::build_cells:
//
//	header               nVids*2^64 + nUpdates (bits 128 and up zero)
//	ceil((V+U)/4) cells  [vid & (2^63-1) ..., key ...], four per cell, the
//	                     first in the top 63-bit slot
//	U*2*numFields cells  per update in key order: pack(c1_0) pack(c2_0) ...
//
// The caller passes:
//   - numFields: the election's declared num_fields (1..=NumFields);
//   - voteIDs: the batch's new vote-id chain first-limbs (u64), any order;
//     a sorted copy is laid out;
//   - updates: (key, ballot) pairs for every batch ballot + every silent
//     refresh, in ballot-chain-then-refresh order; stable-sorted by key.
//     Each Ballot must be exactly BallotFields big-endian hex coords, but
//     only the first 4·numFields entries are consumed.
//
// The Results accumulator is not in the stream: a follower recomputes it
// from the ballots it already holds. The last blob is NOT padded here — pad
// up to cellsPerBlob when packing into blob buffers. Kept as its own
// function so tests can lock the byte layout without a KZG trusted setup.
func TransitionCells(numFields int, voteIDs []uint64, updates []SlotUpdate) ([][32]byte, error) {
	if numFields < 1 || numFields > NumFields {
		return nil, fmt.Errorf("num_fields out of range: got %d, want 1..=%d", numFields, NumFields)
	}
	for i, u := range updates {
		if len(u.Ballot) != BallotFields {
			return nil, fmt.Errorf("updates[%d].Ballot must have %d coords, got %d", i, BallotFields, len(u.Ballot))
		}
	}

	// Sort inputs the same way the guest does.
	sortedVIDs := append([]uint64(nil), voteIDs...)
	sortUint64Asc(sortedVIDs)

	sortedUpdates := append([]SlotUpdate(nil), updates...)
	stableSortByKey(sortedUpdates)

	t := totalCells(len(sortedVIDs), len(sortedUpdates), numFields)
	cells := make([][32]byte, 0, t)

	cells = append(cells, headerCell(len(sortedVIDs), len(sortedUpdates)))
	ints := make([]uint64, 0, len(sortedVIDs)+len(sortedUpdates)+3)
	for _, v := range sortedVIDs {
		ints = append(ints, v&low63)
	}
	for _, u := range sortedUpdates {
		ints = append(ints, u.Key)
	}
	for len(ints)%4 != 0 {
		ints = append(ints, 0)
	}
	for i := 0; i < len(ints); i += 4 {
		cells = append(cells, pack4(ints[i], ints[i+1], ints[i+2], ints[i+3]))
	}
	for i, u := range sortedUpdates {
		for f := 0; f < numFields; f++ {
			base := f * 4
			c, err := packTeHex(u.Ballot[base], u.Ballot[base+1])
			if err != nil {
				return nil, fmt.Errorf("updates[%d] field %d c1: %w", i, f, err)
			}
			cells = append(cells, c)
			c, err = packTeHex(u.Ballot[base+2], u.Ballot[base+3])
			if err != nil {
				return nil, fmt.Errorf("updates[%d] field %d c2: %w", i, f, err)
			}
			cells = append(cells, c)
		}
	}
	if len(cells) != t {
		return nil, fmt.Errorf("internal: expected %d cells, got %d", t, len(cells))
	}
	return cells, nil
}
```

In `BuildTransitionBlobs` (lines 184-191), drop the parameter and the argument:

```go
func BuildTransitionBlobs(
	numFields int,
	processIDBE32, rootBeforeBE32 [32]byte,
	voteIDs []uint64,
	updates []SlotUpdate,
) (*TransitionBlobs, error) {
	cells, err := TransitionCells(numFields, voteIDs, updates)
```

Replace `encU64` and `totalCells` (lines 343-353) with:

```go
// low63 masks a vote id to its packed form (its top bit is implied); it is
// also every integer slot's width.
const low63 = 1<<63 - 1

// headerCell is nVids*2^64 + nUpdates as a 32-byte big-endian cell.
func headerCell(nVids, nUpdates int) [32]byte {
	var out [32]byte
	binary.BigEndian.PutUint64(out[16:24], uint64(nVids))
	binary.BigEndian.PutUint64(out[24:32], uint64(nUpdates))
	return out
}

// pack4 is a*2^189 + b*2^126 + c*2^63 + d for integers below 2^63, built
// from the cell's four 64-bit words. Mirrors da_blob::pack4.
func pack4(a, b, c, d uint64) [32]byte {
	var out [32]byte
	binary.BigEndian.PutUint64(out[0:8], a>>3)
	binary.BigEndian.PutUint64(out[8:16], b>>2|a<<61)
	binary.BigEndian.PutUint64(out[16:24], c>>1|b<<62)
	binary.BigEndian.PutUint64(out[24:32], d|c<<63)
	return out
}

// totalCells matches da_blob::total_cells: 1 + ceil((V+U)/4) + 2*nf*U.
func totalCells(nVids, nUpdates, nf int) int {
	return 1 + (nVids+nUpdates+3)/4 + 2*nf*nUpdates
}
```

- [ ] **Step 5: Update the harness and the vector generator to the new signature**

In `go-sdk/tests/integration/election.go`, the stash comment (lines 119-122), the struct (lines 132-136), its literal (lines 678-682), the `BuildKZGBlock` comment (lines 878-881) and the call (lines 904-910) become:

```go
	// lastDA holds the DA blob inputs produced by BuildStateBlock for this
	// batch: vote id first-limbs and every (key, ballot) update — batch
	// ballots then silent refreshes. BuildKZGBlock reads them to build the
	// transition blobs.
```

```go
type daBatchState struct {
	VoteIDs []uint64
	Updates []davinci.SlotUpdate
}
```

```go
	da := &daBatchState{
		VoteIDs: make([]uint64, n),
		Updates: make([]davinci.SlotUpdate, 0, n+len(items)),
	}
```

```go
// BuildKZGBlock builds the DA blob binding for the batch that BuildStateBlock
// just applied. It rebuilds the exact cell stream the guest constructs from
// verified state (vote ids, batch ballot updates and silent-refresh updates),
// splits it into EIP-4844 blobs, commits and opens
// each at z_b = SHA-256(processID || rootBefore || commitment_b) mod r_bls.
```

```go
	tb, err := davinci.BuildTransitionBlobs(
		e.NumFields,
		pid32, rhb32,
		e.lastDA.VoteIDs,
		e.lastDA.Updates,
	)
```

`newResults` stays in use above (`e.Results = newResults`); only the stash loses it.

In `go-sdk/cmd/sdk-vectors/blob.go`, add `mrand "math/rand"` to the imports (after `"math/big"`), and change `blobMain`, `blobVec` and the head and body of `blobVectors`:

```go
func blobMain() {
	// Own stream, so the blob cases can change without moving wire_prove.json.
	saved := rng
	rng = mrand.New(mrand.NewSource(20261013))
	write("blob.json", blobVectors())
	rng = saved
	write("wire_prove.json", wireProveFixture())
}
```

Delete the `Accumulator []string \`json:"accumulator"\`` field from `blobVec`. Then:

```go
// blobVectors covers V + U = 2, 0, 1 and 3 (mod 4), so every fill of the last
// integer cell, and T = 4097: one cell in a second blob.
func blobVectors() []blobVec {
	var out []blobVec
	for _, c := range []struct{ nf, nVids, nUpdates int }{
		{2, 5, 21}, {6, 40, 80}, {4, 9, 12}, {1, 3, 4}, {16, 1, 127},
	} {
```

In the loop body delete `acc := teBallotHex(c.nf)`, and change the build call and the literal:

```go
		tb, err := davinci.BuildTransitionBlobs(c.nf, pidBE, rootBE, vids, updates)
		must(err)
		v := blobVec{Nf: c.nf, ProcessID: dec(pid), RootBefore: hex.EncodeToString(root), VoteIDs: vids,
			Digest: hex.EncodeToString(tb.Digest[:])}
```

- [ ] **Step 6: Run the Go checks**

Run: `cd /home/p4u/davinci-zkvm/go-sdk && gofmt -l . && go vet ./... && go build ./tests/integration/... && go test $(go list ./... | grep -v /tests/integration) 2>&1 | grep -v 'no test files'`
Expected: `gofmt -l` prints nothing, vet is clean, every package `ok`, including `github.com/vocdoni/davinci-zkvm/go-sdk`.

- [ ] **Step 7: Write the failing Rust tests**

Replace `rust-sdk/tests/blob.rs` with:

```rust
//! DA blob layout, KZG commitments/openings and the decoder, against go-sdk
//! `BuildTransitionBlobs` vectors.

mod common;

use common::*;
use davinci_zkvm_sdk::ballot::Ballot;
use davinci_zkvm_sdk::blob::*;
use davinci_zkvm_sdk::crypto::babyjubjub::Point;
use davinci_zkvm_sdk::crypto::elgamal::Ciphertext;
use davinci_zkvm_sdk::crypto::field::{fr_from_be, Fr};
use davinci_zkvm_sdk::limits::{
    BALLOT_MAX, BALLOT_MIN, MAX_BATCH_SIZE, MAX_REFRESH, TX_BLOB_CAP, VOTE_ID_MIN,
};
use proptest::prelude::*;
use serde_json::Value;

fn be_fr(h: &str) -> Fr {
    let mut b = [0u8; 32];
    hex::decode_to_slice(h.trim_start_matches("0x"), &mut b).unwrap();
    fr_from_be(&b).unwrap()
}

fn be_ballot(v: &Value) -> Ballot {
    let coords: Vec<Fr> = v.as_array().unwrap().iter().map(|h| be_fr(s(h))).collect();
    Ballot::from_coords(&coords.try_into().unwrap()).unwrap()
}

fn transition(v: &Value) -> TransitionData {
    TransitionData {
        vote_ids: v["vote_ids"].as_array().unwrap().iter().map(u64v).collect(),
        updates: v["updates"]
            .as_array()
            .unwrap()
            .iter()
            .map(|u| (u64v(&u["key"]), be_ballot(&u["ballot"])))
            .collect(),
        num_fields: u64v(&v["nf"]) as u8,
    }
}

fn sorted(t: &TransitionData) -> TransitionData {
    let mut t = t.clone();
    t.vote_ids.sort_unstable();
    t.updates.sort_by_key(|(k, _)| *k);
    t
}

fn hexes(v: &Value) -> Vec<String> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|x| s(x).to_string())
        .collect()
}

#[test]
fn blobs_match_go_kzg4844() {
    let v = load("blob.json");
    let mut multi = false;
    let mut residues = [false; 4];
    for case in v.as_array().unwrap() {
        let t = transition(case);
        residues[(t.vote_ids.len() + t.updates.len()) % 4] = true;
        let c: Vec<String> = cells(&t).iter().map(hex::encode).collect();
        assert_eq!(c, hexes(&case["cells"]), "cells nf={}", t.num_fields);
        let pid = fr(&case["process_id"]);
        let root = hex32(&case["root_before"]);
        let tb = build_blobs(&t, &pid, &root).unwrap();
        let n = tb.blobs.len();
        multi |= n > 1;
        assert_eq!(
            n,
            blob_count(t.vote_ids.len(), t.updates.len(), t.num_fields)
        );
        assert_eq!(
            tb.commitments.iter().map(hex::encode).collect::<Vec<_>>(),
            hexes(&case["commitments"])
        );
        assert_eq!(
            tb.zs.iter().map(hex::encode).collect::<Vec<_>>(),
            hexes(&case["zs"])
        );
        assert_eq!(
            tb.ys.iter().map(hex::encode).collect::<Vec<_>>(),
            hexes(&case["ys"])
        );
        assert_eq!(
            tb.proofs.iter().map(hex::encode).collect::<Vec<_>>(),
            hexes(&case["proofs"])
        );
        assert_eq!(
            tb.versioned_hashes
                .iter()
                .map(hex::encode)
                .collect::<Vec<_>>(),
            hexes(&case["versioned_hashes"])
        );
        assert_eq!(hex::encode(tb.digest), s(&case["digest"]));
        assert_eq!(tb.blobs, blobs_from_cells(&cells(&t)).unwrap());
        assert_eq!(decode_blobs(&tb.blobs, t.num_fields).unwrap(), sorted(&t));
        for (b, vh) in tb.blobs.iter().zip(&tb.versioned_hashes) {
            assert_eq!(verify_blob_commitment(b, vh).unwrap().len(), 48);
        }
    }
    assert!(multi, "no multi-blob vector");
    assert_eq!(residues, [true; 4], "V + U must cover every residue mod 4");
}

#[test]
fn verify_blob_commitment_rejects_changes() {
    let v = load("blob.json");
    let t = transition(&v[0]);
    let tb = build_blobs(&t, &fr(&v[0]["process_id"]), &hex32(&v[0]["root_before"])).unwrap();
    let mut b = tb.blobs[0].clone();
    b[31] ^= 1;
    assert!(verify_blob_commitment(&b, &tb.versioned_hashes[0]).is_err());
    let mut vh = tb.versioned_hashes[0];
    vh[5] ^= 1;
    assert!(verify_blob_commitment(&tb.blobs[0], &vh).is_err());
    // A cell >= r_bls is not a valid blob at all.
    let mut b = tb.blobs[0].clone();
    b[..32].fill(0xff);
    assert!(verify_blob_commitment(&b, &tb.versioned_hashes[0]).is_err());
}

// Identity-ballot transition with `v` vote ids and `u` updates.
fn plain(v: u64, u: u64, nf: u8) -> TransitionData {
    TransitionData {
        vote_ids: (0..v).map(|i| VOTE_ID_MIN + i).collect(),
        updates: (0..u)
            .map(|i| (BALLOT_MIN + i, Ballot::identity()))
            .collect(),
        num_fields: nf,
    }
}

#[test]
fn integer_cells_pack_four_per_cell() {
    // One vote id, three keys: one integer cell, first element on top.
    let t = TransitionData {
        vote_ids: vec![VOTE_ID_MIN | 1],
        updates: vec![
            (BALLOT_MAX, Ballot::identity()),
            (0x11, Ballot::identity()),
            (0x10, Ballot::identity()),
        ],
        num_fields: 1,
    };
    let c = cells(&t);
    assert_eq!(c.len(), 1 + 1 + 3 * 2);
    assert_eq!(
        hex::encode(c[0]),
        "0000000000000000000000000000000000000000000000010000000000000003"
    );
    assert_eq!(
        hex::encode(c[1]),
        "000000000000000020000000000000040000000000000008ffffffffffffffff"
    );
    // Every slot at its maximum stays below 2^252.
    let t = TransitionData {
        vote_ids: vec![u64::MAX],
        updates: vec![
            (BALLOT_MAX - 2, Ballot::identity()),
            (BALLOT_MAX - 1, Ballot::identity()),
            (BALLOT_MAX, Ballot::identity()),
        ],
        num_fields: 1,
    };
    assert_eq!(
        hex::encode(cells(&t)[1]),
        "0fffffffffffffffffffffffffffffff7fffffffffffffff7fffffffffffffff"
    );
    assert_eq!(
        decode_blobs(&blobs_from_cells(&cells(&t)).unwrap(), 1).unwrap(),
        t
    );
}

#[test]
fn vote_ids_pack_without_their_top_bit() {
    // The guest masks the id, so an id below 2^63 (which it rejects
    // elsewhere) packs like the same id with the top bit set.
    let mut low = plain(1, 1, 1);
    low.vote_ids[0] = 5;
    let mut high = plain(1, 1, 1);
    high.vote_ids[0] = VOTE_ID_MIN | 5;
    assert_eq!(cells(&low), cells(&high));
}

#[test]
fn layout_boundaries_round_trip() {
    // V and U at the integer-cell edges, at 1 and 16 fields.
    for nf in [1u8, 16] {
        for u in [0u64, 1, 3, 4, 5] {
            for v in [0u64, 1, 3, 4, 5].into_iter().filter(|v| *v <= u) {
                let t = plain(v, u, nf);
                let want = 1 + (v + u).div_ceil(4) + 2 * nf as u64 * u;
                assert_eq!(cells(&t).len() as u64, want, "v {v} u {u} nf {nf}");
                let blobs = blobs_from_cells(&cells(&t)).unwrap();
                assert_eq!(decode_blobs(&blobs, nf).unwrap(), t, "v {v} u {u} nf {nf}");
            }
        }
    }
    // T = 4096 fills one blob; T = 4097 puts one cell in a second.
    for (v, u, n) in [(126, 126, 1), (1, 127, 2)] {
        let t = plain(v, u, 16);
        assert_eq!(cells(&t).len(), 4095 + n);
        assert_eq!(blob_count(v as usize, u as usize, 16), n);
        let tb = build_blobs(&t, &Fr::from(3u64), &[5u8; 32]).unwrap();
        assert_eq!(tb.blobs.len(), n);
        assert_eq!(decode_blobs(&tb.blobs, 16).unwrap(), t);
    }
    // The largest batch with the most refreshes.
    let (v, u) = (MAX_BATCH_SIZE as u64, (MAX_BATCH_SIZE + MAX_REFRESH) as u64);
    let t = plain(v, u, 1);
    let blobs = blobs_from_cells(&cells(&t)).unwrap();
    assert_eq!(blobs.len(), 2);
    assert_eq!(decode_blobs(&blobs, 1).unwrap(), t);
    assert_eq!(blob_count(v as usize, u as usize, 16), 25);
}

// Base transition for the rejection tests: nf 2, 2 vote ids, 3 updates, so
// the integer section is two cells and the second has three unused slots.
fn base() -> (TransitionData, Vec<[u8; 32]>) {
    let g = Point::generator();
    let p = |k: u64| g.mul(&ark_ff::BigInt::from(k));
    let ct = |k: u64| Ciphertext {
        c1: p(k),
        c2: p(k + 1),
    };
    let ballot = |k: u64| {
        let mut b = Ballot::identity();
        b.0[0] = ct(k);
        b.0[1] = ct(k + 2);
        b
    };
    let t = TransitionData {
        vote_ids: vec![u64::MAX, VOTE_ID_MIN | 5],
        updates: vec![
            (0x30, ballot(3)),
            (0x11, ballot(7)),
            (BALLOT_MAX, ballot(11)),
        ],
        num_fields: 2,
    };
    let c = cells(&t);
    (t, c)
}

fn decode_cells(c: &[[u8; 32]], nf: u8) -> Result<TransitionData, davinci_zkvm_sdk::Error> {
    decode_blobs(&blobs_from_cells(c).unwrap(), nf)
}

fn header_cell(n_vids: u64, n_updates: u64) -> [u8; 32] {
    let mut h = [0u8; 32];
    h[16..24].copy_from_slice(&n_vids.to_be_bytes());
    h[24..].copy_from_slice(&n_updates.to_be_bytes());
    h
}

#[test]
fn decode_rejects_malformed_blobs() {
    let (t, c) = base();
    assert_eq!(c.len(), 1 + 2 + 3 * 4);
    assert_eq!(decode_cells(&c, 2).unwrap(), sorted(&t));
    // Wrong nf reads the stream differently and must not succeed silently.
    assert!(decode_cells(&c, 1).is_err());
    assert!(decode_cells(&c, 0).is_err());
    assert!(decode_cells(&c, 17).is_err());
    assert!(decode_blobs(&[], 2).is_err());

    // Header: the version field and every bit above the counts are zero.
    for byte in [0usize, 15] {
        let mut bad = c.clone();
        bad[0][byte] = 1;
        assert!(decode_cells(&bad, 2).is_err(), "header byte {byte}");
    }
    // More vote ids than updates, too many vote ids, too many updates.
    for (v, u) in [(4, 3), (MAX_BATCH_SIZE as u64 + 1, 1100), (1, 3073)] {
        let mut bad = c.clone();
        bad[0] = header_cell(v, u);
        assert!(decode_cells(&bad, 2).is_err(), "counts {v} / {u}");
    }
    // A count the blobs cannot hold, and a whole extra (zero) blob.
    let mut bad = c.clone();
    bad[0] = header_cell(2, 2000);
    assert!(decode_cells(&bad, 2).is_err());
    let mut blobs = blobs_from_cells(&c).unwrap();
    blobs.push(blobs_from_cells(&[]).unwrap().remove(0));
    assert!(decode_blobs(&blobs, 2).is_err());

    // Integer cells: at 2^252, and a non-zero unused slot of the last one.
    let mut bad = c.clone();
    bad[1][0] |= 0x10;
    assert!(decode_cells(&bad, 2).is_err());
    let mut bad = c.clone();
    bad[2][31] = 1;
    assert!(decode_cells(&bad, 2).is_err());

    // Order and namespace rules.
    let mut t2 = t.clone();
    t2.vote_ids.push(u64::MAX);
    assert!(decode_cells(&cells(&t2), 2).is_err());
    let mut t2 = t.clone();
    t2.updates.push((0x05, Ballot::identity()));
    assert!(decode_cells(&cells(&t2), 2).is_err());
    let mut t2 = t.clone();
    t2.updates.push((0x30, Ballot::identity()));
    assert!(decode_cells(&cells(&t2), 2).is_err());

    // First point cell of the first update: y >= p, then a y with no x,
    // then bit 255.
    let first_point = 3;
    let mut bad = c.clone();
    bad[first_point] =
        hex::decode("30644e72e131a029b85045b68181585d2833e84879b9709143e1f593f0000001")
            .unwrap()
            .try_into()
            .unwrap();
    assert!(decode_cells(&bad, 2).is_err());
    let mut off_curve = None;
    for y in 2u64..60 {
        let mut cell = [0u8; 32];
        cell[24..].copy_from_slice(&y.to_be_bytes());
        if Point::decompress(&cell).is_err() {
            off_curve = Some(cell);
            break;
        }
    }
    let mut bad = c.clone();
    bad[first_point] = off_curve.unwrap();
    assert!(decode_cells(&bad, 2).is_err());
    let mut bad = c.clone();
    bad[first_point][0] |= 0x80;
    assert!(decode_cells(&bad, 2).is_err());

    // Trailing junk after the last ballot, in the same blob.
    let mut bad = c.clone();
    let mut junk = [0u8; 32];
    junk[31] = 1;
    bad.push(junk);
    assert!(decode_cells(&bad, 2).is_err());
}

#[test]
fn six_blob_cap_table() {
    // Same table as go-sdk MaxSingleTxBatch (no overwrites) and the six-blob cap.
    let big = 1 << 20;
    for (nf, none, all) in [
        (1u8, 1024, 1024),
        (2, 1024, 1024),
        (5, 1024, 792),
        (6, 992, 664),
        (8, 750, 501),
        (12, 504, 336),
        (16, 379, 253),
    ] {
        assert_eq!(
            max_votes_for_cap(nf, 0, big, TX_BLOB_CAP),
            none,
            "nf {nf} no overwrites"
        );
        assert_eq!(
            max_votes_for_cap(nf, usize::MAX, big, TX_BLOB_CAP),
            all,
            "nf {nf} all overwrites"
        );
    }
    // Small trees need fewer refreshes, so more votes fit.
    assert!(max_votes_for_cap(16, 0, 0, TX_BLOB_CAP) > 379);
    assert_eq!(max_votes_for_cap(2, 0, big, 32), MAX_BATCH_SIZE);
    assert_eq!(blob_count(0, 0, 1), 1);
    assert_eq!(blob_count(126, 126, 16), 1);
    assert_eq!(blob_count(1, 127, 16), 2);
    assert_eq!(blob_count(512, 1024, 16), 9);
    assert_eq!(blob_count(1024, 2048, 16), 17);
}

fn pool() -> &'static Vec<Point> {
    static POOL: std::sync::OnceLock<Vec<Point>> = std::sync::OnceLock::new();
    POOL.get_or_init(|| {
        let g = Point::generator();
        (1..=48u64)
            .map(|k| g.mul(&ark_ff::BigInt::from(k * 7919 + 3)))
            .collect()
    })
}

prop_compose! {
    fn arb_ballot(nf: u8)(idx in prop::collection::vec(0usize..48, 32), neg in any::<u32>()) -> Ballot {
        let p = pool();
        let mut b = Ballot::identity();
        for (f, ct) in b.0.iter_mut().enumerate().take(nf as usize) {
            let pick = |i: usize| {
                let q = p[idx[i]];
                if neg >> (i % 32) & 1 == 1 { q.neg() } else { q }
            };
            *ct = Ciphertext { c1: pick(2 * f), c2: pick(2 * f + 1) };
        }
        b
    }
}

// At most as many vote ids as updates, as in every guest-accepted batch.
fn arb_transition() -> impl Strategy<Value = TransitionData> {
    (1u8..=16, 0usize..30).prop_flat_map(|(nf, u)| {
        (
            prop::collection::btree_set((1u64 << 63)..=u64::MAX, 0..=u),
            prop::collection::btree_map(0x10u64..(1 << 63), arb_ballot(nf), u),
        )
            .prop_map(move |(vids, ups)| TransitionData {
                vote_ids: vids.into_iter().collect(),
                updates: ups.into_iter().collect(),
                num_fields: nf,
            })
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]
    #[test]
    fn decode_inverts_encode(t in arb_transition()) {
        let blobs = blobs_from_cells(&cells(&t)).unwrap();
        prop_assert_eq!(blobs.len(), blob_count(t.vote_ids.len(), t.updates.len(), t.num_fields));
        prop_assert_eq!(decode_blobs(&blobs, t.num_fields).unwrap(), t);
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(6))]
    #[test]
    fn decode_inverts_build_blobs(t in arb_transition()) {
        let tb = build_blobs(&t, &Fr::from(77u64), &[9u8; 32]).unwrap();
        prop_assert_eq!(decode_blobs(&tb.blobs, t.num_fields).unwrap(), t);
    }
}
```

- [ ] **Step 8: Run the Rust tests to see them fail**

Run: `cd /home/p4u/davinci-zkvm && cargo test -p davinci-zkvm-sdk --test blob 2>&1 | grep -E '^error' | head -3`
Expected: `error[E0063]: missing field \`accumulator\` in initializer of \`TransitionData\``.

- [ ] **Step 9: Implement the Rust layout and decoder**

In `rust-sdk/src/blob.rs`, replace the module doc (lines 1-13) with:

```rust
//! EIP-4844 DA blobs of a transition, byte-identical to the guest
//! (`circuit-primitives/src/da_blob.rs`, `circuit/src/kzg.rs`) and go-sdk
//! `blob.go`, plus the decoder a follower needs to rebuild state.
//!
//! ```text
//! header                         n_vids * 2^64 + n_updates (bits 128.. zero)
//! ceil((V + U) / 4) int cells    [vid & (2^63 - 1) ..., key ...], four per
//!                                cell, the first in the top 63-bit slot
//! [pack(c1_0) pack(c2_0) ...]    per update, ascending by key, fields < nf
//! zero cells to the end of the last blob
//! pack(P) = Point::compress(P)
//! z_b    = int_be(sha256(pid_BE32 || BE32(root_before_int) || com_b)) mod r_bls
//! digest = sha256(com_0 || y_0 || ...)
//! ```
//!
//! The Results accumulator is not in the stream: a follower recomputes it
//! from the ballots it already holds.
```

Add `MAX_REFRESH` to the limits import (lines 22-24):

```rust
use crate::limits::{
    required_refresh, BALLOT_MAX, BALLOT_MIN, MAX_BATCH_SIZE, MAX_BLOBS, MAX_REFRESH, NUM_FIELDS,
    VOTE_ID_MIN,
};
```

`TransitionData` (lines 31-40) loses its field:

```rust
/// What one transition publishes. `updates` are new ballots and refreshed
/// ballots alike (the blob cannot tell them apart).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct TransitionData {
    pub vote_ids: Vec<u64>,
    pub updates: Vec<(u64, Ballot)>,
    pub num_fields: u8,
}
```

Replace `enc_u64`, `total_cells` and `blob_count` (lines 62-75) with:

```rust
/// A vote id's packed form (its top bit is implied) and every integer
/// slot's width.
const LOW63: u64 = (1 << 63) - 1;

/// `T = 1 + ceil((V + U) / 4) + 2 * nf * U`.
fn total_cells(n_vids: usize, n_updates: usize, nf: usize) -> usize {
    1 + (n_vids + n_updates).div_ceil(4) + 2 * nf * n_updates
}

/// `ceil(T / 4096)` for `T = 1 + ceil((V + U) / 4) + 2 * nf * U`.
pub fn blob_count(n_vote_ids: usize, n_updates: usize, nf: u8) -> usize {
    total_cells(n_vote_ids, n_updates, nf as usize).div_ceil(CELLS_PER_BLOB)
}

/// The header cell: `n_vids * 2^64 + n_updates`.
fn header(n_vids: usize, n_updates: usize) -> [u8; 32] {
    let mut c = [0u8; 32];
    c[16..24].copy_from_slice(&(n_vids as u64).to_be_bytes());
    c[24..].copy_from_slice(&(n_updates as u64).to_be_bytes());
    c
}

/// `e[0] * 2^189 + e[1] * 2^126 + e[2] * 2^63 + e[3]` for integers below 2^63.
fn pack4(e: [u64; 4]) -> [u8; 32] {
    let [a, b, c, d] = e;
    // Little-endian u64 limbs of the 256-bit cell.
    let limbs = [d | c << 63, c >> 1 | b << 62, b >> 2 | a << 61, a >> 3];
    let mut out = [0u8; 32];
    for (i, l) in limbs.iter().enumerate() {
        out[24 - 8 * i..32 - 8 * i].copy_from_slice(&l.to_be_bytes());
    }
    out
}

/// Inverse of [`pack4`] for a cell below 2^252.
fn unpack4(c: &[u8; 32]) -> [u64; 4] {
    let mut l = [0u64; 4];
    for (i, w) in l.iter_mut().enumerate() {
        let mut b = [0u8; 8];
        b.copy_from_slice(&c[24 - 8 * i..32 - 8 * i]);
        *w = u64::from_be_bytes(b);
    }
    [
        (l[2] >> 61 | l[3] << 3) & LOW63,
        (l[1] >> 62 | l[2] << 2) & LOW63,
        (l[0] >> 63 | l[1] << 1) & LOW63,
        l[0] & LOW63,
    ]
}
```

Replace `cells` (lines 97-122) with:

```rust
/// The cell stream in guest order (vote ids and updates sorted here; the
/// update sort is stable). `num_fields` above 16 is treated as 16.
pub fn cells(t: &TransitionData) -> Vec<[u8; 32]> {
    let nf = (t.num_fields as usize).min(NUM_FIELDS);
    let mut vids = t.vote_ids.clone();
    vids.sort_unstable();
    let mut updates: Vec<&(u64, Ballot)> = t.updates.iter().collect();
    updates.sort_by_key(|(k, _)| *k);

    let ints: Vec<u64> = vids
        .iter()
        .map(|v| v & LOW63)
        .chain(updates.iter().map(|(k, _)| *k))
        .collect();
    let mut out = Vec::with_capacity(total_cells(vids.len(), updates.len(), nf));
    out.push(header(vids.len(), updates.len()));
    for chunk in ints.chunks(4) {
        let mut e = [0u64; 4];
        e[..chunk.len()].copy_from_slice(chunk);
        out.push(pack4(e));
    }
    for (_, ballot) in updates {
        for ct in &ballot.0[..nf] {
            out.push(ct.c1.compress());
            out.push(ct.c2.compress());
        }
    }
    out
}
```

In `impl Cells`, delete the `u64` method (lines 276-284) and the `count` method with its comment (lines 301-309); `total`, `cell`, `take`, `point` and `ciphertexts` stay. Replace `decode_blobs` and its doc (lines 312-376) with:

```rust
/// Parses a transition's blobs (in order) at the election's `nf`. Rejects a
/// non-zero header version, counts outside the guest's limits or with more
/// vote ids than updates, a blob count other than the header's, integer cells
/// at or above 2^252 or with a non-zero unused slot, unsorted or
/// out-of-namespace keys, invalid points and non-zero padding.
///
/// These are layout checks, not provenance. Points are only checked to be on
/// the curve, not in the prime-order subgroup. Trust the output only for blobs
/// whose commitments and evaluations reproduce the `blobs_digest` of verified
/// batch publics: then the cells are the ones the guest laid out from the
/// ciphertexts it stored.
pub fn decode_blobs(blobs: &[Blob], nf: u8) -> Result<TransitionData, Error> {
    let nfu = nf as usize;
    if nfu == 0 || nfu > NUM_FIELDS {
        return Err(Error::Blob(format!("num_fields {nf} not in 1..=16")));
    }
    if blobs.is_empty() || blobs.len() > MAX_BLOBS {
        return Err(Error::Blob(format!(
            "{} blobs, want 1..={MAX_BLOBS}",
            blobs.len()
        )));
    }
    let mut c = Cells { blobs, next: 0 };

    let head = c.take()?;
    if head[..16] != [0u8; 16] {
        return Err(Error::Blob("header version field not zero".into()));
    }
    let word = |r: std::ops::Range<usize>| {
        let mut b = [0u8; 8];
        b.copy_from_slice(&head[r]);
        u64::from_be_bytes(b)
    };
    let (n_vids, n_updates) = (word(16..24), word(24..32));
    if n_vids > MAX_BATCH_SIZE as u64
        || n_updates < n_vids
        || n_updates > (MAX_BATCH_SIZE + MAX_REFRESH) as u64
    {
        return Err(Error::Blob(format!(
            "header counts {n_vids} vote ids, {n_updates} updates"
        )));
    }
    let (n_vids, n_updates) = (n_vids as usize, n_updates as usize);
    let want = blob_count(n_vids, n_updates, nf);
    if blobs.len() != want {
        return Err(Error::Blob(format!(
            "{} blobs, the header needs {want}",
            blobs.len()
        )));
    }

    let n_ints = n_vids + n_updates;
    let mut ints = Vec::with_capacity(n_ints.next_multiple_of(4));
    for _ in 0..n_ints.div_ceil(4) {
        let cell = c.take()?;
        if cell[0] >> 4 != 0 {
            return Err(Error::Blob("integer cell at or above 2^252".into()));
        }
        ints.extend(unpack4(&cell));
    }
    if ints[n_ints..].iter().any(|x| *x != 0) {
        return Err(Error::Blob("unused integer slot not zero".into()));
    }

    let mut vote_ids: Vec<u64> = Vec::with_capacity(n_vids);
    for &i in &ints[..n_vids] {
        let v = i | VOTE_ID_MIN;
        if vote_ids.last().is_some_and(|p| *p >= v) {
            return Err(Error::Blob("vote ids not strictly ascending".into()));
        }
        vote_ids.push(v);
    }
    let mut keys: Vec<u64> = Vec::with_capacity(n_updates);
    for &key in &ints[n_vids..n_ints] {
        if !(BALLOT_MIN..=BALLOT_MAX).contains(&key) || keys.last().is_some_and(|p| *p >= key) {
            return Err(Error::Blob(
                "slot keys not ascending in the ballot namespace".into(),
            ));
        }
        keys.push(key);
    }
    let mut updates = Vec::with_capacity(n_updates);
    for key in keys {
        updates.push((key, c.ciphertexts(nfu)?));
    }

    for i in c.next..c.total() {
        if c.cell(i).is_none_or(|x| x.iter().any(|b| *b != 0)) {
            return Err(Error::Blob("non-zero data after the last ballot".into()));
        }
    }
    Ok(TransitionData {
        vote_ids,
        updates,
        num_fields: nf,
    })
}
```

`max_votes_for_cap`, `blobs_from_cells`, `compute_z`, `versioned_hash`, `blobs_digest`, `build_blobs` and `verify_blob_commitment` do not change.

- [ ] **Step 10: Run the Rust tests against the old vectors**

Run: `cd /home/p4u/davinci-zkvm && cargo test -p davinci-zkvm-sdk --test blob 2>&1 | grep -E '^test result|blobs_match_go_kzg4844'`
Expected: `8 passed; 1 failed`, the failure being `blobs_match_go_kzg4844` (the committed vectors are still the old layout).

- [ ] **Step 11: Regenerate the vectors**

```bash
cd /home/p4u/davinci-zkvm/go-sdk && go run ./cmd/sdk-vectors -out ../rust-sdk/testdata
cd .. && git status --short rust-sdk
sha256sum rust-sdk/testdata/blob.json rust-sdk/testdata/wire_prove.json
```

Expected: only `M rust-sdk/testdata/blob.json` and `M rust-sdk/testdata/wire_prove.json` (the generator's random stream now skips the blob cases). With the generator exactly as in Step 5 the checksums are `1aa5aace14e5ee4119103d20d5826edac70460f187785eb07ea4114d6f167b64` (blob.json, about 1.5 MB) and `66fb9862c050e308790b326ab6d258426a9f6ef30f73b838fd1e859fe449958a` (wire_prove.json). Run the generator a second time: the checksums must not move (CI's stale-vector check).

- [ ] **Step 12: Run the CI Rust and Go checks**

```bash
cd /home/p4u/davinci-zkvm
cargo fmt --all --check
cargo clippy -p davinci-zkvm-service -p davinci-zkvm-sdk -p davinci-zkvm-input-gen --all-targets -- -D warnings
cargo test -p davinci-zkvm-service -p davinci-zkvm-sdk -p davinci-zkvm-input-gen 2>&1 | grep -E '^test result' | grep -v ' 0 failed' ; echo "no failing suites above"
cd go-sdk && go test $(go list ./... | grep -v /tests/integration) 2>&1 | grep -v -E 'no test files|^ok'; echo "no failing go packages above"
```

Expected: fmt and clippy clean; `tests/blob.rs` reports `9 passed`; `tests/wire.rs` passes with the new `wire_prove.json`; nothing printed above either marker line.

- [ ] **Step 13: Update the documented capacities and the builder signature**

`go-sdk/README.md:101`:

```go
blobs, err := davinci.BuildTransitionBlobs(numFields, pid, rootBefore, voteIDs, updates)
```

`BENCHMARK.md:109`: `gives the largest settleable batch (1024 up to 5 fields, 379 at 16).`
`BENCHMARK.md:118`: `| 1 (128 votes at nf=2, or up to about 126 updates at nf=16) | ~498 k |`

`docs/architecture.md:160-163`:

```markdown
changes the guest and its vk. A settlement transaction can carry at most six
blobs (EIP-7594), which caps the settleable batch size below 1024 for ballots
with more than five fields (992 at six, 379 at sixteen); `davinci.MaxSingleTxBatch(nf)` (Go) and
`blob::max_votes_for_cap` (Rust) compute it.
```

`CLAUDE.md:505` (gitignored, edit only): `(1024 up to nf=5, 992 at 6, 750 at 8, 504 at 12, 379 at 16 for a steady batch`.

The per-batch tables in `BENCHMARK.md` and `CLAUDE.md` keep their blob columns: every listed size spans the same number of blobs under the compact stream (for example 512 steady at 16 fields: 33,153 cells, 9 blobs; 1024 at 16 fields: 66,305 cells, 17 blobs).

- [ ] **Step 14: Commit**

```bash
cd /home/p4u/davinci-zkvm
git add go-sdk/blob.go go-sdk/blob_test.go go-sdk/blobcap_test.go go-sdk/tests/integration/election.go \
  go-sdk/cmd/sdk-vectors/blob.go rust-sdk/src/blob.rs rust-sdk/tests/blob.rs \
  rust-sdk/testdata/blob.json rust-sdk/testdata/wire_prove.json \
  go-sdk/README.md BENCHMARK.md docs/architecture.md
git commit -m "feat(sdk): compact da stream in the go and rust builders" -m "Vote ids (top bit implied) and slot keys are packed four per cell behind a
header cell holding both counts, and the Results accumulator leaves the
stream: T = 1 + ceil((V + U) / 4) + 2nf*U. The Rust decoder also rejects a
non-zero version field, more ids than updates, counts above the guest's
limits, integer cells at 2^252 and bits in unused slots. Six blobs now hold
379 steady votes at 16 fields (was 366). blob.json has its own random
stream, so wire_prove.json is regenerated once."
```

---

### Task 2: The guest lays out the compact stream; ELFs and vks refrozen

**Files:**
- Modify: `go-sdk/tests/integration/cheat_test.go` (insert after `TestCheatWrongKZGCommitment`, line 434)
- Modify: `circuit-primitives/src/da_blob.rs` (whole file)
- Modify: `circuit/src/kzg.rs:1-9`, `:59`, `:95-107`, `:161-168`, `:196-197`, `:211-228`
- Modify: `circuit/src/main.rs:207`, `:218-235`
- Rebuild: `circuit/elf/circuit.elf`, `circuit-aggregator/elf/aggregator.elf`, `circuit-results/elf/results.elf`
- Modify: `rust-sdk/src/release.rs:25-35`, `go-sdk/chain/release.go:31-33`, `rust-sdk/tests/wire.rs:210-213`, `:337-342`
- Modify: `circuit/CIRCUIT.md` §2 (`:214-216`), §8 (`:893-960`, `:1015-1018`, `:1033-1039`), §12.8 (`:1231-1233`), §13.5 (`:1286-1289`); `CLAUDE.md:395-399` (gitignored)

**Interfaces:**
- Consumes: Task 1's `BuildTransitionBlobs(numFields, pid, root, voteIDs, updates)` (through the harness's `BuildKZGBlock`).
- Produces (guest, `circuit_primitives::da_blob`): `pub const LOW63: u64`, `pub fn header(n_vids: u64, n_updates: u64) -> [u8; 32]`, `pub fn pack4(a: u64, b: u64, c: u64, d: u64) -> [u8; 32]`, `pub fn total_cells(n_vids: usize, n_updates: usize, nf: usize) -> usize`, `pub fn build_cells(sorted_vote_ids: &[u64], sorted_updates: &[(u64, BallotData)], nf: usize) -> Vec<[u8; 32]>`; `kzg::evaluate_barycentric(blob: &[u8], z: BlsFrRaw, omega: &[BlsFrRaw; N]) -> BlsFrRaw`; `kzg::verify_kzg(kzg, state, num_fields, refreshed_new, fail_mask)` (no `net`).
- Produces (pins, used by Task 4 through the SDK): new `BATCH_PROGRAM_VK`, `RESULTS_PROGRAM_VK`, `CircuitRelease{AggVK, BatchVK, ResultsVK}`.

- [ ] **Step 1: Write the guest-versus-builder cheat test**

In `go-sdk/tests/integration/cheat_test.go`, insert before `// TestCheatWrongCensusRoot verifies that a wrong census root causes FAIL_CENSUS.`:

```go
// kzgBlockWithCount rewrites an encoded KZGBLK to carry n commitments: the
// honest ones truncated, or extended with copies of the last.
func kzgBlockWithCount(block []byte, n int) []byte {
	const header = 8 + 32 + 32 + 8 // magic, process id, root before, n_blobs
	have := (len(block) - header) / 48
	out := append([]byte(nil), block[:header]...)
	binary.LittleEndian.PutUint64(out[header-8:], uint64(n))
	for i := 0; i < n; i++ {
		j := min(i, have-1)
		out = append(out, block[header+48*j:header+48*(j+1)]...)
	}
	return out
}

// TestCheatKZGBlobCount: the honest batch's blobs digest equals the one
// BuildTransitionBlobs computes (the guest and the Go builder lay out the
// same cells), and one commitment fewer or more than ceil(T / 4096) sets
// FAIL_KZG and nothing else, for every fill of the last integer cell
// (V + U mod 4).
func TestCheatKZGBlobCount(t *testing.T) {
	cases := []struct {
		name          string
		voters, first int // election size; voters in an earlier, unchecked batch
		residue       int // (V + U) mod 4 of the checked batch
	}{
		{"one-vote", 1, 0, 2},           // V 1, U 1
		{"two-votes", 2, 0, 0},          // V 2, U 2
		{"vote-one-refresh", 2, 1, 3},   // V 1, U 1 + 1 refresh
		{"vote-three-refresh", 4, 3, 1}, // V 1, U 1 + 3 refreshes
	}
	for _, c := range cases {
		t.Run(c.name, func(t *testing.T) {
			election, err := NewElection(c.voters)
			if err != nil {
				t.Fatalf("NewElection: %v", err)
			}
			var base *cheatElectionInput
			if c.first == 0 {
				base, election, _ = buildCheatInputElection(t, election)
			} else {
				base, election, _ = buildCheatInputBatches(t, election, election.Voters[:c.first], election.Voters[c.first:])
			}
			sd := base.stateData
			if got := (len(sd.VoteIDSmt) + len(sd.BallotSmt) + len(sd.RefreshSmt)) % 4; got != c.residue {
				t.Fatalf("(V + U) mod 4 = %d, want %d", got, c.residue)
			}
			outputs, err := runZiskEmu(base.fullInput())
			if err != nil {
				t.Fatalf("ziskemu: %v", err)
			}
			if outputs[davinci.OutputOverallOk] != 1 {
				t.Fatalf("honest batch rejected: fail_mask=0x%08x", outputs[davinci.OutputFailMask])
			}
			_, tb, err := election.BuildKZGBlock(base.oldRoot)
			if err != nil {
				t.Fatalf("BuildKZGBlock: %v", err)
			}
			var digest [32]byte
			for i := 0; i < 8; i++ {
				binary.LittleEndian.PutUint32(digest[4*i:], outputs[davinci.OutputBlobsDigest+i])
			}
			if digest != tb.Digest {
				t.Fatalf("guest blobs digest %x, BuildTransitionBlobs %x", digest, tb.Digest)
			}
			honest := int(binary.LittleEndian.Uint64(base.kzgBlock[72:80]))
			for _, n := range []int{honest - 1, honest + 1} {
				bad := *base
				bad.kzgBlock = kzgBlockWithCount(base.kzgBlock, n)
				assertCircuitFailsExactly(t, bad.fullInput(), failKZG, fmt.Sprintf("%s-%d-blobs", c.name, n))
			}
		})
	}
}
```

A zero-commitment block cannot come from `EncodeKZGBlock` (it refuses), so the test rewrites the encoded block's count field (offset 72) directly; the guest's parser accepts zero and `verify_kzg` sets `FAIL_KZG`.

- [ ] **Step 2: Run it against the current ELF to see it fail**

```bash
cd /home/p4u/davinci-zkvm && cargo build --release -p davinci-zkvm-input-gen
cd go-sdk/tests && GEN_INPUT_BIN=/home/p4u/davinci-zkvm/target/release/gen-input \
  go test ./integration -run 'TestCheatKZGBlobCount' -count=1 2>&1 | grep -E 'digest|^(ok|FAIL)' | head -4
```

Expected: FAIL, `guest blobs digest … , BuildTransitionBlobs …` with two different values (the committed ELF still lays out the old stream).

- [ ] **Step 3: Write the failing guest host tests**

Replace the `#[cfg(test)] mod tests { … }` block at the end of `circuit-primitives/src/da_blob.rs` (lines 119-253) with:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::bn254_fr::{ONE, ZERO};
    use crate::types::{BALLOT_FIELDS, ZERO_FR};

    // The integer `v` as a 32-byte big-endian cell.
    fn int_cell(v: u64) -> [u8; 32] {
        let mut b = [0u8; 32];
        b[24..32].copy_from_slice(&v.to_be_bytes());
        b
    }

    fn hex32(s: &str) -> [u8; 32] {
        let mut out = [0u8; 32];
        for i in 0..32 {
            out[i] = u8::from_str_radix(&s[2 * i..2 * i + 2], 16).unwrap();
        }
        out
    }

    #[test]
    fn header_layout() {
        assert_eq!(header(0, 0), [0u8; 32]);
        assert_eq!(
            header(1, 2),
            hex32("0000000000000000000000000000000000000000000000010000000000000002")
        );
    }

    #[test]
    fn pack4_layout() {
        // First element in the most significant slot; matches the big-integer
        // formula the SDK tests check (a*2^189 + b*2^126 + c*2^63 + d).
        assert_eq!(pack4(0, 0, 0, 0), [0u8; 32]);
        assert_eq!(pack4(0, 0, 0, 7), int_cell(7));
        assert_eq!(
            pack4(1, 0x10, 0x11, LOW63),
            hex32("000000000000000020000000000000040000000000000008ffffffffffffffff")
        );
        assert_eq!(
            pack4(LOW63, LOW63 - 2, LOW63 - 1, LOW63),
            hex32("0fffffffffffffffffffffffffffffff7fffffffffffffff7fffffffffffffff")
        );
    }

    #[test]
    fn pack_identity_is_one() {
        // TE identity (0, 1) must pack to the 32-byte BE integer `1`.
        assert_eq!(pack_te(&ZERO, &ONE), int_cell(1));
    }

    #[test]
    fn pack_x_parity_sets_bit_254() {
        // y = 1, x parity = 1 (x = 1) -> value = 1 + 2^254.
        let mut want = int_cell(1);
        want[0] = 0x40;
        assert_eq!(pack_te(&[1, 0, 0, 0], &ONE), want);
        // y = 1, x parity = 0 (x = 2) -> value = 1.
        assert_eq!(pack_te(&[2, 0, 0, 0], &ONE), int_cell(1));
    }

    #[test]
    fn total_and_n_blobs_arithmetic() {
        assert_eq!(total_cells(0, 0, 1), 1);
        assert_eq!(total_cells(1, 3, 1), 1 + 1 + 6);
        assert_eq!(total_cells(1, 4, 1), 1 + 2 + 8);
        // 126 votes at 16 fields fill one blob; one vote with 126 refreshes
        // puts a cell in a second.
        assert_eq!(total_cells(126, 126, 16), 4096);
        assert_eq!(total_cells(1, 127, 16), 4097);
        // The largest valid transition (1024 votes, 2048 refreshes, 16 fields).
        assert_eq!(n_blobs_from_t(total_cells(1024, 3072, 16)), 25);
        assert_eq!(n_blobs_from_t(0), 0);
        assert_eq!(n_blobs_from_t(1), 1);
        assert_eq!(n_blobs_from_t(4096), 1);
        assert_eq!(n_blobs_from_t(4097), 2);
        assert_eq!(n_blobs_from_t(8193), 3);
    }

    #[test]
    fn digest_pairs_matches_manual_sha256() {
        // Two fake (commitment, y) pairs.
        let mut com0 = [0u8; 48];
        for i in 0..48 {
            com0[i] = i as u8;
        }
        let y0 = [0xFFu8; 32];
        let mut com1 = [0u8; 48];
        for i in 0..48 {
            com1[i] = (0x30 + i) as u8;
        }
        let mut y1 = [0u8; 32];
        for i in 0..32 {
            y1[i] = i as u8;
        }

        let got = digest_pairs(&[(com0, y0), (com1, y1)]);

        // Hand assemble the byte string and hash it: this locks in the exact
        // (commitment || y) ordering. If digest_pairs regresses (wrong length,
        // wrong order, extra bytes), this diverges.
        let mut expected_input = alloc::vec::Vec::<u8>::with_capacity(2 * (48 + 32));
        expected_input.extend_from_slice(&com0);
        expected_input.extend_from_slice(&y0);
        expected_input.extend_from_slice(&com1);
        expected_input.extend_from_slice(&y1);
        let expected = crate::hash::sha256_once(&expected_input);
        assert_eq!(got, expected);

        // Empty input: digest_pairs must be the sha256 of the empty string.
        let empty_expected = crate::hash::sha256_once(&[]);
        assert_eq!(digest_pairs(&[]), empty_expected);
    }

    #[test]
    fn cell_layout_tiny_transition() {
        // 1 vid, 2 updates, nf=2: header, one integer cell, 2 * 2 * 2 point
        // cells. An identity update packs to 1 in every point cell.
        let nf = 2usize;
        let vids: alloc::vec::Vec<u64> = alloc::vec![0x8000_0000_0000_0001];
        let mut identity: BallotData = [ZERO_FR; BALLOT_FIELDS];
        for f in 0..crate::types::NUM_FIELDS {
            let b = f * 4;
            identity[b] = ZERO;
            identity[b + 1] = ONE;
            identity[b + 2] = ZERO;
            identity[b + 3] = ONE;
        }
        let updates: alloc::vec::Vec<(u64, BallotData)> = alloc::vec![
            (0x10u64, identity),
            (0x11u64, identity),
        ];
        let cells = build_cells(&vids, &updates, nf);
        // T = 1 + ceil(3 / 4) + 2 * 2 * 2 = 10
        assert_eq!(cells.len(), 10);
        assert_eq!(cells.len(), total_cells(1, 2, nf));

        assert_eq!(cells[0], header(1, 2));
        // The id without its top bit, then the keys, one unused slot.
        assert_eq!(cells[1], pack4(1, 0x10, 0x11, 0));
        for i in 2..10 {
            assert_eq!(cells[i], int_cell(1), "point cell {i}");
        }
    }

    #[test]
    fn integer_cells_split_at_four() {
        // V + U = 0..=5: ceil((V + U) / 4) integer cells, missing slots zero.
        let id: BallotData = [ZERO_FR; BALLOT_FIELDS];
        for n in 0..=5usize {
            let updates: alloc::vec::Vec<(u64, BallotData)> =
                (0..n).map(|i| (0x10 + i as u64, id)).collect();
            let cells = build_cells(&[], &updates, 1);
            assert_eq!(cells.len(), 1 + n.div_ceil(4) + 2 * n);
            if n == 5 {
                assert_eq!(cells[1], pack4(0x10, 0x11, 0x12, 0x13));
                assert_eq!(cells[2], pack4(0x14, 0, 0, 0));
            }
        }
        // An id below 2^63 (rejected elsewhere) masks, it does not panic.
        let cells = build_cells(&[5], &[(0x10, id)], 1);
        assert_eq!(cells[1], pack4(5, 0x10, 0, 0));
    }
}
```

- [ ] **Step 4: Run the host tests to see them fail**

Run: `cd /home/p4u/davinci-zkvm/circuit-primitives && cargo test --lib da_blob 2>&1 | grep -E '^error' | head -3`
Expected: `error[E0425]: cannot find function \`header\`` (and `pack4`, `LOW63`). `circuit-primitives` is its own workspace and its host tests build in `circuit-primitives/target/`.

- [ ] **Step 5: Implement the guest layout**

Replace everything above `#[cfg(test)]` in `circuit-primitives/src/da_blob.rs` (lines 1-117) with exactly this text (the guest ELF, and so the vk, depends on every line, including comments, because panic locations carry line numbers):

```rust
//! Data-availability blob cell layout for the vote-batch guest.
//!
//! Cells are 32-byte big-endian BLS12-381 Fr elements. The guest rebuilds
//! them from verified data, packs them into 4096-cell blobs, and evaluates
//! each blob polynomial at the point derived from that blob's commitment.
//!
//! Layout (byte-exact; the Go and Rust SDKs mirror it):
//!
//! ```text
//! header(n_vids, n_updates)
//! pack4(I[0..4]) pack4(I[4..8]) ..                   // ceil((n_vids + n_updates) / 4)
//!     I = [vid_0 & (2^63 - 1) .. vid_{n_vids-1} & (2^63 - 1), key_0 .. key_{n_updates-1}]
//!     vote ids ASCENDING, keys ASCENDING (stable), missing slots zero
//! for each (key, ballot) update, ASCENDING by key (stable):
//!     pack(c1_0) pack(c2_0) .. pack(c1_{nf-1}) pack(c2_{nf-1})
//! zero cells to end of last blob
//! ```
//!
//! `header(v, u)`   = `v * 2^64 + u`; bits 128..255 (the version) are zero.
//! `pack4(a,b,c,d)` = `a*2^189 + b*2^126 + c*2^63 + d`, below 2^252.
//! `pack(x, y)`     = `y_canon + ((x_canon & 1) << 254)` as 32-byte big-endian,
//!                    with `x_canon`, `y_canon` = coords reduced mod BN254 Fr.
//!                    Fits in BLS12-381 Fr since `p_bn254 + 2^254 < r_bls`.
//!                    The TE identity `(0,1)` packs to `1`.
//!
//! Vote ids live in `[2^63, 2^64)`, so the top bit is implied. The mask, not
//! `vid - 2^63`, keeps an out-of-namespace id (already `FAIL_CONSISTENCY`)
//! from panicking here. The Results accumulator is not emitted: a follower
//! recomputes it from the ballots it holds.

use crate::bn254_fr;
use crate::hash::sha256_once;
use crate::types::{BallotData, FrRaw};
extern crate alloc;
use alloc::vec::Vec;

/// Cells per EIP-4844 blob (identical to `kzg::N`).
pub const CELLS_PER_BLOB: usize = 4096;

/// Low 63 bits: a packed vote id and the width of every integer slot.
pub const LOW63: u64 = (1 << 63) - 1;

/// The header cell `n_vids * 2^64 + n_updates` as 32-byte big-endian.
#[inline]
pub fn header(n_vids: u64, n_updates: u64) -> [u8; 32] {
    let mut b = [0u8; 32];
    b[16..24].copy_from_slice(&n_vids.to_be_bytes());
    b[24..32].copy_from_slice(&n_updates.to_be_bytes());
    b
}

/// `a*2^189 + b*2^126 + c*2^63 + d` as 32-byte big-endian, for integers below
/// 2^63 (the result is below 2^252).
#[inline]
pub fn pack4(a: u64, b: u64, c: u64, d: u64) -> [u8; 32] {
    // Little-endian u64 limbs of the 256-bit cell.
    let limbs = [d | c << 63, c >> 1 | b << 62, b >> 2 | a << 61, a >> 3];
    let mut out = [0u8; 32];
    for i in 0..4 {
        let start = (3 - i) * 8;
        out[start..start + 8].copy_from_slice(&limbs[i].to_be_bytes());
    }
    out
}

/// Pack a TE point (x, y) into a 32-byte big-endian BLS12-381 Fr element:
/// `y_canon + ((x_canon & 1) << 254)`, with both coords reduced mod BN254 Fr.
#[inline]
pub fn pack_te(x: &FrRaw, y: &FrRaw) -> [u8; 32] {
    let xc = bn254_fr::reduce(x);
    let yc = bn254_fr::reduce(y);
    // BE bytes of y_canon: LS limb 0 -> bytes 24..32, MS limb 3 -> bytes 0..8.
    let mut out = [0u8; 32];
    for i in 0..4 {
        let start = (3 - i) * 8;
        out[start..start + 8].copy_from_slice(&yc[i].to_be_bytes());
    }
    // BN254 Fr < 2^254, so bit 254 of y_canon is 0 — no collision with the tag.
    if xc[0] & 1 == 1 {
        out[0] |= 0x40; // bit 254
    }
    out
}

/// Total cell count `T = 1 + ceil((n_vids + n_updates) / 4) + 2 * nf * n_updates`.
#[inline]
pub fn total_cells(n_vids: usize, n_updates: usize, nf: usize) -> usize {
    1 + (n_vids + n_updates).div_ceil(4) + 2 * nf * n_updates
}

/// Number of blobs needed for `t` cells: `ceil(t / 4096)`.
#[inline]
pub fn n_blobs_from_t(t: usize) -> usize {
    t.div_ceil(CELLS_PER_BLOB)
}

/// Build the DA blob cells from pre-sorted inputs. The caller sorts vote ids
/// ascending and the updates ascending by key (stable, ballot-chain first
/// among equal keys). Padded fields `f >= nf` are not emitted.
pub fn build_cells(
    sorted_vote_ids: &[u64],
    sorted_updates: &[(u64, BallotData)],
    nf: usize,
) -> Vec<[u8; 32]> {
    let t = total_cells(sorted_vote_ids.len(), sorted_updates.len(), nf);
    let mut cells: Vec<[u8; 32]> = Vec::with_capacity(t);
    cells.push(header(sorted_vote_ids.len() as u64, sorted_updates.len() as u64));
    let n_ints = sorted_vote_ids.len() + sorted_updates.len();
    let int = |i: usize| -> u64 {
        if i < sorted_vote_ids.len() {
            sorted_vote_ids[i] & LOW63
        } else if i < n_ints {
            sorted_updates[i - sorted_vote_ids.len()].0
        } else {
            0
        }
    };
    let mut i = 0;
    while i < n_ints {
        cells.push(pack4(int(i), int(i + 1), int(i + 2), int(i + 3)));
        i += 4;
    }
    for (_, ballot) in sorted_updates {
        for f in 0..nf {
            let base = f * 4;
            cells.push(pack_te(&ballot[base], &ballot[base + 1]));
            cells.push(pack_te(&ballot[base + 2], &ballot[base + 3]));
        }
    }
    debug_assert_eq!(cells.len(), t);
    cells
}

/// Hash the ordered `(commitment_48, y_be32)` pairs into the KZG blobs digest:
/// `SHA-256( com_0 || y_0 || com_1 || y_1 || ... )`.
pub fn digest_pairs(pairs: &[([u8; 48], [u8; 32])]) -> [u8; 32] {
    let mut buf: Vec<u8> = Vec::with_capacity(pairs.len() * (48 + 32));
    for (com, y) in pairs {
        buf.extend_from_slice(com);
        buf.extend_from_slice(y);
    }
    sha256_once(&buf)
}

```

(`digest_pairs` is unchanged; the blank line before `#[cfg(test)]` stays.)

- [ ] **Step 6: Run the host tests**

Run: `cd /home/p4u/davinci-zkvm/circuit-primitives && cargo test --lib da_blob 2>&1 | grep '^test result'`
Expected: `test result: ok. 8 passed; 0 failed; … 58 filtered out`.

- [ ] **Step 7: Build the omega table once per batch and drop `net` from `verify_kzg`**

In `circuit/src/kzg.rs`, the module doc lines 3-6 become:

```rust
//! The blob cells are not host input: the guest rebuilds them from verified
//! state (the sorted vote ids and the sorted slot updates covering new votes,
//! overwrites and silent refreshes alike; layout in `da_blob.rs`) and
//! evaluates each blob polynomial at the point derived from that blob's
```

The doc of `gen_omega_table` (line 59):

```rust
/// Generate the 4096 EIP-4844 roots of unity in bit-reversed order. Built
/// once per batch and shared by every blob.
```

`evaluate_barycentric` takes the table (lines 95-101):

```rust
/// Evaluate one blob polynomial at `z`. `blob` is a flat 4096 x 32 byte buffer,
/// each cell a big-endian BLS12-381 Fr element (matches EIP-4844 / go-eth-kzg);
/// `omega` is `gen_omega_table()`.
pub fn evaluate_barycentric(blob: &[u8], z: BlsFrRaw, omega: &[BlsFrRaw; N]) -> BlsFrRaw {
    debug_assert_eq!(blob.len(), N * 32, "blob must be exactly 4096 x 32 bytes");

    for (k, w) in omega.iter().enumerate() {
```

(the `let omega = gen_omega_table();` line and its blank line go). In `verify_kzg`, delete the `net: &BallotData,` parameter (line 166), call `build_cells(&vids, &updates, num_fields)` (line 197), and build the table once before the blob loop (lines 211-227):

```rust
    // 4) Per blob: copy cells into a 131072-byte buffer, evaluate at z_b, and
    // collect the (com, y) pairs. The last blob is zero-padded past the cell
    // count. The roots of unity are the same for every blob.
    let omega = gen_omega_table();
    const BLOB_BYTES: usize = N * 32;
```

```rust
        let y_b = evaluate_barycentric(&blob_buf, z_b, &omega);
```

The table is built after the count check, so a `FAIL_KZG` input still returns before any field work.

In `circuit/src/main.rs`, line 207 becomes `let (results_ok, _net_results) = match &parsed.state {` (the guest still computes the net for the Results leaf), and the DA block (lines 218-235) becomes:

```rust
    // Data availability
    // The guest rebuilds the DA blob cells from verified state (vote-id list
    // and sorted slot updates covering new votes, overwrites and silent
    // refreshes; the Results accumulator is not emitted, a follower
    // recomputes it) and evaluates each blob polynomial at the point derived
    // from that blob's commitment. The emitted digest binds the sequencer's
    // commitments to what the circuit has just proved; the settlement
    // contract checks each `(commitment, y)` against the blob's versioned
    // hash via the EIP-4844 point-evaluation precompile. Runs AFTER the
    // re-encryption (needs `refreshed_new`) and AFTER verify_state (needs the
    // SMT chains).
    let (kzg_ok, blobs_digest, n_blobs) = kzg::verify_kzg(
        &parsed.kzg,
        parsed.state.as_ref(),
        num_fields,
        &refreshed_new,
        &mut fail_mask,
    );
```

- [ ] **Step 8: Rebuild the three guests**

```bash
cd /home/p4u/davinci-zkvm && scripts/build-guests.sh 2>&1 | tail -3
git status --short -- '*/elf/*.elf'
```

Expected: about 45 s; all three ELFs modified (`circuit/elf/circuit.elf`, `circuit-aggregator/elf/aggregator.elf`, `circuit-results/elf/results.elf`). The last two move although their sources did not: any code change in `circuit-primitives` re-partitions its codegen units and shifts their linked code. With the sources exactly as in this task the checksums are `70318b382de45724d982a9cda56cf037aee8a0b5d0aff64743943db9340fd897` (circuit), `75203da99e8cee79fa798f3b5392cf674db8705e30d55c48d5755d09d29271d1` (aggregator), `ae214e4085e1f94f4f521a20dc030467e0f811a0be260e158b94d8fb8e2a892b` (results). A different checksum means your guest source differs from the plan; diff it before going on. Run the script a second time: `git diff --stat -- '*/elf/*.elf'` must not change (CI's reproducibility job).

- [ ] **Step 9: Run the KZG cheat tests on the new ELF**

```bash
cd /home/p4u/davinci-zkvm/go-sdk/tests && GEN_INPUT_BIN=/home/p4u/davinci-zkvm/target/release/gen-input \
  go test ./integration -run 'TestCheatSanity$|TestCheatKZGBlobCount|TestCheatWrongKZGCommitment|TestCheatMissingKZGBlock' -v -count=1 2>&1 | grep -E '^(--- |ok|FAIL)'
```

Expected: four `--- PASS` lines (`TestCheatKZGBlobCount` with its four subtests) and `ok`, in about 30 s.

- [ ] **Step 10: Refreeze the three vks**

```bash
cd /home/p4u/davinci-zkvm
for elf in circuit/elf/circuit.elf circuit-aggregator/elf/aggregator.elf circuit-results/elf/results.elf; do
  d=$(mktemp -d -p $HOME/.cache/compact-da)
  printf '%s ' "$elf"
  ZISK_CACHE_DIR=$d systemd-run --user --scope -q -p MemoryMax=50G -p MemorySwapMax=0 \
    cargo-zisk setup -e "$PWD/$elf" -k $HOME/.zisk/provingKey 2>&1 \
    | sed -n 's/.*Root hash: \[\(.*\)\]/\1/p' \
    | python3 -I -c "import sys; print('0x' + ''.join('%016x' % int(w) for w in sys.stdin.read().split(',')))"
  rm -rf "$d"
done
```

Expected: about 10 s per guest (the aggregator fits a 50 GB cap on this host). With the ELFs of Step 8:

```
circuit/elf/circuit.elf 0xa0ad5c4c7e39d4a7007933158181418a752e6c1563a211af84c579640db435fa
circuit-aggregator/elf/aggregator.elf 0x7d6c05197d40ae62b0f0c110c2457970b03544972a462891c761109f0722032b
circuit-results/elf/results.elf 0xe693589453ecaee63eba697c48d4c5321a714bb18c2d84a97437b3f368578f68
```

To check the procedure itself, the same loop on `main`'s ELFs prints today's pins (`0x6cfc89d5…7a10`, `0x8b31cbf4…c801`, `0x7bc8c5e9…e794`). Use the values your run printed in Step 11.

- [ ] **Step 11: Update the pins**

`rust-sdk/src/release.rs` (lines 25-35):

```rust
pub const BATCH_PROGRAM_VK: [u8; 32] = [
    0xa0, 0xad, 0x5c, 0x4c, 0x7e, 0x39, 0xd4, 0xa7, 0x00, 0x79, 0x33, 0x15, 0x81, 0x81, 0x41, 0x8a,
    0x75, 0x2e, 0x6c, 0x15, 0x63, 0xa2, 0x11, 0xaf, 0x84, 0xc5, 0x79, 0x64, 0x0d, 0xb4, 0x35, 0xfa,
];
```

```rust
pub const RESULTS_PROGRAM_VK: [u8; 32] = [
    0xe6, 0x93, 0x58, 0x94, 0x53, 0xec, 0xae, 0xe6, 0x3e, 0xba, 0x69, 0x7c, 0x48, 0xd4, 0xc5, 0x32,
    0x1a, 0x71, 0x4b, 0xb1, 0x8c, 0x2d, 0x84, 0xa9, 0x74, 0x37, 0xb3, 0xf3, 0x68, 0x57, 0x8f, 0x68,
];
```

`go-sdk/chain/release.go` (lines 31-33):

```go
	AggVK:     "0x7d6c05197d40ae62b0f0c110c2457970b03544972a462891c761109f0722032b",
	BatchVK:   "0xa0ad5c4c7e39d4a7007933158181418a752e6c1563a211af84c579640db435fa",
	ResultsVK: "0xe693589453ecaee63eba697c48d4c5321a714bb18c2d84a97437b3f368578f68",
```

`rust-sdk/tests/wire.rs`: in `batch_publics_from_recorded_job` (lines 210-213) the recorded GPU fixture keeps the vk it was proved under:

```rust
    // Recorded on the v0.1 batch ELF; the publics layout did not change since.
    assert_eq!(
        s(&snark["program_vk"]),
        "0x6cfc89d562d0b22f04478a5c15b390433eb52f1b03147030b183076260da7a10"
    );
```

and in `release_pins` (lines 337-342):

```rust
        hex::encode(BATCH_PROGRAM_VK),
        "a0ad5c4c7e39d4a7007933158181418a752e6c1563a211af84c579640db435fa"
    );
    assert_eq!(
        hex::encode(RESULTS_PROGRAM_VK),
        "e693589453ecaee63eba697c48d4c5321a714bb18c2d84a97437b3f368578f68"
```

Run: `cd /home/p4u/davinci-zkvm && cargo test -p davinci-zkvm-sdk --test wire 2>&1 | grep '^test result' && (cd go-sdk && go test ./chain/)`
Expected: `9 passed`, `ok …/go-sdk/chain`.

- [ ] **Step 12: Update CIRCUIT.md**

§2 KZGBLK, lines 214-216:

```markdown
No `y_claimed`, no blob bytes. The guest rebuilds the blob cells from
verified state (vote-id list and sorted slot updates; see §8) and evaluates
each polynomial itself.
```

§8: replace from the paragraph `Runs **after** §4.2 …` (line 893) down to the blank line before `### Evaluation point derivation` (line 961) with:

````markdown
Runs **after** §4.2 (SMT chains) and §4.3 (re-encryption): it needs the
state chains and the guest-computed `refreshed_new` ballots.

### Purpose

Bind the sequencer's DA-blob commitments to what the circuit has just proved.
Blob cells are not host input: the guest rebuilds them from verified state
(vote-id list and sorted slot updates covering new votes, overwrites and
silent refreshes alike), packs them into 4096-cell EIP-4844 blobs, and
evaluates each blob polynomial at the point derived from that blob's
commitment. The Results accumulator is not in the blobs: a follower
recomputes it (see "Results accumulator" below). The emitted `BlobsDigest`
(§3) hashes the ordered `(commitment, y)` pairs; the settlement contract
checks each pair against the blob's versioned hash via the EIP-4844
point-evaluation precompile.

The KZG block itself carries only the process context and the per-blob
commitments (see §2 KZGBLK). Absent KZG block or absent STATETX: trivially
pass with `BlobsDigest = 0` and `NBlobs = 0`.

### Cell layout

Cells are 32-byte big-endian BLS12-381 Fr elements. Encoders:

- `header(v, u)` = `v · 2⁶⁴ + u`. Both counts are below 2¹²; bits 128..255
  are a version field, zero, and every other unused bit is zero.
- `pack4(a, b, c, d)` = `a · 2¹⁸⁹ + b · 2¹²⁶ + c · 2⁶³ + d` for four integers
  below 2⁶³, built with shifts; the result is below 2²⁵².
- `pack(x, y)` = `y_canon + ((x_canon & 1) << 254)`, where `x_canon`,
  `y_canon` are the BabyJubJub coords reduced mod BN254 Fr. Fits in
  BLS12-381 Fr since `p_bn254 + 2²⁵⁴ < r_bls`; BN254 Fr `< 2²⁵⁴`, so bit 254
  of `y_canon` is 0 (no collision with the parity tag). The TE identity
  `(0, 1)` packs to the integer `1`.

Layout in cell order:

```
header(n_vids, n_updates)
pack4(I[0], I[1], I[2], I[3])  pack4(I[4], ..) ..       // ceil((n_vids + n_updates) / 4) cells
    I = [vid_0 & (2^63 - 1) .. vid_{n_vids-1} & (2^63 - 1), key_0 .. key_{n_updates-1}]
    missing elements of the last cell are zero
for each (key, ballot) update, ascending by key (stable):
    pack(c1_0) pack(c2_0) .. pack(c1_{nf-1}) pack(c2_{nf-1})
zero cells to end of last blob
```

Where:

- Vote ids come from `state.vote_id_chain[*].new_key[0]` (the upper limbs
  are zero by 4.1.4b), sorted ascending. 4.1 pins every id to
  `[2⁶³, 2⁶⁴)`, so the top bit is implied and the packed form is
  `vid & (2⁶³ − 1)`. The guest masks; it never computes `vid − 2⁶³`, so an
  out-of-namespace id (already `FAIL_CONSISTENCY`) cannot make it panic.
- Updates cover EVERY slot the batch touched: first the ballot-chain entries
  (in ballot-chain order, ballot = `state.voter_ballots[i]`), then the
  refresh-chain entries (in refresh-chain order, ballot = `refreshed_new[j]`
  as re-encrypted in §4.3/§4.5). The combined list is then stable-sorted by
  `key = transition.new_key[0]`, so an on-chain reader sees one uniform
  sorted list and cannot tell an overwrite from a silent refresh. Keys are
  below 2⁶³ (4.1, 4.5.3), so they fill a 63-bit slot as they are. A key
  written for the first time is a new vote: refreshes only touch occupied
  slots (§4.5). `nf = num_fields` from the BallotMode config; padded fields
  `f ≥ nf` are not emitted (soundness rests on §4.3.4 / §4.5.7 pinning
  them to the TE identity).

Cell count:

```
T = 1 + ceil((n_vids + n_updates) / 4) + 2·nf·n_updates
n_blobs = ceil(T / 4096)          // 1 ≤ n_blobs ≤ MAX_BLOBS = 32
```

### Results accumulator

The NEW net Results ballot is not emitted. A follower holds the old Results
leaf and every occupied slot's ballot under `OldStateRoot`, and computes
`old_results + Σ_u (new_ballot_u − old_ballot_{key_u})`, with the identity as
the old ballot of an empty slot. That is the §4.4 value regrouped by slot:
a new vote adds `new − identity`; an overwrite subtracts
`overwritten_ballots[j]`, pinned to the tree's old leaf (4.4.3); a refresh
adds `new_j − old_j` with `old_j` pinned the same way (4.5.5, 4.5.6); a slot
is never refreshed and written in one batch (4.5.3); the first batch starts
from the genesis identity leaf. The group is abelian and both sides use the
complete twisted-Edwards law on canonical coordinates (13.2), so the
follower's tree reaches `NewStateRoot` exactly when its stored ballots are
right.

````

§8 Security rationale: replace the first bullet (lines 1015-1018) with:

```markdown
- **No unbound blob bytes.** Every cell is reconstructed from data already
  bound by earlier phases (SMT chains, ballot leaf hashes and refresh
  checks). A malicious sequencer cannot smuggle extra cells or swap contents
  without breaking one of those.
- **Packed integers.** Four 63-bit slots per cell never overlap: ids enter
  masked and keys are below 2⁶³. The header's counts are no secret:
  `n_vids` is register 18 and `n_updates` the number of keys listed, and the
  list still does not mark which updates are overwrites.
```

§8 Performance note (lines 1033-1039):

```markdown
Each blob costs roughly 16k–25k `arith256_mod` calls, depending on how many
cells are non-zero: the 4096 differences, the batch inverse (the largest
part, ~12k) and two calls per non-zero cell for the weighted sum. The
4096-entry root-of-unity table (~4.1k calls) is built once per batch. Small
transitions fit in one blob. `MAX_BLOBS = 32` (131072 cells) is above the
largest valid transition (1024 votes, 2048 refreshes, 16 fields: T = 99329,
25 blobs); a 1024-vote batch with 1024 refreshes at 16 fields needs 17.
```

§12.8, lines 1231-1233:

```markdown
Blob cells are rebuilt in-guest from verified state (vote-id list and
sorted slot updates covering new votes, overwrites and silent refreshes; a
follower recomputes the Results ballot, §8). Each blob polynomial is
evaluated at
```

§13.5, lines 1286-1289:

```markdown
The KZG block carries no blob bytes: the guest rebuilds the cells from
verified state (vote-id list and sorted slot updates covering new votes,
overwrites and silent refreshes) and evaluates each blob polynomial itself
(§8).
```

Check: `grep -n 'NEW net accumulator\|net-Results leaf' circuit/CIRCUIT.md` prints nothing.

`CLAUDE.md` (gitignored, edit only), the "DA blob is built in-guest" gotcha (lines 395-399):

```markdown
  only `process_id`, `root_hash_before` and the blob commitments. The guest
  lays out the cells itself (a header with both counts, then the sorted vote
  identifiers and slot keys four per cell, then one sorted list of slot
  updates for new votes, overwrites and refreshes alike with the active
  ciphertexts compressed to one field element per point; spec §8; the
  Results accumulator is not in the blobs, a follower recomputes it),
  evaluates every blob polynomial at
```

- [ ] **Step 13: Run every check CI runs**

```bash
cd /home/p4u/davinci-zkvm
cargo fmt --all --check
cargo clippy -p davinci-zkvm-service -p davinci-zkvm-sdk -p davinci-zkvm-input-gen --all-targets -- -D warnings
cargo test -p davinci-zkvm-service -p davinci-zkvm-sdk -p davinci-zkvm-input-gen 2>&1 | grep -E '^test result' | grep -v ' 0 failed'; echo "no failing suites above"
cd go-sdk && gofmt -l . && go vet ./... && go build ./tests/integration/... && go test $(go list ./... | grep -v /tests/integration) 2>&1 | grep -v -E 'no test files|^ok'; echo "no failing go packages above"
go run ./cmd/sdk-vectors -out ../rust-sdk/testdata && git status --porcelain -- ../rust-sdk/testdata ../rust-sdk/assets   # expect nothing
cd tests && GEN_INPUT_BIN=/home/p4u/davinci-zkvm/target/release/gen-input \
  GEN_RESULTS_INPUT_BIN=/home/p4u/davinci-zkvm/target/release/gen-results-input \
  go test ./integration -run 'TestCheat|TestResultsCheat' -v -count=1 -timeout 75m > $TMPDIR/cheat.log 2>&1
grep -E -- '--- (FAIL|SKIP)' $TMPDIR/cheat.log; tail -1 $TMPDIR/cheat.log
```

Expected: no FAIL and no SKIP lines; the last line is `ok  github.com/vocdoni/davinci-zkvm/go-sdk/tests/integration  …` after about 5 minutes (`TestResultsCheat` runs the rebuilt `results.elf`).

- [ ] **Step 14: Commit**

```bash
cd /home/p4u/davinci-zkvm
git add circuit-primitives/src/da_blob.rs circuit/src/kzg.rs circuit/src/main.rs circuit/CIRCUIT.md \
  circuit/elf/circuit.elf circuit-aggregator/elf/aggregator.elf circuit-results/elf/results.elf \
  rust-sdk/src/release.rs go-sdk/chain/release.go rust-sdk/tests/wire.rs go-sdk/tests/integration/cheat_test.go
git commit -m "feat(circuit): compact da stream and one omega table per batch" -m "The guest lays out the header, the packed vote ids and keys and the ballots,
and no longer emits the Results accumulator. The KZG root-of-unity table is
built once per batch. Any code change in circuit-primitives repartitions its
codegen units, so all three ELFs move: batch vk 0xa0ad5c4c...35fa,
aggregator vk 0x7d6c0519...032b, results vk 0xe6935894...8f68, refrozen in
rust-sdk and go-sdk. TestCheatKZGBlobCount checks the guest's blobs digest
against BuildTransitionBlobs and the FAIL_KZG count check at every residue of
V + U mod 4."
```

(Write the vks your Step 10 printed.)

---

### Task 3: Emulator step counts before and after

**Files:**
- Create: `go-sdk/tests/integration/emusteps_test.go`
- Modify: `BENCHMARK.md` (new subsection after the settlement-gas table, before `## Chained mode (ZisK 0.18)`)

**Interfaces:**
- Consumes: the harness (`NewElection`, `CachedBallotBatch`, `BuildReencBlock`, `BuildStateBlock`, `BuildKZGBlock`, `encodeKZGRequest`, `eligibilityBlock`, `findGenInputBin`), all unchanged in signature on `main` and on the branch.
- Produces: `TestEmuSteps`, gated by `EMU_STEPS_OUT=<dir>`.

- [ ] **Step 1: Write the measurement test**

Create `go-sdk/tests/integration/emusteps_test.go`:

```go
// emusteps_test.go measures the vote-batch guest on ziskemu. It writes the
// steady batch of TestPlonkBenchmark (size new votes plus size silent
// refreshes, from the cached ballots) as a guest input and runs
// `ziskemu -X --save-stats` on it. Gated by EMU_STEPS_OUT=<dir>; honours
// BENCH_SIZES (default 256,512) and BALLOT_NUM_FIELDS. Needs ziskemu and
// gen-input, no prover.
package integration

import (
	"encoding/binary"
	"encoding/json"
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"strconv"
	"strings"
	"testing"

	davinci "github.com/vocdoni/davinci-zkvm/go-sdk"
	"github.com/vocdoni/davinci-zkvm/go-sdk/vocdoni/circuits/ballotproof"
)

func TestEmuSteps(t *testing.T) {
	out := os.Getenv("EMU_STEPS_OUT")
	if out == "" {
		t.Skip("set EMU_STEPS_OUT to measure emulator steps")
	}
	ziskemu, err := exec.LookPath("ziskemu")
	if err != nil {
		t.Skip("ziskemu not in PATH")
	}
	elf := os.Getenv("CIRCUIT_ELF_PATH")
	if elf == "" {
		elf = "../../../circuit/elf/circuit.elf"
	}
	if os.Getenv("DAVINCI_TEST_ELECTION_SEED") == "" {
		t.Setenv("DAVINCI_TEST_ELECTION_SEED", "plonk-bench")
	}
	sizes := []int{256, 512}
	if v := os.Getenv("BENCH_SIZES"); v != "" {
		sizes = nil
		for _, s := range strings.Split(v, ",") {
			n, err := strconv.Atoi(strings.TrimSpace(s))
			if err != nil || n <= 0 {
				t.Fatalf("bad BENCH_SIZES entry %q", s)
			}
			sizes = append(sizes, n)
		}
	}
	if err := os.MkdirAll(out, 0o755); err != nil {
		t.Fatal(err)
	}

	for _, size := range sizes {
		// The first batch only moves the harness state; the steady one is run.
		election, err := NewElection(2 * size)
		if err != nil {
			t.Fatalf("NewElection: %v", err)
		}
		first, err := CachedBallotBatch(election, election.Voters[:size], 42)
		if err != nil {
			t.Fatalf("CachedBallotBatch: %v", err)
		}
		_, reenc1, err := election.BuildReencBlock(election.OldRoot, first.Results)
		if err != nil {
			t.Fatalf("BuildReencBlock: %v", err)
		}
		if _, _, err := election.BuildStateBlock(election.Voters[:size], first.Results, reenc1); err != nil {
			t.Fatalf("BuildStateBlock: %v", err)
		}
		voters := election.Voters[size : 2*size]
		batch, err := CachedBallotBatch(election, voters, 43)
		if err != nil {
			t.Fatalf("CachedBallotBatch: %v", err)
		}
		input := steadyInput(t, election, voters, batch.Results)

		name := filepath.Join(out, fmt.Sprintf("steady-%d-nf%d", size, election.NumFields))
		var framed []byte
		framed = binary.LittleEndian.AppendUint64(framed, uint64(len(input)))
		framed = append(framed, input...)
		framed = append(framed, make([]byte, (8-len(input)%8)%8)...)
		if err := os.WriteFile(name+".bin", framed, 0o644); err != nil {
			t.Fatal(err)
		}
		cmd := exec.Command(ziskemu, "-e", elf, "-i", name+".bin", "-o", name+".out",
			"-X", "--save-stats", name+".stats")
		report, err := cmd.CombinedOutput()
		if err != nil {
			t.Fatalf("ziskemu: %v\n%s", err, report)
		}
		if err := os.WriteFile(name+".txt", report, 0o644); err != nil {
			t.Fatal(err)
		}
		regs, err := os.ReadFile(name + ".out")
		if err != nil {
			t.Fatal(err)
		}
		if ok := binary.LittleEndian.Uint32(regs[0:]); ok != 1 {
			t.Fatalf("size %d: guest rejected the batch: fail_mask %#x", size, binary.LittleEndian.Uint32(regs[4:]))
		}
		t.Logf("size %d nf %d: %d blobs, report in %s.txt", size, election.NumFields,
			binary.LittleEndian.Uint32(regs[4*davinci.OutputNBlobs:]), name)
	}
}

// steadyInput assembles the guest input of one batch the way the cheat
// harness does: gen-input's proof section, then STATETX, CENSUS, REENCBLK
// and KZGBLK.
func steadyInput(t *testing.T, e *Election, voters []*Voter, results []*BallotResult) []byte {
	t.Helper()
	dir := t.TempDir()
	if err := os.WriteFile(filepath.Join(dir, "verification_key.json"), ballotproof.CircomVerificationKey, 0o600); err != nil {
		t.Fatal(err)
	}
	for i, res := range results {
		pub, err := json.Marshal(res.PublicInputs)
		if err != nil {
			t.Fatal(err)
		}
		for name, b := range map[string][]byte{"proof": res.ProofJSON, "public": pub, "sig": res.SigJSON} {
			if err := os.WriteFile(filepath.Join(dir, fmt.Sprintf("%s_%d.json", name, i+1)), b, 0o600); err != nil {
				t.Fatal(err)
			}
		}
	}
	bin := filepath.Join(dir, "input.bin")
	cmd := exec.Command(findGenInputBin(t), "--proofs-dir", dir, "--output", bin, "--nproofs", strconv.Itoa(len(results)))
	if out, err := cmd.CombinedOutput(); err != nil {
		t.Fatalf("gen-input: %v\n%s", err, out)
	}
	base, err := os.ReadFile(bin)
	if err != nil {
		t.Fatal(err)
	}

	oldRoot := e.OldRoot
	reenc, reencBallots, err := e.BuildReencBlock(oldRoot, results)
	if err != nil {
		t.Fatalf("BuildReencBlock: %v", err)
	}
	state, _, err := e.BuildStateBlock(voters, results, reencBallots)
	if err != nil {
		t.Fatalf("BuildStateBlock: %v", err)
	}
	kzg, _, err := e.BuildKZGBlock(oldRoot)
	if err != nil {
		t.Fatalf("BuildKZGBlock: %v", err)
	}
	stateBin, err := davinci.EncodeStateBlock(state)
	if err != nil {
		t.Fatal(err)
	}
	reencBin, err := davinci.EncodeReencBlock(reenc)
	if err != nil {
		t.Fatal(err)
	}
	kzgBin, err := encodeKZGRequest(kzg)
	if err != nil {
		t.Fatal(err)
	}
	input := append(base, stateBin...)
	input = append(input, eligibilityBlock(t, e, voters)...)
	input = append(input, reencBin...)
	return append(input, kzgBin...)
}
```

Run: `cd /home/p4u/davinci-zkvm/go-sdk && gofmt -l tests && go vet ./tests/integration/ && go test ./tests/integration -run TestEmuSteps -v 2>&1 | grep -E 'SKIP|ok'`
Expected: `--- SKIP: TestEmuSteps` (gate unset) and `ok`.

- [ ] **Step 2: Measure the baseline on main**

The ballots are the cached ones of `TestPlonkBenchmark` in the main checkout's `benchmark/cache` (seeds 42 and 43, `plonk-bench`, nf 2 and 16, sizes 256 and 512), so no ballot is proved. The KZG block depends on the stream layout, so the baseline must run `main`'s harness with `main`'s ELF:

```bash
cd /home/p4u/davinci-zkvm
git worktree add $HOME/.cache/compact-da/zkvm-main main
cp go-sdk/tests/integration/emusteps_test.go $HOME/.cache/compact-da/zkvm-main/go-sdk/tests/integration/
for side in before:$HOME/.cache/compact-da/zkvm-main after:/home/p4u/davinci-zkvm; do
  dir=${side%%:*}; repo=${side#*:}
  for nf in 2 16; do
    (cd $repo/go-sdk/tests && BALLOT_NUM_FIELDS=$nf EMU_STEPS_OUT=$HOME/.cache/compact-da/steps/$dir \
      BENCH_SIZES=256,512 BENCH_CACHE_DIR=/home/p4u/davinci-zkvm/benchmark/cache \
      GEN_INPUT_BIN=/home/p4u/davinci-zkvm/target/release/gen-input \
      go test ./integration -run TestEmuSteps -v -count=1 -timeout 60m 2>&1 | grep -E 'blobs|^(ok|FAIL)')
  done
done
```

Expected: eight `size … nf …: N blobs` lines and four `ok`, about 1.5 minutes per side. Blobs, the same on both sides: 256 × 2 → 1, 512 × 2 → 2, 256 × 16 → 5, 512 × 16 → 9.

- [ ] **Step 3: Tabulate**

```bash
cd $HOME/.cache/compact-da/steps && python3 -I - <<'EOF'
import re
def get(d, f):
    t = open(f"{d}/{f}.txt").read()
    num = lambda p: int(re.search(p, t, re.M).group(1).replace(",", ""))
    return num(r"^STEPS\s+([\d,]+)"), num(r"^TOTAL\s+([\d,]+)"), num(r"OP arith256_mod\s+([\d,]+)")
for n, nf, b in [(256, 2, 1), (512, 2, 2), (256, 16, 5), (512, 16, 9)]:
    f = f"steady-{n}-nf{nf}"
    (s0, c0, a0), (s1, c1, a1) = get("before", f), get("after", f)
    print(f"| {n} | {nf} | {b} | {s0:,} | {s1:,} | {100*(s1-s0)/s0:+.2f} % | {c0:,} | {c1:,} | {100*(c1-c0)/c0:+.2f} % | {a0:,} | {a1:,} |")
EOF
```

Expected (this plan's scratch run; yours lands within 0.01 %, since the re-encryption seed and the refresh sample are random per run):

```
| 256 | 2 | 1 | 110,906,413 | 110,884,679 | -0.02 % | 24,304,534,982 | 24,326,836,710 | +0.09 % | 3,878,016 | 3,876,846 |
| 512 | 2 | 2 | 227,343,493 | 227,192,865 | -0.07 % | 49,622,421,929 | 49,645,638,886 | +0.05 % | 8,104,386 | 8,097,949 |
| 256 | 16 | 5 | 196,971,166 | 196,513,198 | -0.23 % | 44,432,704,215 | 44,368,783,247 | -0.14 % | 8,209,900 | 8,192,158 |
| 512 | 16 | 9 | 398,325,831 | 397,410,188 | -0.23 % | 89,711,259,340 | 89,582,954,231 | -0.14 % | 16,743,319 | 16,707,965 |
```

`ziskemu --diff-stats before/<f>.stats after/<f>.stats --diff-format csv` breaks a row down by opcode and precompile. A rejected batch fails the test before any report is written, so every row is an accepted batch.

- [ ] **Step 4: Record the numbers in BENCHMARK.md**

Insert before `## Chained mode (ZisK 0.18)`, with your rows from Step 3:

```markdown
### DA stream on the emulator

The compact DA stream (v0.2.0) packs vote ids and slot keys four per cell,
drops the Results accumulator from the blobs and builds the KZG
root-of-unity table once per batch instead of once per blob. Guest cost from
`ziskemu -X` on the steady batch of `TestPlonkBenchmark` (N votes plus N
silent refreshes, the cached ballots), measured with `TestEmuSteps`
(`go-sdk/tests/integration/emusteps_test.go`) on v0.1.1 and on v0.2.0:

| N | nf | blobs | steps v0.1.1 | steps v0.2.0 | Δ | cost v0.1.1 | cost v0.2.0 | Δ | `arith256_mod` v0.1.1 | v0.2.0 |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 256 | 2 | 1 | 110,906,413 | 110,884,679 | −0.02 % | 24,304,534,982 | 24,326,836,710 | +0.09 % | 3,878,016 | 3,876,846 |
| 512 | 2 | 2 | 227,343,493 | 227,192,865 | −0.07 % | 49,622,421,929 | 49,645,638,886 | +0.05 % | 8,104,386 | 8,097,949 |
| 256 | 16 | 5 | 196,971,166 | 196,513,198 | −0.23 % | 44,432,704,215 | 44,368,783,247 | −0.14 % | 8,209,900 | 8,192,158 |
| 512 | 16 | 9 | 398,325,831 | 397,410,188 | −0.23 % | 89,711,259,340 | 89,582,954,231 | −0.14 % | 16,743,319 | 16,707,965 |

The stream saves blob space, not proving: every change stays within 0.25 %.
The layout alone (measured with the omega table still built per blob) adds
0.09 % of cost at 256 × 2 and 0.05 % at 512 × 16; building the table once
saves about 4.1k `arith256_mod` calls per extra blob, 0.19 % at 512 × 16.
Repeated runs differ by about 0.0005 %.
```

- [ ] **Step 5: Clean up and commit**

```bash
cd /home/p4u/davinci-zkvm && git worktree remove --force $HOME/.cache/compact-da/zkvm-main
git add go-sdk/tests/integration/emusteps_test.go BENCHMARK.md
git commit -m "test(circuit): emulator cost of the compact da stream" -m "TestEmuSteps runs the steady batch of TestPlonkBenchmark from the cached
ballots through ziskemu -X. Against v0.1.1 the compact stream moves the
guest cost by -0.14 % to +0.09 % at 256 and 512 votes, 2 and 16 fields."
```

---

### Task 4: The sequencer follows the compact stream

**Files:**
- Modify: `state/src/sync.rs` (new `results_after`; `apply` lines 32-91)
- Modify: `state/src/transition.rs:433-438`
- Modify: `state/tests/sync.rs:9-14`, `:96-98`, append tests
- Modify: `state/tests/transition.rs:408-415`, `:576-578`
- Modify: `sequencer/tests/stack.rs:26`, `sequencer/tests/l1da.rs:17`, `:28`, `sequencer/tests/web3_anvil.rs:343`, `:414`, `:420`, `:1112`, `:2374`, `sequencer/src/web3/blobs.rs:507`, `:515`, `sequencer/src/web3/l1.rs:462`, `:604`, `sequencer/src/web3/tx.rs:756`
- Modify: `docs/lifecycle.md:95-97`

**Interfaces:**
- Consumes: Task 1's `TransitionData { vote_ids, updates, num_fields }`, `decode_blobs`; Task 2's pins (through `davinci_zkvm_sdk::release`).
- Produces: `impl<S: Storage> ProcessState<S> { pub fn results_after(&self, t: &TransitionData) -> Result<Ballot, Error> }`.

- [ ] **Step 1: Branch, and see what the SDK change breaks**

```bash
git -C /home/p4u/davinci-zkvm branch --show-current     # feat/compact-da-stream, after Task 3
cd /home/p4u/davinci-sequencer
git checkout feat/l1-blob-da && git checkout -b feat/compact-da-stream
cargo build --workspace --all-targets --keep-going 2>&1 | grep -E '^error|-->' | head -30
```

Expected: the `davinci-state` library stops on `accumulator` (`state/src/sync.rs:65` and `:82`, `no field \`accumulator\``; `state/src/transition.rs:436`, `has no field named \`accumulator\``), so the crates above it are not compiled yet. Once Step 4 fixes the library, the same command lists the ten fixture lines of Step 7 (Step 2 rewrites the eleventh, `state/tests/sync.rs:98`).

- [ ] **Step 2: Write the failing follower tests**

In `state/tests/sync.rs`, replace the imports (lines 9-14):

```rust
use arbo::{MemoryStorage, Storage, WriteBatch};
use common::*;
use davinci_state::{PreparedBatch, ProcessState, VerifiedVote};
use davinci_zkvm_sdk::ballot::Ballot;
use davinci_zkvm_sdk::blob::{TransitionData, decode_blobs};
use davinci_zkvm_sdk::census::slot_key_address;
use davinci_zkvm_sdk::crypto::elgamal::decrypt;
use davinci_zkvm_sdk::reenc::{ReencChain, reencrypt_ballot};
use rand::SeedableRng;
use rand::rngs::StdRng;
```

In `tampered_blob_content_is_rejected` the published accumulator is gone; swap two published ballots instead (line 98):

```rust
    bad.updates[0].1 = bad.updates[1].1;
```

Append:

```rust
// Leader `a` proves and commits `votes`; follower `b` recomputes the Results
// ballot from the blobs and its stored ballots, then applies the transition.
fn step(
    a: &mut ProcessState<MemoryStorage>,
    b: &mut ProcessState<MemoryStorage>,
    votes: &[VerifiedVote],
    rng: &mut StdRng,
) -> PreparedBatch {
    let p = a.prepare(votes, rng, &none()).unwrap();
    a.commit(&p).unwrap();
    let t = decode_blobs(&p.blobs.blobs, a.config().ballot_mode.num_fields).unwrap();
    assert_eq!(b.results_after(&t).unwrap(), a.committed().accumulator);
    b.apply_synced(&t, p.new_root).unwrap();
    assert_eq!(b.committed(), a.committed());
    p
}

fn slot(i: usize) -> u64 {
    slot_key_address(&voter_address(i))
}

#[test]
fn results_after_matches_the_guest_leaf() {
    let env = env(2, 8);
    let mut a = new_state(&env);
    let mut b = new_state(&env);
    let mut rng = StdRng::seed_from_u64(31);
    let vote = |i: usize, f: &[u64], k: u64| fake_vote(&env, i, f, k);

    // First batch: new votes on the genesis (identity) Results ballot.
    let p = step(
        &mut a,
        &mut b,
        &[
            vote(0, &[1, 0], 400),
            vote(1, &[0, 1], 401),
            vote(2, &[1, 1], 402),
        ],
        &mut rng,
    );
    assert!(p.refresh_slots().is_empty());

    // A new vote and three refreshes, voter 0's slot among them.
    let p = step(&mut a, &mut b, &[vote(3, &[1, 0], 403)], &mut rng);
    assert!(p.refresh_slots().contains(&slot(0)));

    // Voter 0 revotes on the slot refreshed one batch ago, voter 4 is new.
    let p = step(
        &mut a,
        &mut b,
        &[vote(0, &[0, 1], 404), vote(4, &[1, 0], 405)],
        &mut rng,
    );
    assert_eq!((p.overwrites, p.refresh_slots().len()), (1, 3));

    // Only an overwrite, plus refreshes.
    let p = step(&mut a, &mut b, &[vote(1, &[1, 1], 406)], &mut rng);
    assert_eq!((p.overwrites, p.refresh_slots().len()), (1, 4));

    // Only refreshes. The guest wants at least one vote, so no leader builds
    // this; the follower still computes it, and re-encryption keeps the tally.
    let mut chain = ReencChain::new(&[7u8; 32], &b.root());
    let mut slots: Vec<u64> = (0..5).map(slot).collect();
    slots.sort_unstable();
    let updates = slots
        .into_iter()
        .map(|k| {
            let old = b.slot_ballot(k).unwrap().unwrap();
            (k, reencrypt_ballot(&old, &env_pk(&env), 2, &mut chain))
        })
        .collect();
    let t = TransitionData {
        vote_ids: vec![],
        updates,
        num_fields: 2,
    };
    let before = b.committed().accumulator;
    let after = b.results_after(&t).unwrap();
    assert_ne!(after, before);
    for f in 0..2 {
        let tally = decrypt(&env.sk, &before.0[f], 100);
        assert!(tally.is_some());
        assert_eq!(decrypt(&env.sk, &after.0[f], 100), tally, "field {f}");
    }
}

fn slot_db_key(i: usize) -> Vec<u8> {
    [b"slot/".as_slice(), &slot(i).to_be_bytes()].concat()
}

fn put(store: &MemoryStorage, k: Vec<u8>, v: Vec<u8>) {
    let mut w = WriteBatch::new();
    w.put(k, v);
    store.write(w).unwrap();
}

#[test]
fn corrupted_stored_ballot_fails_the_root_check() {
    let env = env(2, 4);
    let mut a = new_state(&env);
    let store = MemoryStorage::new();
    let mut b = ProcessState::create(env.cfg.clone(), store.clone()).unwrap();
    let mut rng = StdRng::seed_from_u64(32);
    let votes = vec![
        fake_vote(&env, 0, &[1, 2], 500),
        fake_vote(&env, 1, &[2, 0], 501),
    ];
    step(&mut a, &mut b, &votes, &mut rng);

    // Voter 0's stored ballot becomes voter 1's: a valid encoding, the wrong value.
    let honest = store.get(&slot_db_key(0)).unwrap().unwrap();
    put(
        &store,
        slot_db_key(0),
        store.get(&slot_db_key(1)).unwrap().unwrap(),
    );

    // Voter 0 revotes: the follower subtracts the wrong old ballot.
    let p = a
        .prepare(&[fake_vote(&env, 0, &[2, 2], 502)], &mut rng, &none())
        .unwrap();
    a.commit(&p).unwrap();
    let t = decode_blobs(&p.blobs.blobs, 2).unwrap();
    let g = b.root();
    let err = b.apply_synced(&t, p.new_root).unwrap_err();
    assert!(
        matches!(err, davinci_state::Error::RootMismatch { .. }),
        "{err}"
    );
    assert_eq!(b.root(), g);

    // With the honest ballot back the same transition applies.
    put(&store, slot_db_key(0), honest);
    b.apply_synced(&t, p.new_root).unwrap();
    assert_eq!(b.committed(), a.committed());
}

#[test]
fn occupied_slot_without_a_ballot_is_an_error() {
    let env = env(2, 4);
    let mut a = new_state(&env);
    let store = MemoryStorage::new();
    let mut b = ProcessState::create(env.cfg.clone(), store.clone()).unwrap();
    let votes = vec![fake_vote(&env, 0, &[1, 2], 600)];
    step(&mut a, &mut b, &votes, &mut StdRng::seed_from_u64(33));

    // List a slot as occupied without storing its ballot, then reopen.
    let ghost = 0x20u64;
    let mut occ = store.get(b"occ").unwrap().unwrap();
    occ.extend_from_slice(&ghost.to_le_bytes());
    let mut slots: Vec<u64> = occ
        .chunks_exact(8)
        .map(|c| u64::from_le_bytes(c.try_into().unwrap()))
        .collect();
    slots.sort_unstable();
    put(
        &store,
        b"occ".to_vec(),
        slots.iter().flat_map(|s| s.to_le_bytes()).collect(),
    );
    let b = ProcessState::open(env.cfg.clone(), store.clone(), &b.committed()).unwrap();

    let t = TransitionData {
        vote_ids: vec![],
        updates: vec![(ghost, Ballot::identity())],
        num_fields: 2,
    };
    let err = b.results_after(&t).unwrap_err();
    assert!(matches!(err, davinci_state::Error::Invalid(_)), "{err}");
}
```

`MemoryStorage` clones share one map, so the test writes the follower's slot store and occupied list (`b"slot/" ‖ slot_be8`, `b"occ"`, the layout documented at the top of `state/src/process.rs`) through its own handle.

- [ ] **Step 3: Run them to see them fail**

Run: `cd /home/p4u/davinci-sequencer && cargo test -p davinci-state --test sync 2>&1 | grep -E '^error' | head -3`
Expected: `error[E0599]: no method named \`results_after\`` (and the `accumulator` error in `state/src/sync.rs`).

- [ ] **Step 4: Implement the recomputation**

In `state/src/sync.rs`, import `Ballot`:

```rust
use davinci_zkvm_sdk::ballot::{Ballot, ballot_leaf_hash, leaf_value_bytes};
```

Insert at the top of `impl<S: Storage> ProcessState<S>`, before `apply_synced`, and update `apply_synced`'s doc:

```rust
    /// The Results ballot after `t` on the committed state: the committed
    /// accumulator plus, per update, the new ballot minus the slot's stored
    /// one (the identity for a slot not yet occupied). The guest's
    /// `old + votes - overwritten + refresh deltas`, grouped by slot: every
    /// overwritten or refreshed ballot it subtracts is the leaf this node
    /// stored under the same root. Complete twisted-Edwards adds on canonical
    /// coordinates, so no subgroup check; the caller's root check binds it.
    pub fn results_after(&self, t: &TransitionData) -> Result<Ballot, Error> {
        let mut acc = self.committed.accumulator;
        for (i, (slot, new)) in t.updates.iter().enumerate() {
            let old = if self.occupied.binary_search(slot).is_ok() {
                self.slot_ballot(*slot)?.ok_or_else(|| {
                    Error::Invalid(format!("update {i}: occupied slot has no stored ballot"))
                })?
            } else {
                Ballot::identity()
            };
            acc = acc.add(new).sub(&old);
        }
        Ok(acc)
    }

    /// Applies a transition another sequencer proved: vote-id leaves, slot
    /// updates and the recomputed results leaf, then requires the tree to
    /// land on `new_root` (the root the accepted proof published). The blob
    /// does not say which updates were overwrites, so `w = votes - new slots`.
```

In `apply`, compute it before touching the tree (after the `num_fields` check) and use it for the leaf and the committed record:

```rust
        let accumulator = self.results_after(t)?;
        for &vid in &t.vote_ids {
```

```rust
        self.tree.update_with_proof(
            &key_bytes(KEY_RESULTS),
            &leaf_value_bytes(&ballot_leaf_hash(&accumulator)),
        )?;
```

```rust
            overwrites: self.committed.overwrites + w as u64,
            accumulator,
        };
```

The slot store and `occupied` only move in `persist` after the root check, so `results_after` reads the pre-batch ballots. Errors name update positions, not slots.

In `state/src/transition.rs`, the blob input loses the field (lines 433-438); `PreparedBatch.accumulator` (the leader's own net) stays:

```rust
        let tdata = TransitionData {
            vote_ids: vote_ids.clone(),
            updates,
            num_fields: nf,
        };
```

- [ ] **Step 5: Run the follower tests**

Run: `cd /home/p4u/davinci-sequencer && cargo test -p davinci-state --test sync 2>&1 | grep -E '^test |^test result'`
Expected: 7 tests `ok` (`follower_reaches_the_same_state_from_blobs`, `tampered_blob_content_is_rejected`, `apply_rejects_malformed_transitions`, `failed_persist_leaves_memory_at_the_committed_root`, `results_after_matches_the_guest_leaf`, `corrupted_stored_ballot_fails_the_root_check`, `occupied_slot_without_a_ballot_is_an_error`).

- [ ] **Step 6: Move the blob-cap test to the new capacity**

Run: `cargo test -p davinci-state --test transition 2>&1 | grep -E 'FAILED|panicked'`
Expected: `exposed_refreshes_above_a_lower_blob_cap_are_refused` fails at line 415: one vote with 250 exposed refreshes now fits two blobs (8,096 cells).

In `state/tests/transition.rs` (lines 409-415):

```rust
    // 260 exposed 16-field slots fit next to one vote in six blobs, not two.
    let env = env(16, 301);
    let mut st = committed_state(&env, 300);
    let vote = vec![fake_vote(&env, 300, &[1], 910_000)];
    let must = slots_of(0..260);
    assert!(blob_count(1, 261, 16) <= TX_BLOB_CAP && blob_count(1, 261, 16) > 2);
```

and the comment of `select_batch_shrinks_for_exposed_refreshes` (lines 577-578):

```rust
    // 900 occupied 16-field slots, 400 new voters waiting. One update is 32
    // cells and a quarter of an integer cell, so six blobs carry about 760.
```

Run: `cargo test -p davinci-state 2>&1 | grep -E '^test result'`
Expected: every suite `ok` (`transition` 20 passed, `sync` 7, `validate` 8; `dryrun` reports 7 passed because its tests return early without `ZISKEMU=1`).

- [ ] **Step 7: Drop the accumulator from the node's fixtures**

Delete the `accumulator: …,` line from each `TransitionData` literal:
- `sequencer/tests/stack.rs:26`, `sequencer/tests/l1da.rs:28`, `sequencer/src/web3/blobs.rs:507` and `:515`, `sequencer/src/web3/l1.rs:604`, `sequencer/src/web3/tx.rs:756` (`accumulator: Ballot::identity(),`);
- `sequencer/tests/web3_anvil.rs:343` (`accumulator: some_ballot(&pk, 3),`), `:420` (`accumulator: some_ballot(&pk, 4),`), `:1112` (`accumulator: some_ballot(&pk, 20 + tag as u64),`), `:2374` (`accumulator: some_ballot(pk, seed + 2),`).

Remove the now-unused `use davinci_zkvm_sdk::ballot::Ballot;` from `sequencer/tests/l1da.rs:17` and `sequencer/src/web3/l1.rs:462` (their fixtures have no updates). The `l1da.rs`/`l1.rs` fixtures (one vote id, no update) build fine; they are never decoded.

`web3_anvil.rs`'s second transition must stay two blobs: 460 votes at nf 4 are now 3,911 cells (one blob). At line 414:

```rust
    // Second transition: two blobs, occupied_before = the first batch's voters.
    let n = 500u64;
```

500 votes at nf 4 are 4,251 cells; the process allows 1,000 voters, so `voters_count` reaches 502. The 500-vote fixtures in `stack.rs`, `web3/blobs.rs` and `web3/tx.rs` stay two blobs (4,251 cells) and need no other edit.

- [ ] **Step 8: Update the follower's description**

`docs/lifecycle.md:95-97`:

```markdown
For a transition another node sent, the node fetches the blobs, matches them against the
transaction's versioned hashes, decodes the vote ids and slot updates, recomputes the Results
accumulator from the ballots it already holds (the blobs do not carry it), applies them on its
committed root, and requires the result to equal the event's `newRoot`. Queued votes whose
```

- [ ] **Step 9: Run the workspace checks and the anvil tests**

```bash
cd /home/p4u/davinci-sequencer
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace 2>&1 | grep -c -E 'FAILED|panicked'     # expect 0
(cd ../davinci-contracts && git checkout feat/l1-blob-da && forge build)
ANVIL=1 cargo test -p davinci-sequencer --test web3_anvil 2>&1 | grep -E '^test result'
```

Expected: fmt and clippy clean (`Cargo.lock` does not change: the davinci-zkvm crate versions did not move), `0`, and `test result: ok. 13 passed` for `web3_anvil` in about a minute, including `registry_on_anvil` (the 500-vote transition settles with two blobs) and `registry_must_be_the_pinned_release` (the node refuses a registry that does not pin the new `BATCH_PROGRAM_VK`). The contracts checkout only has to be built; its vectors do not matter here.

- [ ] **Step 10: Commit**

```bash
cd /home/p4u/davinci-sequencer
git add state/src/sync.rs state/src/transition.rs state/tests/sync.rs state/tests/transition.rs \
  sequencer/tests/stack.rs sequencer/tests/l1da.rs sequencer/tests/web3_anvil.rs \
  sequencer/src/web3/blobs.rs sequencer/src/web3/l1.rs sequencer/src/web3/tx.rs docs/lifecycle.md
git commit -m "feat(state): recompute the results leaf when following a compact stream" -m "The DA stream no longer carries the Results accumulator. A follower now
computes it as the committed accumulator plus, per published update, the
new ballot minus its stored one (the identity for an empty slot), and the
existing root check binds it. A wrong stored ballot fails at the root check,
an occupied slot without one is an error. Fixtures follow the SDK's
TransitionData; the anvil two-blob transition grows to 500 votes."
```

---

### Task 5: Dry runs at the compact stream's blob boundary, and the e2e capacity numbers

**Files:**
- Modify: `state/tests/dryrun.rs:347-365` (`multi_blob_batch_accepted` becomes `blob_boundary_batches_accepted`), `:367-400` (`two_blob_cap_batch_accepted`)
- Modify: `e2e/src/demo.rs:3176-3178`, `e2e/src/demo/wave2.rs:157-161`

**Interfaces:**
- Consumes: Task 2's ELF at `../davinci-zkvm/circuit/elf/circuit.elf`; Task 4's state crate.

- [ ] **Step 1: Confirm the current dry runs disagree with the new ELF only through the stream**

Run: `cd /home/p4u/davinci-sequencer && ZISKEMU=1 cargo test -p davinci-state --test dryrun -- --test-threads=1 2>&1 | grep -E '^test |^test result'`
Expected: `multi_blob_batch_accepted` FAILS (`n_blobs 1`: 125 votes at 16 fields are now 4,064 cells) and `two_blob_cap_batch_accepted` FAILS (`no shrink`: two blobs now hold 126 of these votes); the five others pass, which shows guest and SDK agree byte for byte (`assert_accepted` compares every register, `blobs_digest` included). Against `main`'s ELF (`CIRCUIT_ELF_PATH=<v0.1.1 circuit.elf>`) even `first_batch_accepted_by_the_guest` fails on `blobs_digest`.

- [ ] **Step 2: Rewrite the multi-blob dry runs**

In `state/tests/dryrun.rs`, replace `multi_blob_batch_accepted` (lines 347-365) with:

```rust
/// One commitment fewer (when there is more than one) or more than the
/// transition's blobs: the guest sets `FAIL_KZG` (bit 18) and nothing else.
fn assert_blob_count_bound(b: &davinci_state::PreparedBatch) {
    let n = b.request.kzg.as_ref().unwrap().commitments.len();
    for want in [n - 1, n + 1].into_iter().filter(|w| *w > 0) {
        let mut bad = b.request.clone();
        let c = &mut bad.kzg.as_mut().unwrap().commitments;
        let last = c[n - 1].clone();
        c.resize(want, last);
        let got = run_guest(&bad);
        assert!(
            !got.ok && got.fail_mask == 1 << 18,
            "{want} commitments: mask {:#x}",
            got.fail_mask
        );
    }
}

#[test]
fn blob_boundary_batches_accepted() {
    if !gated() {
        return;
    }
    // 16 fields, values within the mode's max_value (5) and sum cap.
    let env = env(16, 127);
    let fields: Vec<u64> = (0..16).map(|i| i % 6).collect();

    // 126 new votes: T = 1 + 63 + 4032 = 4096, one full blob.
    let mut st = new_state(&env);
    let votes: Vec<_> = (0..126)
        .map(|i| real_vote(&env, i, &fields, 60 + i as u64))
        .collect();
    let b = st
        .prepare(&votes, &mut StdRng::seed_from_u64(24), &none())
        .unwrap();
    assert_eq!(b.expected.n_blobs, 1);
    assert_accepted(&b);
    assert_blob_count_bound(&b);

    // One vote and 126 refreshes: T = 1 + 32 + 4064 = 4097, one cell in a
    // second blob. The committed votes never reach the guest (fake proofs).
    let mut st = new_state(&env);
    let first: Vec<_> = (1..127)
        .map(|i| fake_vote(&env, i, &fields, 700 + i as u64))
        .collect();
    let b = st
        .prepare(&first, &mut StdRng::seed_from_u64(28), &none())
        .unwrap();
    st.commit(&b).unwrap();
    let must: BTreeSet<u64> = (1..127)
        .map(|i| slot_key_address(&voter_address(i)))
        .collect();
    let vote = [real_vote(&env, 0, &fields, 60)];
    let b = st
        .prepare(&vote, &mut StdRng::seed_from_u64(29), &must)
        .unwrap();
    assert_eq!(b.refresh_slots().len(), 126);
    assert_eq!(b.expected.n_blobs, 2);
    assert_accepted(&b);
    assert_blob_count_bound(&b);
}
```

A single-blob request cannot be cut to zero commitments here (`ig::write_kzg_block` refuses an empty list); Task 2's `TestCheatKZGBlobCount` covers zero on the raw block.

In `two_blob_cap_batch_accepted`, the comment and the sizes (lines 372-387) become:

```rust
    // Gnosis takes two blobs per block. 200 committed voters (fake proofs)
    // make each new vote bring a refresh, so two blobs hold 126 of these
    // 127 votes (cached proofs, shared with `blob_boundary_batches_accepted`).
    let env = env(16, 327);
    let mut st = new_state(&env);
    let mut rng = StdRng::seed_from_u64(27);
    let fields: Vec<u64> = (0..16).map(|i| i % 6).collect();
    let first: Vec<_> = (127..327)
        .map(|i| fake_vote(&env, i, &fields, 400 + i as u64))
        .collect();
    let b = st.prepare(&first, &mut rng, &none()).unwrap();
    st.commit(&b).unwrap();

    st.set_blob_cap(2).unwrap();
    let pending: Vec<_> = (0..127)
        .map(|i| real_vote(&env, i, &fields, 60 + i as u64))
        .collect();
```

and the shrink assertion (lines 396-400) becomes `assert_eq!(sel.len(), 126);` (8,160 cells for 126 votes and 126 refreshes; 127 would need 8,225).

- [ ] **Step 3: Run the dry runs**

Run: `cd /home/p4u/davinci-sequencer && CIRCOM_ARTIFACTS=/home/p4u/davinci-circom/artifacts ZISKEMU=1 cargo test -p davinci-state --test dryrun -- --test-threads=1 2>&1 | grep -E '^test |^test result'`
Expected: 7 `ok` (`blob_boundary_batches_accepted`, `csp_batch_accepted`, `exposed_refresh_set_accepted`, `first_batch_accepted_by_the_guest`, `guest_rejects_tampered_requests`, `overwrites_and_refreshes_accepted`, `two_blob_cap_batch_accepted`), about 30 s after the two new ballot proofs (voters 125 and 126) are proved into `state/tests/cache`. A run without the `skipped: set ZISKEMU=1` line is a real run.

- [ ] **Step 4: Pin the e2e two-blob capacity**

In `e2e/src/demo/wave2.rs` (lines 157-161):

```rust
            // Every first ballot to node 1, in chunks of 130. At 16 fields a
            // slot update takes 32 of a blob's 4096 cells plus a quarter of an
            // integer cell, so two blobs hold 126 new votes with their silent
            // refreshes: from the second chunk on the node splits what is
            // pending over several transitions.
```

In `e2e/src/demo.rs`, `the_large_election_spans_blobs` (lines 3176-3178):

```rust
        assert_eq!(blob_count(s.chunk, s.chunk, nf), 2);
        let steady = max_votes_for_cap(nf, 0, s.chunk, 2);
        assert_eq!(steady, 126);
        assert!(steady < s.chunk, "{steady}");
```

Run: `cd /home/p4u/davinci-sequencer && cargo test -p davinci-e2e --lib the_large_election_spans_blobs 2>&1 | grep -E '^test '`
Expected: `test demo::tests::the_large_election_spans_blobs ... ok` (the 130-vote chunk is 4,226 cells, two blobs). This is a pure unit test in the library: `--lib` with the filter starts no anvil or node (the end-to-end suite stays `make e2e` only).

- [ ] **Step 5: Commit**

```bash
cd /home/p4u/davinci-sequencer
cargo fmt --all -- --check && cargo clippy --workspace --all-targets --locked -- -D warnings
git add state/tests/dryrun.rs e2e/src/demo.rs e2e/src/demo/wave2.rs
git commit -m "test(state): dry runs at the compact stream's blob boundary" -m "The emulator now checks transitions of exactly 4096 and 4097 cells at 16
fields and the FAIL_KZG count check around each. Two blobs hold 126 new
votes with their refreshes (was 121); the two-blob cap dry run and the e2e
demo pin that."
```

---

### Task 6: 512-vote batches by default

**Files:**
- Modify: `sequencer/src/config.rs:342-344`, `:811`
- Modify: `.env.example:78-80`, `docs/configuration.md:72`
- Modify: `state/tests/transition.rs` (append one test)

**Interfaces:**
- Produces: `Config::batch_max` defaults to 512; `parse_batch_max` still accepts 1..=1024.

- [ ] **Step 1: Change the default test**

`sequencer/src/config.rs:811`: `assert_eq!(c.batch_max, 512);`

Run: `cd /home/p4u/davinci-sequencer && cargo test -p davinci-sequencer --lib config::tests::defaults_and_overrides 2>&1 | grep -E 'left|right|test result'`
Expected: FAIL, `left: 1024`, `right: 512`.

- [ ] **Step 2: Change the default**

`sequencer/src/config.rs:342-344`:

```rust
    /// Hard cap on a batch; pending votes past it seal one without waiting.
    /// The guest takes up to 1024 votes; the default is 512.
    #[arg(long, env = "DAVINCI_BATCH_MAX", default_value = "512", value_parser = parse_batch_max)]
    pub batch_max: usize,
```

`.env.example:78-80`:

```bash
# Pending votes that seal a batch without waiting, and the largest batch
# (1 to 1024).
# DAVINCI_BATCH_MAX=512
```

`docs/configuration.md:72`:

```markdown
| `--batch-max` | `DAVINCI_BATCH_MAX` | `512` | Pending votes that seal a batch at once, and the largest batch (1 to 1024). |
```

Run: `cargo test -p davinci-sequencer --lib config::tests 2>&1 | grep '^test result'`
Expected: `ok. 14 passed`.

- [ ] **Step 3: Pin what a 512-vote queue seals at 16 fields on one transaction**

Append to `state/tests/transition.rs`:

```rust
#[test]
fn a_512_vote_queue_at_sixteen_fields_seals_what_six_blobs_hold() {
    // The node's default --batch-max is 512. At 16 fields a steady batch
    // (one refresh per new vote) fits six blobs only up to 379 votes, so the
    // selection stops there and reports the transaction full.
    let env = env(16, 1024);
    let mut st = committed_state(&env, 512);
    let pending: Vec<VerifiedVote> = (512..1024)
        .map(|i| fake_vote(&env, i, &[1], 50_000 + i as u64))
        .collect();
    let (sel, blocked) = st.select_batch(&pending, u64::MAX, &none(), 512).unwrap();
    assert_eq!((sel.len(), blocked), (379, true));
    let sel: Vec<VerifiedVote> = sel.into_iter().cloned().collect();
    let b = st
        .prepare(&sel, &mut StdRng::seed_from_u64(17), &none())
        .unwrap();
    assert_eq!(b.blobs.blobs.len(), TX_BLOB_CAP);
}
```

Run: `cargo test -p davinci-state --test transition a_512_vote 2>&1 | grep -E '^test '`
Expected: `ok` in about 9 s (379 votes and 379 refreshes are 24,542 cells, six blobs; the actor passes `batch_max` as `select_batch`'s `max_votes`). It passes on Task 4's code: it guards the default rather than driving new code.

- [ ] **Step 4: Commit**

```bash
cd /home/p4u/davinci-sequencer
cargo fmt --all -- --check && cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace 2>&1 | grep -c -E 'FAILED|panicked'     # expect 0
git add sequencer/src/config.rs .env.example docs/configuration.md state/tests/transition.rs
git commit -m "feat(config): 512-vote batches by default" -m "--batch-max now defaults to 512; the guest still takes 1024. On a chain that
holds one transaction's six blobs the node still seals only what fits (379
steady votes at 16 fields) and keeps the rest queued."
```

---

### Task 7: davinci-contracts vectors for the compact stream

**Files:**
- Modify: `test/vectors/main.go:599-601`, `:628`, `:643`
- Regenerate: `test/vectors/transition.json`
- Modify: `test/Transition.t.sol:50`, `:58-59`, `:148`, `:381`; `test/Grace.t.sol:319`

**Interfaces:**
- Consumes: Task 1's `BuildTransitionBlobs(nf, pid, root, voteIDs, updates)` through `replace github.com/vocdoni/davinci-zkvm/go-sdk => ../../../davinci-zkvm/go-sdk`.

- [ ] **Step 1: Branch, and see the generator break**

```bash
cd /home/p4u/davinci-contracts
git checkout feat/l1-blob-da && git checkout -b feat/compact-da-stream
cd test/vectors && go vet . 2>&1 | head -3
```

Expected: `too many arguments in call to davinci.BuildTransitionBlobs`.

- [ ] **Step 2: Update the generator**

`test/vectors/main.go`, the fixture comment (lines 599-601):

```go
// transitionFixture builds two transitions for the organizer's first process:
// three fresh votes on one blob, then 1000 votes (three overwrites) on two
// blobs. Roots after are placeholders: the forge tests use a mock verifier.
```

the second batch's size (line 628): `for j := uint64(0); j < 1000; j++ {` (820 votes at nf 2 now fit one blob, 3,691 cells; 1000 take two, 4,501), and the build call (line 643):

```go
		tb, err := davinci.BuildTransitionBlobs(nf, pidBE, rootBE, b.voteIDs, updates)
```

- [ ] **Step 3: Regenerate**

```bash
cd /home/p4u/davinci-contracts/test/vectors && go vet . && go run .
cd ../.. && git status --short test/vectors
python3 -I -c "import json; f=json.load(open('test/vectors/transition.json')); print([(t['voters'], t['n_blobs']) for t in f['transitions']], [d['transition']['n_blobs'] for d in f['dynamic']])"
sha256sum test/vectors/transition.json
```

Expected: only `M test/vectors/transition.json`; `[(3, 1), (1000, 2)] [1, 1]`; checksum `a678fb3c1d02c8bf8794aa395ff4a32949253e37fd78cb71e2e53d0a0d972891`, stable across a second `go run .` (CI's drift check).

- [ ] **Step 4: Run forge to see the vote-count literals fail**

Run: `cd /home/p4u/davinci-contracts && forge test 2>&1 | grep -E 'FAIL|tests passed'`
Expected: `360 tests passed, 2 failed`: `test_SubmitStateTransition_TwoBatchesTwoBlobs` (`expected=820, got=1000`) and `test_SetProcessMaxVoters_RevertWhen_PastTheEnd` (`1000 != 820`).

- [ ] **Step 5: Update the literals**

`test/Transition.t.sol`:
- line 50: `emit IProcessRegistry.ProcessStateTransitioned(pid, address(this), t.rootBefore, t.rootAfter, 1000, 3, 2);`
- lines 58-59:

```solidity
        // 3 fresh votes, then 1000 votes of which 3 overwrite the first batch.
        assertEq(p.votersCount, 1000);
```

- line 148 (`votes + overwrites`, the wrong value the test feeds): `_setWord(t.publicValues, 42, 1003);`
- line 381 (one below the second batch's voters): `bytes31 pid = _fixtureProcess(999);`

`test/Grace.t.sol:319`: `assertEq(registry.getProcess(pid).votersCount, 1000);`

Run: `forge test 2>&1 | tail -1`
Expected: `Ran 26 test suites …: 362 tests passed, 0 failed, 0 skipped (362 total tests)`.

- [ ] **Step 6: Commit**

```bash
cd /home/p4u/davinci-contracts
git add test/vectors/main.go test/vectors/transition.json test/Transition.t.sol test/Grace.t.sol
git commit -m "test(vectors): compact da stream fixtures" -m "BuildTransitionBlobs no longer takes the accumulator. The fixture's second
transition grows from 820 to 1000 votes so that it still spans two blobs
under the compact stream; the forge tests that count its votes follow. No
contract source changes."
```

---

## Merge and release (after the Fable audit)

Nothing here runs before the final Fable 5.1 audit of all three branches has passed. The davinci-sequencer and davinci-contracts `feat/compact-da-stream` branches are **not** merged here: they merge with the core sub-project (the OP redeploy, which also changes the registry's immutable `batchProgramVK` and `resultsProgramVK`), after it builds on them. The other pins of these vks (davinci-sdk `src/protocol/release.ts`, davinci-fold, the davinci-contracts deploy defaults in `docker-compose.yml`, `README.md` and `docs/`) move with the core and SDK sub-projects; this release moves the aggregator and results vks as well as the batch vk.

**davinci-zkvm into `main`, one commit**, the way earlier wire changes landed (CONTRIBUTING: change every copy of the DA layout in one commit):

```bash
cd /home/p4u/davinci-zkvm
git checkout main
git merge --squash feat/compact-da-stream
git status --short          # the three commits' files; the three untracked files stay untracked
git commit -m "feat(circuit): compact da stream" -m "Vote ids and slot keys are packed four per cell behind a header with both
counts, and the Results accumulator leaves the stream (followers recompute
it): T = 1 + ceil((V + U) / 4) + 2nf*U. The guest builds the KZG omega table
once per batch. Go and Rust builders, the Rust decoder, the vectors and the
spec follow. All three guest ELFs move; batch, aggregator and results vks
refrozen. Guest cost within 0.25 % of v0.1.1 (BENCHMARK.md)."
```

**Version bump to v0.2.0**, as v0.1.1 was cut (`18f1639 chore(service): v0.1.1` changed only `service/Cargo.toml` and its `Cargo.lock` entry; the workspace and SDK version stays, so the sequencer's lockfile does not move):

```bash
sed -i 's/^version = "0.1.1"$/version = "0.2.0"/' service/Cargo.toml
cargo check -p davinci-zkvm-service           # rewrites the davinci-zkvm-service entry in Cargo.lock
git diff --stat                               # service/Cargo.toml and Cargo.lock, one line each
git add service/Cargo.toml Cargo.lock
git commit -m "chore(service): v0.2.0"
```

**Gated on the owner's explicit confirmation**: pushing `main` publishes `vocdoni/davinci-zkvm:main`, and the tag publishes `vocdoni/davinci-zkvm:v0.2.0` and `:latest`, which the watchtower on the prover host follows. From then on that prover proves only under the new batch vk, and the production Gnosis node on the same host, which pins `0x6cfc89d5…7a10`, will refuse its proofs (Gnosis is being retired). Before the push, the owner may want `ZKVM_REF` pinned to `v0.1.1` in davinci-sequencer's `.github/workflows/main.yml` and `docker-build.yml` and the davinci-zkvm `ref` in davinci-contracts' `.github/workflows/test.yml` on their `main` branches: both check out davinci-zkvm `main`, whose SDK their `main` no longer compiles against until the core sub-project merges.

```bash
# only after the owner confirms
git tag -a v0.2.0 -m "davinci-zkvm v0.2.0"   # annotated, like v0.1.0
git push origin main
git push origin v0.2.0
```

Then delete the merged branch locally (`git branch -D feat/compact-da-stream`; `-D` because a squash merge leaves it unmerged in git's eyes).
