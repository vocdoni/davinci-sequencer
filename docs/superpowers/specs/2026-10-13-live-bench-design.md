# Live benchmark campaign on OP Mainnet: design

Status: draft for the owner's review, 2026-10-13, revised after the pre-flight review
(`live-bench-preflight.md`). This is Track B of the campaign started by Ruling 19
(`.superpowers/sdd/2026-10-13-op-core-stack/progress.md`). It builds on `harness-survey.md` and
`sdk-survey.md` in the same directory.

## 1. Goal

Run DAVINCI on OP Mainnet (chain 10, L1-blob mode) the way real elections run: two competing
sequencers, two GPU provers, several elections at once, and every census origin the harness can
drive. The campaign measures five things:

1. The throughput ceiling: votes settled per minute, against batch size and field count, with two
   provers.
2. How the nodes recover from a race: both seal on the same root, one loses.
3. How they behave under faults: node kills, prover outages, RPC failures, fee caps, early ends.
4. That no vote is lost. Every accepted vote ends settled, or errored with an explicit reason.
5. The same flow driven through the TypeScript SDK.

Prerequisites from Track A (in progress: the ZisK 1.3.2 upgrade is under way):

- ZisK 1.3.2 provers running on both z6 and z7. Node A proves on z6's prover, node B on z7's, which
  runs a CI-built davinci-zkvm image.
- The redeployed OP registry and its new `ZiskVerifier` and `L1BlobVerifier`. None of these addresses
  is known yet, so all of them are parameters: the registry address and start block come in
  `DAVINCI_E2E_REGISTRY` and `DAVINCI_E2E_FROM_BLOCK`, and `Net::open` reads the verifiers from the
  registry and prints them; they must equal `deployments/10.json`.
- A node binary and this harness, both built against the 1.3.2 release pins. `Net::open` checks the
  pins against the registry.

The node binary must also carry Ruling 20 (§11, "How the node errors a vote"): a prover-side job
failure requeues and bisects instead of erroring the batch, node-side check mismatches hold the votes,
and a failed results proof is retried instead of latched. That change is made separately on
`feat/op-core`; no dry or live run starts on a binary without it.

## 2. Setup

```
z6 10.200.0.26 (59 GB, RTX 5090)                         z7 10.200.0.27 (128 GB, RTX 5090)
  prover :8080 (ZisK 1.3.2)                                prover :8080 (ZisK 1.3.2, CI-built image)
     ^ proxy pa                                               ^ proxy pb
  node A :9101  key seq-a (OP seq-a.op, Ethereum seq-a.l1) ---+   (prover A = z6)
  node B :9102  key seq-b (OP seq-b.op, Ethereum seq-b.l1) ---+-> (prover B = z7)
  observer O :9103 (no key: ground truth, blob archive)
  davinci-bench: rig + submitter + ledger + audit + report (one process per step)
  ballot generation: z6 and z7 CPUs, merged on z6
```

- A and B compete on the same elections. Each election's key holder alternates between A and B, so
  both nodes publish results.
- Nothing else may use seq-a or seq-b during the campaign. Ruling 19 holds the production OP node
  until after the campaign. Node A signs with the production keys, and two signers on one key race
  nonces, so `gate` checks it (§13): no `davinci-sequencer` process or container other than the rig's,
  and no sequencer account with a pending nonce above its latest. The organizer is the owner's key
  (`~/optimism-privkey.txt`).
- z6 also runs the production prover container, which must be the 1.3.2 build. z7's prover runs a
  CI-built davinci-zkvm image; the owner allowed changes to z7's compose file and `.env`.
- `ZISK_MINIMAL_MEMORY_FROM` is set to 256 on both hosts (revised Ruling 16). The 2048 that z6 had
  ran large nf-16 batches without `--minimal-memory` and OOM-killed them; at 256 every nf-16 batch
  above about 287 votes uses it, so 512 × 16 batches fit in 59 GB.

## 3. Architecture

| Component | File | Role |
|---|---|---|
| CLI | `e2e/src/bin/davinci-bench.rs` | Steps: `keygen`, `fund`, `gate`, `plan`, `gen`, `merge`, `run`, `audit`, `serve`, `cancel`, `dry` |
| plan | `e2e/src/bench/plan.rs` | Campaign seed and derivations (voter key, `k`, choices, CSP key); `Plan`/`Wave`/`Election`; process snapshot |
| ballots | `e2e/src/bench/ballots.rs` | Shard file format, shard list, host assignment |
| gen, merge | `e2e/src/bench/gen.rs`, `merge.rs` | Prove shards on one host; verify and merge on z6 |
| rig | `e2e/src/bench/rig.rs` | Nodes A, B, O on fixed ports with persistent datadirs, a loopback proxy per upstream, kill/restart |
| schedule | `e2e/src/bench/schedule.rs` | Pure arrival times: rate, Poisson, bursts, revote gaps |
| submit, ledger | `e2e/src/bench/submit.rs`, `ledger.rs` | Routing, retries, append-only vote ledger |
| chaos | `e2e/src/bench/chaos.rs` | Triggers and actions on nodes and proxies |
| scenarios, run, race | `e2e/src/bench/scenarios.rs`, `run.rs`, `race.rs` | Wave definitions; one scenario from gate to audit; forced race rounds |
| chain view | `e2e/src/bench/chain_view.rs` | Transitions with decoded vote ids and timestamps, prover jobs, L1 posts |
| audit | `e2e/src/bench/audit.rs` | Vote classification, tally and counters from chain order |
| metrics, report | `e2e/src/bench/metrics.rs`, `report.rs` | Log events, rates, percentiles, progress lines, JSON and Markdown |
| money | `e2e/src/bench/money.rs` | Fee gate, budget stop rule, seq-b keygen, capped funding |
| changed | `e2e/src/proxy.rs`, `node.rs`, `lib.rs`, `Cargo.toml` | Proxy delay and fail modes and an exact-path upstream, fixed-port start, `min_mix`, the `bench` module and the binary |
| TS bench | `~/davinci-sdk/examples/bench/` | SDK script: create, prove in workers, submit, follow, results, report |

