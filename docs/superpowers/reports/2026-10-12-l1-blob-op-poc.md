# PoC report: Ethereum L1 blobs, settled on OP through the beacon root

Date: 2026-10-12, 19:55-20:25 UTC. Code: `poc/l1-blob-da/` (branch `poc/l1-blob-da`). Previous,
discarded approach (L2 calldata): local tag `archive/calldata-da-poc`.

## Verdict

Feasible, and it keeps every property the owner asked for. A batch's data is published as
ordinary EIP-4844 blobs on Ethereum and the transition settles on OP Mainnet with a fully
non-interactive check: OP's own EIP-4788 contract gives the beacon block root, an SSZ Merkle proof
ties each blob's KZG commitment to it, and the `0x0A` precompile opens the commitment at the
point the zkVM program evaluated. No notary, no bridge message, no change to the zkVM program,
no new cryptography. A 256-vote, 16-field batch (530 KB, 5 blobs) cost **0.017-0.019 USD**; it
settled **101-108 s** after the proof was ready when the blobs were posted only then (the owner's
rule), and **25 s** when they were posted at the start of proving. Data capacity is Ethereum's
blob space, not OP's 100 KB per block.

## How it works

1. The sequencer posts the batch's blobs on Ethereum: a type-3 transaction to its own address,
   EIP-7594 v1 sidecar (cell proofs), at most 6 blobs per transaction.
2. The blob's beacon block root becomes the `parentBeaconBlockRoot` of the next Ethereum
   execution block. OP copies the `parentBeaconBlockRoot` of each L1 origin into its blocks, and
   its EIP-4788 contract (`0x000F3df6D732807Ef1319fB7B8bB8522d0Beac02`) returns it for any OP
   block timestamp of the last ~4.5 h. Verified live on OP, Base and Unichain (not Arbitrum).
3. Once OP's L1 origin reaches that block, the sequencer settles on OP with, per blob: the
   commitment, `y`, the KZG proof at `z`, its index in the beacon block, and the 17-node
   `kzg_commitment_inclusion_proof` served by the beacon API (`/eth/v1/beacon/blob_sidecars`), plus
   the beacon block header (5 fields) and the OP timestamp `T`.
