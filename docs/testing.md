# Testing

## Layout and prerequisites

The tests read sibling checkouts next to this repository:

| Checkout | Used by | Override |
|---|---|---|
| `../davinci-zkvm` | Everything (path dependency), the emulator runs | `CIRCUIT_ELF_PATH` for the circuit |
| `../davinci-contracts` (`forge build`) | Anvil tests, e2e | `DAVINCI_CONTRACTS_DIR` |
| `../davinci-onchain-census-contract` (`forge build`) | Census anvil tests, e2e | `DAVINCI_CENSUS_CONTRACT_DIR` |
| `../davinci-circom/artifacts` | Ballot proofs | `CIRCOM_ARTIFACTS` |
| `../davinci-dkg` | e2e DKG scenarios | `DAVINCI_DKG_DIR` |

`anvil` and `forge` are looked up in `~/.foundry/bin` or on `PATH`, `ziskemu` in `~/.zisk/bin`
or at `ZISKEMU_BIN`.

## Unit and integration tests

```bash
make test                                   # cargo test --workspace
cargo test -p davinci-state                 # one crate
```

The state and API tests use real ballot proofs. They are cached under `state/tests/cache` and
`sequencer/tests/cache` (gitignored); a cold cache proves them from `CIRCOM_ARTIFACTS`.

Tests that need external tools are gated by an environment variable. When it is unset they print
a skip line and pass, so check the output before assuming they ran.

| Command | Needs | Covers |
|---|---|---|
| `ZISKEMU=1 cargo test -p davinci-state --test dryrun -- --test-threads=1` | `ziskemu` and the circuit ELF | Rust-built transitions through the real circuit on the emulator, all 64 outputs compared. No GPU. |
| `ANVIL=1 cargo test -p davinci-sequencer --test web3_anvil` | anvil, davinci-contracts build | One- and two-blob transitions and results on a real registry. |
| `ANVIL=1 cargo test -p davinci-sequencer --test web3_anvil l1_blob_mode` | anvil, davinci-contracts build with `L1BlobVerifier` and `MockL1Block` | L1-blob mode on two anvils: blob post, settlement, recorded anchor, origin mismatch, reorg re-post, dropped-post replacement. |
| `ANVIL=1 cargo test -p davinci-client --test organizer` | anvil, davinci-contracts build | The organizer against a real registry. |
| `ANVIL=1 cargo test -p davinci-sequencer --test census_anvil` (and `--test api`) | anvil, census contract build | The on-chain census index: incremental sync, restart, reorg, origin-3 votes. |
| `CIRCOM_ARTIFACTS=<dir> cargo test -p davinci-client --test prover` | circom artifacts | The ballot prover. |
| `cargo test -p arbo --features sdk-poseidon` | nothing extra | arbo's circom Poseidon vectors. |
| `ARBO_BENCH_1M=1 cargo bench -p arbo` | nothing extra | arbo benchmarks with the 1M-leaf rows. |
| `MEMSOAK=1 cargo test -p davinci-sequencer --test memory memsoak -- --nocapture` | nothing extra (about 3 min, 1-2 GB of disk under `target/tmp` or `MEMSOAK_DIR`) | The L1-blob pipeline on fakes and jemalloc, 40 batches of 512 votes at 16 fields (`MEMSOAK_BATCHES`, `MEMSOAK_NF`): RSS within 600 MB of the live heap, the live heap within 300 MB of the redb cache, and the file within 48 MB a batch. |

`--test l1da` (poster and settle loop) and `--test actor_l1` (the actor in L1-blob mode) run on scripted fakes with no gate.

arbo also has fuzz targets: `cd arbo && cargo +nightly fuzz run <target>`.

## CI

`.github/workflows/main.yml` checks the siblings out next to this repository at the refs listed at
the top of the workflow. It runs fmt and clippy, `cargo test --workspace` with `ANVIL=1`, the
e2e setup phase (`DAVINCI_E2E_SETUP=1`) in blob mode and in L1-blob mode, the emulator dry runs on a CPU-only ZisK, and the Docker
build. No job needs a GPU prover. Branch pushes publish the image under the branch name; a release
tag `vX.Y.Z` publishes `:vX.Y.Z` and moves `:latest` to it, while prerelease tags do not.

