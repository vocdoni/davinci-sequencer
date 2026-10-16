# OP core stack: 512-vote batches on OP Mainnet, released and in production

Status: design approved in conversation by the owner on 2026-10-13 ("yes, go ahead"); written
spec audited by Fable 5.1 (APPROVE WITH CHANGES, all 14 findings folded in), pending the owner's review. Second sub-project of the OP-stack rollout
(1 compact DA stream: done, davinci-zkvm v0.2.0; 2 this core; 3 DKG and Council on OP; 4 SDK and
apps; 5 on-chain census on OP). Every sub-project ends with a strict Fable audit, merges to `main`
with releases. Gnosis is retired.

## 1. Goal

The new stack runs in production on OP Mainnet: a registry on the v0.2.0 program vks whose grace
window fits the Ethereum leg, a node that settles 512-vote batches at every field count (16
included) by posting several Ethereum blob transactions per transition, the L1-blob and
compact-stream work merged to `main` and released, and the production node on the prover host
switched to OP. Benchmarks follow later, on the owner's request.

Non-goals: DKG and Council key modes on OP (sub-project 3), the TypeScript SDK and the elections
app (4), the on-chain census on OP (5), any zkVM or contract source change (none is needed: the
contract accepts several transactions per proof by construction, since `vhs` is concatenated in
`txs` order, `L1BlobVerifier.sol:66-73`, tested in this sub-project; followers already fetch blobs
from several L1 blocks).

## 2. Owner decisions (2026-10-13)

- The production node on this host (`davinci-sequencer-1`, image `ghcr.io/vocdoni/davinci-sequencer:latest`,
  today on Gnosis) becomes the production OP node.
- The new OP registry's grace window: floor 240 s, default 300 s, ceiling 600 s, at most 1800 s
  past the end, 60 s shorten notice (immutable at deployment).
- Gnosis is retired: no new Gnosis deployment, and the node drops it.
- Batches of up to 512 votes (the node's `--batch-max` default since the compact-stream work).

## 3. Several blob transactions per transition (node)

Today an L1-blob node posts one type-3 transaction (at most 6 blobs, `TX_BLOB_CAP`) per
transition, so a batch is capped at what 6 blobs hold (379 steady votes at 16 fields). A 512 × 16
steady batch needs 9 blobs; with all revotes, 13.

- **Caps.** Two caps. The per-transaction cap stays what `resolve_blob_cap` gives (the L1's
  `eth_config` blob schedule, `--max-blobs-per-tx`, else 6) and is the chunk size. The per-batch cap
  is the guest's `MAX_BLOBS` (32) in L1-blob mode and the per-transaction cap in blob mode.
  `davinci-state`'s `set_blob_cap` accepts up to `MAX_BLOBS` (its tests in
  `state/tests/transition.rs:373-378` change); `select_batch`, `capacity_estimate` and the
  prepare-time refresh check use the per-batch cap. The node logs both caps; `op_live` keeps
  asserting the per-transaction one. `--batch-max 512` keeps a batch at or below 13 blobs.
- **Chunks.** The transition's blobs are split in order into chunks of at most the per-transaction
  cap (the last may be shorter), each with its commitments and versioned hashes in the same order. Chunk `i` holds blobs
  `6i .. 6i+5`, so the versioned hashes of the proof's transactions, concatenated in proof order,
  are exactly the transition's, which is what the registry opens commitment `j` against.
