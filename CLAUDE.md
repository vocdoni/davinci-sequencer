# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

The DAVINCI sequencer in Rust, built on the davinci-zkvm prover. `README.md` is the
project documentation: config, API, error codes, security properties, censuses, key
modes, deployments and limitations. The guest specs live in davinci-zkvm:
`circuit/CIRCUIT.md` (vote batch, wire format) and `circuit-results/RESULTS.md`.

## Conventions

- Stage by explicit filename.
- Don't edit sibling checkouts (`../davinci-circom`, `../davinci-node`) from this
  repo. Never read or print secrets (`*privkey*`, `.env` values).
- Plain, human voice in comments and docs. Terse. One short line above a function is
  usually enough. No marketing register, no em-dash overuse.
- `#![forbid(unsafe_code)]`; no panics on external input; `thiserror` in libraries.
- Secrets (re-encryption seed, refresh selection, election secret keys) are never
  logged; the seed and the selection are never persisted. Exception, the exposed
  set: once a batch is sealed, every ballot slot it changed (writes ∪ refreshes)
  joins the process's persisted `exposed` set. That is exactly the key list the
  blob already publishes, so persisting it leaks nothing new.

## Layout

| Crate | Role |
|---|---|
| `arbo/` | Sparse Merkle tree, a port of vocdoni/arbo (SHA-256, circom-compatible proofs). Bench numbers in `arbo/BENCH.md`, Go vector generator in `arbo/testdata/gen`, fuzz targets in `arbo/fuzz` |
| `state/` (`davinci-state`) | Per-process state, vote validation, transition builder, blob sync, results request. No IO besides arbo storage |
| `sequencer/` (`davinci-sequencer`) | The node: config, redb storage, keys, census (`census/onchain.rs` — origin-3 lean-IMT indexer), web3 (alloy), monitor, process actors (`actor.rs`), finalizer, API (`api/`) |
| `client/` (`davinci-client`) | API wire types (`api.rs`, the wire format owner) + client, organizer and voter tooling (circom prover behind feature `prover`) |
| `e2e/` (`davinci-e2e`) | Test-only: the end-to-end acceptance test. Spawns the real node binary as subprocesses |

Outside this repo:

| Path | Role |
|---|---|
| `../davinci-zkvm/rust-sdk` (`davinci-zkvm-sdk`) | Path dependency: wire types, protocol primitives, blob codec, publics parser, release pins (`release.rs`), embedded ballot VK |
| `../davinci-zkvm/input-gen` | Dev-dependency of `davinci-state` for the ziskemu dry runs |
| `../davinci-contracts` (branch `zkvm`) | `ProcessRegistry` and `DavinciDKGAdapter`; ABIs vendored in `sequencer/abi/` (the client's `sol!` reads them from there too) |
| `../davinci-dkg` | DKG contracts and `davinci-dkg-node`, built by the e2e DKG scenarios (`DAVINCI_DKG_DIR`) |
| `../davinci-onchain-census-contract` (branch `davinci-zkvm`) | Origin-3 census contract (`OwnedCensus`) for `census_anvil` and the e2e |
| `../davinci-circom/artifacts` | Ballot circuit wasm/zkey for the client prover and the proof-backed tests (`CIRCOM_ARTIFACTS` overrides) |

## Commands

```bash
make build                    # cargo build --workspace
make test                     # cargo test --workspace; gated tests skip
make e2e                      # acceptance test in a systemd scope, log /tmp/davinci-e2e.log
cargo test -p <crate>
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
# image; run from the directory holding davinci-sequencer/ and davinci-zkvm/
docker build -f davinci-sequencer/Dockerfile -t davinci-sequencer .
```

CI (`.github/workflows/main.yml`, `ubuntu-latest`, no GPU) checks the siblings out
beside the repo at the refs listed in the workflow header and runs fmt/clippy,
`cargo test --workspace` with `ANVIL=1`, the `DAVINCI_E2E_SETUP=1` run, the ziskemu dry
runs (CPU ZisK, `--nokey`) and the Docker build. Never add a job that needs the prover.
Branch pushes publish the image under the branch name; a `vX.Y.Z` tag publishes
`:vX.Y.Z` and `:latest`, which `docker-compose.yml` and Watchtower follow.

Gated tests (they print a skip line and PASS when the variable is unset, so check the
output before claiming they ran):

```bash
# guest oracle: Rust-built transitions through ziskemu, all 64 registers compared
ZISKEMU=1 cargo test -p davinci-state --test dryrun -- --test-threads=1
#   ZISKEMU_BIN (default ~/.zisk/bin/ziskemu), CIRCUIT_ELF_PATH (default ../davinci-zkvm/circuit/elf/circuit.elf)

# real contracts on anvil; needs `forge build` in DAVINCI_CONTRACTS_DIR (default ../davinci-contracts)
ANVIL=1 cargo test -p davinci-sequencer --test web3_anvil
ANVIL=1 cargo test -p davinci-client --test organizer

# circom prover
CIRCOM_ARTIFACTS=$PWD/../davinci-circom/artifacts cargo test -p davinci-client --test prover

# arbo extras
cargo test -p arbo --features sdk-poseidon
ARBO_BENCH_1M=1 cargo bench -p arbo
cd arbo && cargo +nightly fuzz run <target> -- -max_total_time=300

# e2e setup only (chain, contracts, processes, ballots; no prover, no node binary)
DAVINCI_E2E_SETUP=1 cargo test -p davinci-e2e --test e2e setup_without_nodes -- --nocapture
```

The ungated `state/tests/validate.rs` and `sequencer/tests/api.rs` use real ballot
proofs cached under `state/tests/cache` and `sequencer/tests/cache` (gitignored, keyed
by inputs hash); a cold cache proves them from `CIRCOM_ARTIFACTS` (same default path).

`anvil`/`forge` live in `~/.foundry/bin`; `ziskemu` in `~/.zisk/bin`.

## Gotchas

**Byte orders.** The same integer travels in up to four byte orders, and a wrong one
does not error: the guest commits `ok = 0` or the contract reverts. Read the wire
format in davinci-zkvm `circuit/CIRCUIT.md` before touching anything that encodes.
Short version:
- state roots are the raw arbo digest everywhere (publics `reg32`, contract
  `stateRoot`, `TrackerProof.root`);
- STATETX SMT fields and the STATETX process id are arbo-LE hex;
- hashed leaf values are `int_be(sha256(..))` sent arbo-LE, so the wire bytes are
  the reversed digest;
- ballot coordinates, census values, `kzg.process_id` and `kzg.root_hash_before` are
  BE hex;
- the contract `censusRoot` is the census root integer's LE bytes, and the contract
  `processId` is BE (the client's `ProcessId` is the 31-byte BE form).

The SDK wraps these in named newtypes. The ziskemu dry runs are the byte-exactness
oracle: after changing any wire struct, run them.

**Release pins.**
- `rust-sdk/src/release.rs` pins `BATCH_PROGRAM_VK`, `RESULTS_PROGRAM_VK` and
  `ROOT_C_VADCOP_FINAL`. They must equal:
  - the ELFs the prover runs;
  - the immutables of the deployed `ProcessRegistry`;
  - go-sdk `CircuitRelease`.
- The node refuses any snark whose `program_vk` or `root_c_vadcop_final` differs:
  the votes end in `error` mentioning the vk pin.
- Rebuilding either guest or changing the ZisK snark setup means refreezing
  (`release.rs` doc comment) and redeploying the registry.
- The ballot VK is the embedded `rust-sdk/assets/ballot_proof_vkey.json` (from the
  davinci-circom artifacts), not go-sdk's v1.0.0 VK.
  - Its hash is config leaf `0x07` and the registry's `ballotVKHash`.
  - A mismatched `--ballot-vk` makes every process fail the genesis check and be
    ignored.

**e2e.**
- Always run it through `make e2e` (a `systemd-run --user --scope` with
  `MemoryMax`). anvil and the node subprocesses die only when the test's handles
  drop, so a killed test binary would leave them running unbounded.
- If `sequencer/` may change during a run, build once with
  `cargo build --release -p davinci-sequencer`, copy the binary out of the tree, and
  pass it as `DAVINCI_SEQUENCER_BIN`. Otherwise the test rebuilds whatever is in the
  tree mid-edit.
- Use a release build. A debug node binary is far more stack-hungry (see the blob
  gotcha).
- The prover is FIFO; other jobs queued on it slow the run. Restart a shared prover
  only with `queue_len` 0.

**Blobs are 128 KiB: never deserialize or pass one by value through serde or deep
call chains.**
- Deserializing anvil's response into `eip4844::Blob` overflowed the 2 MiB tokio
  worker stack in debug builds and aborted every syncing node.
- `AnvilBlobs` deserializes into heap `Bytes`.
- `sequencer/tests/stack.rs` runs the path on a 512 KiB thread to catch a
  regression.

**CPU-heavy state work stays off the async workers.**
- In the actor, go through `actor.rs::cpu()` (`block_in_place` on the multi-thread
  runtime, inline on the current-thread test runtime). Don't call
  `block_in_place` directly: it panics on a current-thread runtime. This covers
  decode, apply, prepare and results.
- The API runs `validate_vote` in `spawn_blocking` under a semaphore.
- Census proofs go through `CensusStore::proof_async`: a membership lookup first,
  then a single-flight rebuild on the blocking pool.

**Census fetches are attacker-controlled** (the URI comes from on-chain data).
- `file://` is refused unless `--census-dir` is set. When it is set, the path is
  canonicalised, must stay under that directory, must be a regular file, and may
  not be under `/proc`, `/sys` or `/dev`. `/` is refused as the directory.
- HTTP(S):
  - no redirects;
  - `.no_proxy()`, so proxy env vars can't bypass the filter;
  - a public-only resolver (IPv4 and IPv6 special ranges, including NAT64 and
    6to4);
  - loopback and private hosts only with `--census-allow-private`;
  - a 256 MiB body cap and a participant cap;
  - 2 build permits;
  - a bounded failure-backoff map.
- 4xx (except 408/429) and policy refusals are permanent (the process is ignored);
  network errors are retried.
- Tests that serve a census from localhost need `CensusOptions::allow_private`
  (`--census-allow-private`).

**DKG key modes** (README "Key modes"): a process's key is a sequencer's (derived,
results PLONK) or a davinci-dkg committee's (`DKG_AUTOMATIC`, `DKG_LOCKED`). In the
DKG modes no node holds a key: once the grace window closes any signing node sends
`requestResultsDecryption` from its committed tree (this also moves the process to
ENDED), polls, then `finalizeResultsFromDKG` (`finalize.rs::run_finalize_dkg`). No
results PLONK is needed in DKG modes; the committee's Groth16 proofs replace it.

- The e2e flag is `DAVINCI_E2E_DKG=1`; live runs use the `DKGManager` of the
  registry's adapter (`DAVINCI_E2E_DKG_MANAGER` overrides it). On anvil, dev
  accounts 5–7 are DKG operators and 8 is the DKG deployer; the harness builds the
  node from `DAVINCI_DKG_DIR` (default `../davinci-dkg`) and needs ~1.1 GB of
  circuit artifacts in `~/.cache/davinci-dkg-artifacts`.
- DKG application registration is open to any account; the harness checks it
  with a simulated `registerApplication` from the organizer.
- Schema version 6 added `OnchainProcess.key_mode` and `OnchainProcess.dkg`; bump it
  on any record-shape change.

**Observer nodes** (no signing key):
- they refuse `POST /votes` and `POST /processes/keys` with 412/41203;
- they never seal or finalize, even when the keystore holds the election key
  (`maybe_finalize` checks the signer);
- they serve every read.

**Secrets.**
- Never log or persist the seed or the refresh selection. Both come from `OsRng`
  inside `prepare`.
- `PreparedBatch` and the SDK's `ReencryptionJson` have redacted `Debug`. Keep it
  that way, and never `{:?}` a `ProveRequest`.
- The `PreparedBatch` never leaves the actor; the job gets clones of the request
  parts only.
- `CircomInputs`/`PreparedVote` redact the voter secret `k`.
- The node key is a `SecretString` (redacted, zeroized), read only from
  `DAVINCI_PRIVKEY` or `--privkey-file`.
- Election keys are derived statelessly: `sk = reduce(HMAC-SHA256(master,
  "davinci-election-key-v1" ‖ chainId ‖ registry ‖ processId))` into `[1, l)`.
  The only thing in the `enc_keys` table is one 32-byte master secret (key
  `"master"`), drawn on first boot. The file is 0600, the datadir 0700. Losing
  the master secret loses every election key this node has handed out.

**Ingest must mirror the guest.**
- `davinci-state::validate_vote` repeats every per-vote guest check, so one ballot
  can't sink a GPU batch. When the guest gains a check, add it there, with a named
  `VoteError` and a test in `state/tests/validate.rs`.
- The contract enforces `maxVoters` per batch (`votersCount + new voters <=
  maxVoters`). `select_batch` and actor admission both mirror it.
- `select_batch` also applies the six-blob cap, counting the refreshes the guest
  will demand.

**Chain following.**
- Events are routed only up to `head - confirmations`, and heartbeats carry that
  confirmed block too.
- Own commits are receipt-driven at depth 0, with no rewind for a reorged committed
  own transition (known limitation, documented in the README).
- `events()` has no pid topic filter: every actor sees every process's events.
- A failed bootstrap never stalls routing. It goes on a persisted retry list
  (`meta_bytes` key `monitor_boot_retries`), and the process is ignored after 10
  attempts.
- A late bootstrap scans the registry logs from `creation_block` and replays every
  transition from blobs, so the blob source must still hold them.

**Networks.**
- `client/src/networks.rs` is the one table of known deployments (`gnosis`: chain,
  registry, start block, RPCs, blob source, confirmations). The node's `--network`
  presets, the e2e live defaults and the tooling read it. The registry and start
  block are updated there on every redeploy.
- `Config::load`/`Config::parse_args` resolve the network; a bare
  `Config::try_parse_from` leaves `registry`, `rpc_url`, `blob_source`,
  `confirmations` and `start_block` unset. Tests build configs with
  `--network custom`.
- Explicit settings win over the preset. The preset's start block applies only
  with its own registry. The RPC chain id must match the preset's unless
  `--registry` is explicit.

**Storage.**
- Records are JSON in redb. Bump `storage.rs::SCHEMA_VERSION` (now 8) on any
  record-shape change; the node refuses a mismatched file.
- One database per deployment: `<datadir>/<chain id>-0x<registry>/sequencer.redb`
  (`Db::open_deployment`), bound to it by the `meta_bytes` key `deployment`. A new
  registry starts empty and leaves the old directory alone. A flat-layout
  `<datadir>/sequencer.redb` moves in once when its process ids carry the
  deployment's prefix (`storage::pid_prefix`), else it stays.
- redb holds a file lock: a second process can't open the same database, and
  restart tests reopen with a retry loop.
- The fault-injection hook `Db::fail_next_arbo_writes` exists only with feature
  `test-hooks`. The crate enables it for its own tests through a self
  dev-dependency, so release builds can't inject faults.

**Dynamic census (origins 2 and 3).**
- Origin 2 (off-chain dynamic): a `CensusUpdated` triggers a background fetch
  of the newest root only; old roots are pruned once no process references them.
  A pending vote whose census leaf changed errors with "census changed, recast".
  A process whose initial census is permanently bad is ignored; the organizer
  creates a new process.
- Origin 3 (on-chain dynamic): `census/onchain.rs` indexes `CensusMemberAdded`
  from `treeSize()` backward at the confirmed head, replays forward checking each
  `newRoot`, stores per `(chain, contract)` in `census_onchain_meta` /
  `census_onchain_leaf`. A reorg below the last scanned block drops and rescans.
  A weight change or slot collision marks the index unusable permanently.
  The RPC must support `eth_call` at a block hash (EIP-1898). The index fails
  closed on root mismatch and backs off on range errors.
- Census contracts must be append-only with fixed weights. Any
  `WeightChanged` event with `previousWeight != 0` marks the census unusable.

**No cargo target dirs in /tmp.** Where `/tmp` is a RAM-backed tmpfs, a
`cargo build` there (e.g. `CARGO_TARGET_DIR=/tmp/...`) can fill it, stall every shell
and kill a prover running on the same host. Build output goes in the repo's
`target/` or `$HOME/.cache/`; only small log files go in `/tmp`.

**Crate boundaries.**
- arkworks 0.6 (`ark-circom`, wasmer) is allowed only in `davinci-client` behind
  `prover`. The workspace dependency is `default-features = false`, so the node
  binary never links it; only the sequencer's dev-dependency turns `prover` on.
- `davinci-state` and `davinci-client` dev-depend on each other. That is fine as
  dev-deps only; never make it a normal dependency.

**API.**
- Error codes are HTTP status × 100 + a discriminator (`api/error.rs`). Add new ones
  there and to the README table; the e2e asserts a few by value.
- axum 0.7 path syntax is `/:pid`, not `/{pid}`.
- Parse path integers by hand, because axum's rejection is a text/plain 400.

**anvil 1.8.3 accepts v0 blob sidecars on Osaka.** The anvil tests prove the node
sends v1; they don't prove that v0 is refused.