## End-to-end acceptance test

`e2e/tests/e2e.rs`, gated by `DAVINCI_E2E=1`, runs real nodes against real contracts and a real
prover. It starts anvil (Osaka, 1 s blocks), deploys the verifier and the registry pinned to the
SDK release, runs three signing nodes and one observer as subprocesses sharing one davinci-zkvm
prover, and drives these elections through them:

- **Merkle census**, 24 voters round-robin across the nodes, with duplicate submissions and
  revotes through a node that did not see the first ballot; the results proof is checked against
  the expected tally.
- **CSP census**, 3 voters.
- **Rejected packages**: a bad proof and a bad signature (400/40002), an address outside the
  census (400/40001), a reused vote id (409/40901).
- **On-chain census** growing from 3 to 18 members while voting, with a node killed between a
  batch seal and its settlement and restarted on the same datadir.
- **Updatable census** replaced mid-election, with a reweighted member's pending vote errored.
- **Grace window** (`e2e/tests/e2e/grace.rs`): an END with batches in flight, a shortened end, a
  backlog that extends the window round by round, and one that hits `graceMaxTotal`, with queued
  revotes and a node failover on the way.

With `DAVINCI_E2E_DA=l1-blobs` the same scenario runs in L1-blob mode. The registry settles on the
usual anvil (the OP side) through an `L1BlobVerifier` that reads a `MockL1Block`, and the blobs
go to a second anvil (the Ethereum side: chain id 1, Osaka, 2 s blocks). An origin relayer in the
harness copies the Ethereum block `DAVINCI_E2E_L1_LAG` (default 6) blocks behind the head into
`MockL1Block`, as OP's derivation would. Each sequencer posts from its own Ethereum key (accounts
5 to 7 of that anvil), every node reads blobs from the Ethereum anvil, and the scenario adds a
node killed while its blob transaction waits in the paused Ethereum pool: after the restart it
proves the batch again and replaces the pending transaction at its nonce with every fee doubled.
With `DAVINCI_E2E_DKG=1` the DKG contracts and committee run on the OP-side anvil and the
registry takes the DKG manager, as in blob mode. With
`DAVINCI_E2E_NEGATIVE=1` the L1 settlement is replayed tampered: blob mode's entry point
(`WrongDAMode`), a broken header chain (`BrokenHeaderChain`), a transaction proof of another trie
(`MerkleTrie: invalid root hash`), a swapped commitment (`InvalidBlobsDigest`), an extra blob
(`BlobCountMismatch`) and a flipped proof byte (`InvalidProof`).

Every process closes after its grace window, and the harness checks the boundaries through
`eth_call`: the results call reverts `GraceOpen` in the block before the grace end, and a
transition in the block after it reverts `InvalidTimeBounds`.

The nodes run with short timers (`BATCH_MAX=8`, `BATCH_TIME=3s`, `SOLO_WAIT=9s`,
`FLUSH_HORIZON=10s`, `CONFIRMATIONS=0`, a 1 s poll) so they race, and the registry's grace
parameters are sized for real proofs (`davinci_e2e::chain::GRACE_ARGS`).

```bash
make e2e E2E_LOG=$HOME/.cache/davinci-e2e/e2e.log                              # blob mode
DAVINCI_E2E_DA=l1-blobs make e2e E2E_LOG=$HOME/.cache/davinci-e2e/e2e-l1.log   # L1-blob mode
DAVINCI_SEQUENCER_BIN=/path/to/bin make e2e                                     # a prebuilt node binary
```

`make e2e` runs the test in a memory-capped systemd scope (`E2E_MEMORY`, default `16G`; the log
path is `E2E_LOG`). Anvil and the nodes are killed only when the test's handles drop, so a killed
test binary leaves them to the scope. Without `DAVINCI_SEQUENCER_BIN` the test builds the node
with `cargo build --release -p davinci-sequencer`. If the tree may change during a run, build once,
copy the binary elsewhere and pass it in.

It needs the prover at `DAVINCI_ZKVM_URL` (default `http://127.0.0.1:8080`) plus the checkouts
above. With the DKG and negative options below, a run takes about 50 minutes on one RTX 5090
prover. On failure the node logs, anvil log and datadirs are kept and their paths printed.

