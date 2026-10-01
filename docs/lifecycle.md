# Election lifecycle

How an election moves through a node: bootstrap, vote admission, batching, settlement, the grace
window and the results. Registry calls are named as in davinci-contracts; the circuit checks are
specified in davinci-zkvm (`circuit/CIRCUIT.md` for vote batches, `circuit-results/RESULTS.md`
for results).

## Bootstrap

One loop polls the registry events up to `head − confirmations` and routes them. A
`ProcessCreated` bootstraps the process after these checks:

- the election key is a subgroup point other than the identity;
- the census origin is one of the four supported (see [census.md](census.md));
- the on-chain root is the genesis root the node derives from the process configuration. A node
  that starts late requires the process's first transition to start from that root instead.

Merkle censuses (origins 1 and 2) are downloaded and their root checked. For an on-chain census
(origin 3) the node indexes the census contract and serves votes after the first sync. A process
that fails a permanent check is marked `ignored` with the reason. Transient failures are retried
from a persisted list, and the process is ignored after 10 attempts. A late bootstrap scans the
registry logs from the process's creation block and replays every transition from its blobs.

## Admission

API handlers first run the stateless checks on a bounded blocking pool: the ballot proof, the
signature, the census proof and the inputs hash. These repeat every per-vote check the circuit
makes (`davinci_state::validate_vote`), so one bad ballot cannot sink a batch. The vote then goes
to the process actor, the single writer of that process's state, which checks that:

- the vote id is not in the tree or the queue;
- the slot holds fewer than `--slot-depth` queued votes;
- max voters is not exceeded;
- the process is accepting votes.

## Batching

The actor seals a batch when one of these holds:

| Trigger | Condition |
|---|---|
| capacity | The queue no longer fits one transaction, or `--batch-max` votes are pending. |
| timer | The batch holds at least `--min-mix` distinct slots and the oldest vote has waited `--batch-time`. |
| solo | Fewer than `--min-mix` slots, and the oldest vote has waited `--solo-wait`. |
| flush | The election ends within `--flush-horizon`, or has ended and its grace window is still open. |

Both waits carry a ±10% jitter, drawn once per batch.

With the defaults, a batch holding at least `--min-mix` slots settles about `--batch-time` after
its oldest vote, and a lone vote waits `--solo-wait` (both ±10% jitter) — though any later vote
gives a waiting batch company and can seal it sooner. From `--flush-horizon` before the end the
batching waits no longer apply: everything pending seals as the proving budget allows, through
the grace window, so the waits cannot hold a vote past its election. `GET /processes/{pid}`
shows `pendingVotes` and `nextSealNotBefore`, so a client can tell a vote queued for company
from one that is stuck.

A batch is also sized to the time left. Proving is estimated as `--prove-base` plus a per-vote
cost, a moving average of the prover's measured job time that starts high. While the election is
open, the batch holds at most the votes whose estimated proof plus `--settle-margin` ends before
`min(graceEnd, now + graceFloor)`; once it has ended, before `graceEnd`. The second term covers
the worst legal close: the organizer ends the election right after the seal and lowers the grace
to the registry's floor. When not even one vote fits, nothing seals and the votes wait.

To build a batch, the actor:

1. takes votes in arrival order, the oldest per slot, within `maxVoters`, the size above and the
   per-transaction blob limit (counting the refreshes the circuit will require);
2. draws a fresh seed and a refresh selection from the OS random number generator;
3. re-encrypts every ballot along the seed's scalar chain and re-randomizes a sample of other
   occupied slots, so an observer cannot tell a revote from a routine refresh;
4. builds the state-tree witnesses, the blobs with their KZG commitments and openings, and the
   public outputs it expects.

## Settlement

A background job proves the batch. The node accepts the proof only when the circuit reports
success, every public output equals the node's own value, and the program verification key and
`rootCVadcopFinal` equal the release pins. It then simulates `submitStateTransition` and sends the
blob transaction. A node keeps at most one batch in flight per process; processes run in
parallel.

Transient prover errors back off (up to about 5 minutes) with the votes still pending, and end
in `error` only if the election closes first. A prover refusal, or a circuit failure, errors the
votes at once.

## Races and sync

Settlement is permissionless. When another sequencer lands first, the node's simulation or
transaction reverts with `InvalidStateRoot`. The node then rolls its tree back, counts a lost
race, puts the votes back in the queue, syncs, and builds the next batch on the new root. A lost
race costs only the gas of the reverted transaction.

For a transition another node sent, the node fetches the blobs, matches them against the
transaction's versioned hashes, decodes the vote ids, slot updates and accumulator, applies them
on its committed root, and requires the result to equal the event's `newRoot`. Queued votes whose
id is now in the tree become `settled`. Gaps are replayed from the registry logs. Every node
stores the transitions and blobs it sees and serves them under `/processes/{pid}/transitions`.

## The end and the grace window

Admission closes at the election's end time, or when the process is ended or canceled.
Settlement continues: the registry accepts transitions until the grace end,

```
graceEnd = min(end + graceMaxTotal, max(end, lastVoteAt) + grace)
```

where `lastVoteAt` is the time of the latest transition and `grace` is set per process (the
registry's `defaultGrace`, changed before the end with `setProcessGrace` within
`graceFloor..graceCeil`). Every landing in the grace pushes the end out, so a backlog drains round
by round, and `graceMaxTotal` caps the extension.

- The node is in flush mode from `--flush-horizon` before the end until the grace end, while the
  process is ready, ended, or paused past its end. A process paused before its end seals nothing.
