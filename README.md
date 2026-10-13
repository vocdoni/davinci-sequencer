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

Each election is created with a sequencer key, a DKG key or a Council key. With a sequencer key,
the node that handed out the key decrypts the final tally and proves it with a second zkVM
program. With a DKG key no single party holds the secret, and a
[davinci-dkg](https://github.com/vocdoni/davinci-dkg) committee threshold-decrypts the tally. A
Council key belongs to an invite-only
[davinci-dkg-council](https://github.com/vocdoni/davinci-dkg-council) ceremony, whose members
decrypt the tally once the ceremony's decryption date opens.

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
| [davinci-dkg-council](https://github.com/vocdoni/davinci-dkg-council) | Invite-only key ceremonies behind Council-key elections. |
| [davinci-onchain-census-contract](https://github.com/vocdoni/davinci-onchain-census-contract) | Census contract for on-chain censuses. |
| [davinci-circom](https://github.com/vocdoni/davinci-circom) | Ballot circuit; clients prove ballots with its artifacts. |

## Quick start

Run a node on OP Mainnet with Docker Compose. You need Docker, an OP Mainnet account holding a
little ETH for the settlement transactions, a second, dedicated Ethereum account holding a little
ETH for the blob transactions, and a davinci-zkvm prover (or a local NVIDIA GPU, see the profiles
below). Nothing else needs configuring: the defaults follow the OP Mainnet deployment in
[docs/deployments.md](docs/deployments.md), whose blobs go to Ethereum.

```bash
git clone https://github.com/vocdoni/davinci-sequencer.git
cd davinci-sequencer
cp .env.example .env
# The key files must be readable by the container user (uid 10001): the OP
# settlement key and the dedicated Ethereum key that posts the blobs.
sudo install -o 10001 -m 0400 /path/to/hex-private-key sequencer.key
sudo install -o 10001 -m 0400 /path/to/hex-ethereum-key sequencer-l1.key
docker compose --profile dev up -d
docker compose --profile dev logs -f sequencer   # Ctrl-C once it serves the API
curl -s http://localhost:9090/info
```

Set `DAVINCI_PROVER_URL` in `.env` to your prover (the default,
`http://host.docker.internal:8080`, is a prover published on the same host). `/info` should show
`"chainId": 10`, the `ProcessRegistry` of [docs/deployments.md](docs/deployments.md) as
`processRegistry`, and your account as `sequencerAddress`. Empty key files
(`sudo install -o 10001 -m 0400 /dev/null sequencer.key`, the same for `sequencer-l1.key`) run an
observer, which follows and checks every election and needs neither ETH nor a prover. Nothing
but the node may send from the Ethereum account (see
[configuration.md](docs/configuration.md#l1-blob-mode)).

Upgrading a compose node from v0.5.0: create `sequencer-l1.key` before the image moves
(`sudo install -o 10001 -m 0400 /dev/null sequencer-l1.key` for an observer, or the funded
Ethereum key for a signing node), or compose will not start the service: the file is a secret's
source. A signing node without a key in it refuses to boot.

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
DAVINCI_L1_PRIVKEY_FILE=/etc/davinci/sequencer-l1.key \
DAVINCI_PROVER_URL=http://127.0.0.1:8080 \
./target/release/davinci-sequencer
```

The `optimism` network preset (the default) runs the node on OP Mainnet in L1-blob mode:
transitions settle on OP and their blobs go to Ethereum. It supplies the registry, its deployment
block, the OP and Ethereum RPCs, the Ethereum beacon API and the confirmation depth, so a node
needs only its two keys and the prover: the OP key that signs settlements and a dedicated Ethereum
key for the blob posts (`DAVINCI_L1_PRIVKEY_FILE`, funded on Ethereum, used by nothing else). See
[configuration.md](docs/configuration.md#l1-blob-mode) and
[deployments.md](docs/deployments.md#op-mainnet). At boot the node checks that the registry and
its verifier pin the proof programs this release proves, and refuses to start otherwise. Gnosis
is retired: this release has no `gnosis` preset.

### Configuration

Every flag has a `DAVINCI_*` environment variable. The ones operators usually set:

| Variable | Flag | Default | Meaning |
|---|---|---|---|
| `DAVINCI_PRIVKEY_FILE` | `--privkey-file` | unset | File with the hex secp256k1 key that signs settlements. Unset: observer. |
| `DAVINCI_PROVER_URL` | `--prover-url` | `http://127.0.0.1:8080` | davinci-zkvm prover service. |
| `DAVINCI_NETWORK` | `--network` | `optimism` | Network preset, or `custom` with explicit chain settings. |
| `DAVINCI_RPC_URL` | `--rpc-url` | network | Execution-layer JSON-RPC endpoints, comma-separated for failover. |
| `DAVINCI_BLOB_SOURCE` | `--blob-source` | network | `beacon:<url>[,<url>...]`, or `anvil` on a local chain. |
| `DAVINCI_L1_RPC_URL` | `--l1-rpc-url` | network | L1-blob mode (OP Stack registries): Ethereum JSON-RPC endpoints. |
| `DAVINCI_L1_PRIVKEY_FILE` | `--l1-privkey-file` | unset | L1-blob mode: the dedicated Ethereum key that posts blobs. |
| `DAVINCI_DATADIR` | `--datadir` | `~/.davinci-sequencer` | Database directory. |
| `DAVINCI_API_PORT` | `--api-port` | `9090` | HTTP API port. |
| `DAVINCI_BATCH_TIME` | `--batch-time` | `15m` | How long the oldest pending vote waits before its batch seals. |
| `DAVINCI_CENSUS_SYNC_EVERY` | `--census-sync-every` | `30s` | Least time between background syncs of one on-chain census contract; while a contract has no confirmed snapshot yet, a waiting vote or participant query syncs it on demand. |
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
- [docs/deployments.md](docs/deployments.md): contract addresses and pinned values of the OP
  Mainnet deployment, and the retired ones.
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