Reused as they are: `Net` (live/anvil, pins, keys, balances), `Net::node_config`, `Node`,
`Proxy::tapped`, `fixture` (census file, metadata, Merkle witness, `prove_all_on`), `census::Census`
(origin 3), `l1::posts`, `cost`, `wait::until`, `redact_urls`, and `decode_blobs` from
`davinci_zkvm_sdk::blob`.

## 4. Steps, waves, files

The campaign directory `~/.cache/davinci-bench/<campaign>/` has mode 0700 and lives on z6:

```
plan.json        public: chain, registry, start block, waves, elections (+ process snapshots)
pins.json        public: release pins for the TS script
secret/seed      32 random bytes, hex, 0600 (dir 0700)
census/          census and metadata files (the nodes' --census-dir, file:// URIs)
nodes/{a,b,o}-data/, nodes/{a,b,o}.log
shards/w<N>/<tag>/r<round>-<from>-<to>.jsonl (+ .meta.json)
ballots/w<N>/<tag>.jsonl, ballots/w<N>/index.json
runs/w<N>-<scenario>/ledger.jsonl, progress.log, report.json, report.md
report.json, report.md   campaign roll-up, rewritten after every scenario
```

Each wave runs `gate → plan → gen (z6 ∥ z7) → merge → run`; the audit is part of `run`. There are
three waves:

1. Realistic elections and the throughput ceiling.
2. Races.
3. Chaos.

`plan` and `gen` also take a list of waves (`--wave 1,2,3`). One `plan` and one `gen` session for
all three waves before wave 1 runs meets the owner's "generate before starting" literally and saves
two gen/merge/scan rounds (the elections live 72 h; generation takes about 70 min in all). The cost
is that wave 2 and 3 ballots are spent if wave 1 findings change those scenarios. The owner picks at
checkpoint 1; `plan --wave N` is additive, so both work.

A failed scenario is rerun in a new wave, because its ballots are spent. `dry` runs plan, gen, merge
and run for one scaled-down scenario on two anvils (`DAVINCI_E2E_DA=l1-blobs`,
`DAVINCI_E2E_GRACE=300,240,600,1800,60`) against the real provers. Every scenario goes through
`dry` before it runs live.

## 5. Ballots

**Derivation.** Everything comes from the campaign seed with domain-separated SHA-256:

- the voter key from `(tag, voter)`;
- `k` from `(tag, voter, round)`, then `random_k`;
- the choices from `(tag, voter, round, nf)`: 1 to 6 units spread over the fields, each field at most
  5, so they fit `ballot_mode_nf(nf)` with weight 10;
- whether a voter revotes, from `(tag, voter)` and a per-mille rate;
- the CSP key from `(wave)`.