| Variable | Effect |
|---|---|
| `DAVINCI_E2E_SETUP=1` | Runs only the part before the nodes: chain, contracts, processes and ballots. Needs neither the prover nor the node binary. |
| `DAVINCI_E2E_DA=l1-blobs` | L1-blob mode, see above. With `DAVINCI_E2E_SETUP=1` it also checks that the relayer moves `MockL1Block` and that blob mode's entry point reverts `WrongDAMode`. |
| `DAVINCI_E2E_L1_BLOCK_TIME`, `DAVINCI_E2E_L1_LAG` | L1-blob mode on anvil: the Ethereum anvil's block time (default 2 s) and the relayer's lag (default 6 blocks, at most 48). |
| `DAVINCI_E2E_GRACE` | The anvil registry's grace window, `default,floor,ceil,max_total,notice` in seconds (default `davinci_e2e::chain::GRACE_ARGS`). |
| `DAVINCI_E2E_GRACE_ONLY=1` | Runs only the grace window phase. |
| `DAVINCI_E2E_DKG=1` | Adds a DKG automatic process ended with a batch in flight, a locked one decrypted after the reveal, and a zero-vote one. On anvil the harness deploys the DKG contracts and runs three `davinci-dkg-node` operators, built from `DAVINCI_DKG_DIR` unless `DAVINCI_E2E_DKG_NODE_BIN` is set; their circuit artifacts (about 1.1 GB) go to `DAVINCI_E2E_DKG_ARTIFACTS`. After each decryption request the observer, which never settles (in L1-blob mode it rebuilt every transition from Ethereum blobs), replays its archive to the requested root, accumulator and siblings. Works in both DA modes. There is no Council e2e: the Council key mode is covered by the live OP test only. |
| `DAVINCI_E2E_NEGATIVE=1` | Replays settled transactions, tampered, through `eth_call` and requires each to revert with the expected error. |
| `DAVINCI_E2E_LIVE=1` | Runs on an existing deployment instead of anvil, see below. |
| `DAVINCI_E2E_RESILIENCE=1` | Live only: full RPC, prover and beacon outages and an observer RPC failover, through loopback proxies. |
| `DAVINCI_E2E_NODE_LOG` | The nodes' log filter. |
| `DAVINCI_E2E_TIMEOUT_SCALE` | Multiplies every settle and sync timeout. |

The negative checks cover a replayed transition (`InvalidStateRoot`), a flipped proof byte,
public value or tally (`InvalidProof`), extra blobs or blob hashes (`BlobCountMismatch`),
organizer calls after the end or on a static census, and DKG results calls that are tampered,
repeated, premature or made on the wrong key mode.

### Live runs

With `DAVINCI_E2E_LIVE=1` the same scenario runs on an existing blob-mode deployment
(`DAVINCI_E2E_NETWORK` names its preset; the default, `optimism`, takes the compact run below).
It needs key files in `DAVINCI_E2E_ORGANIZER_KEY` and
`DAVINCI_E2E_SEQUENCER_KEYS` (comma-separated), with 0.05 of the native token in every account,
and checks the registry pins before it starts. Only addresses are printed. `e2e/src/net.rs`
documents the overrides (`DAVINCI_E2E_RPC`, `DAVINCI_E2E_NODE_RPCS`, `DAVINCI_E2E_BEACON`,
`DAVINCI_E2E_REGISTRY`, `DAVINCI_E2E_FROM_BLOCK`). With `DAVINCI_E2E_DKG=1` the run uses the
committee behind the registry's DKG adapter (`DAVINCI_E2E_DKG_MANAGER` overrides it). Every run
ends with its gas bill, counting only the processes it created.

