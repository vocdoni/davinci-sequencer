# Ethereum blob DA with OP Stack settlement

Status: approved for implementation by the owner on 2026-10-12, reviewed by Fable 5.1 (round 8) ("post at proving start; keep the
work until it is fully integrated, deployed and working on Optimism, with e2e tests and
benchmarks"). Basis: the mainnet PoC (`docs/superpowers/reports/2026-10-12-l1-blob-op-poc.md`).
Repositories: davinci-contracts, davinci-sequencer (node, client, e2e). No zkVM change.

## 1. Goal

Run DAVINCI on OP Mainnet (and, by configuration, other OP Stack chains) at L2 prices while the
transition data stays in Ethereum blobs: no notary, no bridge message, no committee, no change to
the zkVM program, and a fully non-interactive on-chain check. Same Solidity source and the same
node binary serve today's blob mode (Gnosis, Ethereum) and the new mode; the mode is chosen when
the registry is deployed.

Non-goals (follow-ups): the compact DA stream (a zkVM change; deferred so this work needs no new
program vk), DKG and Council key modes on OP (need their managers on OP), Arbitrum (no L1 roots on
L2), transitions whose blobs span several L1 transactions in the node (the contract supports it;
the node starts with one L1 transaction, up to 6 blobs).

## 2. How a transition settles

1. The node seals a batch and builds its blobs, exactly as today.
2. **At proving start** it posts the blobs on Ethereum: one type-3 transaction to its own L1
   address, EIP-7594 v1 sidecar, at most 6 blobs.
3. When the proof is ready, the blob transaction is mined in L1 block `n`, and OP's L1 origin
   (`L1Block.number()`) has reached `n`, the node builds an L1 proof and calls the new registry
   entry point on OP.
4. The registry runs today's checks, gets the blob versioned hashes from the L1 proof, opens each
   commitment with `0x0A` at the circuit's `z`, checks the digest against `REG_BLOBS_DIGEST`,
   verifies the PLONK proof and applies the transition.
5. Followers read the L1 block and versioned hashes from the event and fetch the blobs from an
   Ethereum beacon API (or an archive or a peer), then sync exactly as today.

## 3. The L1 proof (execution-layer path)

The PoC proved commitments through the beacon root (EIP-4788). That path depends on the beacon
block layout, which EIP-7732 (ePBS) changes, and on a 4.5-hour ring buffer. The production design
proves through the execution layer instead, which only changes by appending header fields:

- OP's `L1Block` predeploy (`0x4200000000000000000000000000000000000015`) exposes `hash()`, the
  hash of the current L1 origin block `m`, derived by OP's own state transition (the trust the
  registry already places in OP).
- The proof carries RLP headers newest first, from about the L1 head down to the blob block
  `n`; each header's `parentHash` equals `keccak` of the next one. The chain is anchored at **any**
  header `j` with `keccak(headers[j]) == L1Block.hash()`; blob transactions may only reference
  headers at `j` or older (headers newer than the origin are not yet canonical for OP and are
  ignored). Sending a few headers above the current origin keeps the proof valid while the origin
  advances, so the settlement does not race it.
- Headers are decoded as RLP lists by index without asserting the item count (London 15,
  Shanghai 16, Cancun 19, Prague 20, later forks append): `parentHash` item 0,
  `transactionsRoot` item 4, `number` item 8; lists shorter than 9 items are rejected.
- For each blob transaction: an MPT proof of key `rlp(txIndex)` against `transactionsRoot`
  (RLP field 4) of its block. The value is the typed envelope `0x03 ‖ rlp([...])`; field 10 is
  `blob_versioned_hashes`.
- The transaction key is `rlp(txIndex)`: `0x80` for index 0, one byte for 1-127, `0x81 xx` for
  128-255, `0x82 xx xx` above. The value is the type-3 envelope: first byte `0x03`, then an RLP
  list whose item 10 is `blob_versioned_hashes`, non-empty, each 32 bytes starting with `0x01`.