Round 1 is every voter; rounds 2 and up are the revoters only. The seed is secret: anyone holding it
could vote in the benchmark's elections. A ballot binds the pid, the election key, the voter, the
weight and `k`. So the elections must exist before generation. Their keys come from A's and B's
datadirs, which persist for the whole campaign (a lost datadir loses that node's results).
`plan.json` embeds a snapshot of each process (`pid`, encryption key, ballot mode, census origin and
root), so `gen` needs no RPC.

**Shard format.** A shard is a JSONL file with one `BallotRecord` per line:

```json
{"tag":"R1","voter":17,"round":1,"choices":[2,0,1,3],"req":{ VoteRequest, camelCase as POST /votes }}
```

The sidecar `<shard>.meta.json` records `{wave, tag, round, from, to, count, sha256 (of the jsonl), host,
binSha256, wasmSha256, zkeySha256, secs}`. Shards hold no voter keys and no `k`. A ballot takes about
6 KB, so wave 1 (about 44k ballots) is about 265 MB.

Shards are 500 voters each, listed in a fixed order by `(tag, round, from)`. Host `i` of `H` takes
shard `k` when `k % H == i`; z6 and z7 have the same CPU. A shard counts as done once its meta exists
and its sha256 matches, so a restarted `gen` skips it. Readers hash the file and compare it to the
sidecar before parsing a line, so a damaged shard fails on its sha256, not on its JSON.

**Flow across the two hosts.**

1. `plan` runs on z6. It needs the network and the organizer.
2. Build the binary once on z6 from the committed tree, then `scp -p` it to z7
   (`~/davinci-bench/bin/davinci-bench-<sha>`) along with the circom artifacts, `plan.json` and the
   seed file (`scp -p` keeps the seed's 0600; the run also sets it with `chmod 600` on z7). z7's
   checkout state does not matter. Both hosts run Ubuntu 26.04; check `ldd` and compare sha256 on
   both hosts. Building on z7 from a pushed branch is the fallback. It is not planned,
   because it needs a davinci-zkvm sibling at the same commit.
3. Run `gen --host 0 --hosts 2` on z6 and `--host 1` on z7, each as a `systemd-run --user` service with
   `MemoryMax=16G`, 8 threads, and no secrets in its arguments.
4. `rsync -a` z7's shards back to z6, then delete the seed on z7 and run the secret scan there.
5. `merge` on z6 checks that every planned `(tag, voter, round)` appears exactly once. It also checks
   that each sidecar's sha256 matches its shard, that no vote id repeats, and the following for each
   ballot:
   - the Groth16 proof verifies with the release ballot vk;
   - the signature recovers the derived voter address;
   - the pid, weight and choices match the plan;
   - the snapshot equals the registry (live only).

   It then writes one file per election in `(round, voter)` order, plus `index.json`.

Expected rate: about 6.3 proofs/s per host at 8 threads, so about 12.5/s for both hosts and about 60
minutes for wave 1.

## 6. The two-node rig

- **Nodes.** The rig starts the real binary (`DAVINCI_SEQUENCER_BIN`) as A, B and O on
  `127.0.0.1:9101/9102/9103`. Their datadirs are under `nodes/`, and the census dir is `census/`.
- **Lifetimes.**
  - Nodes run for one step and are dropped at its end (SIGINT/SIGTERM is handled). The datadirs
    outlive the step.
  - A node started later bootstraps every campaign election from the registry logs.
  - `serve` keeps the rig up until SIGTERM, for external drivers such as the TS bench and manual
    checks. No long-lived rig with a control API is needed.
- **Proxies.** Every upstream URL gets its own loopback `Proxy`, per node: the provers (tapped on
  `/prove` for job ids), every OP RPC, the Ethereum read and send RPCs, and the beacons. Node
  environments and logs only ever contain `127.0.0.1` URLs. Upstream URLs, including the private
  `$OP_RPC`, live only in the rig's memory. Chaos acts on these proxies. The proxy forwards `/` to the
  upstream URL exactly, because private RPC URLs carry a path.
- **Knobs per scenario.** The rig restarts a node when its knobs change. A restart re-seeds the
  node's proving estimate (it lives in memory), so the throughput scenarios avoid knob changes
  (§8 S). The knobs are `batch_max`,
  `batch_time`, `min_mix`, `confirmations` and `l1_max_post_fee`. The defaults are production's:
  512, `15m`, 2, the preset's confirmations, and the harness's live cap of `0.00015eth`. The L1 timing
  defaults are those of `Net::node_config` live.
- **Host.** Each step runs in
  `systemd-run --user --scope --unit davinci-bench-<step> -p MemoryMax=12G`. z6's prover needs the rest
  of the RAM, so `gen` never runs on z6 during a run.

## 7. Submitter

**Routing.**

| Route | Effect |
|---|---|
| `ByVoter` | `sha256(tag, voter)` picks A or B, and all of a voter's ballots go to that node. This is what `docs/api.md` asks of clients. |
| `Only(A or B)` | Every ballot goes to one node. |
| `Both` | Each package goes to A and B at the same moment (lockstep). |
| `Cross` | Round 1 goes to one node and the revote to the other. |

**Schedule.** A schedule is a list of phases: `Rate{per_min, secs, poisson}`, `Burst{n}` and
`Idle{secs}`. It maps to round-1 offsets from a fixed seed. Later rounds go after their voter's
previous round:

- `Overwrite`: 5 to 30 s later, on the same node, while the earlier ballot is still pending.
- `Revote`: at least 15 minutes later, after the earlier ballot has settled.

Overload scenarios keep sending past the end, so the 41201 refusals are measured.

**Retries.** At most 64 requests are in flight per node.

- 429 and 42903 back off from 50 ms up to 2 s.
- 40902 (slot full) is retried.
- A 408, a 5xx or a transport error is resent. A 40901 after a resend means the first attempt got in.
  The actor checks the vote id before the slot depth, so a 40902 always means the vote is not in.
- Any other 4xx is a refusal, unless an earlier try of the same send was resent: then it is
  uncertain. The first attempt may have admitted the vote and the retry met a later condition (41201
  after the end, 40002 on a replay); only 40901 on a resend means the first attempt got in.
- A ballot still refused after 10 minutes is recorded as refused, or as uncertain if any try was
  resent.
- The fee gate can pause new sends (§13).

**Ledger.** Each send ends in one ledger line, carrying the time, pid, vote id, tag, voter, round,
node, result (`accepted`, `refused` or `uncertain`), code, tries and latency.

## 8. Scenarios

Sizes below are at scale 1; `--scale` shrinks them for `dry`. Every election ends, waits out its
grace window and publishes results; then the audit runs.

**Wave 1: realistic elections and the ceiling** (about 44k ballots)

| ID | Elections (nf, origin, voters, revoters) | Route, schedule | End, grace | Allowed errors |
|---|---|---|---|---|
| R | Four at once, starts 5 min apart: R1 2/Merkle/2000/15 %, R2 8/CSP/1500/10 %, R3 16/Merkle/1500/20 %, R4 16/on-chain/128/12 % (if feasible) | ByVoter. R1 Poisson 60/min plus a burst of 300; R2 overload, 600/min for 2.5 min, then 30/min; R3 Poisson 40/min; R4 10/min. Half the revoters overwrite, half revote. | R1 and R4: shortened end with a 60 s notice, grace 300. R2: end right after the overload, grace 600. R3: AGM end with batches in flight, grace 300. | R2 and R3: `process closed`, counted (R3: a race lost at the AGM end can leave a re-attempt that no longer fits the 300 s grace) |
| S | Three groups, nf {2, 8, 16}; per group 4 Merkle elections, 2 per node, 6 × 512 votes each, run as cells B {128, 256, 512} in ascending order **without restarting the nodes** (`batch_max 512` for the group; the submitter sends each cell as bursts of B, waiting for the previous burst to seal). Each election spends 4 bursts at B 128, 4 at B 256 and 3 at B 512 (512 + 1024 + 1536 = 3072), so every cell gets at least 6·B votes per node; `dry` scales each burst to ⌈scale·B⌉. | `Only`, `batch_time 60s` | End once all have settled, grace 300 | none |
| S-c | nf 16, 2 elections × 768 votes, as 3 bursts of 256 sent to both nodes, the next once both have sealed the previous one. It runs right after the nf-16 S group with the same knobs, so no restart. | `Both` | as S | none |

R4 runs only if the census contract builds and the nodes index it on OP (`plan --onchain-census`).
Otherwise it is skipped and the report says so.

For sanity, BENCHMARK.md gives the proving-only steady rates per prover: nf 2 at 512, 364 votes/min;
nf 16 at 256, 200 votes/min; nf 16 at 512, 187 votes/min. A process keeps one batch in flight and the
Ethereum leg takes about 110 s, so each node needs two open elections to keep its GPU busy. That is
why S puts two elections on each node.

The node sizes an open-window batch to OP's 240 s grace floor with a proving estimate seeded at
0.05 s/vote/field (187 votes at nf 16 until about eight proofs have converged it), and the
estimate is in memory, so every restart re-seeds it. The ceiling table therefore reports each
cell by its actual mean batch size and the steady rate over batches of at least 0.9·B, and the
report says when the floor, not the prover, bounded the cell.

Even converged, a 512-vote batch at nf 16 needs about 30 + 512 × 0.26 + 60 ≈ 223 s of the 240 s
floor, so it only just fits; that is the registry's floor, not a knob. The cells within a group share
the estimate (it is per nf), so the 128 and 256 cells converge it before the 512 cell. Below 512 a
burst does not reach `batch_max` and seals on the 60 s timer: because the next burst is sent at the
previous seal, it waits in the queue during the flight and seals at the landing (an L1 landing takes
longer than the timer), so the timer stays off the cell's critical path. Waiting for the landing
instead would add about 60 s to every batch below 512.

R2's overload is 1500 votes against the node's 16k pending cap: its 42903 refusals come from the
API's validation semaphore, not from the queue.

**Wave 2: races** (about 2.5k ballots). The mechanics are in §9.

| ID | Setup | Forced how | Rounds |
|---|---|---|---|
| X1 | nf 2, K = 64 fresh votes per round, both nodes `batch_max = K` | `Both`, in a burst: both nodes hit capacity together | 10 |
| X2 | as X1 | the loser's prover proxy holds `POST /prove` for 150 s | 5 with A winning, 5 with B winning |
| X3 | as X1 | as X2, plus the loser runs with `confirmations 150` (5 min behind on events), so its pre-flight reverts `InvalidStateRoot` | 5 |
| X4 | nf 16, 2 elections × 300 votes | `Both`, then AGM end: both flush on the same root inside the grace window | 1 |
| X5 | nf 8, 200 voters × 2 rounds | `Cross`: round 1 to A and its revote to B 5 s later, half of them the other way round | 1 |

Allowed errors are none, except X4: `process closed`, expected and counted. A race lost at an AGM end
at nf 16 leaves a re-attempt that must carry the whole exposed set (at least two Ethereum chunks)
inside the 300 s grace, and near the end it may no longer fit.

X3 is slow: a node at `confirmations 150` sees events 5 min late, so its own landings commit only at
the confirmed event and, after a lost race, its sync wait clears only when the sync arrives. Each X3
round takes at least 5 min; five rounds plus retries take about 45 min on both provers.

**Wave 3: chaos** (about 6k ballots). Each scenario has its own nf 8 election with background load
(Poisson 30/min, `ByVoter`); §10 has the details.

| ID | Fault | Allowed errors |
|---|---|---|
| C1 | Kill A while its Ethereum blob post is pending; restart after 30 s | none |
| C2 | Kill B right after a `batch sealed`; restart after 120 s | none |
| C3 | Kill the key holder 60 s into the grace window; restart after 180 s | none |
| C4 | Prover A down for 10 min | none |
| C5 | Prover B slow (+120 s per job) for 20 min | none |
| C6 | A's OP RPC answers 503 for 5 min, then goes down for 5 min; B's Ethereum read RPC down for 5 min; O's first beacon down for 5 min | none |
| C7 | B restarted with `--l1-max-post-fee 0.000000001eth` for 15 min, then restored. Then both capped at the end and restored 120 s into the grace window | none |
| C8 | Shortened end (notice 60 s) over a backlog of 1000; grace 600 | `process closed` |

What the node does in C4 and C7, so the run expects it rather than flags it:

- **C4.** A down prover refuses the connection, which is a transient failure, not a failed job, so
  Ruling 20's bisection does not start. But in L1-blob mode each attempt posts the batch's blobs
  before the proof and the post runs to completion when the proof fails, so three attempts cost three
  posts, count three failed cycles and pause A's sealing for 10 min. Recovery is bounded by the pause
  (about 11-12 min), not by the outage; no vote errors.
- **C7.** With a cap below any post, the post loop retries on `FeeCap` until the job deadline
  (now + 240 s while the election is open), then the batch is requeued as transient with backoff, with
  no failed cycle, and the proof runs again on every cycle: about 3-4 wasted GPU jobs in 15 min, and no
  ETH. Node follow-up, out of scope here: abort the post when the proof fails before the first chunk is
  sent.

## 9. Races

When a foreign transition lands, a loser finds out in one of two ways:

- **Event path.** The registry event arrives while its batch is in flight. The node logs
  `foreign transition beat the in-flight batch`.
- **Revert path.** Its pre-flight or settlement reverts `InvalidStateRoot`. The node logs
  `lost the settlement race, waiting for sync`.

Either way it rolls back, requeues and syncs (`synced transition from another sequencer`).

Every round is forced the same way:

1. **Same root.** Before the round, wait until A, B and O are all at the chain root with nothing
   pending (`/processes/{pid}`: `stateRoot`, `pendingVotes`), and both provers are idle: an aborted
   flight's job keeps running on its prover. The chain root is the registry's `stateRoot` for the
   process, read from OP, not any node's view; a lagging node (X3's `confirmations 150`) counts once
   its `/processes` root equals it.
2. **Same moment.** Set `batch_max = K` with `batch_time 15m`, then send K fresh votes to both nodes
   in lockstep. Both seal on the K-th vote. The capacity trigger has no jitter; the ±10 % only applies
   to the timer and solo waits. The run records the gap between the two `batch sealed reason=at_max`
   lines. The nodes poll independently (every 2 s) and admitting K votes takes time, so a round with a
   gap above 6 s, or with one node never sealing, is reported `path: None` and does not count toward
   the rounds; the run does not abort on it.
3. **A winner.** In L1-blob mode, the Ethereum leg (about 110 s, slot-random) is longer than a
   64-vote proof (about 22 s), so X1's winner is random. X2 delays the loser's proof start by
   D = 150 s, more than the leg plus the OP settlement minus the proof. That makes the other node win
   every round, through the event path. When the loser gives up on the winner's event it closes the
   held request, and the proxy drops it rather than forwarding it after the delay, so X2 costs the
   loser's post but no prover job, and the next round's "provers idle" check does not wait for one.
   X3 also puts the loser 150 OP blocks (about 5 min) behind on events, so it reaches settlement
   without having seen the winner's event. The pre-flight `eth_call` at the latest state then reverts
   `InvalidStateRoot`, which is the revert path.
4. **Count, don't assume.** The run records whatever actually happened: the winner (the transition
   sender), the loser's path from its log, and the counter deltas (`lostRaces`, `settledBySelf`,
   `syncedFromOthers`). A round that did not race is reported as such, and the run keeps going until
   it has its target number of races, up to twice the planned rounds.

