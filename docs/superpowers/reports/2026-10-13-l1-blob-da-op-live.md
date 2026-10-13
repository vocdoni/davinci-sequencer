# L1-blob DA on OP Mainnet: deployment, live run and benchmarks

Date: 2026-10-12 23:45 UTC to 2026-10-13 03:49 UTC. Branches `feat/l1-blob-da`: davinci-sequencer
(this report's commit), davinci-contracts `c72accc`. Spec:
`docs/superpowers/specs/2026-10-12-l1-blob-da-op-design.md`; plans 1 to 3 in
`docs/superpowers/plans/`. Earlier measurements: `2026-10-12-l1-blob-op-poc.md` (beacon-root PoC)
and `2026-10-12-l1blob-verifier-op-check.md` (first verifier on OP).

## Verdict

DAVINCI runs on OP Mainnet in L1-blob mode end to end: transitions settle on OP, their blobs live
in Ethereum blob transactions, and the registry checks them with a non-interactive proof against
OP's `L1Block` (RLP headers, a transactions-trie proof, the `0x0A` opening) with no zkVM change,
no notary and no bridge message. A live run with the real prover settled two elections (Merkle and
CSP census) in 4 transitions, an observer rebuilt both from Ethereum beacon blobs alone, the
results matched the tallies, and every negative replay was refused on mainnet. A batch settled
85-115 s after its seal; a blob post cost 1.9-3.7 µETH on Ethereum (about 0.005-0.01 USD at
0.06-0.12 gwei and a blob base fee under 0.01 gwei) and the OP settlement about 0.04-0.05 µETH. The
main limit is structural: the deployed registry's 150 s grace floor leaves little room for the
Ethereum leg, so at 16 fields batches above about 128 votes are split while an election is open.

## Deployment

| Contract | Address | Transaction | Gas | Cost |
|---|---|---|---:|---:|
| `L1BlobVerifier` (on `L1Block` `0x4200…0015`) | `0x3B95BC14F43ed8Addb83F84D10A949De74eE7529` | `0xb1cec981…c12f` (block 158125135) | 1,683,118 | 2.9e9 wei L2 + 7.06e10 wei L1 data |
| `ProcessRegistry` (L1-blob mode) | `0x8b63fAE96948D69CF92Eb40e4EE0fCE40962267d` | `0x133d7cfd…a3c6` (block 158125136) | 5,393,663 | 9.2e9 wei L2 + 2.78e11 wei L1 data |
| `ZiskVerifier` (reused) | `0x6781FFF50D615CB3F331CCD3Bdad3416d4ce0d5c` | — | — | — |

From davinci-contracts `e7de916` with `deploy_all.sh` (`forge script` without `--broadcast`
first), the release pins (equal to the Gnosis registry's), production grace defaults (180 s, floor
150 s), no DKG or Council adapter. `verify_deployment.py`: every check `OK` (runtime code of all
three contracts equal to the local build, the pins, the chain id, `l1BlobVerifier.l1Block`). The
records `broadcast/DeployAll.s.sol/10/run-latest.json` and `deployments/10.json` contain no key and
no private RPC. Not source-verified on an explorer (no API key). The `optimism` preset now points at
the registry (start block 158,125,136); an observer booted with `--network optimism` detected
L1-blob mode, Ethereum chain 1, the `L1Block` predeploy and a blob cap of 6.

Endpoint probes (read-only): publicnode's beacon API served full blobs
(`/eth/v1/beacon/blobs/{slot}?versioned_hashes=`) for a recent and a day-old slot; lodestar's public
beacon answered 403; publicnode's Ethereum RPC refused a 1.5 MB body (503), dRPC took it; dRPC has
no `eth_blobBaseFee`. OP's origin lagged Ethereum by 6 blocks.

## e2e on anvil (real prover)

- **L1-blob mode** (`DAVINCI_E2E_DA=l1-blobs make e2e`, two anvils, origin relayer 6 blocks
  behind): PASSED in 42.8 min. Three sequencers and an observer, 8 processes with results equal to
  the tallies, lost races handled, grace checks, 49 Ethereum blob posts. Node B killed with its
  blob transaction pending at L1 nonce 4 and restarted: it re-proved and replaced the pending
  transaction at nonce 4 with every fee doubled, and the settlement carried the replacement's
  blobs. L1 negative replays (`eth_call` at N-1 with a passing control): replay `InvalidStateRoot`,
  blob-mode entry point `WrongDAMode`, proof byte `InvalidProof`, header chain
  `BrokenHeaderChain`, proof of another trie `MerkleTrie: invalid root hash`, swapped commitment
  `InvalidBlobsDigest`, extra blob `BlobCountMismatch`.
- **Blob mode** (regression): PASSED in 42.0 min with the same registry address as before and every
  blob-mode negative check passing.

## Live run on OP Mainnet

The first attempt (01:20 UTC) admitted 11 votes and never sealed: while an election is open the
node budgets for the organizer ending it now under the 150 s grace floor, and the harness's 120 s
settle margin plus the proof never fit, silently. No blob was posted. The fix (`109f6ac`, then
`1647616`): in L1-blob mode the Ethereum leg (`--l1-settle-lag`, 110 s) runs during the proof and
must fit on its own, the settle margin covers only the OP leg (30 s default in this mode), a
signing node refuses to boot when the two exceed the registry's floor, and the node logs why a
batch is held.

The second attempt (01:40 UTC, `davinci-sequencer-109f6ac`) PASSED in 13.0 min:

- One signing node (dedicated Ethereum key `0x85F4…2F40`, OP key `0xD8eb…22cA`) and one observer;
  a Merkle and a CSP process; round 1 (11 votes) settled in 250 s, round 2 (3 revotes) in 106 s.
- 4 transitions, all by the node; the observer synced all 4 from Ethereum beacon blobs (no peer
  source configured) and its trees matched the chain roots.
- First live settlement: blob in Ethereum block 26,180,353, settled on OP in
  `0xad3a3a72…9ee0` with a 3-header proof, about 72 s after the post.
- Results on-chain equal to the tallies ([15, 8, 3, 20] and [6, 3, 2, 7]); grace checks and every
  L1 negative replay refused on mainnet as on anvil.
- Restart: the node was killed right after its first post (nonce 0); that transaction was mined
  before the restart's first post, so the node picked up its record and continued at nonces 1-3.
  The replacement path (a still-pending post replaced at its nonce) is proven on anvil only.
- Gas bill: 14 transactions, 8.7 µETH in total; each blob post 2.05-2.27 µETH (base fee about
  0.08 gwei, blob base fee about 0.004 gwei); each OP settlement 785k-862k gas at about 3,400 wei.

## Benchmarks

`docs/benchmarks.md` ("L1-blob batch sizes on OP Mainnet") holds the table and
`2026-10-13-l1-blob-da-op-bench.json` every row: 3 runs of 64, 128 and 256 votes at 2 and 16 fields,
first and steady batches, 1 h 56 min, 104.9 µETH spent on Ethereum (budget 900 µETH).

- Every settlement carried a 3-header proof; `verify` cost 430k-460k gas, the whole settlement
  about 800k-860k on OP.
- Seal to settlement: 85-115 s per batch. Proofs took 22-64 s; OP's origin lag (about 72 s from the
  post) dominates, so `proof -> settled` was 40-70 s.
