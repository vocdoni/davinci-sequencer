# Contributing

Bug reports and pull requests are welcome. This file covers the setup, the conventions and the
parts of the code that are easy to get wrong.

## Setup

The workspace depends on davinci-zkvm by path, and the tests read other sibling checkouts (see
[docs/testing.md](docs/testing.md#layout-and-prerequisites)). Clone them next to this repository:

```
davinci-zkvm/                     path dependency: rust-sdk, input-gen
davinci-contracts/                branch zkvm, forge build
davinci-onchain-census-contract/  branch davinci-zkvm, forge build
davinci-circom/                   ballot circuit artifacts
davinci-dkg/                      only for the e2e DKG scenarios
```

## Before sending a change

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Gated tests pass silently when their variable is unset. If your change touches what they cover,
run them explicitly: `ZISKEMU=1` for anything that encodes transitions, `ANVIL=1` for contract
calls. CI has no GPU, so anything that needs a prover stays behind a gate. Use conventional
commit titles (`fix(web3): ...`, `feat(client): ...`).

## Crates

| Crate | Role |
|---|---|
| `arbo` | Sparse Merkle tree, a port of vocdoni/arbo. Go vector generator in `arbo/testdata/gen`, fuzz targets in `arbo/fuzz`. |
| `state` (`davinci-state`) | Per-process state: vote validation, transition builder, blob sync, results request. No IO besides arbo storage. |
| `sequencer` (`davinci-sequencer`) | The node: configuration, storage, election keys, census, chain access, monitor, process actors, finalizer, API. |
| `client` (`davinci-client`) | API wire types (the wire format owner), the HTTP client, organizer and voter tooling, the ballot prover behind feature `prover`. |
| `e2e` (`davinci-e2e`) | Test-only: acceptance test, benchmarks and demo driver. Spawns the real node binary. |

- arkworks 0.6 (`ark-circom`, wasmer) is allowed only in `davinci-client` behind `prover`. The
  node binary never links it; only the sequencer's dev-dependency turns `prover` on.
- `davinci-state` and `davinci-client` dev-depend on each other. Never make that a normal
  dependency.
- The contract ABIs are vendored in `sequencer/abi/`; the client reads them from there too.

## Conventions

- `#![forbid(unsafe_code)]`, no panics on external input, `thiserror` in libraries.
- Comments are short and plain. One line above a function is usually enough; explain why, not
  what.
- Secrets are never logged: the re-encryption seed, the refresh selection, election secret keys,
  the voter secret `k`, the node key. The seed and the selection are never persisted either.
  `PreparedBatch`, `ReencryptionJson`, `CircomInputs` and `PreparedVote` have redacted `Debug`;
  keep it that way and never `{:?}` a `ProveRequest`.
- The one persisted exception is the exposed slot set: every slot a sealed batch changed. It is
  exactly the key list the blob already publishes, so persisting it leaks nothing new.
- API error codes are the HTTP status times 100 plus a discriminator (`sequencer/src/api/error.rs`).
  Add new ones there and to [docs/api.md](docs/api.md); the e2e asserts some by value.
- axum 0.7 path syntax is `/:pid`, not `/{pid}`. Parse path integers by hand, because axum's
  rejection is a text/plain 400.

## Things to know

**Byte orders.** The same integer travels in up to four byte orders, and a wrong one does not
error: the circuit commits `ok = 0` or the contract reverts. Read the wire format in davinci-zkvm
`circuit/CIRCUIT.md` before touching anything that encodes.

- State roots are the raw arbo digest everywhere (public outputs, contract `stateRoot`, tracker
  proofs).
- Transition SMT fields and the transition's process id are arbo little-endian hex.
- Hashed leaf values are `int_be(sha256(..))` sent little-endian, so the wire bytes are the
  reversed digest.
- Ballot coordinates, census values and the KZG block's process id and root are big-endian hex.
- The contract `censusRoot` is the census root's little-endian bytes; the contract `processId` is
  big-endian (the client's `ProcessId` is the 31-byte big-endian form).

The SDK wraps these in named newtypes. The emulator dry runs (`ZISKEMU=1`) are the byte-exactness
oracle: run them after changing any wire struct.

**Ingest mirrors the circuit.** `davinci_state::validate_vote` repeats every per-vote circuit
check, so one ballot cannot sink a batch. When the circuit gains a check, add it there with a
named `VoteError` and a test in `state/tests/validate.rs`. `select_batch` and actor admission also
mirror the contract's `maxVoters` check, and `select_batch` applies the blob limit, counting the
refreshes the circuit will demand.

**Release pins.** `rust-sdk/src/release.rs` in davinci-zkvm pins `BATCH_PROGRAM_VK`,
`RESULTS_PROGRAM_VK` and `ROOT_C_VADCOP_FINAL`. They must equal the programs the prover runs, the
registry's immutables and davinci-zkvm's go-sdk `CircuitRelease`, and the node refuses any proof
that differs (the votes end in `error`
mentioning the pin). Rebuilding a zkVM program or changing the ZisK setup means refreezing the
pins and redeploying the registry. The ballot VK is the SDK's embedded
`assets/ballot_proof_vkey.json`; its hash is state leaf `0x07` and the registry's `ballotVKHash`.

**Networks.** `client/src/networks.rs` is the one table of known deployments. Update the registry
and start block there, and in [docs/deployments.md](docs/deployments.md), on every redeploy.
`Config::load` and `Config::parse_args` resolve the preset; a bare `Config::try_parse_from`
leaves the chain settings unset, so tests build configs with `--network custom`.

**Storage.** Records are JSON in redb. Bump `storage.rs::SCHEMA_VERSION` on any record-shape
change; the node refuses a database with another version. redb holds a file lock, so restart tests
reopen with a retry loop. The fault-injection hook `Db::fail_next_arbo_writes` exists only with
feature `test-hooks`, which the crate enables for its own tests through a self dev-dependency.

**Blobs are 128 KiB.** Never deserialize or pass one by value through serde or deep call chains:
a by-value blob overflows the 2 MiB tokio worker stack in debug builds. `AnvilBlobs` deserializes
into heap `Bytes`, and `sequencer/tests/stack.rs` runs that path on a 512 KiB thread.

**CPU-heavy work stays off the async workers.** In the actor, go through `actor.rs::cpu()`
(`block_in_place` on the multi-thread runtime, inline on the current-thread test runtime); calling
`block_in_place` directly panics on a current-thread runtime. The API runs `validate_vote` in
`spawn_blocking` under a semaphore, and census proofs go through `CensusStore::proof_async`.

**Census fetches are attacker-controlled.** The URI comes from on-chain data. Keep the
restrictions in `census.rs`: no redirects, `.no_proxy()`, a public-only resolver covering the IPv4
and IPv6 special ranges, body and participant caps, canonicalised `file://` paths under
`--census-dir`. Tests that serve a census from localhost need `CensusOptions::allow_private`.

**Chain following.** Events are routed only up to `head − confirmations`, and heartbeats carry
that block too. Own commits follow the receipt at depth 0. `events()` has no process-id filter:
every actor sees every process's events.

**The e2e test.** Run it through `make e2e`, never bare: anvil and the node subprocesses die only
when the test's handles drop. Use a release node binary (a debug build is far more stack-hungry),
and copy it out of the tree with `DAVINCI_SEQUENCER_BIN` if the tree may change during the run.
The prover is FIFO, so other jobs on it slow the run.

**Build output.** Keep cargo target directories in the repository's `target/` or under
`~/.cache`. On hosts where `/tmp` is a RAM-backed tmpfs, a build there can fill it.

**anvil accepts v0 blob sidecars on Osaka.** The anvil tests prove the node sends v1; they do not
prove v0 is refused.