- An L1 block that contains a blob transaction was only valid if its blobs were available, so this
  gives the same availability guarantee as `BLOBHASH` on L1.
- `vhs` is the concatenation, in proof order, of the listed transactions' versioned hashes and must
  have exactly `n` entries; each `vhs[i]` is bound to `commitment_i` by `0x0A`, and the digest fixes
  the commitment order. A blob transaction therefore carries exactly one transition's blobs.
- Reusing a blob transaction for another transition is harmless: `z` binds `pid ‖ rootBefore ‖
  com`, and the zkVM cells of a different transition differ. Replaying a settlement is stopped by
  root continuity.
- If no header matches `L1Block.hash()` the call reverts `L1OriginMismatch` and the node resends
  with a fresh chain (rare once the chain reaches above the origin).
- Residual: a bug in `L1BlobVerifier` requires a registry redeploy, the same class as a new program
  vk.

## 4. Contracts (davinci-contracts)

`L1BlobVerifier` (new, standalone, stateless, no owner):

```solidity
struct L1TxProof { uint256 headerIndex; bytes key; bytes[] proof; }   // key = rlp(txIndex)
struct L1BlobProof { bytes[] headers; L1TxProof[] txs; }
/// Returns the blob versioned hashes of `txs` in order (flattened) and, per tx, the number of
/// the L1 block that contains it.
function verify(L1BlobProof calldata p) external view returns (bytes32[] memory vhs, uint256[] memory txBlocks);
```

- RLP decoding and the Merkle-Patricia proof use Optimism's audited `RLPReader` and `MerkleTrie`
  (contracts-bedrock, MIT), vendored with attribution.
- `L1Block` address is an immutable constructor argument (OP Stack value in production, a mock in
  the e2e harness).
- Errors: `L1OriginMismatch`, `BrokenHeaderChain`, `NotBlobTx`, `BadTxProof`.

`ProcessRegistry`:

- Constructor gains `address _l1BlobVerifier`; immutable. Zero means today's blob mode; non-zero
  means L1-blob mode. One source, one artifact.
- `submitStateTransition` (unchanged ABI) reverts `WrongDAMode` in L1-blob mode.
- New: `submitStateTransitionL1(bytes31 processId, bytes publicValues, bytes proofBytes,
  bytes[] commitments, bytes32[] ys, bytes[] kzgProofs, L1BlobProof l1)`, reverts `WrongDAMode`
  in blob mode. Order: shared prepare checks (status, time, ok, root, census, occupied, counts);
  `n = REG_N_BLOBS`, arrays of length `n`; digest of `(commitment_i, y_i)` equals
  `REG_BLOBS_DIGEST`; `vhs = l1BlobVerifier.verify(l1)` with `vhs.length == n`; per blob
  `0x0A(vhs[i], z_i, y_i, commitment_i, kzgProof_i)` (`0x0A` itself checks that the commitment
  hashes to `vhs[i]`); PLONK verification; shared apply; event.
- The shared checks and state writes move into `_prepareTransition` and `_applyTransition`, and
  `_checkBlobsDigest` splits into a digest-only check plus the mode's versioned-hash source: the
  L1-blob path never calls `BLOBHASH` (no `blobHash(n) == 0`, no `MissingBlob`). The blob-mode path
  keeps its behaviour and its tests.
- New event `ProcessStateTransitionedL1(bytes31 indexed processId, uint256[] l1BlockNumbers,
  bytes32[] versionedHashes)` emitted right after the unchanged `ProcessStateTransitioned`, so
  followers know which L1 blocks hold the blobs.
- Bytecode headroom is 3,022 bytes; the L1 logic lives in `L1BlobVerifier`, so the registry grows
  by one entry point and one immutable. Measured after the first build.
- Deploy: `L1_BLOB_VERIFIER` (or `L1_BLOCK` to deploy one) in `DeployAll.s.sol`, read back by
  `verify_deployment.py`; Go bindings and the vendored ABI regenerated.

## 5. Node (davinci-sequencer)