Each race records the following:

- detection: the loser's log time (the node's clock) minus the winner's OP block time (the block
  timestamp, 2 s granularity), so a detection is known to about ±2 s;
- recovery: the loser's sync time minus the winner's OP block time;
- the loser's wasted proof (its prover job, matched to no transition);
- the loser's wasted Ethereum posts and ETH: its `blobs posted on L1` lines and their receipts.

Lost races never count as failed cycles (Ruling 17), so racing does not trip the node's sealing
pause.

## 10. Chaos injection

**Triggers.** A trigger is one of:

- `At(t)` from the scenario start;
- `L1Pending(node)`: the pending nonce is above the latest, polled every 250 ms (as in `op_live`; on
  anvil, `dry` pauses L1 mining);
- `Sealed(node)`: a new `batch sealed` line in the node's log;
- `AfterEnd(t)`.

**Actions.**

| Action | Implementation |
|---|---|
| Kill and restart | `Node::kill`, then restart on the same port, datadir and knobs after `down` |
| Prover down | The prover proxy refuses connections (`set_down`) |
| Prover slow | The proxy delays `POST /prove` (`set_delay`). It stays under the client's 300 s request timeout. A held request whose client closes (the node gave up on the flight) is dropped, not forwarded. |
| RPC fail | The RPC proxy answers 503 (`set_fail`) |
| RPC down | The RPC proxy refuses connections |
| Knobs | Restart the node with other knobs (fee cap, confirmations) |
| Organizer | End, shorten with notice, set grace |

