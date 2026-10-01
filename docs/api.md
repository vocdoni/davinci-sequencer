# HTTP API

The node serves JSON over HTTP, by default on port 9090. The wire types live in
`davinci_client::api`, and `davinci_client::SequencerClient` covers every route.

## Wire format

- Field names are camelCase.
- Field elements are decimal strings; bytes are `0x` hex.
- A process id is `0x` followed by 62 hex digits (`bytes31`).
- A vote id is `0x` followed by 16 hex digits and must be at least 2^63.
- Points are twisted Edwards `{x, y}`.
- A ballot is exactly 16 `{c1: {x, y}, c2: {x, y}}` ciphertexts.

Decoding is strict: field elements must be below the field modulus, points on the curve, byte
lengths exact, and vote bodies may carry no unknown fields. Bodies are capped at 256 KiB and
requests time out after 60 s.

## Routes

| Method | Path | Purpose | Error codes |
|---|---|---|---|
| GET | `/ping` | Liveness (`pong`). | |
| GET | `/info` | Sequencer address (`null` for an observer), chain id, registry, ballot VK hash, both program verification keys, `observer`, and the counters `settledBySelf`, `syncedFromOthers` and `lostRaces`. | |
| GET | `/processes` | Process ids this node knows. | |
| GET | `/processes/{pid}` | On-chain parameters plus the node's view, described below. | 40001, 40402 |
| POST | `/processes/keys` | Body `{"processId"}`: this node's election key for that future process, as `{x, y}`. | 40001, 41203, 42901 |
| GET | `/processes/{pid}/participants/{address}` | Weight and Merkle census proof, which a voter needs to build a ballot. Merkle censuses only. | 40001, 40402, 40401 |
| GET | `/processes/{pid}/transitions` | Settled transitions: roots, transaction hash, block, sender, voters, overwrites, blob count. | 40001, 40402 |
| GET | `/processes/{pid}/transitions/{index}/blobs` | The raw blobs of one transition, `0x` hex. | 40001, 40402, 40401 |
| POST | `/votes` | Submit a vote. | see [Errors](#errors) |
| GET | `/votes/{pid}/voteId/{voteId}` | Vote status. | 40001, 40402, 40401 |
| GET | `/votes/{pid}/voteId/{voteId}/proof` | Tracker proof: the vote id's leaf under the node's committed root, which is an on-chain root. | 40001, 40402, 40401 |
| GET | `/votes/{pid}/address/{address}` | The re-encrypted ballot stored in that voter's slot. For a CSP census, only addresses this node served. | 40001, 40402, 40401 |

`GET /processes/{pid}` adds the node's view to the registry's parameters:

- `isAcceptingVotes`: false before the start time and from the end on;
- `localStateRoot`: the node's committed tree root, and `synced`: whether it equals the on-chain
  root;
- `pendingVotes` and `nextSealNotBefore`: the batching queue — how many votes wait, and the
  earliest instant the open batch can seal (an estimate: new votes only bring it forward);
- `result`: the tally, once it is on-chain;
- `ignored` and `note`: set when the node refused to serve the process, with the reason.

`POST /processes/keys` derives the key instead of storing it, so the same process id always gets
the same key. Organizers ask for the registry's `getNextProcessId(organizer)`.

## Submitting a vote

`POST /votes` takes:

| Field | Content |
|---|---|
| `processId` | The process. |
| `address` | The voter's address. |
| `voteId` | The vote id. |
| `ballot` | 16 ciphertexts. |
| `ballotProof` | snarkjs Groth16 proof. |
| `ballotInputsHash` | Hash of the ballot proof inputs. |
| `signature` | 65 bytes `r‖s‖v`, a personal-sign over the vote id. |
| `weight` | Decimal string. |
| `censusProof` | Required for a CSP census: `{"type": "csp", "r", "s", "recid", "index"}`. The CSP signature covers the top-level `weight`. For a Merkle census the node derives the proof from its own copy and ignores this field. |

The response is `{"voteId": "0x..."}`.

A paused process still accepts votes until its end; they settle when it resumes, or during the
grace window if it is still paused at the end. An `ended` or `canceled` process, or any process at
or past its end time, refuses with 41201. A process before its start time refuses with 41204.

One ballot slot holds up to `--slot-depth` queued votes on a node, and they settle in the order
they were cast. Across nodes there is no order: when one voter's ballots go to different nodes,
the one that settles last stands. Clients should send all of one voter's ballots to one node.

## Vote status

`GET /votes/{pid}/voteId/{voteId}` answers `{"status": "..."}`:

| Status | Meaning |
|---|---|
| `pending` | Queued. |
| `aggregated` | In a batch being proved. |
| `processed` | Proof checked, settlement pending. |
| `settled` | On-chain. |
| `error` | Failed; an `error` field says why (a circuit failure, `process closed`, a settlement revert, a prover refusal, or `census changed, recast`). |

A batch that loses a race goes back to `pending`, and so does one the settlement refuses because
the organizer paused the process. Votes still queued when the grace window closes, or when the
process is canceled, end in `process closed`. A node that never stored a vote still answers
`settled` when its id is in the tree, and 404 otherwise.

Once a vote is settled, `GET /votes/{pid}/voteId/{voteId}/proof` returns a tracker proof, and
`davinci_client::api::verify_tracker` checks it against the registry's state root.

## Errors

Errors are JSON `{"error": "<message>", "code": <code>}`. The code is the HTTP status times 100
plus a discriminator. A 500 body says only `internal error`; the detail goes to the node log.

| Status | Code | Meaning |
|---|---|---|
| 400 | 40001 | Malformed request or field. For votes also: address not in the census, missing CSP proof, or a vote for another process. |
| 400 | 40002 | The vote failed a protocol check: ballot proof, signature, inputs hash, census binding, ballot encoding or weight. |
| 404 | 40401 | Not found. |
| 404 | 40402 | Unknown process, or one this node does not serve. |
| 408 | 40801 | The 60 s request deadline fired. The request may still have taken effect: a timed-out `POST /votes` may have admitted the vote, and a retry then answers 40901. |
| 409 | 40901 | Vote id already submitted, queued or in the tree. |
| 409 | 40902 | The slot already holds `--slot-depth` queued votes; retry once one settles. |
| 412 | 41201 | The process does not accept votes, or its census cannot be used (an update that fails to load, an unusable census contract); the message says which. |
| 412 | 41202 | Max voters reached. |
| 412 | 41203 | Observer node. |
| 412 | 41204 | Not open yet: the start time is ahead. |
| 413 | 41301 | Body over 256 KiB. |
| 429 | 42901 | Key generation rate limit (`--keys-per-minute` per client IP). |
| 429 | 42903 | Busy: vote validation at capacity, the process queue full (16384), or the census still loading. |
| 500 | 50001 | Internal error. |