- **Posting.** At proving start the node posts the chunks one after another through the existing
  window-1 poster: each chunk is one transaction at the account's next nonce, recorded before it
  is sent, waits until mined (replaced with every fee doubled after 3 blocks, under the per-post
  fee cap), then the next chunk is posted. Two or three posts take about 12-24 s each, inside a
  512 × 16 proof (about 100-130 s). Sending the chunks back to back at consecutive nonces is
  rejected (blob-pool replacement rules and head-of-line stalls; Fable's earlier advice). The seal
  guard reserves the leg per chunk: `l1_settle_lag + (chunks − 1) × POST_TIME + settle_margin` must
  fit, with `POST_TIME` ≈ 24 s (two L1 blocks); when it does not, the batch is sized down to fewer
  chunks (the cap loop in `maybe_seal`), not held.
- **Settling.** The settle loop waits until every chunk's receipt has `REORG_MARGIN` blocks on
  top. `choose_anchor` takes both blocks: depth (`CURRENT_MAX_HEADERS`, `MAX_L1_PROOF_HEADERS`,
  `L1_MAX_AGE`) is measured from the oldest chunk block; the current-origin wait, the
  `REORG_MARGIN` wait and a recorded origin's eligibility (`r.number >= n_newest`) use the newest
  chunk block (the verifier rejects any transaction above its anchor, `TxAboveOrigin`). `record()`
  is attempted once, after the last chunk is mined, when OP's origin reaches that block and the
  proof is still running; any re-post (one chunk or all) clears the recorded origin, as today
  (`settle.rs:174`). `build_proof` takes every chunk transaction in chunk order (a re-posted chunk 0
  landing after chunk 1 is fine: `header_index` is per transaction); `verify_local` must return
  exactly the transition's versioned hashes in order.
- **Failures.** A chunk whose transaction is gone (after `GONE_FOR`) is posted again alone, at the
  next nonce. A proof too deep (past the header budget or `MAX_AGE`) posts every chunk again.
  `MAX_REPOSTS` counts re-posts per transition: a single-chunk re-post counts one and a full re-post
  counts one. Worst case per transition: `chunks + MAX_REPOSTS × chunks` posts, each under
  `--l1-max-post-fee` (13 blobs in 3 chunks: at most 12 posts, 0.012 ETH at the default cap). The
  post record is per transaction, as today, so a restart replaces whichever chunk transaction is
  pending.
- **Event and followers.** `ProcessStateTransitionedL1` carries one L1 block per versioned hash;
  `blobs_for_l1` already groups by block. Unchanged.
- **Gas on OP.** One settlement proves k transactions: one shared header chain plus k trie proofs;
  `verify` gas grows by about one trie proof per extra transaction. The settlement gas is the
  estimate + 20 %. Worst case: 64 headers + 3 trie proofs ≈ 6-7 M gas and ≈ 50 KB calldata, under
  OP's 128 KB transaction limit; `verify_local` and the pre-flight `eth_call` run before every send.

## 4. The new OP registry

Deployed with `DeployAll.s.sol` from davinci-contracts at the release commit, owner's key, no
`.env` (variables in a subshell, as on 2026-10-12):

- `ZISK_VERIFIER` = `0x6781FFF50D615CB3F331CCD3Bdad3416d4ce0d5c` (reused; its root-c and code hash
  are unchanged in v0.2.0).
- `L1_BLOB_VERIFIER` = `0x3B95BC14F43ed8Addb83F84D10A949De74eE7529` (reused: stateless apart from
  the permissionless origin record, and its checks do not depend on the vks).
- Pins: batch `0xa0ad5c4c7e39d4a7007933158181418a752e6c1563a211af84c579640db435fa`, results
  `0xe693589453ecaee63eba697c48d4c5321a714bb18c2d84a97437b3f368578f68`, `ROOT_C_VADCOP_FINAL` and
  the ballot vk hash unchanged.
- Grace: `GRACE_FLOOR=240 GRACE_DEFAULT=300 GRACE_CEIL=600 GRACE_MAX_TOTAL=1800 NOTICE_MIN=60`.
- `forge script` without `--broadcast` first, then `deploy_all.sh` (after confirming no `.env` is
  in the checkout: `deploy_all.sh` sources one), then `verify_deployment.py` (all checks `OK`,
  including `l1BlobVerifier.l1Block`), then a secret scan of the deployment records, as on
  2026-10-12. `PINS_FROM_REGISTRY` is not used (the new registry has other vks and grace), so
  `verify_deployment.py` gains a `--grace` check (this sub-project) and the deployed
  `defaultGrace/graceFloor/graceCeil/graceMaxTotal/noticeMin` must read `300/240/600/1800/60` and
  `batchProgramVK`/`resultsProgramVK` the v0.2.0 pins (`DeployAll.s.sol` silently defaults the
  floor to 150 if a variable is unset or misspelled).
- The `optimism` preset's registry and start block, `docs/deployments.md` and the contracts'
  `README.md`/`docs/deployment.md`/`deployments/10.json` follow; the 2026-10-12 registry
  `0x8b63fAE96948D69CF92Eb40e4EE0fCE40962267d` is recorded as retired (v0.1 vks).
- With the node's L1 defaults (`--l1-settle-lag 110s`, `--settle-margin 30s`) the boot check
  passes (110 + 30 ≤ 240) and a batch's proof may use up to 240 − 30 − `--prove-base` (30) =
  180 s of estimate while voting is open.

## 5. Gnosis retired in the node and docs

- The node's default `--network` becomes `optimism`; the `gnosis` preset is removed from
  `client/src/networks.rs` (a v0.2.0 node cannot settle on Gnosis R2: its vks are v0.1).
- Docs (`README.md`, `docs/configuration.md`, `docs/deployments.md`, `docs/lifecycle.md`) describe
  OP as the network and keep the Gnosis deployments only as a retired history entry.
- Tests that used the `gnosis` preset move to `optimism` or `custom`. Files: `docker-compose.yml`
  (`DAVINCI_NETWORK:-gnosis`), `e2e/src/net.rs` (`DAVINCI_E2E_NETWORK` default, `GNOSIS_RPC`,
  `gnosis_node_rpcs`), `e2e/tests/demo.rs` (gnosisscan link), `docs/testing.md`, `docs/demo.md`,
  `sequencer/src/config.rs` (`gnosis_is_the_default_network` and the `--network GNOSIS` parse
  test), `client/src/networks.rs` (`DEFAULT`, `lookup`, `gnosis_preset_is_deployed`).
  `davinci-client`'s `DEFAULT` (Organizer/Voter tooling) moves to `OPTIMISM`; the live e2e
  defaults move to `optimism` with the L1 key variables. Chain data stays (`web3/contracts.rs`
  known-chain tables, `known_chain_blob_cap(100)`, the RPC behaviour notes in `tx.rs`).

## 6. Leftovers from the compact-stream release

- davinci-zkvm: re-record `rust-sdk/testdata/snark_batch.json` from a v0.2.0 batch proof (the
  first one the e2e run produces) and restore the `BATCH_PROGRAM_VK` assertion in
  `rust-sdk/tests/wire.rs`; a test-only commit on `main` (no release).
- davinci-contracts: deploy defaults and docs (`docker-compose.yml`, `README.md`,
  `docs/deployment.md`, `docs/verification.md`) on the v0.2.0 vks; the Go bindings
  (`golang-types/`, hardhat artifacts) regenerated so they carry `submitStateTransitionL1`,
  `l1BlobVerifier()` and `ProcessStateTransitionedL1`.
- CI pins: davinci-sequencer and davinci-contracts check out davinci-zkvm `v0.2.0` (already on the
  feature branches).

## 7. Validation (all on the new stack, before the release)

- Unit and fake-chain tests for the chunking, the multi-transaction settle loop (origin vs newest
  chunk, depth vs oldest, a record at `n_oldest + 1 < n_newest` must not anchor, one chunk gone,
  too deep, restart with one chunk pending), the two caps and the per-chunk seal guard.
- Contract tests (test-only, no source change): `L1BlobVerifier.verify` with two transactions in
  two different blocks, including the re-posted order (transaction 0 in the newer block), and a
  `TransitionL1` settlement whose two transactions carry the openings in chunk order.
- A `ZISKEMU=1` dry run of a transition above the old per-transaction cap (at least 7 blobs; a
  512 × 16 steady batch with cached proofs and fake committed votes), accepted by the guest with
  the expected `n_blobs` and commitments.
- `ANVIL=1` two-anvil test: a real settlement whose blobs span two L1 transactions.
- GPU e2e (`DAVINCI_E2E_DA=l1-blobs make e2e`) passing, plus one 512-vote, 16-field transition on
  two anvils whose registry is deployed with the production grace (`300/240/600/1800/60`) and the
  node's L1 defaults, so the batch fits the seal budget (sealed after the end, flush, landing
  within the grace), posted as several blob transactions, with the real prover; the shared
  prover's queue is checked empty first.