A fee spike is simulated safely with a node fee cap below any real post. The node then holds the post
and retries until the cap allows it or the job deadline passes (`L1 post failed; retrying`, `FeeCap`).
It spends nothing, and a post that is never sent does not count as a failed cycle. At the deadline the
batch is requeued and its proof runs again on the next cycle, so a long cap wastes prover jobs (§8,
C7).

Each action records:

- when it was triggered and applied;
- its effect on the node's logs: `RPC failover`, `beacon failover`, the post record picked up after a
  restart, a replacement at the same nonce, pauses;
- recovery time: from the undo to the node's first settlement or sync of its own votes;
- the delay it added to the affected votes.

Out of scope here: stopping a prover container, power-capping a GPU, and rewriting RPC responses.

## 11. Vote ledger and audit

The ground truth for "settled" is the chain, not the node API:

- for each transition, its index, block, timestamp, sender and roots;
- its vote ids, decoded with `decode_blobs(blobs, nf)` from the observer's archive
  (`/processes/{pid}/transitions/{i}/blobs`, which the observer fetched from Ethereum beacons and
  matched to the versioned hashes). A transition the observer never synced is read from A's or B's
  stored blobs instead (every node stores what it settled or synced), and the audit is marked
  `observer incomplete`.

**When the audit runs.** After the grace end and the results wait (`grace_end + 15 min`), the audit
waits, up to 15 min, until O's transition count for every election equals the chain's
(`chain::transitions` from the registry logs) and A and B report `pendingVotes == 0`. A beacon that
serves a blob late leaves O behind, and while a node cannot read the landing time (C6) it does not
close out its queue, so classifying earlier would report harness lag as `Lost`. If O never catches
up, the missing transitions come from A's or B's blobs and the audit is `observer incomplete`, not
`Lost`. A vote still pending after the wait is a real finding.