- Ethereum per batch: 1.9-3.7 µETH (more blobs, more blob gas); OP per batch: 0.04-0.05 µETH.
- At 16 fields with 128 and 256 votes the seal budget split the batch in two (for example 1 + 3 or
  4 + 1 blobs); the second waited for the first to land, so those cases finished 200-230 s after
  the first seal.

## Comparison with the PoC

| | Beacon-root PoC (2026-10-12) | Production, OP Mainnet (this report) |
|---|---|---|
| L1 proof | SSZ branch to the 4788 beacon root | RLP headers to `L1Block.hash()` + trie proof; survives ePBS |
| Proof size on OP | 17 SSZ nodes per blob | 3 headers + one trie proof per transaction |
| OP gas per settlement | 466k (1 blob) - 851k (6 blobs) | 785k-862k (1-4 blobs) |
| Ethereum cost per batch | 2.9-7.7 µETH at 0.10-0.11 gwei | 1.9-3.7 µETH at 0.06-0.12 gwei |
| Post to settlement | 91-105 s (OP lag 6-7 blocks) | about 72 s |
| 256 x 16 in one transaction | yes (5 blobs) | split in two while open (150 s floor) |

## Deviations from the spec

- `L1BlobVerifier` keeps a permissionless record of OP origins (`record()`) and anchors at the
  current or a recorded origin with a one-day `MAX_AGE`; its event reports one L1 block per blob.
  The spec called it stateless; the record holds only values OP itself derived.
- A restart re-proves (batch secrets are never persisted) and replaces the pending post at its
  nonce from a persisted post record, instead of resuming the posted transition.
- The OP settlement gas is an estimate plus 20 %, not a fixed 1.2M (measured 785k-862k).
- The seal budget models the Ethereum leg as overlapping the proof (`--l1-settle-lag`, 110 s) and
  defaults the settle margin to 30 s in L1-blob mode.

## Spend

| Account | Ethereum | OP |
|---|---:|---:|
| Owner (funding, deployment, organizer) | 1,502.4 µETH (of which 1,500 µETH to the node key) | 200.5 µETH (of which 200 µETH to the node key) |
| Node A (blob posts, settlements, `record()`) | 138.3 µETH (55 posts) | 2.1 µETH |

Left on the node keys for a production node: 1,361.7 µETH on Ethereum, 197.9 µETH on OP. Owner
balances after the runs: 2,201.1 µETH on Ethereum, 1,606.3 µETH on OP.

## Operator notes

See `docs/configuration.md` ("L1-blob mode", "On OP Mainnet"): keep `--settle-margin 30s` and
`--l1-settle-lag 110s`; organizers keep the grace at 180 s or above; include an Ethereum endpoint
that takes 1.5 MB bodies (dRPC) or set `--l1-send-rpc-url`; name a beacon API that keeps full
blobs, ideally your own; fund a dedicated Ethereum key used by nothing else.

## Open items

1. **Grace floor (structural).** The OP registry's 150 s floor leaves 10 s of slack over the
   Ethereum leg plus the margin while an election is open and splits large 16-field batches. A
   registry meant for L1-blob mode should have a higher floor (about 240 s); it is immutable, so
   this means a new deployment.
2. The replacement of a still-pending blob post after a restart is proven on anvil, not on mainnet.
3. One public beacon API with full blobs in the preset; followers should add their own.
4. publicnode's Ethereum RPC refuses 1.5 MB bodies; six-blob sends rely on dRPC in the preset.
5. CI's `CONTRACTS_REF` is `main`, which lacks the L1-blob contracts: the new CI step fails until
   the contracts branch is merged or pinned. Nothing was pushed.
6. davinci-contracts' Go bindings (`golang-types/`) and Hardhat artifacts are not regenerated.
7. `/processes/:pid` reports `nextSealNotBefore` without the time budget; a held batch shows only
   in debug logs.
8. The deployment is not source-verified on an explorer.
