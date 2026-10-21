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

Merkle censuses (origins 1 and 2) are downloaded off the event loop, so a slow census server does
not stall routing, and their root is checked. The process waits on the retry list until its census
is stored, and a failed download costs one attempt. At most 4 downloads and 2 builds run at once;
a download that brings less than 16 KiB in a 30 s window after its first byte is cut off and
retried. For an on-chain census (origin 3) the node indexes the census contract and serves votes
after the first sync. A process that fails a permanent check is marked `ignored` with the reason.
Transient failures are retried from a persisted list, and a process with no stored record is
ignored after 10 attempts. A process with a stored record (its actor failed to start, say after a
crash) is never ignored: it keeps retrying at the longest backoff (32 poll intervals) and logs an
error each time. A late bootstrap scans the registry logs from the process's creation block and replays
every transition from its blobs.

## Admission

API handlers first run the stateless checks on a bounded blocking pool: the ballot proof, against
the 71 public inputs the circuit derives from the vote and the process, the signature and the
census proof. These repeat every per-vote check the circuit makes (`davinci_state::validate_vote`),
so one bad ballot cannot sink a batch. The vote then goes
to the process actor, the single writer of that process's state, which checks that:

- the vote id is not in the tree or the queue;
- the slot holds fewer than `--slot-depth` queued votes;
- max voters is not exceeded;
- the process is accepting votes.

## Batching

The actor seals a batch when one of these holds:

| Trigger | Condition |
|---|---|
| capacity | The queue no longer fits one transaction (a vote kept out only by its own coverage history does not count), or `--batch-max` votes are pending. |
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
to the registry's floor. In L1-blob mode the Ethereum leg (`--l1-settle-lag`) runs during the
proof, so it plus `--settle-margin` must fit before that deadline as well, plus 24 s for each blob
transaction after the first (they go out one after another): a short window takes a batch of
fewer transactions. The node selects the batch once per transaction count the window allows and
keeps the largest (the fewest transactions on a tie), so a vote that needs more transactions than
the window fits waits without holding up the votes behind it, other than later votes of its own
slot. When not even one vote fits, nothing seals and the votes wait.

To build a batch, the actor:

1. takes votes in arrival order, the oldest per slot, within `maxVoters`, the size above and the
   blob limit, counting the refreshes the circuit will require: the chain's per-transaction limit
   in blob mode, the circuit's 32 blobs in L1-blob mode (at most the transactions the time left
   allows). A vote is charged the coverage history of the votes the batch carries (its own and
   theirs), never that of votes left out; one that does not fit waits for a later batch (later
   votes of its slot wait behind it), and one whose own history fits no transition ends in
   `error` alone (`exposed refresh set too large, recast`);
2. draws a fresh seed and a refresh selection from the OS random number generator;
3. re-encrypts every ballot along the seed's scalar chain and re-randomizes a sample of other
   occupied slots, so an observer cannot tell a revote from a routine refresh; every batch that
   ever carried one of its votes (the node's earlier attempts, another sequencer's batches) is
   refreshed again in full when that vote is resealed (see [security.md](security.md));
4. builds the state-tree witnesses, the blobs with their KZG commitments and openings, and the
   public outputs it expects.

## Settlement