The full scenario does not run live in L1-blob mode. On the `optimism` preset (the default,
in L1-blob mode), `DAVINCI_E2E_OP_LIVE=1 DAVINCI_E2E_LIVE=1` runs a compact one instead
(`e2e/tests/e2e/op_live.rs`): one signing node and an observer, a Merkle process with eight
voters and three revotes and a CSP process with three, batches of four, the signing node killed
once its first blob transaction is in Ethereum's pool and restarted, both results and, with
`DAVINCI_E2E_NEGATIVE=1`, the L1 negatives. Besides the organizer and sequencer key files it
needs `DAVINCI_E2E_L1_KEYS` (one dedicated Ethereum key file per sequencer, never the
organizer's account) and reads `DAVINCI_E2E_L1_RPC` (Ethereum RPCs, default the preset's),
`DAVINCI_E2E_L1_SEND_RPC` (endpoints that take 1.5 MB bodies), `DAVINCI_E2E_L1_MAX_POST_FEE`
(default `0.00015eth`), `DAVINCI_E2E_L1_MIN_BALANCE` (default 0.0002 ETH) and
`DAVINCI_E2E_MIN_BALANCE` (OP; default 0.00005 ETH on an L1-blob preset). `DAVINCI_E2E_BEACON`
then names Ethereum beacon APIs that keep full blobs. A run posts about six blob transactions.

### One 512 x 16 transition in L1-blob mode

`e2e/tests/l1_big.rs`, gated by `DAVINCI_E2E_L1_BIG=1`, settles one transition of 512 votes at 16
fields with the real prover, on two anvils whose registry has the OP Mainnet grace window
(`DAVINCI_E2E_GRACE=300,240,600,1800,60`), or live with the `op_live` environment. One node runs
the L1-blob defaults (`--l1-settle-lag 110s`, `--settle-margin 30s`, `--prove-base 30s`) with
`--batch-max 512`. 512 first votes settle as the seal budget allows; then 512 new voters vote while
the process is paused, the organizer ends it, and the flush seals all of them with 512 silent
refreshes: nine blobs, posted as two Ethereum transactions and settled in one proof. The process
takes the registry's 600 s ceiling grace, so a fresh node's proving estimate fits the batch after
the end; the node then publishes the results. `DAVINCI_E2E_RECORD_DIR` writes the first batch
job's snark and register dump in davinci-zkvm's `rust-sdk/testdata` format. A run takes about 30
minutes, half of it the grace window, and wants the shared prover's queue empty.

`DAVINCI_E2E_L1_BIG_VOTES` sets the vote count N (1 to 1024, default 512; the node runs with
`--batch-max N`). The expected blob count is derived from the encoding, `blob_count(N, 2N, 16)`,
and the expected transaction count is that over 6 blobs per transaction, rounded up: 9 blobs in 2
transactions for 512, 7 in 2 for 384. The run prints the expectation first. A prover host with
limited memory may need a smaller N: an RTX 5090 host with 59 GB of RAM ran out of memory proving
512 votes with their 512 refreshes, and about 370 proved, so use 384 there:

```bash
DAVINCI_E2E_L1_BIG=1 DAVINCI_E2E_DA=l1-blobs DAVINCI_E2E_GRACE=300,240,600,1800,60 \
  systemd-run --user --scope -p MemoryMax=16G -- \
  cargo test -p davinci-e2e --test l1_big -- --nocapture
```

### A small election through a running node

`e2e/tests/node_smoke.rs`, gated by `DAVINCI_E2E_NODE_URL`, drives a node that already runs (a
production node after an upgrade) on the live deployment of the preset: a CSP process with three
voters and the node's election key, created with the organizer key file
`DAVINCI_E2E_ORGANIZER_KEY`. The organizer ends it once the votes are queued; the node seals them
in its flush, settles them and publishes the tally, which must match. It spawns nothing and takes
about 10 minutes, most of it the grace window.
The host that runs it needs the circom artifacts (`CIRCOM_ARTIFACTS`, else `../davinci-circom/artifacts`)
to prove the three ballots.

## Docker runner

`e2e/bench.sh` runs the e2e crate in the `e2e/Dockerfile.bench` image, with nothing native on the
host: the nodes are the binary of a published node image (`NODE_IMAGE`), anvil and forge come
from foundry, and the harness is built in the container from the mounted checkouts. The container
uses the host network, so provers on loopback or a private network are reachable, and runs as your
user. Cargo's registry and target directory live in the `davinci-bench-cache` volume; reports,
logs and the datadirs of a failed run go to `~/.cache/davinci-bench` (`BENCH_RUNS`). The sibling
checkouts default to the directories next to this one.

