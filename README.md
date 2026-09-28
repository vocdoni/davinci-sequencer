# davinci-sequencer

A DAVINCI sequencer in Rust, built on the davinci-zkvm prover. It replaces
davinci-node (Go, gnark). A node collects encrypted ballots and groups them into
batches. Each batch is proved as one ZisK PLONK and settled on the
`ProcessRegistry` contract. The transaction carries EIP-4844 blobs holding
everything needed to rebuild the state. Any node can settle any election, and
nodes that lose a race rebuild from the winner's blobs.

The election key comes from a sequencer or from a davinci-dkg committee. With a
sequencer key, the node that generated it decrypts the final tally and proves it
with a second small zkVM guest (`circuit-results`), which the same on-chain PLONK
verifier checks. With a DKG key no single party holds the secret, and the
committee threshold-decrypts the tally.

The guests are specified in davinci-zkvm: `circuit/CIRCUIT.md` (vote batch) and
`circuit-results/RESULTS.md` (results). Conventions for contributors are in
`CLAUDE.md`.

## Layout

| Crate | Role |
|---|---|
| `arbo/` (`arbo`) | Sparse Merkle tree, a port of vocdoni/arbo (SHA-256, circom-compatible proofs, redb storage). See [arbo](#arbo). |
| `state/` (`davinci-state`) | Per-process state: vote validation (mirrors every per-vote guest check), transition builder, blob sync, results request. No IO besides arbo storage. |
| `sequencer/` (`davinci-sequencer`) | The node: config, redb storage, election keys, census, web3 (alloy), monitor, process actors, HTTP API. |
| `client/` (`davinci-client`) | API wire types and client, organizer and voter helpers, the circom ballot prover (feature `prover`, on by default). See [client/README.md](client/README.md). |
| `e2e/` (`davinci-e2e`) | Test-only crate: the end-to-end acceptance test, the batch-size and throughput benchmarks, and their Docker runner (`e2e/bench.sh`). |

External pieces:

- **davinci-zkvm** (`../davinci-zkvm`): the prover service (`POST /prove`,
  `POST /results`), the vote-batch ELF `circuit/elf/circuit.elf` and the results ELF
  `circuit-results/elf/results.elf`. The Rust SDK `rust-sdk` (crate
  `davinci-zkvm-sdk`) is a path dependency. It carries the wire types, protocol
  primitives, blob codec and the release pins (`release.rs`). `input-gen` is a
  dev-dependency for the emulator dry runs.
- **davinci-contracts**, branch `zkvm` (`../davinci-contracts`): `ProcessRegistry`
  with zkVM settlement (PLONK verify, root continuity, census root, blob
  point-evaluation checks), `setProcessResults`, and the DKG results path through
  `DavinciDKGAdapter`. Both ABIs are vendored in `sequencer/abi/`.
- **davinci-dkg** (`../davinci-dkg`): the threshold committee behind the DKG key
  modes, its contracts and `davinci-dkg-node`. Only the e2e DKG scenarios build it.
- **davinci-onchain-census-contract**, branch `davinci-zkvm`
  (`../davinci-onchain-census-contract`): the census contract behind origin 3.
- **davinci-circom artifacts** (`../davinci-circom/artifacts`):
  `ballot_proof.wasm` and `ballot_proof_pkey.zkey` for the client prover. The
  verification key the node accepts is embedded in the SDK
  (`rust-sdk/assets/ballot_proof_vkey.json`).

## Build and test

```bash
make build   # cargo build --workspace
make test    # cargo test --workspace (gated tests skip)
make e2e     # end-to-end acceptance test, see below
cargo clippy --workspace --all-targets -- -D warnings
```

The state and sequencer API tests use real ballot proofs. On a cold cache
(`state/tests/cache`, `sequencer/tests/cache`) they prove them from the circom
artifacts at `CIRCOM_ARTIFACTS` (default `../davinci-circom/artifacts`).

Gated tests print a skip line and pass when their variable is unset:

| Command | Needs |
|---|---|
| `ZISKEMU=1 cargo test -p davinci-state --test dryrun -- --test-threads=1` | `ziskemu` (`ZISKEMU_BIN` or `~/.zisk/bin/ziskemu`) and the circuit ELF (`CIRCUIT_ELF_PATH` or `../davinci-zkvm/circuit/elf/circuit.elf`). Runs Rust-built transitions through the real guest and compares all 64 output registers. No GPU. |
| `ANVIL=1 cargo test -p davinci-sequencer --test web3_anvil` | anvil (`~/.foundry/bin` or `PATH`) and `forge build` output in `DAVINCI_CONTRACTS_DIR` (default `../davinci-contracts`). Real 1- and 2-blob transitions and results on the registry. |
| `ANVIL=1 cargo test -p davinci-client --test organizer` | same as above. |
| `ANVIL=1 cargo test -p davinci-sequencer --test census_anvil` (and `--test api`) | anvil and `forge build` output in `DAVINCI_CENSUS_CONTRACT_DIR` (default `../davinci-onchain-census-contract`, then `~/davinci-onchain-census-contract`). The on-chain census index against a real `OwnedCensus`: incremental sync, restart, reorg; origin-3 votes. |
| `CIRCOM_ARTIFACTS=<dir> cargo test -p davinci-client --test prover` | the circom artifacts. |
| `cargo test -p arbo --features sdk-poseidon` | nothing extra; adds arbo's circom Poseidon vectors. |
| `ARBO_BENCH_1M=1 cargo bench -p arbo` | adds the 1M-leaf rows. |
| `DAVINCI_E2E=1` / `DAVINCI_E2E_SETUP=1` | see [End-to-end test](#end-to-end-test). |

### CI and Docker

CI (`.github/workflows/main.yml`) checks the sibling repos out next to this
one, at the refs listed at the top of the workflow, and runs on
`ubuntu-latest`: fmt and clippy, `cargo test --workspace` with `ANVIL=1` and
then the `DAVINCI_E2E_SETUP=1` run, the ziskemu dry runs on a CPU-only ZisK
without a proving key, and the Docker build. Branch pushes publish the image to
Docker Hub and GHCR under the branch name. A release tag `vX.Y.Z` publishes
`:vX.Y.Z` and moves `:latest` to it; prerelease tags (`v1.2.3-rc1`) don't
move it. No job needs the GPU prover, so
`DAVINCI_E2E=1`, the live runs and the bench stay local.

The path dependencies put the Docker build context one level up, in the
directory holding `davinci-sequencer/` and `davinci-zkvm/`:

```bash
docker build -f davinci-sequencer/Dockerfile -t davinci-sequencer .
docker run -v davinci-data:/data -p 9090:9090 davinci-sequencer --help
```

The image runs as a non-root user (uid 10001) with the datadir at `/data`
(`DAVINCI_DATADIR`).

`docker-compose.yml` runs the published image
(`ghcr.io/vocdoni/davinci-sequencer:${DAVINCI_SEQUENCER_TAG:-latest}`), so
Watchtower moves the node to each release. When the tag is not published,
compose builds it from the parent directory. Profiles:

| Profile | Services |
|---|---|
| `prod` | node, Traefik (Let's Encrypt certificate for `DOMAIN`), Watchtower |
| `prod-gpu` | `prod` plus a davinci-zkvm GPU prover |
| `dev` | node and Watchtower |
| `gpu` | node and a davinci-zkvm GPU prover |

```bash
cp .env.example .env    # every variable is listed there
sudo install -o 10001 -m 0400 /path/to/key sequencer.key
docker compose --profile prod up -d
```

The key never goes in `.env`: compose mounts `SEQUENCER_KEY_FILE` (default
`./sequencer.key`) as a secret for `--privkey-file`. It must be readable by uid
10001; an empty file runs an observer. The datadir is the named volume `data`.
The node follows `DAVINCI_NETWORK` (default `gnosis`), so a Gnosis `.env` sets
no registry, RPC or start block. A release that points the network at a new
registry needs no action: after Watchtower restarts the node, it opens a fresh
database for the new deployment (see [the datadir](#running-a-node)).

`prod` and `dev` use `DAVINCI_PROVER_URL` (`http://host.docker.internal:8080`
for a prover published on the same host). The GPU profiles run the published
prover, `ghcr.io/vocdoni/davinci-zkvm:${DAVINCI_ZKVM_TAG:-latest}`, which
Watchtower updates too, with the proving keys from `ZISK_KEYS_DIR` (default
`../davinci-zkvm/zisk-keys`, see `make keys` there). Its port stays unpublished
and `DAVINCI_KEEP_INPUTS` is pinned to 0. To build both images from the sibling
checkouts instead, give them a tag Watchtower cannot find upstream:

```bash
printf 'DAVINCI_SEQUENCER_TAG=local\nDAVINCI_ZKVM_TAG=local\n' >> .env
docker compose --profile gpu build
```

## Running a node

A node needs:
- an execution-layer RPC;
- a blob source: a beacon API, or anvil for local chains;
- a `ProcessRegistry` deployed from the davinci-contracts `zkvm` branch;
- a davinci-zkvm prover.

The first three come from the network (`--network`, default `gnosis`), so a
Gnosis operator gives only the key file and the prover:

```bash
cargo build --release -p davinci-sequencer
DAVINCI_PRIVKEY_FILE=/etc/davinci/sequencer.key \
DAVINCI_PROVER_URL=http://127.0.0.1:8080 \
./target/release/davinci-sequencer
```

**Networks.** The known deployments live in `davinci_client::networks`, which the
organizer and voter tooling share. A network sets the chain id, the
`ProcessRegistry` and its deployment block, the RPC list, the blob source and the
confirmations:

| Network | Chain | RPCs | Blob source | Confirmations |
|---|---|---|---|---|
| `gnosis` | 100 | `https://gnosis-rpc.publicnode.com`, `https://gnosis-rpc.blockreq.com/v1/rpc/public`, `https://rpc.gnosischain.com` | `beacon:https://rpc-gbc.gnosischain.com` | 3 |

The registry and its block are under [Deployments](#deployments).

- Every explicit setting (`--registry`, `--start-block`, `--rpc-url`,
  `--blob-source`, `--confirmations`, or its `DAVINCI_*` variable) replaces the
  network's value, so a custom deployment works on any chain.
- The network's start block belongs to its registry: with another `--registry`
  it does not apply, and the node scans from `--start-block` or block 0.
- An explicit registry pins the node to it. Only a node that takes the registry
  from the network follows it to a new one on upgrade.
- `--network custom` uses explicit settings only and needs `--registry`,
  `--rpc-url` and `--blob-source`; `--confirmations` defaults to 2.
- At boot the RPC's `eth_chainId` must be the network's. A mismatch stops the
  node unless `--registry` was given, which only warns. The node logs the
  network, chain id, registry, start block and deployment directory it uses.

The prover's ELFs and the registry's pinned vks must match the SDK release pins
(`BATCH_PROGRAM_VK`, `RESULTS_PROGRAM_VK`, `ROOT_C_VADCOP_FINAL`). The node checks
every proof against those pins and refuses to settle on a mismatch. Run the
prover with `DAVINCI_KEEP_INPUTS=0`: its `input.bin` holds the batch seed.

**Boot check.** Before the API serves, signer and observer alike read the
registry's `batchProgramVK`, `resultsProgramVK`, `rootCVadcopFinal`,
`ballotVKHash` and `chainID`, and its `ziskVerifier()`. The three vks must equal
the release pins, `ballotVKHash` the hash of the ballot VK the node uses
(`--ballot-vk` or the embedded one), and `chainID` the RPC's `eth_chainId`. The
verifier must answer `getRootCVadcopFinal()` with `ROOT_C_VADCOP_FINAL`, and the
keccak256 of its code must equal `ZISK_VERIFIER_CODEHASH`. Any difference stops
the node with an error naming each field, the expected value and the one found.
There is no override.

The node sizes batches to the chain's per-transaction blob limit and picks the
blob sidecar version (v1 cell proofs from Osaka) at boot, both from `eth_config`
(EIP-7910). When the RPC does not serve it, the sidecar version comes from calling
the P256VERIFY precompile (EIP-7951, new in Osaka), retried with backoff, and
the blob cap from `--max-blobs-per-tx` or a known-chain table. If the probe keeps
failing, Gnosis and Chiado fall back to cell proofs; on any other chain the node
refuses to start rather than guess.

SIGTERM or SIGINT stops the node within a few seconds in every phase, including
startup checks and event catch-up; anything still busy after ten seconds is cut
off (the database is crash-safe).

`--rpc-url` and `--blob-source beacon:` take comma-separated lists
(`--rpc-url https://a.example/KEY,https://b.example`). Requests go to one
endpoint until it fails, then move to the next and stay there; there is no
per-call rotation, so the node never mixes providers at different heads within a
scan. An RPC fails over on a connection error, timeout, any non-2xx status (429
and 5xx, but also 401/403/413: a bad key or a proxy page on one provider says
nothing about the next), a 2xx body that is not JSON-RPC, or a node-side
JSON-RPC error (missing historical state, Nethermind's "No state available",
rate limits, `-32603`); reverts, nonce and funding errors come back as they
are, and so does a too-wide `eth_getLogs` range, which the census scan halves.
A beacon API fails over on a connection error, 429 or 5xx. A 404 (a slot pruned
there) is asked once of the next beacon, without switching, so an archive
beacon listed second can serve old blobs. At boot every RPC must report
the same `eth_chainId`; an unreachable one is kept with a warning, and at least
one must answer. Logs name endpoints by host only, since URLs often carry API
keys.

Every outgoing HTTP request (RPC, beacon API, prover, census downloads) carries
`User-Agent: davinci-sequencer/<version>`; some public RPCs refuse requests
without one.

A custom deployment:

```bash
DAVINCI_NETWORK=custom \
DAVINCI_REGISTRY=0x... \
DAVINCI_START_BLOCK=123456 \
DAVINCI_RPC_URL=https://el.example:8545 \
DAVINCI_BLOB_SOURCE=beacon:http://127.0.0.1:5052 \
DAVINCI_PROVER_URL=http://127.0.0.1:8080 \
DAVINCI_PRIVKEY_FILE=/etc/davinci/sequencer.key \
./target/release/davinci-sequencer
```

Every flag has a `DAVINCI_*` environment variable. Durations take `90`, `90s`,
`500ms`, `5m` or `2h`.

| Flag | Env | Default | Meaning |
|---|---|---|---|
| `--network` | `DAVINCI_NETWORK` | `gnosis` | Known deployment that fills every chain setting not given, or `custom`. |
| `--registry` | `DAVINCI_REGISTRY` | network | `ProcessRegistry` address. |
| `--blob-source` | `DAVINCI_BLOB_SOURCE` | network | `beacon:<url>[,<url>...]` (consensus-layer beacon APIs) or `anvil` (`anvil_getBlobsByTransactionHash` on the RPC). |
| `--rpc-url` | `DAVINCI_RPC_URL` | network | Execution-layer JSON-RPC; a comma-separated list for failover. |
| `--prover-url` | `DAVINCI_PROVER_URL` | `http://127.0.0.1:8080` | davinci-zkvm prover service. |
| `--privkey-file` | `DAVINCI_PRIVKEY_FILE` | unset | File with the hex secp256k1 key that signs settlement transactions. |
| (env only) | `DAVINCI_PRIVKEY` | unset | The same key, from the environment. |
| `--datadir` | `DAVINCI_DATADIR` | `~/.davinci-sequencer` | Holds one `<chain id>-<registry>/sequencer.redb` per deployment. |
| `--api-host` | `DAVINCI_API_HOST` | `0.0.0.0` | API listen address. |
| `--api-port` | `DAVINCI_API_PORT` | `9090` | API port. |
| `--batch-max` | `DAVINCI_BATCH_MAX` | `1024` | Pending votes that seal a batch without waiting (1 to 1024). |
| `--max-blobs-per-tx` | `DAVINCI_MAX_BLOBS_PER_TX` | from `eth_config` | Most blobs one settlement transaction carries (1 to 6). Unset: the chain's `eth_config` `current.blobSchedule.max`, at most 6. Without `eth_config` (most Gnosis RPCs): 2 on Gnosis (100) and Chiado (10200), else 6, with a warning. A value above what `eth_config` advertises is kept, with a warning. |
| `--batch-time` | `DAVINCI_BATCH_TIME` | `5m` | Longest a pending vote waits before its batch is sealed. |
| `--settle-margin` | `DAVINCI_SETTLE_MARGIN` | `120s` | No batch is sealed when the election ends within this margin. |
| `--confirmations` | `DAVINCI_CONFIRMATIONS` | network, else `2` | Blocks behind head the monitor treats as final. Raise it with load-balanced RPCs whose backends lag. |
| `--start-block` | `DAVINCI_START_BLOCK` | network | First block a fresh deployment scans for registry events: the registry deployment block. Ignored once the deployment has scanned. The network's applies to its own registry only; otherwise unset scans from block 0, with a warning (about 9,700 `eth_getLogs` pages on Gnosis). Progress is saved after every 5,000-block page. |
| `--poll-interval` | `DAVINCI_POLL_INTERVAL` | `5s` | Chain polling interval. |
| `--heartbeat` | `DAVINCI_HEARTBEAT` | poll interval | Actor heartbeat: drives batch timing, window close-out and finalization. |
| `--prover-poll` | `DAVINCI_PROVER_POLL` | `2s` | Prover job polling interval. |
| `--prover-timeout` | `DAVINCI_PROVER_TIMEOUT` | `30m` | Wait for one proving job. On timeout the node waits on the same job up to 3 more times. |
| `--census-dir` | `DAVINCI_CENSUS_DIR` | unset | Directory `file://` census URIs may read from. Unset: `file://` is refused. |
| `--census-allow-private` | `DAVINCI_CENSUS_ALLOW_PRIVATE` | off | Allow census downloads from loopback and private addresses (local dev). |
| `--census-max-participants` | `DAVINCI_CENSUS_MAX_PARTICIPANTS` | `4194304` | Largest census accepted. |
| `--keys-per-minute` | `DAVINCI_KEYS_PER_MINUTE` | `10` | `POST /processes/keys` per client IP per minute. |
| `--ballot-vk` | `DAVINCI_BALLOT_VK` | embedded | Ballot proof verification key JSON. The default is the SDK's embedded key. Its hash is config leaf `0x07`, so it must match the registry's `ballotVKHash`, or every process fails the genesis check and is ignored. |
| `--log-level` | `DAVINCI_LOG_LEVEL` | `info` | Tracing filter; `RUST_LOG` takes precedence. |

**The signing key** is read only from `DAVINCI_PRIVKEY` or from the file named by
`--privkey-file`. A `--privkey` command-line value is rejected, because other users
can read process arguments. Setting both sources is an error. The key is never
printed and is zeroed on drop.

**Key modes.** Every process has a sequencer key or a DKG key, chosen at
creation. Only sequencer-key processes need `POST /processes/keys`; see
[Key modes](#key-modes).

**Observer mode.** Without a key the node follows every process and replays
every transition from its blobs. It serves reads and tracker proofs, but it never
settles or finalizes, even if it holds an election key. It refuses
`POST /votes` and `POST /processes/keys` with 412/41203, and `/info` reports
`"observer": true`.

**The datadir** holds one directory per deployment, named after the chain id and
the registry in lowercase hex, each with one redb file (mode 0600, directories
0700):

```
~/.davinci-sequencer/
  100-0x3cde68c39e26ecf94bd029b6ed3b9f945441daf3/sequencer.redb
```

A node that meets a new registry (a release moved the network, or another
`--registry`) creates its directory and starts there from an empty store at the
registry's start block; the previous deployment's directory stays untouched, and
a node pointed back at it resumes where it stopped. Each file records its
deployment and refuses to open under another. The key file lives outside the
datadir and does not move.

A datadir of the earlier flat layout (`<datadir>/sequencer.redb`) is adopted
once: when its processes carry this deployment's process-id prefix (bytes
20..24, from the chain id and the registry) it moves into the deployment's
directory. Otherwise, or when it holds no process, it stays in place and the
node logs why; delete it once that deployment is no longer needed.

Each database includes that deployment's node master secret, drawn on first
boot, from which every election key the node hands out there is derived. Losing
it loses the ability to publish results for those elections, so back it up and
protect it like a key.

## HTTP API

Wire conventions (`client/src/api.rs`):
- field names are camelCase;
- field elements are decimal strings, and bytes are `0x` hex;
- a process id is `0x` + 62 hex digits (`bytes31`);
- a vote id is `0x` + 16 hex digits and must be at least 2^63;
- points are twisted Edwards `{x, y}`;
- a ballot is exactly 16 `{c1: {x, y}, c2: {x, y}}`.

Decoding is strict: field elements must be below p, points on the curve, byte
lengths exact, and vote bodies carry no unknown fields.

| Method | Path | Purpose | Error codes |
|---|---|---|---|
| GET | `/ping` | Liveness (`pong`). | |
| GET | `/info` | Sequencer address (`null` for an observer), chain id, registry, ballot VK hash, both program vks, `observer`, and the counters `settledBySelf`, `syncedFromOthers`, `lostRaces`. | |
| GET | `/processes` | Process ids this node knows. | |
| GET | `/processes/{pid}` | On-chain parameters plus the node's view: `isAcceptingVotes`, `localStateRoot` (its committed tree root), `result` once on-chain, and `ignored`/`note` when the node refused to serve the process. | 40001, 40402 |
| POST | `/processes/keys` | Body `{"processId"}`: this node's election key for that (future) process id, as `{x, y}`. The key is derived, not stored, so the same id always gets the same key. Organizers ask for the registry's `getNextProcessId(organizer)`. | 40001, 41203, 42901 |
| GET | `/processes/{pid}/participants/{address}` | Weight and Merkle census proof, which voters need to build a ballot. Merkle census only. | 40001, 40402, 40401 (not a member, or a CSP census) |
| GET | `/processes/{pid}/transitions` | The settled transitions: roots, tx hash, block, sender, voters, overwrites, blob count. | 40001, 40402 |
| GET | `/processes/{pid}/transitions/{index}/blobs` | The raw blobs of one transition, `0x` hex. | 40001, 40402, 40401 |
| POST | `/votes` | Submit a vote. A paused process still accepts votes: they queue locally and settle when the process resumes (only `ended`/`canceled`/past-end refuse with 41201). | see below |
| GET | `/votes/{pid}/voteId/{voteId}` | Vote status. | 40001, 40402, 40401 |
| GET | `/votes/{pid}/voteId/{voteId}/proof` | Tracker proof: the vote-id leaf under the node's committed root, which is an on-chain root. `davinci_client::api::verify_tracker` checks it. | 40001, 40402, 40401 |
| GET | `/votes/{pid}/address/{address}` | The ballot currently stored in that voter's slot (re-encrypted). For a CSP census, only addresses this node served. | 40001, 40402, 40401 |

`POST /votes` fields:
- `processId`, `address`, `voteId`;
- `ballot` (16 ciphertexts);
- `ballotProof` (snarkjs Groth16);
- `ballotInputsHash`;
- `signature` (65 bytes `r‖s‖v`, personal-sign over the vote id);
- `weight` (decimal string);
- `censusProof`.

For a Merkle census, the node derives the census proof from its own copy of the
census and ignores the one the client sends. For a CSP census,
`censusProof` is required: `{"type": "csp", r, s, recid, index}`. The CSP
signature covers the top-level `weight`.

Vote status is one of:
- `pending`: queued;
- `aggregated`: in a batch being proved;
- `processed`: proof checked, settlement pending;
- `settled`: on-chain;
- `error`, with an `error` string: a guest fail bit, `process closed`, a
  settlement revert, or a prover refusal.

A batch that loses a race puts its votes back to `pending`. A node that never
stored the package still answers `settled` when the vote id is in its tree, and
404 otherwise.

Errors are JSON `{"error": "<message>", "code": <code>}`. The code is the HTTP
status times 100 plus a discriminator. 500 bodies say only `internal error`; the
detail goes to the node log. A request that runs longer than 60 s gets a 408.

| Status | Code | Meaning |
|---|---|---|
| 400 | 40001 | Malformed request or field. For votes also: address not in the census, missing CSP proof, or a vote for another process. |
| 400 | 40002 | The vote failed a protocol check: ballot proof, signature, inputs hash, census binding, ballot encoding or weight. |
| 404 | 40401 | Not found. |
| 408 | 40801 | The 60 s request deadline fired. The handler may still have finished: a timed-out `POST /votes` can have admitted the vote, in which case a retry answers 409. |
| 404 | 40402 | Unknown process (or one this node does not serve). |
| 409 | 40901 | Vote id already submitted or already in the tree. |
| 409 | 40902 | The slot already has a queued vote; retry once it settles. A resubmission of a vote id that is still queued also gets this code. |
| 412 | 41201 | The process does not accept votes. |
| 412 | 41202 | Max voters reached. |
| 412 | 41203 | Observer node. |
| 413 | 41301 | Body over 256 KiB. |
| 429 | 42901 | Key generation rate limit. |
| 429 | 42903 | Busy: vote validation at capacity or the process queue full (16384). |
| 500 | 50001 | Internal error. |

## How it works

**Monitor.** One loop polls the registry events up to `head − confirmations` and
routes them.

A `ProcessCreated` bootstraps the process. The node checks that:
- the election key is a subgroup point other than the identity;
- the census origin is 1 (static Merkle), 2 (off-chain dynamic), 3 (on-chain
  census contract) or 4 (CSP);
- the on-chain root is the genesis root the node derives from the config. For a
  node that starts late, the process's first transition must start from that
  root instead.

For origins 1 and 2 it also downloads the census and checks its root; a
`CensusUpdated` later fetches the new root in the background. For origin 3 it
registers the census contract and waits for the first sync before serving
votes. A process
that fails a permanent check is recorded as `ignored` with the reason. Transient
failures go on a persisted retry list, and a process is ignored after 10
attempts. Every other event goes to the process actor. The monitor also sends
each actor a heartbeat with the head block and time.

**Admission.** API handlers run the stateless checks on the blocking pool, with a
bounded number running at once. These checks are the Groth16 proof, the
signature, the census proof and the inputs hash; `davinci-state::validate_vote`
mirrors every per-vote guest check, so one bad ballot cannot sink a batch. The
vote then goes to the process actor. The actor is the single writer of that
process's tree, accumulator, counters and vote rows, and it checks the stateful
rules before queueing the vote:
- the vote id is not in the tree or the queue;
- the slot has no other queued vote;
- max voters is not exceeded;
- the process is accepting.

**Batches.** The actor seals a batch when `--batch-max` votes are pending or the
oldest has waited `--batch-time`. It does not seal when the election ends within
`--settle-margin`. It then:
1. takes votes in FIFO order, at most one per slot, within `maxVoters` and
   within the per-transaction blob cap (`--max-blobs-per-tx`, counting the
   refreshes the guest will require);
2. draws a fresh seed and a refresh selection from the OS RNG;
3. re-encrypts every ballot along the seed chain and re-randomizes the refreshed
   slots (silent revoting);
4. builds the state-tree witnesses, the DA blobs with their KZG commitments and
   openings, and the publics it expects.

A spawned job then proves the batch on the prover. It requires `ok = 1`,
`fail_mask = 0`, every register equal to the host's values, and the program vk
and `rootCVadcopFinal` equal to the SDK pins. It simulates
`submitStateTransition` with the blob hashes and sends the blob transaction.
Each node keeps at most one batch in flight per process, and processes run in
parallel.

**Races.** Settlement is permissionless. If another sequencer lands first, our
pre-flight or transaction reverts with `InvalidStateRoot`. The actor then rolls
the tree back, counts a lost race, puts the votes back in the queue, syncs, and
builds the next batch on the new root.

**Sync.** For a transition another node sent, the actor:
1. fetches the blobs and matches them against the transaction's versioned hashes;
2. decodes the vote ids, the slot updates and the accumulator;
3. applies them on its committed root, and requires the result to equal the
   event's `newRoot`.

Queued votes whose id is now in the tree become `settled`. Gaps are replayed from
the registry logs. Every node stores the transitions and blobs it sees and
serves them under `/processes/{pid}/transitions`.

**Finalization.** A node finalizes once the process has ended (status ENDED or
past its end time), no batch is in flight and its tree is at the on-chain root.
For a sequencer-key process only the node holding the election key does it:
1. decrypts the accumulator (baby-step giant-step, bounded by voters times the
   ballot's max value);
2. builds the 16 Chaum–Pedersen decryption proofs and the inclusion proofs of
   the key leaf `0x03` and the results leaf `0x04`;
3. proves them with `POST /results`, and checks the published root and tally
   against its own and the vks against the SDK pins;
4. calls `setProcessResults`.

For a DKG-key process any signing node drives the committee's decryption; see
[Key modes](#key-modes). Other nodes pick the results up from
`ProcessResultsSet`.

**Persisted:**
- process records;
- votes and the queue;
- the per-process arbo tree and committed state;
- transitions and blobs;
- census leaves (origins 1 and 2) and the origin-3 lean-IMT index;
- the per-process exposed slot set, until all exposed votes settle or error
  (see [Security properties](#security-properties));
- the master secret election keys are derived from.

The per-batch seed and refresh selection exist only in memory for one batch.
They are never written to disk or logged. On restart, votes in `aggregated` or
`processed` go back to `pending`, because their seed is gone by design. The node
then syncs from the chain. If its own transaction landed before the crash, those
votes settle through that sync without being proved again.

## Security properties

The zkVM guest and the registry enforce the protocol. The node's part is to keep
votes the guest would reject away from the prover and to keep the protocol's
secrets; it settles only proofs that match what it computed itself.

- **One bad ballot cannot sink a batch.** `davinci-state::validate_vote` repeats
  every per-vote check the guest makes (ballot proof, signature, inputs hash,
  census binding, ballot encoding, weight), and the actor applies the stateful
  rules: unused vote id, one queued vote per slot, `maxVoters`. The batch
  builder also mirrors the contract's `maxVoters` check and the blob cap. A vote
  the guest would reject never reaches the prover.
- **Proofs are checked against pins.** A batch settles only when the proof's
  `program_vk` and `rootCVadcopFinal` equal the SDK release pins and every
  public register equals what the node computed. The boot check refuses a
  registry or verifier that pins anything else. On-chain, every proof goes
  through `verifySnarkProof(programVK, rootCVadcopFinal, publicValues, proof)`,
  whose public input is `sha256(programVK ‖ publicValues ‖ rootCVadcopFinal)`, so
  a proof of another guest or another setup does not verify. The ballot VK is
  bound too: the guest hashes the VK it is given and requires it to equal state
  leaf `0x07`, written at genesis from the registry's `ballotVKHash`.
- **The batch seed and the refresh selection stay secret.** Both come from the
  OS RNG and live in memory for one batch only. They are never logged or
  written to disk, and the prover deletes the `input.bin` that holds them when it
  runs with `DAVINCI_KEEP_INPUTS=0`. A crash therefore loses an unfinished
  batch by design; its votes go back to pending.
- **Silent revoting.** Every batch also re-randomizes occupied slots it did not
  write, a uniform sample that nothing public determines, so an observer cannot
  tell an overwrite from a routine refresh. Participation is not hidden: a
  refresh only touches an occupied slot, so a slot's first write is a new vote,
  and with a Merkle census the slot follows from the voter's address.
- **Re-sealing a batch reveals nothing.** A sealed batch may reach the chain even
  if the node later drops it (a lost race, a prover failure, a restart). Once a
  batch is sealed, every ballot slot it changed, writes and refreshes as one
  set, joins a persisted per-process exposed set, and every later batch refreshes
  each of those slots it does not write. Without this, two attempts with
  different refresh draws could be compared, and the slot of a vote dropped
  between them would stand out as an overwrite. The set holds exactly the slot
  keys the blob publishes, and its stored form does not say which were writes,
  so persisting it leaks nothing new. It is cleared once every exposed vote is
  settled or errored. If the set no longer fits one transition (`MAX_REFRESH` or
  the blob cap), the node errors the exposed pending votes and clears it rather
  than publish a transition that covers only part of it.
- **Election keys are derived, not stored.** A sequencer key is
  `HMAC-SHA256(master, "davinci-election-key-v1" ‖ chainId ‖ registry ‖
  processId)`, expanded and reduced into `[1, l)`. `POST /processes/keys` writes
  nothing, so key requests cannot fill the disk, and a key handed out for one
  process id is useless under any other.
- **State is rebuilt, not trusted.** A transition from another node is applied
  only when its blobs match the transaction's versioned hashes and replaying them
  on the committed root gives the event's `newRoot`. An observer does this for
  every transition, which checks tallied-as-recorded independently of the
  sequencers.
- **Recorded-as-cast.** A tracker proof is the vote-id leaf under an on-chain
  root; `davinci_client::api::verify_tracker` checks it against the registry.
- **Results are bound to the final root.** With a sequencer key the results
  guest proves the key and accumulator leaves under the final root and the 16
  Chaum–Pedersen decryptions, and the registry checks that root against its own.
  With a DKG key the registry checks the accumulator's inclusion itself and the
  committee proves every decryption share.
- **External input is hostile.** Census URIs come from on-chain data, so
  downloads are restricted (see [Operational notes](#operational-notes-and-known-limitations)).
  The API decodes strictly, caps bodies at 256 KiB, bounds every queue and
  times requests out after 60 s.

**Trust assumptions.** A sequencer key is held by one node, which can decrypt every
ballot published in the blobs and is the only party able to publish the results.
A DKG key moves both to a threshold of the committee (see
[Key modes](#key-modes)). Re-encryption scalars keep a 2^-7.6 bias (see
[Operational notes](#operational-notes-and-known-limitations)).

## Censuses

| Origin | Census | Root a transition may use |
|---|---|---|
| 1 | Static Merkle (lean-IMT), downloaded from `censusURI`. | The root fixed at creation. |
| 2 | Off-chain dynamic Merkle: the organizer replaces it with `setProcessCensus`. | The current root only. |
| 3 | An on-chain census contract; the node builds the lean-IMT from its `CensusMemberAdded` logs. | Any root the contract recorded at or after the process's creation block. |
| 4 | CSP: an ECDSA/secp256k1 signature from a census provider, whose address is the root. | The CSP address. |

**Ballot slots.** Each voter's ballot lives at one slot of the state tree. For a
Merkle census the slot comes from the voter address:
`slot = 0x10 + (be64(sha256("davinci-slot-v1" ‖ address20)[0..8]) mod (2^63 − 16))`.
A CSP census uses `0x10 + index`, with the index the CSP signs. The slot is not
the voter's position in the tree: a lean-IMT proof does not bind the leaf index,
so a proof of one leaf verifies at other indexes, and a growing tree would move
voters between slots. An address-derived slot stays put however the census
grows.

**Slot uniqueness.** Two members on one slot would overwrite each other's
ballots, so the census manager must keep slots unique. The census contract
rejects a registration whose slot is taken, the node refuses any Merkle census
(of any origin) with colliding slots, and the guest rejects two votes for one
slot in a batch. Grinding a collision into a census of N members costs about
2^63/N key generations.

**Origin 2.** Only the organizer can call `setProcessCensus`, with the same
origin, while the process is READY or PAUSED and before its end. It emits
`CensusUpdated`, and settlement accepts only the current root. A batch seals
against the current root; a `CensusUpdated` during a flight rolls the batch back,
and it is sealed again at the new root once that census loads. A pending vote
whose census leaf changed (the member was removed or reweighted) ends in
`error: census changed, recast`.

**Origin 3.** The registry accepts a transition's census root when the
contract's `getRootBlockNumber(root)` is non-zero, not in the future and not
before the process's creation block. At creation it refuses
`onchainAllowAnyValidRoot = true` and a census address without code. The node
seals against the newest confirmed root, and the pre-flight simulation re-checks
it before sending. The contract must be append-only with fixed weights: the
`davinci-zkvm` branch of davinci-onchain-census-contract checks slot uniqueness
on registration and never evicts old roots. Any `WeightChanged` with a non-zero
previous weight makes the node mark the census unusable.

Not supported: chained (folded) mode with dynamic censuses, and binding the slot
owner into the ballot leaf.

## Key modes

Every process has one of three key modes, chosen in `newProcess`. The organizer
talks only to the `ProcessRegistry`; the DKG sits behind it.

- **Sequencer** (`KeyMode::Sequencer`). The organizer calls
  `POST /processes/keys` on a node to get the election key; that node derives
  and owns the secret and is the only one that can publish results (the PLONK
  results proof path). It also holds a key that opens every ballot in the blobs,
  so this mode trusts one node with ballot secrecy. It suits testing and users
  who accept that trust.
- **DKG_AUTOMATIC** (`KeyMode::DkgAutomatic`). The process key comes from a
  davinci-dkg committee epoch: `PK_aid = P_j`, and the committee threshold-decrypts
  the final accumulator once the process ends. No sequencer and no organizer
  holds the secret; each committee member holds one share.
- **DKG_LOCKED** (`KeyMode::DkgLocked`). The key is `P_j + PK_org`, where `sk_org`
  is an organizer secret returned as `CreatedProcess.organizer_secret` at creation
  and never stored by the registry. The committee's partials do not begin until the
  organizer calls `reveal_process_key` (or `revealProcessKey` on the registry),
  which lets the organizer decide when the tally appears, but not which one.
  Losing `sk_org` loses the results.

The committee never reconstructs the election secret: it would open every
ballot in the blobs. It threshold-decrypts only the final accumulator, one
ciphertext per ballot field.

**How a DKG process gets its key.** The registry's constructor deploys a
`DavinciDKGAdapter`, which is the DKG application registrant and the only address
allowed to submit ciphertexts for it. A registry deployed without a DKG manager
has no adapter, and the DKG modes are disabled (`Error::DkgDisabled` in the
client). Each process gets an application id
`aid = keccak256(chainid ‖ registry ‖ pid) mod Q` (never zero), which the
organizer can compute from `getNextProcessId`. Automatic mode takes a free pool
key from the newest Live epoch. Locked mode names its epoch, because the
organizer's Schnorr proof of possession of `sk_org` binds `(eid, aid, PK_org)`;
`adapter.registrationEpoch()` tells clients which epoch to use. If the pool
empties or a new epoch goes Live between that read and the transaction,
`newProcess` reverts and the client retries once. The DKG works in reduced
twisted Edwards form (a = −1); the registry converts `PK_aid` to circomlib form,
and the genesis root pins it like any other key. The zkVM guests are the same in
every mode: the batch guest reads key leaf `0x03` whatever its source.

**Results.** In DKG modes no node holds a key. After the process ends, any
signing node sends `requestResultsDecryption(pid, accumulator, siblings)` from
its committed tree, on its first heartbeat after the end. The registry checks the
accumulator's SHA-256 SMT inclusion (key `0x04`) under the latest state root and
submits each active field's ciphertext to the committee through the adapter. It
accepts the request once per process and moves the process to ENDED, which only
the registry can leave (for RESULTS). This matters because the combined
plaintexts are public on the DKG before they reach the registry: a process left
READY could be canceled by an organizer who disliked the tally. The node then
polls until every ciphertext is combined and calls `finalizeResultsFromDKG`,
which reads the plaintexts, stores the results and emits `ProcessResultsSet`.

There is no results PLONK in DKG modes: the committee's Groth16 proofs of every
partial decryption and combine, and the registry's inclusion check, replace it.
A field whose ciphertext is the identity (a process with no votes) decrypts to 0
under any key, so the registry records 0 without the DKG, which rejects identity
points.

The Rust client API (`davinci_client::organizer`):

```rust
let next = org.next_process_id().await?;
let created = org.create_process(&NewProcess {
    process_id: next,
    key_mode: KeyMode::DkgLocked,
    ..
}).await?;
let sk = created.organizer_secret.unwrap();   // keep it
// ... votes, end ...
org.reveal_process_key(&created.pid, &sk).await?;
// wrong sk → Error::Reverted("InvalidOrganizerSecret")
```

**Trust and accepted risks.**
- A threshold of an epoch's committee can decrypt every ballot of the processes
  keyed on that epoch (with `sk_org` as well, in locked mode). The design trusts
  the threshold not to collude.
- A process's key belongs to one epoch's committee, and there is no resharing. If
  more than n − t of its members leave before the process ends, its results are
  lost. Operators must keep a committee up for the life of every process on it.
- The committee's discrete-log search stops at 2^50 per field. DAVINCI caps
  results well below that; a larger tally would taint the application.
- Anyone can create DKG-mode processes, or register applications on the DKG
  directly, and each takes a pool key, so sixteen cheap calls spend an epoch.
  The nodes then create the next epoch at once (`createEpoch` is allowed early
  once the newest pool is spent), and DKG-mode creation pauses for one epoch
  setup, about 2 minutes with the Gnosis windows.
  The attacker pays more gas than the committee. A fee or an allowlist is the
  answer if that stops being enough.
- Between the end and the first `requestResultsDecryption`, the organizer can
  still cancel a READY or PAUSED process without having seen the tally, the same
  power it has in sequencer mode. Sequencers request on their first heartbeat
  after the end to keep that window short.
- `revealProcessKey` works at any time. A locked organizer that reveals during
  voting drops that process to the automatic trust model.
- `registrationEpoch` looks back 8 epochs. If all 8 are spent or dead, automatic
  mode reverts `NoLiveEpoch` until a new epoch is Live. Locked mode names its
  epoch and is unaffected.

Not supported: key resharing across epochs, fees against pool draining, and
chained (folded) mode with DKG keys.

## Operational notes and known limitations

- **Sequencer keys.** The node that answered `POST /processes/keys` holds the
  only secret and is the only one that can publish results. Use a DKG mode when
  no single node should hold the key.
  - The key is `sk = HMAC-SHA256(master, "davinci-election-key-v1" ‖ chainId ‖
    registry ‖ processId)`, expanded to 64 bytes and reduced into `[1, l)`. Only
    the master secret sits in the redb file. A process whose key is not the one
    this node derives for its id is not this node's to finalize, so a key used
    under another process id has no decryptor. `davinci-client`'s organizer
    checks the created id. If another `newProcess` from the same account lands
    between the key request and the transaction, the process is still created
    and the organizer gets `Error::WrongProcessId` with its id. It must cancel
    that process (`setProcessStatus` CANCELED), since no node can finalize it.
  - The SDK's scalar multiplication is not constant-time, so run key-holding
    nodes on dedicated hosts.
  - `POST /processes/keys` is unauthenticated, as in davinci-node. It stores
    nothing, so the per-IP rate limit only bounds CPU.
  - The per-IP window keys on the full peer address: an IPv6 client can rotate
    through a /64 to dodge it, and behind a reverse proxy every request shares
    the proxy's IP (the node does not read `X-Forwarded-For`). Terminate at a
    proxy that enforces its own per-client limit, or expose the port directly
    and firewall IPv6 to /64 granularity, if key-generation abuse matters.
- **Racing sequencers can waste gas, never state.** An in-flight settlement
  tx may still broadcast after another sequencer's transition lands; the
  contract's root-continuity check reverts it and the node handles it as a
  lost race (rolls back, resyncs from the winner's blobs, requeues the
  votes). The only cost is the reverted transaction's gas.
- **Census.** Merkle voters get the address-derived ballot slot, so a growing
  tree does not move them; a census where two addresses share a slot is refused.
  Votes on origins 2 and 3 are checked against the latest root the node holds.
  - The census file is davinci-node's `{participants: [{key, weight}]}`; the
    census dump (`{root, participants}` with `address` and `addressIndex`,
    root checked) and JSONL (one participant per line; `application/x-ndjson`
    over http, sniffed for files) are also accepted. Weights may be JSON
    numbers up to 2^88 - 1.
  - Origin 3 is indexed from the contract's `CensusMemberAdded` logs at the
    confirmed head, every add replayed against the lean-IMT and its `newRoot`.
    A reorg below the indexed block drops the index and rescans. A contract
    that changes a weight or collides two slots is marked unusable and its
    votes are refused. The RPC must support `eth_call` at a block hash
    (EIP-1898). A provider that silently truncates `eth_getLogs` results
    fails closed: the replayed tree does not match `treeSize`/`getCensusRoot`
    and the sync backs off and retries. There is no flag to shrink the log
    span; the node halves it on range errors.
  - An origin-2 update is fetched in the background, newest root only, and
    votes answer 429 while it loads. An update that can never load (bad
    format, root mismatch, refused URI) answers 412 with the reason until the
    next `CensusUpdated`. An origin-2 process whose initial census is bad is
    ignored at bootstrap; the organizer has to create a new process. Stored
    censuses no process references are deleted once a newer root loads.
  - The on-chain root is authoritative.
  - Only `http(s)` URIs work by default, with public hosts only, no redirects,
    no proxy and a 256 MiB cap. `file://` needs `--census-dir`, and local hosts
    need `--census-allow-private`.
- **Blob retention.** Sync and late bootstrap read blobs from the beacon API
  (about 18 days of retention on mainnet). A node started after the blobs of an
  election were pruned cannot rebuild that election. Nodes archive and serve the
  blobs they saw, but no node fetches from another node's archive yet. Keep a
  node (or a blob archiver) running for the whole election. A late bootstrap also
  scans the registry logs from the process's creation block.
- **Reorgs.** Other sequencers' transitions are applied only once they are
  `--confirmations` blocks deep (default 2; the e2e uses 0 on anvil). The node's
  own commits are driven by the transaction receipt, at depth 0. A transaction
  the node saw land whose event never confirms is rolled back after 90 s and its
  votes requeued. If a fully committed own transition is reorged out, there is
  no rewind path: the node's copy of that process stays ahead of the chain and
  stops following it. Confirmation-gated own commits are future work.
- **Settle margin.** A transition that lands after the election window closes
  reverts, and its votes end in `error: process closed`. Set `--settle-margin`
  above the time it takes to prove and settle a full batch. The default of 120 s
  covers small batches only: a 1024-vote batch at nf=2 takes about 3 minutes to
  prove on an RTX 5090 (davinci-zkvm `BENCHMARK.md` has the table).
- **Prover failures.** Transient prover errors back off (up to about 5 minutes)
  with the votes still `pending`; they end in `error` only if the election closes
  first. A prover refusal (a rejected request or a failed job) or a guest
  `ok = 0` errors the votes once, with no retry.
- **Blob sidecar version.** At connect, `eth_config` decides whether the node
  sends v1 (cell proofs, Osaka) or v0 sidecars. It switches to the other
  version, and remembers it, if the RPC rejects a send for its version. Local
  anvil accepts both.
- **Scalar bias.** Re-encryption scalars are SHA-256 digests reduced mod the
  BN254 prime. The paper puts them 2^-7.6 from uniform modulo the subgroup
  order. A 512-bit reduction would close the gap, but it changes the batch guest
  and its program vk, so it is out of scope.

## arbo

`arbo/` is a Rust port of Go vocdoni/arbo: a binary sparse Merkle tree that
keeps the same node encodings, packed siblings, dump format and circomlib-style
proofs. The DAVINCI state tree (SHA-256, 64 levels, 8-byte keys) lives on it,
backed by the node's redb file. Its processor proofs are the SMT witnesses the
zkVM guest verifies byte for byte. All historical nodes are kept, so any past
root can be reopened, which is how the node rolls back a failed batch. The
tests replay Go-generated differential vectors, run the guest's own SMT
verifier as an oracle, and include proptests and fuzz targets (`arbo/fuzz`).
Rust beats Go on 9 of 10 benchmark rows; the one slower row
(per-add commits on redb) is a property of the storage backend. See
[arbo/README.md](arbo/README.md) and [arbo/BENCH.md](arbo/BENCH.md).

## End-to-end test

`e2e/tests/e2e.rs` (crate `davinci-e2e`, gated by `DAVINCI_E2E=1`) is the
acceptance test. It starts anvil (Osaka, 1 s blocks) and deploys `ZiskVerifier`
and `ProcessRegistry` pinned to the SDK release vks. It then runs three real
`davinci-sequencer` nodes and one observer without a key as subprocesses, all
sharing one davinci-zkvm prover, and drives the elections below through them. The
nodes run with `BATCH_MAX=8`, `BATCH_TIME=3s`, `SETTLE_MARGIN=20s`,
`CONFIRMATIONS=0` and a 1 s poll, so they race.

Run it inside a memory-capped systemd scope:

```bash
make e2e                                     # log in /tmp/davinci-e2e.log
DAVINCI_SEQUENCER_BIN=/path/to/bin make e2e  # use a prebuilt node binary
```

The scope matters because anvil and the nodes are only killed when the test's
handles are dropped. A killed or crashed test binary leaves them to the scope.
`E2E_MEMORY` (default `16G`) and `E2E_LOG` override the cap and the log path.

Prerequisites:
- anvil and forge in `~/.foundry/bin` or on `PATH`;
- the prover service at `DAVINCI_ZKVM_URL` (default `http://127.0.0.1:8080`);
- the forge project at `DAVINCI_CONTRACTS_DIR` (default `../davinci-contracts`);
- the census contract project at `DAVINCI_CENSUS_CONTRACT_DIR` (default
  `../davinci-onchain-census-contract`, branch `davinci-zkvm`), needed
  for the origin-3 scenario (deploys `OwnedCensus` with PoseidonT3);
- the circom artifacts at `CIRCOM_ARTIFACTS` (default `../davinci-circom/artifacts`).

**DKG scenarios.** `DAVINCI_E2E_DKG=1` enables three additional processes that exercise
`DKG_AUTOMATIC`, `DKG_LOCKED` (with reveal) and a zero-vote automatic process. On anvil
the harness builds and runs three `davinci-dkg-node` processes (dev accounts 5–7 as operators,
8 as the DKG deployer) and waits for an epoch to go Live before the registry is deployed.
The node binary is built from `DAVINCI_DKG_DIR` (default `../davinci-dkg`) unless
`DAVINCI_E2E_DKG_NODE_BIN` is set; artifacts go to `~/.cache/davinci-dkg-artifacts` (~1.1 GB
on first download). A live run uses the external committee of the registry
adapter's `DKGManager` (`DAVINCI_E2E_DKG_MANAGER` overrides it) and starts no nodes.

**Live runs.** `DAVINCI_E2E_LIVE=1` runs the same scenario on an existing
deployment, by default the `gnosis` network of `davinci_client::networks`
(`e2e/src/net.rs` lists the `DAVINCI_E2E_RPC`, `DAVINCI_E2E_NODE_RPCS`,
`DAVINCI_E2E_BEACON`, `DAVINCI_E2E_REGISTRY` and `DAVINCI_E2E_FROM_BLOCK`
overrides). It needs key
files in `DAVINCI_E2E_ORGANIZER_KEY` and `DAVINCI_E2E_SEQUENCER_KEYS` (comma
separated), prints only addresses, checks the registry pins first and wants 0.05
of the native token in every account. `DAVINCI_E2E_RESILIENCE=1` adds full RPC,
prover and beacon outages and an observer RPC failover, through loopback
proxies. `DAVINCI_E2E_NEGATIVE=1` replays settled transactions, tampered, through
`eth_call`. Every run ends with its gas bill.

**Benchmark.** `DAVINCI_E2E_BENCH=sizes cargo test -p davinci-e2e --test bench --
--nocapture` proves a first and a steady batch per size on one node (sizes from
`DAVINCI_E2E_BENCH_SIZES`, fields from `DAVINCI_E2E_BENCH_NF`), on anvil or live.
`DAVINCI_E2E_BENCH_OUT` names a file for the Markdown table.

**Throughput.** `DAVINCI_E2E_BENCH=throughput cargo test -p davinci-e2e --test
throughput -- --nocapture` measures sustained votes per second with several
provers. It starts one node per URL in `DAVINCI_E2E_BENCH_PROVERS` (comma
separated), and each node sequences its own origin-1 process, so the provers
never race on one root chain. Every ballot is proved first
(`DAVINCI_E2E_BENCH_VOTES` per process, default 2048, at
`DAVINCI_E2E_BENCH_NF`, default 2). The clock then runs from the first submit
to the last settlement, with `--batch-max` set by `DAVINCI_E2E_BENCH_BATCH`
(default 512). Each node reaches its prover through a loopback proxy that
records its `/prove` jobs. The report has per-batch queue and proving times
from the prover's job API, overall and steady-state votes/s, each prover's
share and its GPU busy fraction. The run fails if a vote errors or goes
missing. It then ends the processes and checks the tally on-chain
(`DAVINCI_E2E_BENCH_RESULTS=0` skips that). The other knobs are listed at the
top of `e2e/tests/throughput.rs`. A steady batch of N votes carries up to N
refreshes, and 1024 + 1024 needs about 41 GB of RAM in the prover, so stay at
512 on a 64 GB prover host.

On two RTX 5090 provers, each serving a node with two processes of 2048 voters
at nf=2 and batches of 512, the system sustained **12.07 votes/s** (8192 votes
settled in 678.7 s; 12.45 votes/s in steady state), with both GPUs busy the
whole time. The full report, hardware and image versions are in davinci-zkvm's
`BENCHMARK.md`.

`e2e/bench.sh` runs either benchmark in the `e2e/Dockerfile.bench` image, with
nothing native on the host. The nodes are the binary from
`ghcr.io/vocdoni/davinci-sequencer:main` (`NODE_IMAGE` overrides it), anvil
and forge come from foundry v1.8.3, and the harness is built in the container
from the mounted checkouts. The container uses the host network, so provers
on loopback or a private network are reachable. It runs as your user and
keeps cargo's registry and target dir in the `davinci-bench-cache` volume.
Reports, logs and the datadirs of a failed run go to `~/.cache/davinci-bench`
(`BENCH_RUNS`).

```bash
DAVINCI_E2E_BENCH_PROVERS=http://127.0.0.1:8080,http://10.200.0.27:8080 e2e/bench.sh
DAVINCI_E2E_BENCH=sizes e2e/bench.sh    # the batch-size benchmark
```

The sibling checkouts default to the directories next to this one.
`DAVINCI_ZKVM_DIR`, `DAVINCI_CONTRACTS_DIR`, `DAVINCI_CENSUS_CONTRACT_DIR`
and `CIRCOM_ARTIFACTS` point the script elsewhere.

**Demo elections.** `e2e/tests/demo.rs`, gated by `DAVINCI_E2E_DEMO`, fills a
live deployment with eight elections, one of every ballot kind, census origin,
key mode and lifecycle (`davinci_e2e::demo::elections`), through nodes that
already run (`DAVINCI_DEMO_NODES`, default ports 9090 and 9091). `prepare` draws
the voter keys into `~/.davinci-gnosis/demo` (mode 0700, never the repository)
and writes the public census and metadata files into `e2e/demo/`. `check`
proves every planned ballot against the circuit, offline. Once the files are
pushed, `run` checks they are served unchanged, creates the elections with
their URIs under `DAVINCI_DEMO_BASE_URL`, casts the votes, ends, reveals and
cancels, and prints a table (with links when `DAVINCI_DEMO_EXPLORER_URL`
names an explorer). It records its progress in
`state.json` next to the keys and resumes from it.

```bash
DAVINCI_E2E_DEMO=prepare e2e/bench.sh
DAVINCI_E2E_DEMO=check e2e/bench.sh
DAVINCI_E2E_DEMO=run \
  DAVINCI_DEMO_BASE_URL=https://raw.githubusercontent.com/vocdoni/davinci-sequencer/<commit>/e2e/demo \
  e2e/bench.sh    # organizer key: DAVINCI_DEMO_ORGANIZER_KEY, default ~/gnosis-chain-privkey.txt
```

Without `DAVINCI_SEQUENCER_BIN`, the test builds the node with
`cargo build --release -p davinci-sequencer`. On failure it keeps the node logs,
anvil log and datadirs and prints their paths. `DAVINCI_E2E_NODE_LOG` sets the
nodes' log filter. `DAVINCI_E2E_SETUP=1` runs only the part before the nodes:
chain, contracts, both processes and all ballots. That part needs neither the
prover nor the node binary.

The vk fail-fast compares the registry pins and each node's `/info` against
`davinci_zkvm_sdk::release`. The prover's `/health` carries no vks, so the
prover's own vks are only exercised through the nodes' publics checks.

The scenario runs four processes in order on the same chain and nodes:

- **Process 1:** 24-voter Merkle census (`file://`), nf=4, election key from node A.
  - Voters vote round-robin across the three nodes.
  - 4 identical packages also go to a second node.
  - 8 voters later revote through a node that did not see their first ballot.
- **Process 2:** a CSP census with 3 voters sent to node C (key from node C).
- **Four rejected packages**, checked by HTTP status and error code:
  - bad proof: 400/40002;
  - bad signature: 400/40002;
  - address not in the census: 400/40001;
  - reused vote id: 409/40901.
- **Process 3 (origin 3):** `OwnedCensus` growing from 3 to 18 members while
  voting runs round-robin. A duplicate `addMember` reverts with
  `AlreadyRegisteredAddress` (slot already taken). Node B is killed between a
  batch seal and its settlement and restarted on the same datadir, so the
  exposed set has to survive the restart. 2 revotes through another node.
  Expected: 18 voters, 2 overwrites.
- **Process 4 (origin 2):** 6-member off-chain census. 4 votes land before
  `setProcessCensus` replaces the census with a 12-member JSONL that reweights
  one member. That member's pending vote errors with "census changed, recast".
  6 new members vote and 1 original voter revotes. Expected: 10 voters, 1 overwrite.

Process 1 and 2 assert roots, tracker proofs, tally and inter-node races.
Process 3 and 4 additionally assert census growth and update, recovery across
the restart, and the reweight-induced error.

On anvil with one RTX 5090 prover (2026-09-27), processes 1 and 2 take about
5 minutes and processes 3 and 4 about 9 more:

| Phase (processes 1 and 2) | Time |
|---|---|
| round 1 (31 packages) settled | 172 s |
| round 2 (8 revotes) settled | 102 s |
| results on-chain after the end | 18 s |

- Transitions: 7 for process 1 (from 3 distinct senders), 1 for process 2.
- Node counters in that run (settled by self / synced from others / lost races):
  - node A: 3 / 5 / 0;
  - node B: 2 / 6 / 3;
  - node C: 3 / 5 / 5;
  - observer: 0 / 8 / 0.

## Deployments

### Gnosis chain

Chain 100, deployed 2026-09-28 with the DKG key modes. All contracts are
source-verified on gnosisscan.io. Earlier registries on Gnosis are retired.

DAVINCI contracts:

| Contract | Address | Block |
|---|---|---|
| ZiskVerifier | `0x0DBeF559Cccb2A085D9D9Ac3a13b67148bdB9936` | 48483866 |
| ProcessRegistry | `0x48a5091B64434a6690AeA32455712Bd2b7EE3E77` | 48483867 |
| DavinciDKGAdapter | `0xd79B9B55830Bf6C277850c56B29Fe9b75cF2543b` | 48483867 |

DKG contracts:

| Contract | Address | Block |
|---|---|---|
| DKGManager | `0x9999f38ff8bf959e98ddd5d4551f82775219c01b` | 48483860 |
| DKGAppManager | `0xd4d8f9708c380d81aec294b199081c5d2c782087` | 48483862 |
| DKGRegistry | `0x45ab8b64633076ddc020b12d1f1325fa55f629c5` | 48483859 |
| ContributionVerifier | `0x6d198bc613205957444b53a09bb22ed7bc650912` | 48483855 |
| FinalizeVerifier | `0xc354ea7f3ef6db4ca0b89a1a5a6395c2d6126b38` | 48483856 |
| PartialDecryptVerifier | `0x0f19886ee73fd74e3f88ce3a061490facd7561db` | 48483857 |
| DecryptCombineVerifier | `0x2980e664edef91f554cc75b15cb8eeea61586644` | 48483858 |

Pinned values, equal to the SDK release pins:

| Value | Where | Hash |
|---|---|---|
| batch program vk | registry `batchProgramVK` | `0x6cfc89d562d0b22f04478a5c15b390433eb52f1b03147030b183076260da7a10` |
| results program vk | registry `resultsProgramVK` | `0x7bc8c5e9235548386a44b1885732a2a7ffb1badddc8c7fba599d07ece47be794` |
| `rootCVadcopFinal` | registry and verifier | `0x05006517b6ccde5da4d890587ba62845b5af8a307c00e87d4b9d05099b16dc80` |
| ballot VK hash | registry `ballotVKHash`, state leaf `0x07` | `0xbf1e6590bb1ba883d601c4d7d1c6fa2722a78590716874019db6d68fc776bb0e` |
| verifier code keccak256 | `ZiskVerifier` runtime code | `0x82385a405b7301345d7e246017846ca3228aaea349cb68b116d76e0e77056566` |

Rebuilding a guest changes its program vk, which needs a new registry. Three
checks refuse a mismatch: the node's boot check, `davinci_client::verify_registry`
for clients, and davinci-contracts' `script/verify_deployment.py`, which compares
the deployed runtime code with a local build (immutables masked) and reads back
every pin:

```bash
python3 script/verify_deployment.py --rpc https://rpc.gnosischain.com \
  --registry 0x48a5091B64434a6690AeA32455712Bd2b7EE3E77 --chain-id 100 \
  --batch-vk  0x6cfc89d562d0b22f04478a5c15b390433eb52f1b03147030b183076260da7a10 \
  --results-vk 0x7bc8c5e9235548386a44b1885732a2a7ffb1badddc8c7fba599d07ece47be794 \
  --root-c 0x05006517b6ccde5da4d890587ba62845b5af8a307c00e87d4b9d05099b16dc80 \
  --ballot-vk-hash 0xbf1e6590bb1ba883d601c4d7d1c6fa2722a78590716874019db6d68fc776bb0e
```

Deploy order: the DKG contracts (`DeployAll.s.sol` in davinci-dkg), then the
`ProcessRegistry` (constructor: verifier, vks, `dkgManager`), which creates the
adapter.

**Node configuration.** The `gnosis` network (the default) carries the
registry, its start block, the RPCs, the beacon API and 3 confirmations:

```bash
davinci-sequencer --privkey-file /path/to/key --prover-url http://127.0.0.1:8080
```

**The DKG committee.** Three operators run the `davinci-dkg` image, each in its
own container with its key (`DAVINCI_DKG_PRIVKEY`) in a mode-0600 environment
file. Auto-create is on with the
epoch policy threshold 2, committee 3, minimum valid 2, alpha 10000, so the
nodes open a new epoch when the current pool is spent. The DKG runs a fast
configuration: epochs of 17280 blocks (a day at 5 s), committee selection 8
blocks, key assembly 12, finalize gap 1, policy bounds t ≥ 2, n ≥ 3, α ≤ 20000 (basis points). The first
epoch, `aab5fe7d0000000000000001`, went Live about 2 minutes after
`createEpoch`. Application registration on the DKGAppManager is open to
any contract, so other applications can share the committee.

**Chain facts.**
- Gnosis runs Fusaka: blob transactions carry EIP-7594 cell-proof sidecars. The
  node detects this from `eth_config`, or from the P256VERIFY probe on RPCs
  without it.
- A block takes at most 2 blobs. The node reads the cap from `eth_config` or its
  chain table and sizes batches to fit; a batch that needs more blobs is split
  into several transactions.
- The blob base fee sits at its 1 gwei floor and the execution base fee at a few
  wei. A settlement uses about 400–460k gas and costs almost only its blob fee,
  about 0.00013 xDAI per blob.
- Slots are 5 s. Use `--confirmations 3`.

| RPC | Notes |
|---|---|
| publicnode | Needs a User-Agent (the node sends one). `eth_config`, 50k-block logs, fast. Rate-limits bursts. |
| BlockReq | Needs a User-Agent. `eth_config`, 50k-block logs. |
| rpc.gnosischain.com | No `eth_config` and no `eth_blobBaseFee`: the node falls back to the P256 probe, the chain table and `eth_feeHistory`. |
| Pocket | Intermittent "historical state is not available"; refuses calls without an explicit block tag (the node always sends one); slow. Fine as a secondary. |

**Live e2e.** `make e2e` with `DAVINCI_E2E_LIVE=1 DAVINCI_E2E_DKG=1
DAVINCI_E2E_RESILIENCE=1 DAVINCI_E2E_NEGATIVE=1` passed on this deployment on
2026-09-28 in 41.3 minutes. Three signing sequencers and an observer on
different RPCs shared one RTX 5090 prover, and the committee above ran the DKG
processes. The run sent 60 transactions for 0.0123 xDAI in total; each DKG
operator has spent about 1e-8 xDAI on its epochs, partial decryptions and
combines since it was funded.

| Process | Key mode | Tally on-chain, equal to the expected one |
|---|---|---|
| origin 1, static Merkle | sequencer | `[36, 24, 14, 50]` |
| origin 4, CSP | sequencer | `[6, 3, 2, 7]` |
| origin 3, `OwnedCensus` 3 → 18 | sequencer | `[29, 18, 8, 34]` |
| origin 2, organizer-updated 6 → 12 | sequencer | `[16, 9, 3, 17]` |
| origin 1 | DKG automatic | `[9, 6, 4, 11]`, 99 s after the end; request to results took 7 blocks |
| origin 1 | DKG locked | `[9, 6, 2, 14]`, 53 s after the reveal (8 partials) |
| origin 1, no votes | DKG automatic | `[0, 0, 0, 0]`, set by the request itself, nothing sent to the DKG |

Before the reveal, the locked process had no partial decryption for 19 blocks
after its request, and `finalizeResultsFromDKG` reverted `ResultsNotReady`. A
reveal with a wrong secret was refused (`InvalidOrganizerSecret`), and the
sequencer refused a ballot encrypted for another process (400/40002). The run
went through an observer RPC failover, a prover outage, a sequencer kill and
restart, a full RPC outage on one node and a beacon outage.

The negative checks, run as `eth_call` against the live registry, all reverted
as expected, and their controls passed:
- a replayed transition: `InvalidStateRoot`;
- a flipped proof byte, a flipped public value or a tampered tally:
  `InvalidProof`;
- an extra blob in the arrays or an extra blob hash: `BlobCountMismatch`;
- `setProcessDuration` after the end: `InvalidStatus`;
- `setProcessCensus` on a static census: `CensusNotUpdatable`;
- a tampered accumulator coordinate, a flipped sibling byte or another
  process's accumulator in `requestResultsDecryption`: `InvalidInclusionProof`;
- an accumulator coordinate equal to Q: `InvalidAccumulator`;
- a second request: `ResultsAlreadyRequested`;
- finalize before the combines: `ResultsNotReady`;
- a cancel after the request: `InvalidStatus`;
- `setProcessResults` on a DKG process: `InvalidKeyMode`.

**Batch-size benchmark.** Measured on Gnosis on 2026-09-27, against the
previous registry, with one RTX 5090 prover and one node per case
(`--batch-max N`, `--batch-time 2m`, 3 confirmations). Each case is an origin-1
process with a 2N-voter census: the first batch holds voters 0..N and no
refreshes, the steady batch voters N..2N plus N silent refreshes (2N slot
updates). Times are seconds from the first submit of the batch: "sealed" is
when none of its votes is still pending, "settled" when all are on-chain. Cost is
gas plus blob fee, summed over the batch's transactions.

| N | nf | batch | refreshes | txs | blobs | gas | cost (xDAI) | sealed s | settled s |
|---:|---:|---|---:|---:|---|---:|---:|---:|---:|
| 4 | 4 | first | 0 | 1 | 1 | 439141 | 0.000131 | 2.0 | 27.1 |
| 4 | 4 | steady | 4 | 1 | 1 | 404318 | 0.000131 | 1.0 | 25.1 |
| 16 | 4 | first | 0 | 1 | 1 | 440346 | 0.000131 | 2.1 | 32.1 |
| 16 | 4 | steady | 16 | 1 | 1 | 405273 | 0.000131 | 1.0 | 21.1 |
| 64 | 4 | first | 0 | 1 | 1 | 438785 | 0.000131 | 2.2 | 30.3 |
| 64 | 4 | steady | 64 | 1 | 1 | 406911 | 0.000131 | 1.2 | 35.4 |
| 128 | 4 | first | 0 | 1 | 1 | 441022 | 0.000131 | 2.3 | 42.7 |
| 128 | 4 | steady | 128 | 1 | 1 | 404609 | 0.000131 | 1.3 | 43.8 |
| 256 | 4 | first | 0 | 1 | 1 | 440084 | 0.000131 | 2.6 | 61.6 |
| 256 | 4 | steady | 256 | 1 | 2 | 460648 | 0.000262 | 1.6 | 64.7 |
| 512 | 4 | first | 0 | 1 | 2 | 495762 | 0.000262 | 3.3 | 104.6 |
| 512 | 4 | steady | 512 | 2 | 2+1 | 864966 | 0.000393 | 124.3 | 160.5 |
| 64 | 16 | first | 0 | 1 | 1 | 439663 | 0.000131 | 2.2 | 32.4 |
| 64 | 16 | steady | 64 | 1 | 2 | 461562 | 0.000262 | 1.2 | 41.4 |

The 512-vote steady batch needed three blobs, more than a Gnosis block takes, so
it went out as two transactions (2+1); the remainder waited for the 2-minute
batch timer, hence its sealed time. Settlement cost is almost all blob fee,
0.000131 xDAI per blob. The client-side circom ballot proofs took about 0.16 s
each (1024 in 164.9 s).

## License

AGPL-3.0, see [LICENSE](LICENSE).