The audit asks every node that accepted a vote, or may have admitted it, for its status, including
votes that appear in a transition. Under `Both` routing a vote that one node errored can settle
through the other, and the node that errored it keeps reporting `error`: a stale error.

**How the node errors a vote.** The node binary carries Ruling 20:

- **Prover-side job failure** (a job that reached `failed` after the prover's own retries, or a
  permanent 4xx on submit). The node rolls the batch back, requeues it and bisects. The first failure
  retries at full size; each later one halves the next seal's cap. The retry cycles are proof-first
  (no blob post before the proof), so they post nothing when the proof fails and count no failed
  cycle; only the first failure, which did post, counts one. A failing single-vote batch errors that
  vote alone, with the reason `prover: … (isolated by bisection)`. After three isolated errors in a
  row without a landing the node logs that the prover fails every batch and holds the votes pending
  instead of erroring more. A landing or sync that settles the suspect votes ends the bisection; a
  restart forgets it and retries once at full size. `/info` counts `bisections` and
  `isolated_votes`. A deterministic failure costs at most log2(n) + 1 extra proofs before one vote
  errors.
- **Node-side mismatch** (proved or snark publics that differ from the node's, a program vk or
  `root_c` that is not the pinned one) and an L1 settlement that fails after its post. The batch is
  held: requeued with the usual backoff, never bisected, and its votes never error before the close.
  At the close they error `process closed (held: …)`, which the audit never counts as an allowed
  `process closed`. `/info` counts `held_batches`.
- **Guest rejection** (a fail mask). The whole batch errors with `guest rejected the batch: …`: the
  mask names the check, not the vote. `validate_vote` mirrors every per-vote check, so a single one
  means a node bug.
- **Refresh overflow.** When the exposed set alone exceeds `MAX_REFRESH` (2048) or the blob cap,
  every pending exposed vote errors `exposed refresh set too large, recast`. One failed 512-vote
  attempt exposes about 1536 slots, so two attempts over different vote sets of one process can cross
  it; R and C8 are where it can show. It is on no allowed list.
- **Close.** Whatever is still pending at the grace end, a cancel or the stop rule errors
  `process closed`, and so does an in-flight batch (a bisection retry included) that reverts
  `InvalidStatus` or `InvalidTimeBounds` after the end.
- **Results.** A permanent results-proof failure is retried with the same hold rule instead of
  latching until a restart. No other node can publish a sequencer-key election's results, hence
  `results_ok` below.

`classify` returns one verdict per vote id. The table lists them in order; the first one that holds
applies.

| Verdict | When |
|---|---|
| `Twice` | The vote id appears in two transitions. Fails the audit. |
| `Inconsistent` | In a transition, but no node accepted it and no attempt was uncertain. Fails the audit. |
| `Settled{transition, counted, stale_error}` | In exactly one transition. `counted` means it is its voter's latest settled ballot in chain order. `stale_error` lists the nodes that accepted it and still report `error`: reported as a finding under `clean`, never a failure. |
| `Errored{reason, expected}` | Not in any transition, and every node that accepted it reports `error` with a reason. `expected` means the reason is on the scenario's allowed list. |
| `NeverAdmitted` | Every attempt was uncertain (a 408, a transport error, or a refusal on a resend) and every node answers 404. Not an accepted vote. |
| `Lost{status}` | Accepted, in no transition, and some node still reports it `pending`, `aggregated` or `processed`, or reports nothing. Fails the audit. |

A vote id found in a transition but absent from the ledger is reported as `Phantom` and fails
the audit. The audit classifies only once O's transition count equals the chain's and A and B
report `pendingVotes == 0` for the election (see "When the audit runs" above).

Allowed reasons match by substring, except that a reason carrying `(held:` never matches.

**Pass criteria.**

- `no_vote_lost`: no `Lost`, `Twice`, `Inconsistent` or `Phantom`.
- `clean`: no unexpected `Errored` and no `stale_error`. Under Ruling 20 a prover failure isolates
  at most one vote per failing batch (reason `isolated by bisection`), a guest rejection errors
  its batch, and `exposed refresh set too large` errors the exposed votes; all three are findings.
- `tally_ok`: the on-chain results, and every node's view of them, equal the sum of the counted
  ballots' choices, taken from the chain order. This is what makes `Cross` correct: across nodes,
  whichever settles last stands.