- Live run on OP Mainnet against the new registry (`op_live`), within the existing budget caps,
  including one 512 × 16 transition if the funded keys allow it (about 2 transactions, 9 blobs).
- Strict Fable audits: spec, plan pre-flight, every task, final whole-release.

## 8. Merge and release

- Pull requests on GitHub for davinci-sequencer and davinci-contracts, each from the branch that
  holds the L1-blob work plus the compact-stream work plus this sub-project, onto `main`; merged
  once CI is green and the final Fable audit passes, by rebase (both `main` branches are linear;
  the contracts `main` requires a pull request). Order: (1) the davinci-contracts PR is merged and
  tagged `v0.0.51` first (versions and notes in `CHANGELOG.md` and `package.json`); (2)
  davinci-sequencer sets `CONTRACTS_REF` to `v0.0.51` (a tag, not `main`), CI green, merged, tagged
  `v0.6.0`.
- Before the `v0.6.0` tag, this host's `davinci-sequencer-1` is taken off `:latest` (it follows it
  through watchtower): stopped, or its watchtower label removed / image pinned to `v0.5.0`. The OP
  node is started as a new container (§9) after the tag; it never inherits the Gnosis container's
  environment (a v0.6.0 image on that environment would default to `optimism` with no L1 key and
  crash-loop).