- **Configuration.** New network preset `optimism` (chain 10): OP RPCs, registry and start block
  after deployment, `confirmations` 1, DA source `l1-blobs`. New options: `--l1-rpc-url` (Ethereum
  execution RPCs, failover list; must accept ~1.5 MB request bodies for 6-blob sends),
  `--l1-send-rpc-url` (optional, send only), `--l1-beacon-url` (Ethereum beacon APIs for
  followers, list), `--l1-privkey-file` (a dedicated L1 account: the blob pool forbids nonce gaps,
  caps pending transactions per account and needs a 100 % fee bump to replace, so the L1 account
  must not share nonces with anything else). The node reads the registry's
  `l1BlobVerifier()` at boot and refuses a mismatch with its configuration.
- **L1 blob sender.** One blob transaction in flight per L1 account; EIP-7594 v1 sidecars (today's
  `build_sidecar`); tip `max(1 gwei/1000, suggested)`, `max_fee = 2·base + tip`,
  `max_fee_per_blob_gas ≥ 2·blob base fee`; replacement after 3 L1 blocks with a 100 % bump of all
  three fees (blob-pool rule); a per-transition fee ceiling, above which it waits within the grace
  window; never a nonce gap.
- **Actor flow.** Seal → build blobs → write ahead (persist batch, sidecar, L1 nonce and tx hash)
  → post on L1 (at proving start) → proof → wait for the L1 receipt plus one L1 block (reorg
  margin) and for `L1Block.number() ≥ n` on OP → build the L1 proof (headers from the L1 head down
  to `n`) → send the OP settlement with a fixed gas limit (about 1.2M) → on `L1OriginMismatch`
  rebuild and resend.
- **Restart and reorgs.** Blobs stay in the datadir until the transition settles. On restart a
  posted transition resumes: receipt found → wait for the origin; pending → wait or replace;
  unknown (dropped, or reorged out of L1) → re-post the stored sidecar at the same nonce if free,
  else a new one. If the blob transaction's block changes after the receipt, the proof is rebuilt. A lost race (`InvalidStateRoot`) follows
  today's rollback and requeue path; the posted blobs are wasted (a few cents at most).
- **L1 proof builder.** Headers from `eth_getBlockByNumber` re-encoded with alloy's `Header`
  (checked against the block hash); the transactions trie of block `n` rebuilt from its full
  transaction list (EIP-2718 envelopes) with `alloy-trie`, proof retained for `rlp(txIndex)`; local
  verification before sending.
- **Sync.** `ProcessStateTransitionedL1` gives the L1 blocks and versioned hashes; the follower maps
  each block to its slot (block timestamp and beacon genesis), fetches the blobs from the
  configured beacon APIs (only nodes that hold full blobs serve them after PeerDAS), the posting
  node's `/transitions/:index/blobs` or peers (`anvil` in tests), checks them against the
  versioned hashes, and applies them as today. Storage and the transitions API are unchanged.
- **Origin watch.** The node polls `L1Block.number()` each OP block; sequencer drift can hold an
  origin for minutes, which is latency, not failure. Races run on the unsafe head plus one block.
- **Client.** `networks.rs` gets the OP preset; `Organizer` works unchanged against the OP
  registry; `verify_registry` reports the DA mode.

## 6. Tests

- **Contracts (forge):** `L1BlobVerifier` against real mainnet fixtures: Cancun and Prague-era
  headers, a blob transaction at index 0, one at index ≥ 128, a block with more than 256
  transactions, anchors at the first, a middle and the last header, headers above the origin that
  the proof must not use; every failure path (no anchor, broken chain, non-blob transaction, bad
  proof, wrong item count); registry in both modes, wrong-mode reverts, digest and opening
  failures, event contents; gas report.
- **Node unit and integration:** the L1 proof builder against at least 100 random mainnet blocks,
  Cancun and Prague, index 0, index ≥ 128 and blocks with more than 256 transactions (it must
  reproduce `transactionsRoot` and the block hash, and its proofs must pass `L1BlobVerifier`); `FakeChain` support for the L1 mode (post, settle, origin
  race, lost race, restart with a blob posted but not settled); config and preset tests.