- At the grace end the node errors whatever is still queued (`process closed`). A cancel closes
  out at once.
- A settlement that reverts `InvalidStatus` or `InvalidTimeBounds` while the window is open puts
  its votes back to pending.
- The node reads the grace parameters from the registry at boot, and re-reads the process after
  every landing to learn `lastVoteAt`.

The organizer can shorten a running election with `setProcessDuration`, to no less than
`noticeMin` from now. Nodes see the new end within a poll or two, flush if it is within the
horizon, and refuse votes from the new end on.

The grace lets a sequencer include votes cast after the end, as long as it received them before:
nodes refuse late votes, but nothing on-chain can tell a late vote from a delayed one. The
exposure is bounded by the window and visible, since every grace landing is a transition with its
time on-chain.

## Results

Results unlock at the grace end: before it the registry reverts the results calls with
`GraceOpen`. A node finalizes once the window has closed, no batch is in flight and its tree is at
the on-chain root. Other nodes pick the results up from `ProcessResultsSet`.

For a sequencer-key process, only the node holding the election key finalizes. It:

1. decrypts the accumulator (baby-step giant-step, bounded by voters times the ballot's maximum
   value);
2. builds the 16 Chaum–Pedersen decryption proofs and the inclusion proofs of the key and
   results leaves;
3. proves them with the prover's results program and checks the published root, tally and
   program key;
4. calls `setProcessResults`.

With `--eager-results` (the default) steps 1 to 3 run during the grace window, once nothing is
queued or in flight. The node submits the proof on the first heartbeat after the grace end if the
root has not moved, and proves again otherwise.

For a DKG-key process, see below.

## Key modes

Every process has one of three key modes, chosen at creation. The organizer talks only to the
`ProcessRegistry`; the DKG sits behind it, and the circuits are the same in every mode.

| Mode | Election key | Who publishes the results |
|---|---|---|
| Sequencer (`KeyMode::Sequencer`) | Derived by the node that answered `POST /processes/keys`. | That node, with a results proof. |
| DKG automatic (`KeyMode::DkgAutomatic`) | A davinci-dkg committee key. | Any signing node, after the committee decrypts. |
| DKG locked (`KeyMode::DkgLocked`) | The committee key plus an organizer key. | Any signing node, after the organizer reveals its secret and the committee decrypts. |

A sequencer key trusts one node with ballot secrecy: it holds a key that opens every ballot in the
blobs, and it alone can publish the results. It suits testing and users who accept that trust.

In the DKG modes no sequencer and no organizer holds the secret; each committee member holds a
share. The committee never reconstructs the election secret, which would open every ballot. It
threshold-decrypts only the final accumulator, one ciphertext per ballot field. In locked mode the
key is `P_j + PK_org`, where `sk_org` is returned to the organizer at creation
(`CreatedProcess::organizer_secret`) and never stored by the registry. The committee's partial
decryptions only start after the organizer calls `revealProcessKey`, so the organizer decides
when the tally appears, but not which one. Losing `sk_org` loses the results.

### How a DKG process gets its key

The registry deploys a `DavinciDKGAdapter` in its constructor, which registers with the DKG and is
the only address allowed to submit ciphertexts for it. A registry deployed without a DKG manager
has no adapter, and the DKG modes are disabled (`Error::DkgDisabled` in the client).

Each process gets an application id `aid = keccak256(chainid ‖ registry ‖ pid) mod Q`. Automatic
mode takes a free key from the newest live epoch's pool. Locked mode names its epoch, because the
organizer's proof of possession of `sk_org` binds it; `adapter.registrationEpoch()` tells clients
which epoch to use. If the pool empties or a new epoch goes live between that read and the
transaction, `newProcess` reverts and the client retries once.

### DKG results

After the grace end, any signing node sends `requestResultsDecryption(pid, accumulator,
siblings)` from its committed tree. The registry checks the accumulator's inclusion under the
latest state root, submits each active field's ciphertext to the committee, and moves the process
to ended, which only the registry can leave. That matters because the decrypted values are public
on the DKG before they reach the registry: a process left ready could be canceled by an organizer
who disliked the tally. The node then waits until every ciphertext is decrypted and calls
`finalizeResultsFromDKG`, which stores the results.

Every signing node reaches both calls at about the same time, so each waits a random moment (up
to 10 s) and sends only if a fresh read still lacks the call. A node that loses the race anyway
takes the winner's call as its own. A field whose ciphertext is the identity (a process with no
votes) decrypts to 0 under any key, and the registry records 0 without asking the DKG.

### Client example

```rust
let next = org.next_process_id().await?;
let created = org.create_process(&NewProcess {
    process_id: next,
    metadata: uri,                            // where the metadata document is served
    metadata_hash: metadata_hash(&document),  // SHA-256 of its exact bytes
    key_mode: KeyMode::DkgLocked,
    ..
}).await?;
let sk = created.organizer_secret.unwrap();   // keep it
// ... votes, end ...
org.reveal_process_key(&created.pid, &sk).await?;
```

A wrong secret reverts with `InvalidOrganizerSecret`.

## Restarts

The node persists process records, votes and the queue, each process's state tree and committed
state, transitions and blobs, the census data, the per-process exposed slot set (see
[security.md](security.md)) and the master secret election keys are derived from.

The per-batch seed and refresh selection exist only in memory for one batch, by design. On
restart, votes that were in a batch (`aggregated` or `processed`) go back to `pending` and the
node syncs from the chain. If its own transaction landed before the crash, those votes settle
through that sync without being proved again.
