# Censuses

A census decides who may vote and with what weight. The organizer picks its origin when creating
the process.

| Origin | Census | Root a transition may use |
|---|---|---|
| 1 | Static Merkle tree (Keccak lean-IMT), downloaded from `censusURI`. | The root fixed at creation. |
| 2 | Off-chain dynamic Merkle tree: the organizer replaces it with `setProcessCensus`. | The current root only. |
| 3 | An on-chain census contract; the node builds the tree from its `CensusMemberAdded` logs. | Any root the contract recorded at or after the process's creation block. |
| 4 | CSP: a census provider signs each voter's eligibility with a secp256k1 key whose address is the root. | The CSP address. |

For a Merkle census (origins 1 to 3) voters fetch their proof from
`GET /processes/{pid}/participants/{address}`, and the node checks every vote against its own copy
of the census. For a CSP census the vote carries the provider's signature over the process, the
address, the weight and an index.

## The Merkle tree

Origins 1 to 3 use a lean incremental Merkle tree with Keccak nodes: a node is
`keccak256(be32(left) ‖ be32(right))` (Solidity `keccak256(abi.encode(left, right))`), a lone
right-most node moves up unhashed, and the root of an empty tree is 0. A leaf is
`(address << 88) | weight`, below 2^248; the circuit refuses a leaf at or above it, which keeps
leaves apart from internal nodes.

Roots, leaves and siblings are 256-bit big-endian integers. Most roots are above the BN254 field
modulus, so the node never treats them as field elements: they are stored and compared as bytes,
and every JSON wire carries them as `0x` followed by exactly 64 hex digits (see
[api.md](api.md#wire-format)).

## Census files

Origins 1 and 2 are downloaded from `censusURI`. The node accepts:

- davinci-node's census file, `{"participants": [{"key": "0x<address>", "weight": "<n>"}]}`;
- a census dump, `{"root", "participants"}` with `address` and `addressIndex` per entry, whose
  root is checked; the root is `0x` and 64 hex digits (a decimal or numeric root is refused);
- JSONL, one participant per line (`application/x-ndjson` over HTTP, detected for files).

Weights are decimal strings or JSON numbers below 2^88. The on-chain root is authoritative: a
file whose tree does not match it is refused.

Downloads are restricted, because the URI comes from on-chain data:

- only `http(s)` by default, to public hosts only, with no redirects, no proxy and a 256 MiB cap;
- a download that brings less than 16 KiB in a 30 s window after its first byte is cut off and
  retried later. At most 32 connections and 2 tree builds run at once. Downloaded bytes are
  charged to a 1 GiB budget as they arrive, so a slow or silent server holds a socket but almost
  no budget and cannot hold up other censuses. A downloaded census keeps its bytes until a build
  takes it, so at most 1 GiB plus the 2 bodies being built are in memory;
- a `file://` document's bytes are reserved from the same budget before it is read; a download that cannot get budget is retried later, not counted as a failure;
- loopback and private hosts need `--census-allow-private`;
- `file://` needs `--census-dir`, and the path must resolve to a regular file under that
  directory;
- the census may not exceed `--census-max-participants` (origin 3 too: a contract with more members is refused).

At bootstrap, a 4xx answer (except 408 and 429) or a refused URI is permanent and the process is
ignored; network errors are retried.

## Ballot slots

Each voter's ballot lives at one slot of the state tree. For a Merkle census the slot comes from
the voter's address:

```
slot = 0x10 + (be64(sha256("davinci-slot-v1" ‖ address20)[0..8]) mod (2^63 − 16))
```

A CSP census uses `0x10 + index`, with the index the CSP signs. An address-derived slot stays put
however the census grows.

Two members on one slot would overwrite each other's ballots, so slots must be unique. The census
contract rejects a registration whose slot is taken, the node refuses any Merkle census with
colliding slots, and the circuit rejects two votes for one slot in a batch. Grinding a collision
into a census of N members costs about 2^63/N key generations.

## Updatable census (origin 2)

Only the organizer can call `setProcessCensus`, with the same origin, while the process is ready
or paused and before its end. It emits `CensusUpdated`, and settlement accepts only the current
root.

- The node fetches the new census in the background, newest root only. Votes answer 42903 while
  it loads. An update that can never load (bad format, root mismatch, refused URI) answers 41201
  with the reason until the next `CensusUpdated`.
- A batch in flight during an update is rolled back and sealed again at the new root.
- A pending vote whose census entry changed (the member was removed or reweighted) ends in
  `error: census changed, recast`.
- A process whose initial census is bad is ignored; the organizer has to create a new process.

## On-chain census (origin 3)

The registry accepts a transition's census root when the contract's `getRootBlockNumber(root)` is
non-zero, not in the future and not before the process's creation block. At creation it refuses
`onchainAllowAnyValidRoot = true` and a census address without code.

The contract must be append-only with fixed weights and build the same Keccak tree.
davinci-onchain-census-contract (its `KeccakLeanIMT`, which links no library) checks slot
uniqueness on registration and never evicts old roots.

- The node indexes `CensusMemberAdded` at the confirmed head, replaying every addition against the
  tree and its `newRoot`, and seals against the newest confirmed root. Background syncs run at
  most every `--census-sync-every`; while a contract has no confirmed snapshot yet, a waiting vote
  or participant query syncs it on demand, best effort. Once indexed, a member added on-chain
  becomes votable within that window at most — a `not in the census` refusal right after an
  addition just needs a retry.
- A reorg below the indexed block drops the index and rescans. An endpoint whose head is behind
  the target (or does not serve the stored block yet) is retried later: its short answers never
  count against the contract. After three full scans from an up-to-date endpoint still fall short
  of `treeSize` the contract is marked unconfirmed (in memory): new votes are refused, queued votes
  stay pending, it is retried at the longest backoff, and a good sync clears the mark.
- Background syncs run per contract: at most four first scans and, separately, four incremental
  syncs at a time, so a slow or endless first scan delays only its own contract.
- A `WeightChanged` with a non-zero previous weight, or a slot collision, marks the census unusable
  and its votes are refused.
- The RPC must support `eth_call` at a block hash (EIP-1898). A provider that silently truncates
  `eth_getLogs` results fails closed: the replayed tree does not match the contract and the sync
  retries.

Not supported: binding the slot owner into the ballot leaf.