With `DAVINCI_E2E=1` it runs the acceptance test on the latest release image. The `DAVINCI_E2E_*`
variables pass through, and live key files are mounted read-only:

```bash
DAVINCI_E2E=1 DAVINCI_E2E_DKG=1 DAVINCI_E2E_NEGATIVE=1 \
  DAVINCI_ZKVM_URL=http://127.0.0.1:8080 e2e/bench.sh    # anvil

DAVINCI_E2E=1 DAVINCI_E2E_LIVE=1 DAVINCI_E2E_DKG=1 DAVINCI_E2E_NEGATIVE=1 \
  DAVINCI_E2E_ORGANIZER_KEY=/path/to/organizer.key \
  DAVINCI_E2E_SEQUENCER_KEYS=/path/to/sequencer-a.key,/path/to/sequencer-b.key,/path/to/sequencer-c.key \
  DAVINCI_ZKVM_URL=http://127.0.0.1:8080 e2e/bench.sh    # live
```

With `DAVINCI_E2E_DKG=1` on anvil the image cannot build the DKG node: build a static one with
`CGO_ENABLED=0 go build -o ~/.cache/davinci-e2e/dkg/davinci-dkg-node ./cmd/davinci-dkg-node` in
the davinci-dkg checkout (`DAVINCI_E2E_DKG_NODE_BIN` points elsewhere).

## Benchmarks

Both benchmarks run on anvil by default, or live with `DAVINCI_E2E_LIVE=1`, and write a Markdown
report to `DAVINCI_E2E_BENCH_OUT` when set. Results are in [benchmarks.md](benchmarks.md).

**Batch sizes** (`e2e/tests/bench.rs`): one node proves a first and a steady batch per size. The
steady batch carries as many silent refreshes as votes.

```bash
DAVINCI_E2E_BENCH=sizes cargo test -p davinci-e2e --test bench -- --nocapture
```

Sizes come from `DAVINCI_E2E_BENCH_SIZES` (default `4,16,64,128,256`) and the field count from
`DAVINCI_E2E_BENCH_NF` (default 4).

**L1 blobs** (`e2e/tests/bench_l1.rs`, `DAVINCI_E2E_BENCH=l1`): the batch-size benchmark of the
L1-blob mode, live on OP Mainnet or, as a dry run, on anvil with `DAVINCI_E2E_DA=l1-blobs`. Per
case and run a fresh node proves a first and a steady batch; per settlement it records the proof
time, the latencies from seal and from proof-ready to settled, the Ethereum post (execution plus
blob gas), the OP settlement (L2 execution plus L1 data fee, plus any `record()`), the L1 proof's
header count and the gas of `L1BlobVerifier.verify` alone. Sizes come from
`DAVINCI_E2E_BENCH_SIZES` (default `64,128,256`, at most 256), fields from `DAVINCI_E2E_BENCH_NFS`
(`2,16`), runs from `DAVINCI_E2E_BENCH_RUNS` (3); runs are `DAVINCI_E2E_BENCH_PAUSE` (60) s apart
and each waits for an empty prover queue; no new run starts once the node's Ethereum key spent
`DAVINCI_E2E_BENCH_L1_BUDGET` (0.0009 ETH). It writes the Markdown table to
`DAVINCI_E2E_BENCH_OUT` and the rows as JSON next to it.

**Throughput** (`e2e/tests/throughput.rs`): sustained votes per second across several provers.
One node per URL in `DAVINCI_E2E_BENCH_PROVERS` sequences its own processes, so the provers never
race on one state root. Every ballot is proved before the clock starts, and the clock runs from
the first submit to the last settlement. The report has per-batch queue and proving times, the
overall and steady-state rate, and each prover's share and busy fraction. The run fails if a vote
errors or goes missing, and then checks the tally on-chain. The options are listed at the top of
the test file.

```bash
DAVINCI_E2E_BENCH_PROVERS=http://127.0.0.1:8080,http://prover2.example:8080 e2e/bench.sh
DAVINCI_E2E_BENCH=sizes e2e/bench.sh
```

A steady batch of N votes carries up to N refreshes, and 1024 + 1024 needs about 41 GB of RAM in
the prover, so stay at batches of 512 on a 64 GB prover host.