A background job proves the batch. The node accepts the proof only when the circuit reports
success, every public output equals the node's own value, and the program verification key and
`rootCVadcopFinal` equal the release pins. It then simulates `submitStateTransition` and sends the
blob transaction. On OP Mainnet (the `optimism` preset) the registry is in L1-blob mode and the
blobs go to Ethereum instead, see [L1-blob mode](#l1-blob-mode-op-stack). A node keeps at most
one batch in flight per process; processes run in parallel.

Transient prover errors back off (up to about 5 minutes) with the votes still pending, and end
in `error` only if the election closes first. So does a node fault while the batch is built (a
failed write to the state tree): its votes are never errored for it. `nextSealNotBefore` shows
the backoff. Other failures:

- A job the prover fails for good (a `failed` job, a refused input, outputs it cannot decode)
  puts the votes back in the queue and retries the batch once at full size. Each further failure
  halves the next batch, until a single vote fails: that vote alone ends in `error`, with the
  prover's reason and `(isolated by bisection)`. Retries back off up to 30 s. After three such
  votes in a row the node takes the prover to fail every input: it stops erroring and keeps
  retrying full batches; votes still held at the close end in `process closed (held: prover:
  <reason>)`. A landing (the node's own or a synced one) ends the bisection once its
  votes are settled, and so do the close and a restart (the state is in memory). The count and
  the hold end only at the node's own landing or a restart: another sequencer's landing says
  nothing about this node's prover. While it holds, a batch carries only the held votes, so
  their re-attempts do not grow with votes cast meanwhile (see [security.md](security.md)).
- A proof the node's own checks refuse (public outputs, program key or `rootCVadcopFinal` other
  than the pins), and in L1-blob mode an Ethereum proof that keeps carrying other blobs for 60 s
  (a single one is an inconsistent endpoint read, rebuilt each poll), would fail the same way for
  every batch: the votes stay pending under the usual backoff, never bisected or
  errored, and the retries carry only the held votes. Votes still held at the close end in
  `process closed (held: <reason>)`.
- A circuit failure (the guest's fail mask) errors the votes at once. Admission repeats every
  per-vote check, so it means a node bug.

In L1-blob mode the retries of a bisection are proof-first: the blobs go out only once the proof
passed its checks, so a failing retry posts nothing and is not a failed cycle (only the first
failure, which posted, is). Such a batch is sized for the proof and the Ethereum leg one after
the other: each transaction count leaves less time to prove, and the count that carries the most
votes wins. Each case logs an error and counts in `GET /info` (`bisections`, `isolatedVotes`,
`heldBatches`, `guestRejections`).

## Races and sync

Settlement is permissionless. When another sequencer lands first, the node's simulation or
transaction reverts with `InvalidStateRoot`. The node then rolls its tree back, counts a lost
race, puts the votes back in the queue, syncs, and builds the next batch on the new root. A lost
race costs only the gas of the reverted transaction. A transition that lands while the batch is
still proving aborts it the same way, and the node cancels its prover job with a best-effort
`DELETE /jobs/{id}` (a prover without the route answers 405, ignored), so the next batch does not
queue behind it.

For a transition another node sent, the node fetches the blobs, matches them against the
transaction's versioned hashes, decodes the vote ids and slot updates, recomputes the Results
accumulator from the ballots it already holds (the blobs do not carry it), applies them on its
committed root, and requires the result to equal the event's `newRoot`. Every vote of the
batch the node stored becomes `settled`, queued or not (one it errored may still land through
another sequencer). A queued vote cast before one of its voter's votes the batch settled ends in
`error` (`superseded by a later vote`): sealed now, it would overwrite the later one. Gaps are
replayed from the registry logs. Every node
stores the transitions and blobs it sees and serves them under `/processes/{pid}/transitions`.

### Reorgs

Each poll the monitor reads the hash of the block at its event cursor and compares it with the
hash it stored with the cursor. A different hash is a reorg: the node reads the number of the
`finalized` block, logs an error and routes the range above it again, however deep the reorg was:

- the finalized block is below the cursor: the cursor goes back to it (never below the start
  block), since nothing at or below a finalized block can change;
- the finalized block is at or past the cursor: the stored hash was never on the finalized chain
  (an RPC fault, a datadir run against another chain, or a node down across a reorg that lasted
  longer than finality). The cursor goes back to the block before `--start-block` and every event
  is routed again, one saved page at a time, so a restart resumes where it stopped;
- the endpoint refuses the `finalized` tag (a JSON-RPC error for it): the cursor goes back a fixed
  128 blocks, with a warning the first time;
- the read fails (a transport error, a node-side error, no block): the cursor stays and the check
  runs again at the next poll.

The rewound cursor is stored with its own block hash in one write; if that hash cannot be read,
the cursor stays and the check runs again at the next poll, so there is never a window without
one. Either rewind counts as a reorg. Routing an event twice changes nothing, and a
transition the new branch re-included in a later block is recognised by its old and new roots.
After 5 failed block reads in a row (a block the endpoint does not know yet counts as failed) the
monitor routes on without the check (each such page counts in `anchor_stalls`), and re-arms it at
the first read that succeeds.

**The anchor.** The cursor block with its hash and timestamp is the monitor's *anchor*: every
event up to it was routed and its hash was verified. Every Head carries it to every actor. A
reorg is a new, lower anchor (and the Head carries the monitor's reorg count, so an actor learns
of one even when the next anchor it sees is higher). Pages routed without a hash do not move it.

Every state decision of an actor comes from a routed event or from one read of its process at
the current anchor (`eth_call` at the anchor's block hash, EIP-1898 with `requireCanonical`), and
decides only what lies at or below that block. Reads by number give only time and hints, and can
only delay a step. The anchored read runs at most once per anchor, and only while something waits
on the chain (an event or landing it has not covered, a sync latch, a divergence, a suspended
confirmation, pending votes, a landed flight waiting for its event). It:

- merges every field the events move (duration, grace, `maxVoters`, census, status, results),
  with the events' side effects, unless the anchor is below the newest block an applied event or
  own landing came from; such a read merges nothing and the record waits for the next one;
- finds a divergence: the chain at a root the transition log left at or below the anchor block;
- confirms results and cancels, and undoes them (below);
- clears the "needs a check" state once it is at or above every event and landing applied and
  shows the pinned root. Until then nothing closes out and no results are sent.

A reorg deeper than `--confirmations` can take a transition the node already committed: its own
(committed at the receipt in blob mode, at the event in L1-blob mode) or one it synced. The node
is then ahead of the chain: an anchored read shows a root its transition log has passed, or an
event shows a transition leaving such a root for another one. From then on it seals nothing and
proves no results. Back on its root at or above the block the divergence was seen at, it clears.
If it does not come back within 90 s, the read at the current anchor (never an older one)
decides, and the node rebuilds the process:

1. One write drops the state tree, the transition log and the stored blobs, puts the votes the
   lost transitions settled back in the queue and pins the chain's root. The coverage history
   stays, so their re-seals refresh every slot of the lost batches that carried them.
2. The node re-creates the genesis and replays every transition the chain has from its blobs,
   as a late bootstrap does.
3. The votes re-seal on the chain's root, or settle from the replay if the chain kept them in
   another transition; once the window has closed, the close-out errors what is left.

The rebuild logs an error with its depth and leaves a note in the process record. Votes it keeps
settled are checked once the replay converges; it stores where each landed (transition index and
position) so a voter's votes of one second go back in cast order. A rebuild while an earlier one's
check is pending keeps that check and merges its order with the current log's. A marker that does not
decode (corrupt, or a format only unreleased builds wrote) holds the process: no rebuild, no
check, no seal, an error logged at each retry, until the operator removes it. There is no command
for that: stop the node and either delete the process's row from the `rebuild_check` table of the
datadir's redb file (which skips the check: a kept vote the chain lost stays settled there), or
move the database aside so the node bootstraps from the chain again (its
queued votes not yet on chain are lost).

**Confirmations.** Results and a cancel are confirmed at the block of their event, or at the
anchor of a read that shows them. A later anchored read at or above that block that lacks them
undoes them: the results go in again, a cancel reopens admission. A read below that block (the
anchor rewound) suspends them instead: close-out and results wait, and the record keeps them,
until a read at or above that block decides.

**Close-out** errors the pending votes only at the current anchor's read, with nothing applied
above it, and when the anchor block's own timestamp is past the record's grace end (a later block
can neither land a transition nor extend the window), or when the process is canceled or has
results.

**Reads by hash failing.** If reads by hash fail for 90 s (an endpoint without EIP-1898 calls),
the node logs a warning, counts it, and decides nothing from the chain until they answer again:
sealing on events goes on, close-out and results wait. With `--degraded-number-reads` (for an
endpoint with a single backend only: behind a balancer a read by number can come from another
branch) it reads instead at the anchor's block number, between two checks that the endpoint's
block there has the anchor's hash, probes by hash again every 10 heads, and leaves degraded mode
at the first read that succeeds.

Rulings where the design left a choice (for review):

- The anchor lives in memory and is stored as the cursor and its hash; at boot it is loaded with
  time 0 (no close-out until the first poll re-reads its timestamp). A page routed without a hash
  still stores the cursor without one, as before, but does not move the anchor.
- A failed anchored read retries after 2 s at the same anchor. A read made is never asked again
  at that anchor (a block's state at its hash does not change): when something here moves with no
  new anchor (the roots, the floor, the sync latch, the check flag), that read is applied again,
  once the anchor's block hash still reads. Close-out and rebuild check that hash too right before they act, and a sync latch clears
  only on a read made after it was set.
- Field events routed again after a rewind are skipped at or below the newest (block, log index)
  the fields came from: replaying them would rewind the record transiently (an older cap erroring
  new-slot votes). The anchored read at or above the newest event merges the branch's values.
- A lost race needs an anchored read at or above the reverted transaction's block (or the head,
  for a refused simulation) before its sync latch can clear, and so does the boot hint at its
  head block; only a block the endpoint can return raises that floor, so a bogus `latest` holds
  nothing.
- The results and DKG tasks read the process at the anchor current when they start. Before a send
  they also read the head block by its hash: results or a request another node landed there only
  skip the send (the anchored read records them later).
- At boot one read by number at the head may latch the sync (a transition whose event was lost)
  so the node does not seal on a root the chain left; only an event or an anchored read clears it.
- In L1-blob mode a landed flight whose event is late waits while the anchored read shows its
  root, or until the anchor passes the block where one read by number showed it; then it rolls
  back.
- A stored finalized or canceled record counts as confirmed at block 0 after a restart.
- A depth-0 cancel no longer closes admission before it is confirmed; votes admitted meanwhile
  are errored by the close-out once it is.
- Every anchored decision logs a debug line (target `davinci_sequencer::decision`) with the
  anchor's block and hash; the chaos harness checks each was canonical and read by hash. An
  endpoint that lags behind a reorg serves the monitor and the reads the same old branch: the node
  decides at that branch's anchor until the endpoint catches up (nothing the node can check).
- The anchored-read floor (the newest block an applied event or own landing came from, and the
  block of the snapshot the record's fields came from, at least the stored cursor after a restart)
  only grows, rebuilds included: a read below it merges nothing and undoes nothing, which after a reorg to a shorter branch can hold close-out and results until the new
  branch passes the old one's newest event.

## L1-blob mode (OP Stack)

A registry deployed with an `L1BlobVerifier` settles on an OP Stack chain while its blobs live in
Ethereum blob transactions. The node reads `l1BlobVerifier()` at boot and refuses to start when
its configuration disagrees: `--l1-rpc-url` set for a blob-mode registry, or missing for an
L1-mode one. A signing node also needs its dedicated Ethereum key.

Each batch:

1. Seals and builds its blobs as in blob mode.
2. Posts the blobs on Ethereum while it is proved, in type-3 transactions from the node's
   Ethereum account to itself (EIP-7594 sidecar): blobs 0-5 in the first, 6-11 in the second and
   so on, at Ethereum's per-transaction limit (six). A 512-vote batch at 16 fields takes 9 blobs
   in two transactions, 13 in three when every vote is a revote. The account sends one
   transaction at a time; the next waits until the one before is mined. The tip is at least
   0.001 gwei (0.05 gwei when the endpoint fails `eth_maxPriorityFeePerGas`), the max fee twice
   the base fee plus the tip, the blob fee twice the blob base fee.
   A transaction not mined after 3 Ethereum blocks is replaced at its nonce with every fee
   doubled. A post whose worst-case cost would pass `--l1-max-post-fee` (or that L1 refuses) is
   retried while the proof runs, waiting for cheaper fees, up to the grace end: over the cap after
   one poll interval, doubling to once a minute; after an L1 error, every poll interval. Before
   posting, the node prices the smallest OP settlement (800k gas plus 20 %) at OP's current fee:
   if `--op-max-tx-fee` would withhold it, nothing is posted, and once the proof is ready the votes
   go back to the queue, as for a withheld send. A fee that falls during the proof lets the blobs
   go out then.
3. If OP's L1 origin (`L1Block.number()`) reaches the block of the last transaction before the
   proof is ready, the node calls `record()` on the verifier once, which stores that origin on OP.
4. With the proof ready, waits until every blob transaction has one Ethereum block on top and
   OP's origin reached the newest, then builds one L1 proof of all of them, in blob order:
   Ethereum headers from at most two blocks above the origin down to the oldest blob block while
   that is at most 16 headers; else from a recorded origin at or above the newest blob block
   down; else up to 64 headers from the current origin. Deeper than that, or 7,200 blocks behind
   the origin, it posts every transaction again.
5. Checks the proof locally, simulates `submitStateTransitionL1` on OP and sends it (gas estimate
   plus 20 %). `L1OriginMismatch` (the origin moved past the proof, or the node's Ethereum RPC is
   behind OP's origin) rebuilds the proof from fresh reads, at most 5 times, then the votes go back
   to the queue.
6. A blob transaction that disappears (dropped, or reorged out of Ethereum) is posted again
   alone, at the account's next nonce, so it may land after the transactions that follow it. A
   missing receipt counts as gone only after 36 s, since a lagging endpoint reads the same. A
   proof that cannot be built (Ethereum RPC trouble) or anchored for 60 s also posts every
   transaction again. A proof that fails for 60 s in a way no post fixes (a header the node cannot
   re-encode to its hash, headers that do not chain, a bad trie proof) puts the votes back in the
   queue without posting. Re-posts (one transaction or all) are capped at 3 per batch; then the
   votes go back to the queue.
7. Commits when its `ProcessStateTransitioned` event reaches `--confirmations`, not at the receipt:
   OP's unsafe blocks reorg with Ethereum. If the event is still missing after 90 s, the node
   looks at the registry: while it still shows the settlement's root the node keeps waiting (no
   needless re-prove); once the root is gone the settlement is rolled back and its votes
   requeued, like a lost race.

The node bounds the spend of cycles that keep failing. A failed cycle posted at least one blob
transaction but did not land: the settlement gave up (any of the give-ups above, an OP send failure,
the deadline), it reverted with an error the node does not handle, or the batch failed after a post.
After a failed cycle the next attempts are proof-first until the node's own landing, so a prover
that keeps failing spends no further post. After three in a row the node pauses sealing for that process for 10 minutes, and each further
failed cycle doubles the pause, up to an hour. Each pause logs an error with its length, the count
and the last failure; the votes stay pending (the grace end errors them as usual) and
`nextSealNotBefore` shows the pause end. The node's own landing resets the count; another
sequencer's ends the pause but takes only one off the count, so a node that keeps failing still
pauses when others land often. A restart resets both.

The registry emits `ProcessStateTransitionedL1(processId, l1BlockNumbers, versionedHashes)` right
after `ProcessStateTransitioned`; the node merges the two and fetches the blobs from Ethereum:
beacon APIs by slot (`--blob-source beacon:`, Ethereum beacons in this mode), or the L1 anvil.
After PeerDAS only beacon nodes that keep full blobs (supernodes, archives) serve them, so list
several. Each node stores and serves the blobs under `/processes/{pid}/transitions/{index}/blobs`
as in blob mode. A lost race wastes the loser's blob transaction (a few cents at most).

## The end and the grace window

Admission closes at the election's end time (by the latest head or the node's own clock, whichever
is later), or when the process is ended or canceled.
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
- At the grace end the node errors whatever is still queued (`process closed`), once its tree is
  at the on-chain root, nothing is in flight, and the read at the monitor's anchor (see
  [Reorgs](#reorgs)) shows the window closed at the anchor block's own time. A landing the events
  have not shown yet moves the grace end in that read, and a vote another sequencer settled
  settles from the sync instead of erroring. A cancel or the results close out the same way,
  without waiting for the grace end; both are known only from their event or an anchored read, so
  a cancel at the newest block, not yet confirmed, closes nothing and does not close admission.
  One the chain loses again reopens the process at the next anchored read at or above it
  (admission and sealing resume, queued votes stay).
- A settlement that reverts `InvalidStatus` or `InvalidTimeBounds` while the window is open puts
  its votes back to pending, and so does `MaxVotersReached` (the organizer lowered `maxVoters`):
  only the votes for a new slot that the new cap can never fit end in `error`.
- The node reads the grace parameters from the registry at boot, and reads the process at the
  anchor after every landing to learn `lastVoteAt`.

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

A results receipt may be shallow enough to reorg out, so the node marks the process finalized only
at its `ProcessResultsSet` event (or a read showing it), in every key mode. If 90 s pass without
it, the node reads the process: results there finalize it; otherwise it logs a warning and submits
again, reusing the held proof when it has one. A finalized process whose results a deeper reorg
dropped is active again at the next read that shows them gone, and the results go in again.

An attempt that fails for good (a failed proving job, a proof that fails the checks, a revert the
node does not expect) is retried, never abandoned: after 2 s, then 4 s, and from the third failure
in a row every 5 minutes, each logged as an error.

For a DKG-key process, see below.

## Key modes

Every process has one of four key modes, chosen at creation. The organizer talks only to the
`ProcessRegistry`; the DKG sits behind it, and the circuits are the same in every mode.

| Mode | Election key | Who publishes the results |
|---|---|---|
| Sequencer (`KeyMode::Sequencer`) | Derived by the node that answered `POST /processes/keys`. | That node, with a results proof. |
| DKG automatic (`KeyMode::DkgAutomatic`) | A davinci-dkg committee key. | Any signing node, after the committee decrypts. |
| DKG locked (`KeyMode::DkgLocked`) | The committee key plus an organizer key. | Any signing node, after the organizer reveals its secret and the committee decrypts. |
| Council (`KeyMode::Council`) | The key of a Council ceremony (an invite-only committee), named by its id. | Any signing node, after the ceremony's members decrypt. |

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

Each process gets an application id `aid = salt << 160 | adapter`, where `salt` is the top 92 bits
of `keccak256(chainid ‖ registry ‖ pid)` (`adapter.aidFor(pid)`). The DKG registers an id only for
the account in its low 160 bits, so nobody but the adapter can take a process's id. Automatic
mode takes a free key from the newest live epoch's pool. Locked mode names its epoch, because the
organizer's proof of possession of `sk_org` binds it; `adapter.registrationEpoch()` tells clients
which epoch to use. If the pool empties or a new epoch goes live between that read and the
transaction, `newProcess` reverts and the client retries once.

### How a Council process gets its key

A registry deployed with a Council manager also creates a `CouncilAdapter`
(`councilAdapter()`); without one, `newProcess` in this mode reverts `CouncilDisabled`. The
process names a Live ceremony (`KeyMode::Council(ceremony_id)` in the client), whose organizer
has allowed that adapter and authorized the creating account; the manager refuses anything else
(`NotAllowedAdapter`, `NotAuthorizedCreator`, `WrongPhase`, `UnknownCeremony`) and the client
does not retry. The registry stores the ceremony id as `dkgEpochId` and the request id the process
decrypts under as `dkgAid`. Every process bound to a ceremony shares its key.

### DKG results

After the grace end, any signing node sends `requestResultsDecryption(pid, accumulator,
siblings)` from its committed tree, the siblings zero-padded to 65 entries. The registry checks the accumulator's inclusion under the
latest state root, submits each active field's ciphertext to the committee, and moves the process
to ended, which only the registry can leave. That matters because the decrypted values are public
on the DKG before they reach the registry: a process left ready could be canceled by an organizer
who disliked the tally. The node then waits until every ciphertext is decrypted and calls
`finalizeResultsFromDKG`, which stores the results. A Council process takes the same path: the
registry submits its active fields as one request through the Council adapter (first index 0),
and the node asks that adapter, not the DKG one, whether the whole request is combined.

Every signing node reaches both calls at about the same time, so each waits a random moment (up
to 10 s) and sends only if a fresh read still lacks the call. A node that loses the race anyway
takes the winner's call as its own. A field whose ciphertext is the identity (a process with no
votes) decrypts to 0 under any key, and the registry records 0 without asking the DKG.

A Council ceremony also fixes when decryption may open: on a scheduled date, or when its organizer
opens it, optionally with a fallback date. The gate can open months after the vote ends, and a
date opens it without any transaction. Until it opens the registry publishes no result of a
Council process, not even all zeros: the request is still accepted and ends the process, but an
all-identity tally stays ended instead of finalizing, and `finalizeResultsFromDKG` reverts
`DecryptionNotOpen`. The node treats a closed gate as pending, never as a failure: after the
request it reads the gate through the Council adapter (`isDecryptionOpen`), polling 15 s at
first and doubling up to every 5 minutes, and a `DecryptionNotOpen` revert counts the same. Once
the gate reads open it publishes the zeros, or waits for the committee's combines at the usual
15 s pace. The requested flag is persisted, so a restart resumes the wait without a new request.
The wait for the combines is logged once at info when it starts, then at warn every 10 minutes
with how many of the requested ciphertexts are combined (DKG modes; a Council request is read
whole), so a committee stuck below its threshold shows in the log.

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
state, transitions and blobs, the census data, the per-process exposed slot set and coverage
history (see [security.md](security.md)) and the master secret election keys are derived from.

The per-batch seed and refresh selection exist only in memory for one batch, by design. On
restart, votes that were in a batch (`aggregated` or `processed`) go back to `pending` and the
node syncs from the chain. If its own transaction landed before the crash, those votes settle
through that sync without being proved again. A bisection in progress is forgotten: the next
batch is full size. Those votes go back ahead of the stored queue, so a requeue whose write
failed (a batch's votes left out of the stored queue while the node holds them pending) is
written again from the node's own queue order before anything seals, and the writes that put
votes back at the head (a rebuild, its kept-vote check) rewrite the whole stored queue to the
node's order in the same commit, so no crash leaves such a vote ahead of an earlier vote its
voter cast in the same second.

A landing is two writes: the state tree with its committed record, then the votes, the
transition record and the process record together. The tree's record is authoritative, so a
crash between the two reopens at it: the batch's votes settle from the tree, the pinned root
follows the chain once a read shows the tree's root there, and the missing transition record is
rewritten from its event and blobs (which moves the pinned root too). The node seals nothing
until the record is back, so the next landing cannot take its index. The repair waits for the
blobs: until then the process seals nothing and applies no later transition, until the blob
source serves them again or the operator removes the process's state; another node that archived
the blobs keeps serving the election meanwhile. The close-out and the results still run once the
tree is at the chain's root.

A process's genesis is written leaf by leaf with its record last, so a crash in between leaves a
tree without a record: the next start writes the genesis again. A crash between a rebuild's
write and its new genesis (see [Reorgs](#reorgs)) reopens the same way, at genesis with an empty
log, and the replay goes on.

Events the monitor routed into an actor's queue die with the process, and the event cursor has
already moved past them. At boot, and whenever a landing makes the actor re-read the process,
the node therefore merges `getProcess` into its record: status, duration, grace, max voters,
census, metadata, the DKG request and results, with the same effects as their events. The read is
at the monitor's anchor (the cursor block's hash, stored with the cursor, so a backend behind or
on another branch fails it), and is not merged while it is below a block the node already acted
on. At boot one read by number at the head may also hold sealing until the sync, when it shows a
root the log does not know (a transition whose event was lost).

A canceled or finalized process keeps following the chain: a read after the cancel or the
results makes the process's final root the target, and the node replays the transitions it
missed until its tree is there, without sealing. A finalized process does not report itself
synced until a read has confirmed that root.

In L1-blob mode the node records each Ethereum blob transaction, with the sending account, before
sending it and drops the record once it is mined. After a restart the batch is proved again (its
secrets died with the process) and its first post replaces a still-pending transaction at its
nonce with doubled fees instead of queueing behind it, whichever of the batch's transactions that
was. Records of another account (a rotated L1 key) are dropped at start. Transactions mined
before the crash are wasted.

A datadir written by an earlier release (storage schema 10 or older) is refused at start: it
predates the coverage history, so the batches it sealed or synced have no entries and a later
re-seal of their votes would leave slots out. Start the node with a fresh datadir (the chain is
the source of truth), and keep the old one aside: it holds the master secret.