- `results_ok`: the registry's results were published (`ProcessResultsSet` by
  `grace_end + 15 min`) and equal the chain-order tally. A missing result is a finding, reported with
  the key node's `finalize` log lines; C3 (the key holder killed in the grace) is where it can show.
- `counters_ok`:
  - the registry's `voters_count` equals the number of distinct voters settled;
  - `overwritten_votes_count` equals the settled votes minus the distinct voters;
  - both also equal the sums of the observer's per-transition `voters` and `overwrites`;
  - the observer's `syncedFromOthers` equals the number of transitions (not checked when the audit
    is `observer incomplete`, which is reported instead);
  - the sum of A's and B's `settledBySelf` equals the number of transitions.

`audit --run DIR` reruns the audit from `ledger.jsonl` and the observer's datadir, for a run that
crashed.

## 12. Metrics and reports

None of these need per-vote polling.

**Per transition:**

- seal time (the prover tap's `/prove` time);
- proof time, and start and finish times (prover job API; matched to the transition by
  `BatchPublics.root_after == new_root`);
- Ethereum inclusion of the last blob transaction (`l1::posts`, block timestamp);
- the OP settlement (block timestamp);
- votes, overwrites, blobs, sender, seal reason;
- Ethereum and OP cost (`cost::fetch`, `l1Fee`).

**Per vote:** submit time from the ledger, and settle time from its transition. That gives
submit → settled p50, p90, p99 and max, plus each stage (queued, proving, L1, OP) for every vote.

**Throughput:**

- overall votes/min, from the first submit to the last settlement;
- steady votes/min, from settlements after the first landing (the first batch's votes excluded);
- each prover's busy fraction (the sum of its job times over wall time);
- the share of transitions per node.

**Nodes:** `/info` counters every 30 s (with Ruling 20's `bisections`, `isolated_votes` and
`held_batches`); `batch sealed`, settled, synced, lost-race, pause, failover and `L1 post failed`
events, parsed from the logs incrementally. Per scenario and node the report counts these vote-loss
metrics: `bisections` (and the votes they isolated), `held` (held batches and their reasons),
`refresh_overflow` (events and the votes they errored), `guest_rejection` (batches and masks) and
results failures (`finalize failed`).

**Spend:** balance deltas of the four sequencer accounts and the organizer per scenario, plus the
wasted share (lost races, replacements, re-posts).

**Progress line** (stderr and `progress.log`, every 30 s):

```
[bench 01:23:45 w1/R] sent 1830/2300 acc 1828 ref 2 | settled 1500 err 0 open 328 | 5m 61 v/min |
A tx 9 lost 2 | B tx 7 lost 1 | spent L1 A 210 B 180 µETH | fees 0.21+0.002 gwei, blob 0.003 gwei
```

**Reports:** each run writes `runs/w<N>-<scenario>/report.{json,md}`, and the campaign roll-up is
rewritten after every scenario. The roll-up contains:

- a ceiling table (nf × B → steady votes/min, total and per prover, with each cell's actual mean
  batch size and a `floor` mark when the 240 s floor, not the prover, bounded it);
- latency tables;
- a races table;
- a chaos table;
- the audit verdicts (`no_vote_lost`, `clean`, `tally_ok`, `results_ok`, `counters_ok`, and
  `observer incomplete` when it applies);
- findings, one row per kind and election with its count: `unexpected_error` (reason),
  `stale_error` (node), `bisections`, `isolated` (vote ids), `held` (reason, and votes closed as
  `process closed (held: …)`), `refresh_overflow`, `guest_rejection` (mask), `results_failed`,
  `observer_incomplete` (missing transitions) and `pending_after_wait`;
- spend.

After every scenario the controller also appends one line to its progress log
(`.superpowers/sdd/2026-10-13-op-core-stack/progress.md`): what ran, the audit verdicts and the spend,
so the owner can follow the campaign without opening the reports.

## 13. Spend caps and go/no-go gates

**Caps.** These numbers are proposed; the owner confirms them at checkpoint 1.

| Account | Cap | Funding |
|---|---|---|
| seq-a.l1, seq-b.l1 (Ethereum) | 0.005 ETH each for the campaign | seq-b.l1 0.006 ETH from the owner; seq-a.l1 topped up to 0.006 only if below |
| seq-a.op, seq-b.op (OP) | 0.0005 ETH each | seq-b.op 0.0006 ETH from the owner |
| Organizer (OP) | 0.002 ETH | — |
| Per post (node `--l1-max-post-fee`) | 0.00015 ETH | — |

The `fund` step reads the owner key in-process and enforces hard ceilings in code: at most 0.01 ETH on
Ethereum and 0.001 ETH on OP per call. It needs `--confirm <recipient>`.

**Estimate.** The waves and the TS runs settle about 285 batches. Counting losers and re-posts, that
is about 440 Ethereum transactions and 1150 blobs. At the gate's ceilings it costs at most about
8.5 mETH on Ethereum, within the two 5 mETH caps. At recent fees (0.1 gwei, blob 0.002 gwei) it is
about 1-2 mETH. OP is under 0.1 mETH.

**Fee gate.** Same style as Task 12: base fee plus tip at most 0.5 gwei, and blob base fee at most
0.02 gwei.

- `gate` checks it, along with balances, spend against the caps and both provers' health
  (`status ok`, `queue_len 0`), and exits 0 only on a go. It also fails when any `davinci-sequencer`
  process or container other than the rig's is running, when any of the four sequencer accounts has a
  pending nonce above its latest (§2), or when `DAVINCI_E2E_BEACON` lists fewer than two beacons (C6
  takes O's first one down). It prints the sha256 of the node binary and of itself, which the report's
  setup section copies.
- `run` checks it before every scenario and every 5 minutes. A breach pauses new sends for up to 30
  minutes. If the breach persists, the run ends that scenario's elections and audits them, and the
  scenario is reported as stopped on fees. At the start of a scenario, the run waits for up to 30
  minutes and then skips it.

**Stop rule.** When an account's spend reaches its cap, the run stops sending, cancels the
scenario's open elections (queued votes error with `process closed`, an expected reason), audits, and
exits.

**Owner checkpoints.**

1. seq-b keys and funding, and whether to plan and generate all three waves before wave 1 (§4).
2. Wave 1 go.
3. Wave 2 go.
4. Wave 3 go.
5. TS live runs go.

Each go shows fees, balances, the expected spend and the hours on both provers.

## 14. Safety

- **Secrets.**
  - Keys travel only as files: `DAVINCI_PRIVKEY_FILE` and `DAVINCI_L1_PRIVKEY_FILE` for the nodes, read
    in-process by the harness. Only addresses are printed.
  - `keygen` writes the seq-b key files itself: mode 0600, never overwriting, printing addresses only.
  - The private RPC reaches only the harness's environment (`op-core.sh`, then `bench.sh`) and the
    rig's proxies; it never appears in an argv, a node environment or a log.
  - Every error goes through `redact_urls`.
  - The seed file is 0600. It is copied to z7 with `scp -p` and set to 0600 there (the content never
    goes in an argument), and deleted there after `gen`.
  - `scan_secrets.py` (Task 12 brief) runs after every step: on z6 over the campaign dir (minus
    `secret/`) and the logs, with the four sequencer key files, the organizer key and the seed; on z7
    over its campaign dir, with the seed. The TS script reads `ORGANIZER_KEY_FILE` and `OP_RPC` from
    the environment and redacts its output.
- **Leftovers.**
  - Steps run in named scopes (`davinci-bench-*`).
  - Before a step: no `davinci-bench-*` unit is running, ports 9101-9103 are free, and both provers
    are idle.
  - After an abort: `systemctl --user stop davinci-bench-<step>.scope`, then `cancel`, which cancels
    the plan's READY or PAUSED elections.
  - The TS script ends or cancels its own process on any failure after creating it. At the end of the
    campaign, `cancel --all` cancels every READY or PAUSED process the organizer created since the
    campaign's start block (from the registry logs, like `cancel_leftovers`), so the TS runs' processes
    are covered too, not only `plan.json`'s.
  - `cancel_open` is never run in the middle of the campaign, because it would void the generated
    ballots.

## 15. Out of scope

- DKG and Council (not on OP).
- Origin-2 census updates (covered by the anvil e2e).
- Merkle censuses in the TS script, which uses CSP.
- A fee proxy that rewrites RPC responses.
- More than two signing nodes, or nodes on z7.
- Proving-only benchmarks (BENCHMARK.md covers them).
- Any change to the node, the contracts or the SDK source. A bug found here gets its own fix and
  regression test, then a new wave. (Ruling 20 is a node change, but it is made separately on
  `feat/op-core` before any run, §1.)
- The production node switch.
- A long-lived rig with a control API.

## 16. Risks

| Risk | Mitigation |
|---|---|
| Pre-generated ballots bind the pid and the key node's election key: a lost datadir, a recreated election or a rerun spends them | Elections are created before generation; datadirs persist under the campaign dir; reruns are new waves; `cancel_open` is never run mid-campaign |
| Forced races depend on L1 timing; each race wastes a real blob post | Lockstep capacity seals, a prover skew of at least the Ethereum leg, a confirmation lag for the revert path; races are counted, not assumed; spend is capped |
| The audit depends on beacon blobs reaching the observer | The observer stores every blob; the audit waits up to 15 min for O to hold every transition the registry logs show, then takes any missing one from A's or B's stored blobs and reports `observer incomplete`, never `Lost`; blobs stay available about 18 days and the audit runs right after the scenario |
| z6 RAM is shared by the prover, three nodes and the harness | Scope `MemoryMax=12G`; `ZISK_MINIMAL_MEMORY_FROM` set to 256 on both hosts (2048 OOM-killed nf-16 batches) and checked before every wave; `gen` is never run during a run |
| A ZisK crash that survives the prover's retries fails a job | Ruling 20: the node requeues and bisects proof-first; at most one vote errors per failing batch, reported as a finding; the prover's v0.2.1 retry is a prerequisite |
| The production node still signs with seq-a | `gate` fails on any `davinci-sequencer` process or container other than the rig's, and on a pending nonce above the latest on any sequencer account |
| OP's 240 s grace floor and a cold proving estimate bound open-window batches, so a ceiling cell may never reach B | Cells of one nf run in ascending B without a restart; each cell is reported by its actual mean batch size and steady rate over batches of at least 0.9·B, marked `floor` when the floor bounded it |
| A permanent results failure leaves a sequencer-key election without results | Ruling 20 retries it with the hold rule instead of latching; the audit's `results_ok` reports a missing result with the key node's `finalize` log lines |
| Many campaign elections post through each node's one-at-a-time Ethereum window | Measured in R (four elections at once); the report shows the Ethereum leg per transition |