- **Anvil (`ANVIL=1`):** two anvil chains, Ethereum-like on the mainnet hardfork (blobs) and
  OP-like (the registry in L1-blob mode, a mock `L1Block` at the predeploy address updated by the
  harness from the L1 anvil with a configurable lag); real settlement end to end; an
  `anvil_reorg` on L1 that forces a re-post. The mock reproduces everything the contract sees; it
  cannot reproduce OP derivation timing and drift, unsafe-head reorgs, real blob-pool replacement,
  RPC body limits, beacon availability after PeerDAS or fee markets: the live phase covers those.
- **e2e (`DAVINCI_E2E=1`, `DAVINCI_E2E_DA=l1-blobs`):** the same harness with the real prover and
  three nodes plus an observer: Merkle census with races, CSP, rejected packages, a node killed
  between posting and settling and restarted, an observer that only follows, the grace window;
  negative replays (wrong header chain, wrong transaction proof, swapped commitment).
- **Live (`DAVINCI_E2E_LIVE=1`, OP preset):** on the OP Mainnet deployment with the real prover.
- **Benchmarks:** batch sizes 64, 128 and 256 at 2 and 16 fields on OP Mainnet: proof time,
  post-to-settle latency, L1 and OP cost.

## 7. Deployment on OP Mainnet

A dedicated L1 key for blob transactions, funded from the owner's account. `L1BlobVerifier` (with
the OP `L1Block` address) and `ProcessRegistry` in L1-blob mode, using the
`ZiskVerifier` already deployed at `0x6781FFF50D615CB3F331CCD3Bdad3416d4ce0d5c` (codehash equal to
the pinned one) and the current release pins; no DKG or Council adapters. The OP preset and
`docs/deployments.md` record the addresses, and the preset lists beacon providers that serve full
blobs. `verify_deployment.py` checks `l1BlobVerifier()` and the verifier's `L1Block` immutable.
Docs note for SDKs and explorers: OP transitions have no `BLOBHASH`; consumers read
`ProcessStateTransitionedL1`. An OP Sepolia soak was recommended but needs testnet ETH on
Sepolia and OP Sepolia, which the owner would have to provide; without it the live phase starts
with small elections on OP Mainnet. Funding: the owner's key (Ethereum 0.0037 ETH, OP
0.0018 ETH); a transition costs about 0.01-0.02 USD, almost all on L1.

## 8. Phases

0. **L1 proof spike:** `L1BlobVerifier` plus the Rust proof builder, validated against at least
   100 real mainnet blocks (fixtures above) and with one settlement on OP. Gate: Fable reviews.
1. **Registry:** the mode, the entry point, the event, tests, deploy scripts, ABI and bindings.
   Gate: Fable reviews.
2. **Node and client:** configuration, L1 sender, actor flow, proof builder, sync, presets, unit and
   integration tests. Gate: Fable reviews.
3. **Anvil and e2e harness:** dual-chain anvil, mock `L1Block` relayer, e2e scenarios and negatives,
   CI. Gate: all suites green.
4. **OP Mainnet:** deployment, live e2e, benchmarks, report, docs.

## 9. Risks

| risk | mitigation |
|---|---|
| OP origin advances between building and mining the settlement | anchor at any header; send headers above the origin; `L1OriginMismatch` retry as fallback |
| Node killed between posting and settling | write-ahead record, resume or re-post on restart |
| L1 fee spikes | per-transition ceiling, wait within the grace window |
| Blob transactions stuck (blob-pool replacement rules) | dedicated L1 account, one in flight, 100 % bump after 3 blocks |
| Public L1 RPCs reject 1.5 MB bodies or blob methods | separate send endpoint; production nodes use their own or paid L1 endpoints |
| Followers cannot fetch blobs after 18 days or from non-supernode beacon APIs | several sources: beacon APIs, the node's own archive, peers |
| Shared GPU prover with a production node | bounded runs, spaced benchmarks |
| Registry bytecode limit | the L1 logic is a separate contract |