4. The contract checks: `4788(T) == hash_tree_root(header)`; each commitment's SSZ branch
   (gindex `27·8192 + index`) leads to the header's `body_root`; versioned hash from the
   commitment; `z = sha256(pid ‖ rootBefore ‖ com) mod r`; `0x0A` opening; digest
   `sha256(com_0 ‖ y_0 ‖ …)` (the zkVM program's `BlobsDigest`); PLONK verification; state writes.
5. Followers fetch the blobs from any beacon node (or archive) by slot and index.

## Setup

- `BlobDAPoC` on OP Mainnet `0x06713Ec6797A6EBCbf94bB897cD6cDdA6618Cd4e`, using the
  production-identical `ZiskVerifier` `0x6781FFF50D615CB3F331CCD3Bdad3416d4ce0d5c` (codehash equal
  to the Gnosis verifier and to `ZISK_VERIFIER_CODEHASH`).
- Every settlement verified the real PLONK proof of Gnosis settlement `0xc1fc0ad4…526b`; the blob
  data were random streams of the scenario's exact length, embedded 31 bytes per cell.
- Driver: `poc/l1-blob-da/driver`. Reads go to the public publicnode RPC and Lighthouse beacon
  API; the blob transactions are sent through dRPC because publicnode rejects request bodies
  above about 1 MB (a 5-blob transaction is 687 KB, about 1.4 MB as hex).
- Prices: Ethereum base fee 0.10-0.11 gwei, blob base fee 0.005-0.007 gwei; OP base fee about
  1,000 wei, tip 10,000 wei; ETH 2,508 USD.
- Local tests: 20 forge tests (including a full settlement on a real mainnet blob with the real
  `0x0A` precompile), 4 Rust tests; read-only checks of two other mainnet blob transactions.

## Results

All seven runs passed every check: the inclusion proof, the header against the beacon API,
the next block's `parentBeaconBlockRoot`, OP's origin reaching that block, `4788(T)` matching,
the settled digest and beacon root, OP and L1 statuses, and the follower rebuild from the beacon
API.

| scenario | data | blobs | posted | proof-ready -> settled | OP gas | L1 cost | OP cost | total |
|---|---:|---:|---|---:|---:|---:|---:|---:|
| 256 votes, 2 fields | 72 KB | 1 | after proof | 102.6 s, 109.4 s | 465.9k | 2.9-3.0 µETH | 0.03 µETH | 0.0074-0.0077 USD |
| 256 votes, 2 fields | 72 KB | 1 | at proving start (46 s proof) | 47.5 s | 465.9k | 3.3 µETH | 0.03 µETH | 0.0083 USD |
| 256 votes, 16 fields | 530 KB | 5 | after proof | 101.0 s, 107.8 s | 774.0k | 7.2-7.5 µETH | 0.05 µETH | 0.018-0.019 USD |
| 256 votes, 16 fields | 530 KB | 5 | at proving start (77 s proof) | **24.9 s** | 774.0k | 6.8 µETH | 0.05 µETH | 0.017 USD |
| 6 blobs (maximum per tx) | 762 KB | 6 | after proof | 109.2 s | 851.3k | 7.7 µETH | 0.05 µETH | 0.019 USD |

Timeline: the blob transaction was included 0.5-10.6 s after it was sent; OP's L1 origin reached
the following block 91-105 s after sending (OP's origin lag was 6-7 L1 blocks); the OP settlement
was included within one or two OP blocks.

## Findings

1. **Latency is OP's L1-origin lag.** About 85-95 s from L1 inclusion until the root is readable on
   OP. Posting the blobs when proving starts hides it: 25 s for a 77 s proof, about 5 s for proofs
   longer than ~95 s (512-1024-vote batches). Posting only after the proof gives about 100-110 s.
2. **Cost is the L1 blob transaction.** About 21,000 gas at the L1 base fee plus about 131k blob gas
   per blob: about 0.005 USD plus 0.002 USD per blob at today's prices. OP settlement is about
   389k gas plus 77k per blob, under 0.0002 USD.
3. **Gas on OP:** 465.9k (1 blob), 774.0k (5), 851.3k (6), of which 307k is the PLONK
   verification.
4. **Capacity is Ethereum's blob space** (target 14, max 21 blobs per 12 s block after BPO2),
   shared with all rollups; OP's per-block DA cap no longer applies.
5. **RPC practicalities:** multi-blob transactions need an endpoint that accepts ~1.5 MB request
   bodies and EIP-7594 v1 sidecars; free tiers differ in supported methods (`eth_blobBaseFee`).
   Production sequencers should use their own L1 node or a paid endpoint, and a beacon node that
   serves `blob_sidecars`.

## Comparison with the discarded calldata approach (same scenario, 256 votes × 16 fields)

| | L2 calldata (archived PoC) | L1 blobs + OP beacon proof |
|---|---|---|
| cost | 0.013 USD | 0.017-0.019 USD |
| latency after proof | ~12 s | 101-108 s (25 s if posted at proving start) |
| data capacity | OP's ~100k est. bytes per block, ~50 KB/s chain-wide | Ethereum blob space, ~1.5 MB per block |
| trust | Ethereum + OP | Ethereum + OP |
| circuit change | yes (`DataHash` output) | none |

## Limitations

- One 30-minute window on a Sunday evening (UTC), quiet L1 fees (0.1 gwei). At 1 gwei the L1 part
  is about 8× higher (about 0.06 USD for 5 blobs).
- The beacon proof path depends on the beacon block layout (`blob_kzg_commitments` at body field
  11). A fork that moves it (ePBS) changes the gindex; the production registry needs a plan for
  that, since it is immutable today.
- The 4788 ring buffer on OP covers about 4.5 h: settlement must happen within that window after
  the blob lands (it took under 2 minutes here).
- Only OP Stack chains expose L1 beacon roots this way; Arbitrum does not.

## Spend

About 38 µETH on Ethereum (seven blob transactions, about 0.10 USD) and under 1 µETH on OP
(deployment and seven settlements). Balances after the runs: 0.003703 ETH on Ethereum,
0.001807 ETH on OP.