- davinci-sequencer `v0.6.0` (its image moves `ghcr.io/vocdoni/davinci-sequencer:latest`),
  davinci-contracts `v0.0.51`, each with GitHub release notes in the repository's style; the
  versions are set where the previous releases set them (the plan finds the files).
- Tags and pushes happen after the owner confirms, as for v0.2.0.

## 9. The production node switches to OP

- The host's prover (`davinci-zkvm:latest`) has run v0.2.0 since 2026-10-13 12:45 UTC
  (watchtower), so the Gnosis node (v0.1 pins) cannot settle new batches. The switch is done
  promptly; until then the Gnosis node is stopped (or kept only as a read-only beta endpoint, the
  owner decides) rather than left to fail proofs. The public beta (`sequencer2.davinci.vote`, the
  SDK and apps on Gnosis R2 until sub-project 4) has an outage window the owner accepts.
- Find how `davinci-sequencer-1` is configured (compose file or run script, environment, datadir,
  prover URL, key files), without printing secrets.
- Prepare the change: `DAVINCI_NETWORK=optimism` (or the default), the new registry from the
  preset, `DAVINCI_L1_PRIVKEY_FILE` (the funded `seq-a.l1` key) and the OP settlement key
  (`seq-a.op`), Ethereum endpoints from the preset (dRPC takes the 1.5 MB sends), a fresh datadir
  directory for the new deployment (the node keeps one database per deployment), the image pinned
  to `v0.6.0` or `:latest`.
- Apply it only after the owner confirms; then check the node boots in L1-blob mode on the new
  registry (log lines), serves `/info`, and settles a small live election.

## 10. Risks

| risk | mitigation |
|---|---|
| A chunk mined, the next one stuck under a fee spike | window-1 posting with the fee cap; the transition waits within the grace window; `MAX_REPOSTS` bounds re-posts |
| Origin far behind the newest chunk while the oldest ages | depth measured from the oldest chunk; `record()` once the origin reaches the newest; re-post all past the budget |
| The 240 s floor still tight for slow provers | the seal budget sizes batches to fit; benchmarks on request will show it |
| `:latest` of the sequencer image moves for hosts that follow it (this one does, via watchtower) | before the tag, `davinci-sequencer-1` is taken off `:latest` (stopped, label removed or pinned to `v0.5.0`); the OP node starts as a new container after the tag; release notes say v0.6.0 is OP-only |
| Several elections post at once through the one-post window | posts queue and the leg grows by ~24 s per queued chunk; the deadline requeues a batch that misses, `MAX_REPOSTS` bounds cost; accepted for this release, measured in the benchmarks |
| Production node switch loses data | a fresh datadir directory per deployment; the old Gnosis database is left in place |
