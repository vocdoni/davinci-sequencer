# davinci-sequencer

A sequencer node for the [DAVINCI](https://davinci.vote) voting protocol, written in Rust. It
collects encrypted ballots, proves them in batches with a
[davinci-zkvm](https://github.com/vocdoni/davinci-zkvm) prover and settles every batch on the
`ProcessRegistry` contract. This repository also ships `davinci-client`, the Rust library
organizers and voters use to create elections and cast ballots.

[![Build and Test](https://github.com/vocdoni/davinci-sequencer/actions/workflows/main.yml/badge.svg)](https://github.com/vocdoni/davinci-sequencer/actions/workflows/main.yml)
[![License: AGPL v3](https://img.shields.io/badge/License-AGPL%20v3-blue.svg)](LICENSE)

## Overview

Voters send encrypted ballots, each with a zero-knowledge proof that it follows the election
rules, to any sequencer. The node checks every ballot the way the circuit will, groups them into
batches, re-encrypts them and has the prover turn each batch into one PLONK proof. The settlement
transaction carries EIP-4844 blobs holding everything needed to rebuild the election state, so
any node can settle any election and a node that loses a race rebuilds from the winner's blobs.
Without a signing key the node runs as an observer that replays and checks every transition.

Each election is created with a sequencer key or a DKG key. With a sequencer key, the node that
handed out the key decrypts the final tally and proves it with a second zkVM program. With a DKG
key no single party holds the secret, and a [davinci-dkg](https://github.com/vocdoni/davinci-dkg)
committee threshold-decrypts the tally.

```
voters ── ballots ──▶ sequencer ── batch ──▶ davinci-zkvm prover
                         │  ◀───── proof ─────────┘
                         └── tx + blobs ──▶ ProcessRegistry (davinci-contracts)
```

| Repository | Role in the stack |
|---|---|
| [davinci-zkvm](https://github.com/vocdoni/davinci-zkvm) | Prover service, the vote-batch and results programs, and the Rust SDK this node builds on. |
| [davinci-contracts](https://github.com/vocdoni/davinci-contracts) | `ProcessRegistry`, the on-chain proof verifier and the DKG adapter. |
| [davinci-dkg](https://github.com/vocdoni/davinci-dkg) | Threshold key committee behind DKG-key elections. |
| [davinci-onchain-census-contract](https://github.com/vocdoni/davinci-onchain-census-contract) | Census contract for on-chain censuses. |
| [davinci-circom](https://github.com/vocdoni/davinci-circom) | Ballot circuit; clients prove ballots with its artifacts. |

## Quick start

Run a node on Gnosis with Docker Compose. You need Docker, a funded Gnosis account for the
settlement transactions, and a davinci-zkvm prover (or a local NVIDIA GPU, see the profiles
below).

```bash
git clone https://github.com/vocdoni/davinci-sequencer.git
cd davinci-sequencer
cp .env.example .env
# The key file must be readable by the container user (uid 10001).
sudo install -o 10001 -m 0400 /path/to/hex-private-key sequencer.key
docker compose --profile dev up -d
curl -s http://localhost:9090/info
```

Set `DAVINCI_PROVER_URL` in `.env` to your prover (the default,
`http://host.docker.internal:8080`, is a prover published on the same host). An empty
`sequencer.key` (`sudo install -o 10001 -m 0400 /dev/null sequencer.key`) runs an observer.

| Profile | Services |
|---|---|
| `dev` | Node and Watchtower. |
| `prod` | Node, Traefik with a Let's Encrypt certificate for `DOMAIN`, and Watchtower. |
| `gpu` | Node and a davinci-zkvm GPU prover on this host. |
| `prod-gpu` | `prod` plus the GPU prover. |

The images default to `:latest`, the newest release. In the `dev`, `prod` and `prod-gpu`
profiles Watchtower moves the node (and, in `prod-gpu`, the prover) to each new release; `gpu`
runs no Watchtower. The GPU profiles read the ZisK proving keys from `ZISK_KEYS_DIR`
(default `../davinci-zkvm/zisk-keys`, installed by `make keys` in davinci-zkvm).
[docs/configuration.md](docs/configuration.md#docker-compose) covers pinning a release and
building the images locally.

## Usage

### Running the binary

The workspace depends on davinci-zkvm by path, so check it out next to this repository:

```bash
git clone https://github.com/vocdoni/davinci-zkvm.git
git clone https://github.com/vocdoni/davinci-sequencer.git
cd davinci-sequencer
cargo build --release -p davinci-sequencer
DAVINCI_PRIVKEY_FILE=/etc/davinci/sequencer.key \
DAVINCI_PROVER_URL=http://127.0.0.1:8080 \
./target/release/davinci-sequencer
```

The `gnosis` network preset (the default) supplies the registry, its deployment block, the RPCs,
the beacon API and the confirmation depth, so a Gnosis node needs only the key and the prover. At
boot the node checks that the registry and its verifier pin the proof programs this release
proves, and refuses to start otherwise.

### Configuration

Every flag has a `DAVINCI_*` environment variable. The ones operators usually set:

| Variable | Flag | Default | Meaning |
|---|---|---|---|
| `DAVINCI_PRIVKEY_FILE` | `--privkey-file` | unset | File with the hex secp256k1 key that signs settlements. Unset: observer. |
| `DAVINCI_PROVER_URL` | `--prover-url` | `http://127.0.0.1:8080` | davinci-zkvm prover service. |
| `DAVINCI_NETWORK` | `--network` | `gnosis` | Network preset, or `custom` with explicit chain settings. |
| `DAVINCI_RPC_URL` | `--rpc-url` | network | Execution-layer JSON-RPC endpoints, comma-separated for failover. |
| `DAVINCI_BLOB_SOURCE` | `--blob-source` | network | `beacon:<url>[,<url>...]`, or `anvil` on a local chain. |
| `DAVINCI_DATADIR` | `--datadir` | `~/.davinci-sequencer` | Database directory. |
| `DAVINCI_API_PORT` | `--api-port` | `9090` | HTTP API port. |
| `DAVINCI_BATCH_TIME` | `--batch-time` | `15m` | How long the oldest pending vote waits before its batch seals. |
| `DAVINCI_LOG_LEVEL` | `--log-level` | `info` | Log filter; `RUST_LOG` takes precedence. |

Things to know before running a node in production:

- The key is read only from `DAVINCI_PRIVKEY` or the file named by `--privkey-file`, never from
  the command line.
- The datadir holds the master secret every sequencer election key is derived from. Losing it
  loses the ability to publish results for those elections: back it up like a key.
- Run the prover with `DAVINCI_KEEP_INPUTS=0` (the compose GPU profiles do). Its job inputs hold
  each batch's secret re-encryption seed.
- The public RPCs of the preset rate-limit busy clients. Use your own endpoints for production
  load.

The full option list, the network presets and the Compose setup are in
[docs/configuration.md](docs/configuration.md). For a live meeting that needs results minutes
after the vote closes, see [docs/agm.md](docs/agm.md).

### HTTP API

The node serves a JSON API on port 9090:

```bash
curl -s http://localhost:9090/info                        # node address, chain, registry, pins
curl -s http://localhost:9090/processes                   # process ids this node follows
curl -s http://localhost:9090/processes/0x<pid>           # parameters, status and tally
curl -s http://localhost:9090/votes/0x<pid>/voteId/0x<id> # vote status
```

Votes go to `POST /votes`. Organizers ask a node for a sequencer election key with
`POST /processes/keys`, and voters fetch their census proof from
`GET /processes/{pid}/participants/{address}`. Errors are `{"error": "...", "code": 40001}`,
where the code is the HTTP status times 100 plus a discriminator. The routes, request bodies,
vote states and error codes are in [docs/api.md](docs/api.md).

### Client library

`davinci-client` wraps the API and the registry: `Organizer` creates and manages elections,
`Voter` and `BallotProver` build and prove ballots, and `verify_tracker` checks that a vote is in
an on-chain root. See [client/README.md](client/README.md) for examples.

## Documentation

- [docs/configuration.md](docs/configuration.md): every option, network presets, RPC failover,
  the datadir and Docker Compose.
- [docs/api.md](docs/api.md): HTTP routes, wire format, vote states and error codes.
- [docs/lifecycle.md](docs/lifecycle.md): how an election moves through the node, from creation
  and batching to the grace window, results and key modes.
- [docs/census.md](docs/census.md): census origins, file formats and ballot slots.
- [docs/security.md](docs/security.md): security properties, trust assumptions and known
  limitations.
- [docs/agm.md](docs/agm.md): settings for live meetings.
- [docs/deployments.md](docs/deployments.md): contract addresses and pinned values of the Gnosis
  deployment.
- [docs/testing.md](docs/testing.md): unit, anvil and end-to-end suites, and the Docker runner.
- [docs/demo.md](docs/demo.md): the demo election driver.
- [docs/benchmarks.md](docs/benchmarks.md): measured batch sizes, costs and throughput.
- [client/README.md](client/README.md) and [arbo/README.md](arbo/README.md): the client library
  and the sparse Merkle tree crate.

## Development

The workspace has five crates: `arbo` (sparse Merkle tree), `state` (per-election state and vote
validation), `sequencer` (the node), `client` (API types, organizer and voter tooling) and `e2e`
(acceptance tests, benchmarks and the demo driver).

```bash
make build                                              # cargo build --workspace
make test                                               # cargo test --workspace
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
```

Tests that need anvil, the zkVM emulator or a GPU prover are gated by environment variables and
skip otherwise; [docs/testing.md](docs/testing.md) lists them. See
[CONTRIBUTING.md](CONTRIBUTING.md) before sending changes.

## License

GNU Affero General Public License v3.0 or later. See [LICENSE](LICENSE).
