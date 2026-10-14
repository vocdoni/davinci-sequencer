# Configuration

A node needs an execution-layer RPC, a blob source (a beacon API, or anvil on a local chain), a
`ProcessRegistry` deployed from davinci-contracts, and a davinci-zkvm prover.
The first three come from the network preset, so on a known network only the signing key and the
prover are left to configure.

Every flag has a `DAVINCI_*` environment variable (`--rpc-url` is `DAVINCI_RPC_URL`). Durations
take `90`, `90s`, `500ms`, `5m` or `2h`. In an environment file, leave unused variables unset: an
empty value such as `DAVINCI_START_BLOCK=` is rejected.

## Networks

Known deployments live in `davinci_client::networks`, which the node, the organizer and the
voter tooling share. A preset sets the chain id, the `ProcessRegistry` and its deployment block,
the RPC list, the blob source and the confirmations.

| Network | Chain | RPCs | Blob source | Confirmations |
|---|---|---|---|---|
| `optimism` | 10 | `https://optimism-rpc.publicnode.com`, `https://mainnet.optimism.io` | `beacon:https://ethereum-beacon-api.publicnode.com` (Ethereum) | 1 |

The `optimism` preset, the default, is in L1-blob mode: its transitions settle on OP Mainnet and
its blobs go to Ethereum, see [L1-blob mode](#l1-blob-mode) and
[deployments.md](deployments.md#op-mainnet). The `gnosis` preset is gone: the Gnosis registries
pin the previous release's program vks (see [deployments.md](deployments.md#retired-deployments)).

The registry of each network is listed in [deployments.md](deployments.md).

- An explicit setting (`--registry`, `--start-block`, `--rpc-url`, `--blob-source`,
  `--confirmations`) replaces the preset's value, so a custom deployment works on any chain.
- The preset's start block belongs to its registry. With another `--registry` the node scans from
  `--start-block`, or from block 0 with a warning.
- An explicit registry pins the node to it. Only a node that takes the registry from the preset
  follows it to a new one when a release moves the network.
- `--network custom` uses explicit settings only and needs `--registry`, `--rpc-url` and
  `--blob-source`. `--confirmations` defaults to 2.
- At boot the RPC's `eth_chainId` must match the preset. A mismatch stops the node, or only warns
  when `--registry` is explicit.

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

## Options

| Flag | Env | Default | Meaning |
|---|---|---|---|
| `--network` | `DAVINCI_NETWORK` | `optimism` | Preset that fills every chain setting not given, or `custom`. |
| `--registry` | `DAVINCI_REGISTRY` | network | `ProcessRegistry` address. |
| `--rpc-url` | `DAVINCI_RPC_URL` | network | Execution-layer JSON-RPC; a comma-separated list for failover. |
| `--blob-source` | `DAVINCI_BLOB_SOURCE` | network | `beacon:<url>[,<url>...]` (beacon APIs) or `anvil` (`anvil_getBlobsByTransactionHash` on the RPC). |
| `--l1-rpc-url` | `DAVINCI_L1_RPC_URL` | network | L1-blob mode: Ethereum execution JSON-RPCs, comma-separated for failover. Set exactly when the registry has an `L1BlobVerifier`. |
| `--l1-send-rpc-url` | `DAVINCI_L1_SEND_RPC_URL` | `--l1-rpc-url` | Endpoints that only receive the blob transactions. A 6-blob send is about 1.5 MB; many public RPCs refuse such bodies. |
| `--l1-privkey-file` | `DAVINCI_L1_PRIVKEY_FILE` | unset | File with the hex key of the dedicated Ethereum account that posts blobs. A signing node in L1-blob mode needs it. |
| (env only) | `DAVINCI_L1_PRIVKEY` | unset | The same key, from the environment. |
| `--l1-max-post-fee` | `DAVINCI_L1_MAX_POST_FEE` | `0.001eth` | Most one blob post may cost at the fees it offers (`<wei>`, `<n>gwei`, `<n>eth`); above it the node waits for cheaper fees. Must be above 0. |
| `--op-max-tx-fee` | `DAVINCI_OP_MAX_TX_FEE` | `0.002eth` | Most one transaction on the registry chain may cost in execution gas, `gas limit × max fee per gas` (`<wei>`, `<n>gwei`, `<n>eth`); above it the send is withheld and retried later. Must be above 0. |
| `--start-block` | `DAVINCI_START_BLOCK` | network | First block a fresh deployment scans for registry events. Ignored once the deployment has scanned. Progress is saved every 5,000 blocks. |
| `--confirmations` | `DAVINCI_CONFIRMATIONS` | network, else `2` | Blocks behind head treated as final. Raise it with load-balanced RPCs whose backends lag. |
| `--prover-url` | `DAVINCI_PROVER_URL` | `http://127.0.0.1:8080` | davinci-zkvm prover service. |
| `--privkey-file` | `DAVINCI_PRIVKEY_FILE` | unset | File with the hex secp256k1 key that signs settlement transactions. |
| (env only) | `DAVINCI_PRIVKEY` | unset | The same key, from the environment. |
| `--datadir` | `DAVINCI_DATADIR` | `~/.davinci-sequencer` | One `<chain id>-<registry>/sequencer.redb` per deployment. |
| `--db-cache-mb` | `DAVINCI_DB_CACHE_MB` | `256` | redb page cache, in MiB. It stays in memory for the node's lifetime, so it bounds most of its steady memory use. |
| `--api-host` | `DAVINCI_API_HOST` | `0.0.0.0` | API listen address. |
| `--api-port` | `DAVINCI_API_PORT` | `9090` | API port. |
| `--batch-max` | `DAVINCI_BATCH_MAX` | `512` | Pending votes that seal a batch at once, and the largest batch (1 to 1024). The prover's host memory limits the largest batch at high field counts: on an RTX 5090 host with 59 GB of RAM, 512 votes at 16 fields with their refreshes ran out of memory while about 370 proved, so set it below that there (384 is used). |
| `--batch-time` | `DAVINCI_BATCH_TIME` | `15m` | Age of the oldest pending vote at which a batch of at least `--min-mix` slots seals (±10% jitter). |
| `--min-mix` | `DAVINCI_MIN_MIX` | `2` | Distinct slots a timer batch needs; fewer wait for `--solo-wait`. |
| `--solo-wait` | `DAVINCI_SOLO_WAIT` | 3 × batch time | Age at which a batch below `--min-mix` seals anyway (±10% jitter). Never below `--batch-time`. |
| `--flush-horizon` | `DAVINCI_FLUSH_HORIZON` | `3m` | From this long before the election end, and through the grace window, every pending vote seals at once. |
| `--settle-margin` | `DAVINCI_SETTLE_MARGIN` | `60s` (`30s` in L1-blob mode) | Time reserved to land a transaction: a batch is sized so its estimated proof plus this margin ends before the window closes. |
| `--l1-settle-lag` | `DAVINCI_L1_SETTLE_LAG` | `110s` | L1-blob mode: the Ethereum leg of a landing (blob inclusion, OP's L1 origin lag, the reorg margin). It runs during the proof, so a batch seals only if this plus `--settle-margin`, plus 24 s for each blob transaction after the first, fits before the window closes too. Lower it only for a faster L1, such as a local anvil. |
| `--prove-base` | `DAVINCI_PROVE_BASE` | `30s` | Fixed part of the proving-time estimate. Raise it on a slow prover. |
| `--slot-depth` | `DAVINCI_SLOT_DEPTH` | `3` | Queued votes one ballot slot may hold, in flight included. |
| `--max-blobs-per-tx` | `DAVINCI_MAX_BLOBS_PER_TX` | from the chain | Most blobs one blob transaction carries (1 to 6): the most per batch in blob mode; in L1-blob mode a batch takes up to 32, posted in transactions of this many. See [Chain limits](#chain-limits). |
| `--eager-results` | `DAVINCI_EAGER_RESULTS` | `true` | Prove a sequencer-key election's results during its grace window and submit them when it closes. |
| `--poll-interval` | `DAVINCI_POLL_INTERVAL` | `5s` | Chain polling interval. |
| `--heartbeat` | `DAVINCI_HEARTBEAT` | poll interval | Drives batch timing, the close-out at the grace end and finalization. |
| `--prover-poll` | `DAVINCI_PROVER_POLL` | `2s` | Prover job polling interval. |
| `--prover-timeout` | `DAVINCI_PROVER_TIMEOUT` | `30m` | Wait for one proving job. On timeout the node waits on the same job up to 3 more times. |
| `--census-dir` | `DAVINCI_CENSUS_DIR` | unset | Directory `file://` census URIs may read from. Unset: `file://` is refused. |
| `--census-allow-private` | `DAVINCI_CENSUS_ALLOW_PRIVATE` | off | Allow census downloads from loopback and private addresses (local development). |
| `--census-max-participants` | `DAVINCI_CENSUS_MAX_PARTICIPANTS` | `4194304` | Largest census accepted. |
| `--census-sync-every` | `DAVINCI_CENSUS_SYNC_EVERY` | `30s` | Least time between background syncs of one on-chain census contract; while a contract has no confirmed snapshot yet, a waiting vote or participant query syncs it on demand. |
| `--keys-per-minute` | `DAVINCI_KEYS_PER_MINUTE` | `10` | `POST /processes/keys` per client IP per minute. |
| `--ballot-vk` | `DAVINCI_BALLOT_VK` | embedded | Ballot proof verification key JSON. Its hash must equal the registry's `ballotVKHash`, or every process is ignored. |
| `--log-level` | `DAVINCI_LOG_LEVEL` | `info` | Tracing filter; `RUST_LOG` takes precedence. |

How the batching options interact is described in [lifecycle.md](lifecycle.md#batching).

## Signing key and observer mode

The key is read from `DAVINCI_PRIVKEY` or from the file named by `--privkey-file`. Setting both is
an error, and a `--privkey` command-line value is rejected because other users can read process
arguments. The key is never printed and is zeroed on drop.

Without a key the node is an observer. It follows every process, replays every transition from
its blobs and serves every read, but never settles or finalizes. It refuses `POST /votes` and
`POST /processes/keys` with 412/41203, and `/info` reports `"observer": true`.

## L1-blob mode

A registry with an `L1BlobVerifier` (OP Stack chains) settles on OP and keeps its blobs on
Ethereum. Such a node needs, besides the OP settings:

- `--l1-rpc-url`: Ethereum execution RPCs. They post the blob transactions, serve the headers and
  block transactions the L1 proofs are built from, and the block timestamps followers map to
  beacon slots.
- `--blob-source beacon:<url>,...`: Ethereum beacon APIs that keep full blobs (or `anvil` for a
  local L1 anvil at `--l1-rpc-url`).
- A dedicated Ethereum key (`--l1-privkey-file` or `DAVINCI_L1_PRIVKEY`) for signing nodes.
  Nothing else may send from that account: the blob pool forbids nonce gaps, caps pending
  transactions per account and wants every fee doubled to replace one. Fund it on Ethereum; a
  post costs about 21,000 gas plus 131,072 blob gas per blob. If you change the key, post records
  of the old account are dropped at start.
- `--l1-send-rpc-url` when the read RPCs refuse large bodies.

At boot the node checks the Ethereum chain id and the verifier's `L1Block` address against the
network (the `optimism` preset expects chain 1 and `0x4200000000000000000000000000000000000015`),
and reads Ethereum's per-transaction blob limit. A batch may take up to the circuit's 32 blobs:
the node posts them in transactions of that limit, one after another, and proves all of them in
one settlement. It logs both caps (`blob cap per transaction`, `blob cap per batch`). Observers
need no Ethereum key.

The blobs are posted when proving starts, so the Ethereum leg of a landing (about 110 s on
mainnet, `--l1-settle-lag`) overlaps the proof, and `--settle-margin` (default `30s` here) only
covers the OP leg. A batch seals only while both the estimated proof and the Ethereum leg, each
plus the margin, end before the window can close; each blob transaction after the first adds 24 s
to the leg, so a short window takes a batch of fewer transactions. While the election is open that is
`now + graceFloor`, so `--l1-settle-lag` plus `--settle-margin` must stay under the registry's
grace floor (240 s on OP); otherwise nothing could seal while a window is open, so a signing
node refuses to boot (an observer only warns).

On OP Mainnet:

- Keep the L1-blob defaults: `--settle-margin 30s` and `--l1-settle-lag 110s`. They leave a batch
  up to 240 - 30 - 30 = 180 s of estimated proof while an election is open, and five blob
  transactions of Ethereum leg. Do not lower `--l1-settle-lag` below about 100 s: a post settled
  about 72 s after it was sent in the live run, and the rest is headroom for a missed slot or a
  wider origin lag.
- Organizers should keep the grace at the 300 s default or above, not the 240 s floor: after the
  end, a follow-up batch can seal only within `grace - 140 s` of the previous landing. A 512-vote
  batch at 16 fields after the end wants the 600 s ceiling on a node that has not proved yet (its
  estimate starts at 0.8 s a vote).
- `--l1-rpc-url` must include an endpoint that takes 1.5 MB request bodies (a six-blob send):
  publicnode refuses them, dRPC takes them, and the node sends a blob transaction to every listed
  endpoint, so the preset's pair works. With your own endpoint, set it as `--l1-send-rpc-url`.
- `--blob-source` must name beacon APIs that keep full blobs; after PeerDAS only supernodes and
  archives do. publicnode's served recent and day-old slots when the preset was filled in;
  lodestar's public endpoint refused blob requests (403). Production followers should add a beacon
  node of their own.
- `--l1-max-post-fee` bounds what one post (one blob transaction) may cost at the fees it offers
  (twice the base fee and twice the blob base fee); a replacement doubles them. A one-blob post
  cost about 2 µETH at 0.08 gwei and 0.004 gwei; the 0.001 ETH default leaves room for spikes and
  replacements. A batch of three transactions, re-posted at most three times, sends at most 12.
  The cost counts `21000 × max fee + blobs × 131072 × blob fee`, so at the default the gas term
  alone reaches the cap at an Ethereum base fee of about 24 gwei, whatever the blob fee; above it
  every post waits. Each replacement doubles every fee, so from a 1 gwei base fee about four
  replacements fit under the default. A post over the cap is retried after one poll interval, then
  after twice as long each time up to once a minute (fees move once per Ethereum block), never
  past the batch deadline. The node logs `L1 post failed; retrying` at warn level when the
  refusals start and `L1 post went through after failing` (with `after` and `checks`) when a post
  clears them, at debug level in between. Ethereum RPC errors are logged the same way and retried
  every poll interval.
- `--op-max-tx-fee` bounds every transaction the node sends on the registry chain: transitions,
  results, the DKG and Council results calls, and `record()` in L1-blob mode. It counts
  `gas limit × max fee per gas` at the price the node would offer; OP's L1 data fee is charged
  apart and not counted, nor is the blob fee of a blob-mode transition. A
  `submitStateTransitionL1` took 0.8 to 3.5 M gas in measured runs, so the 0.002 ETH default
  admits about 0.57 gwei on the largest one (about 0.48 gwei counting the 20 % margin on the gas
  limit) and about 2 gwei on the smallest. Above it the send is refused before
  signing, logged at error level with the price and the cap, and the batch is retried under the
  usual backoff; a replacement whose bump would cross it is withheld and the transaction waits at
  its current price. Near the grace end the cap trades spend for liveness: watch for
  `send withheld` and `replacement withheld`. In L1-blob mode the node also checks it before
  posting a batch's blobs on Ethereum, for the smallest settlement (800k gas plus the margin,
  about 2.1 gwei at the default): if even that would be withheld, nothing is posted and no
  Ethereum fee is spent. The post is retried like a fee-capped one while the proof runs and goes
  out once OP's fee falls; if it is still over when the proof is ready, the votes go back to the
  queue. A larger settlement can still pass this check and be withheld at its
  send. On another chain in blob mode (Ethereum, Gnosis), raise it to that chain's prices. Both
  caps must be above zero: `0` would withhold every send, so the node refuses it at boot.
- Votes that sit `pending` with `nextSealNotBefore` in the past are held by the time budget; the
  node logs why at debug level ("no time to land an L1-blob batch", "no batch fits", "the exposed set needs more blob
  transactions than the window fits").

## Data directory

The datadir holds one directory per deployment, named after the chain id and the registry, each
with one redb database (file mode 0600, directories 0700):

```
~/.davinci-sequencer/
  100-0x<registry, lowercase hex>/sequencer.redb
```

A node that meets a new registry, because a release moved the network or `--registry` changed,
starts an empty database for it at the registry's start block. The previous deployment's
directory stays untouched, and pointing the node back at it resumes where it stopped. Each
database records its deployment and refuses to open under another.

A database from the older flat layout (`<datadir>/sequencer.redb`) is moved into its deployment
directory once, when its process ids belong to that deployment. Otherwise it stays in place and
the node logs why; delete it when that deployment is no longer needed.

Each database holds the deployment's master secret, drawn on first boot, from which every
sequencer election key the node hands out is derived. Back it up and protect it like a key.

## RPC and beacon endpoints

`--rpc-url` and `--blob-source beacon:` take comma-separated lists. Requests go to one endpoint
until it fails, then move to the next and stay there, so the node never mixes providers at
different heads within one scan.

- An RPC fails over on a connection error, a timeout, any non-2xx status, a 2xx body that is not
  JSON-RPC, or a node-side JSON-RPC error (missing historical state, rate limits, `-32603`).
  Reverts, nonce and funding errors come back as they are. A too-wide `eth_getLogs` range is
  halved instead.
- A rate limit (429, or a JSON-RPC rate-limit error) rests that endpoint for its `Retry-After`
  (up to 5 minutes), or else for 1 s doubling on each limit in a row (up to a minute). When every
  endpoint rests, a request waits up to 10 s for the first one back.
- `eth_sendRawTransaction` goes to every endpoint that is not resting, and the first to accept
  it answers. The nonce never goes below the node's last mined transaction, since a
  load-balanced backend can lag.
- A beacon API fails over on a connection error, 429 or 5xx. A 404 (a slot pruned there) is asked
  once of the next beacon without switching, so an archive beacon listed second can serve old
  blobs.
- At boot every RPC must report the same `eth_chainId`. An unreachable one is kept with a
  warning, and at least one must answer.

Logs name endpoints by host only, since URLs often carry API keys. Every outgoing request carries
`User-Agent: davinci-sequencer/<version>`; some public RPCs refuse requests without one.

## Startup checks

Before the API serves, signer and observer alike read the registry's `batchProgramVK`,
`resultsProgramVK`, `rootCVadcopFinal`, `ballotVKHash` and `chainID`, and its `ziskVerifier()`.

- The three verification keys must equal the release pins of the davinci-zkvm SDK.
- `ballotVKHash` must equal the hash of the ballot key the node uses (`--ballot-vk` or the
  embedded one), and `chainID` the RPC's `eth_chainId`.
- The verifier must report the pinned `rootCVadcopFinal`, and the keccak256 of its code must
  equal the pinned `ZISK_VERIFIER_CODEHASH`.

Any difference stops the node with an error naming each field, the expected value and the one
found. There is no override. The prover's programs must match the same pins: the node checks
every proof against them and refuses to settle on a mismatch.

### Chain limits

The node sizes batches to the chain's per-transaction blob limit and picks the blob sidecar
version (cell proofs from Osaka on) from `eth_config` (EIP-7910). When the RPC does not serve it,
the sidecar version comes from probing the P256VERIFY precompile (EIP-7951), and the blob limit
from `--max-blobs-per-tx` or a known-chain table (2 on Gnosis and Chiado, else 6, with a
warning). If the probe keeps failing, Gnosis and Chiado fall back to cell proofs; on any other
chain the node refuses to start. A send the RPC rejects for its sidecar version switches to the
other version.

## Shutdown

SIGTERM or SIGINT stops the node within a few seconds in every phase, including startup checks
and event catch-up. Anything still busy after ten seconds is cut off; the database is
crash-safe. Votes of a batch that was being proved go back to pending on restart.

## Docker Compose

`docker-compose.yml` runs the published image
`ghcr.io/vocdoni/davinci-sequencer:${DAVINCI_SEQUENCER_TAG:-latest}`. Configuration goes in
`.env`; [.env.example](../.env.example) lists every variable.

| Profile | Services |
|---|---|
| `dev` | Node and Watchtower. |
| `prod` | Node, Traefik (Let's Encrypt certificate for `DOMAIN`) and Watchtower. |
| `gpu` | Node and a davinci-zkvm GPU prover. |
| `prod-gpu` | `prod` plus the GPU prover. |

- The key never goes in `.env`. Compose mounts `SEQUENCER_KEY_FILE` (default `./sequencer.key`)
  as a secret for `--privkey-file`. The image runs as uid 10001, which must be able to read it.
  An empty file runs an observer.
- The datadir is the named volume `data`, mounted at `/data`.
- `dev` and `prod` reach the prover at `DAVINCI_PROVER_URL`. A prover published on the same host
  is `http://host.docker.internal:8080`, and it must listen on the Docker bridge, not only on
  `127.0.0.1`.
- The GPU profiles run `ghcr.io/vocdoni/davinci-zkvm:${DAVINCI_ZKVM_TAG:-latest}` with the
  proving keys from `ZISK_KEYS_DIR` (default `../davinci-zkvm/zisk-keys`). The prover port stays
  unpublished and `DAVINCI_KEEP_INPUTS` is pinned to 0. Running it needs the NVIDIA Container
  Toolkit.

In the `dev`, `prod` and `prod-gpu` profiles, Watchtower updates the containers labelled for it
(node, prover, Traefik) whenever a new image is published under their tag; `gpu` runs no
Watchtower. `:latest` is the newest release, so the node follows releases; set
`DAVINCI_SEQUENCER_TAG=v1.2.3` to stay on one. A release that moves the network to a new registry
needs no action: the node opens a fresh database for it.

To build both images from sibling checkouts instead, give them a tag Watchtower cannot find
upstream:

```bash
printf 'DAVINCI_SEQUENCER_TAG=local\nDAVINCI_ZKVM_TAG=local\n' >> .env
docker compose --profile gpu build
```

The image alone builds from the directory that holds both `davinci-sequencer/` and
`davinci-zkvm/`:

```bash
docker build -f davinci-sequencer/Dockerfile -t davinci-sequencer .
docker run -v davinci-data:/data -p 9090:9090 davinci-sequencer --help
```
