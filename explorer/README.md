# DAVINCI explorer

A read-only web explorer for a DAVINCI deployment: processes, state
transitions, blobs, votes and results, with the checks an auditor, an
organizer or a voter can run for themselves. It talks to the chain from the
browser (JSON-RPC and a beacon API, optionally sequencer node APIs) and has
no backend. [EXPLORER.md](EXPLORER.md) describes the architecture and the
building blocks the pages use.

Stack: Vite 5, React 18, TypeScript (strict), Tailwind CSS v4, Radix
primitives, TanStack Query / Table / Virtual, viem, react-router 6; vitest
and Playwright for tests. The design system, kit and app shell come from the
davinci-dkg explorer, so both look like one family; this one adds a
light/dark/system theme switch.

## Development

```sh
cd explorer
pnpm install
pnpm dev            # http://127.0.0.1:5173
```

Append `?demo=1` to any URL to browse a deterministic synthetic network
with no chain behind it (see [Demo mode](#demo-mode)).

The dev server reads `public/config.json`, the committed defaults (the
Gnosis Chain deployment). To point it somewhere else without touching that
file, write `public/config.local.json` (gitignored) with the same shape; the
dev server serves it as `/config.json` when it exists. For sequencer nodes
or a beacon without CORS, the dev server offers the same proxies as the
image:

```sh
SEQUENCER_URLS=https://seq1.example,https://seq2.example BEACON_URL=https://beacon.example pnpm dev
```

and `config.local.json` then lists `"/proxy/sequencer/0"`, `"/proxy/sequencer/1"`
and `"beaconUrl": "/proxy/beacon"`.

```sh
pnpm lint           # tsc --noEmit + eslint
pnpm format         # prettier --write
pnpm test           # vitest: decoders, config, reducers, selectors, hooks, shell
pnpm test:e2e       # Playwright smoke suite against demo mode (builds, then vite preview)
pnpm build          # → dist/
```

`EXPLORER_LIVE=1 pnpm test src/indexer/live.test.ts` indexes the deployment
in `public/config.json` over its public RPCs and checks every transition, as
the transition page does (it needs the network, so it is off by default).

Playwright needs Chromium once: `pnpm test:e2e:install` (it goes to
`~/.cache/ms-playwright`). `E2E_BASE_URL=http://127.0.0.1:5173 pnpm test:e2e`
runs the suite against a server that is already up.

## Configuration

The app reads `/config.json` at boot. In the Docker image the entrypoint
renders it from environment variables; a variable left unset keeps the
default from `public/config.json`, and an optional one set to the empty
string is removed.

| Variable | Meaning | Default |
|---|---|---|
| `NETWORK_NAME` | Display name | `Gnosis Chain` |
| `CHAIN_ID` | Expected chain id. The explorer checks the RPC's at boot and stops with a banner on a mismatch | `100` |
| `RPC_URL` | JSON-RPC endpoints, comma-separated, in failover order | publicnode, BlockReq, rpc.gnosischain.com |
| `BEACON_URL` | Beacon API for blob sidecars | `https://rpc-gbc.gnosischain.com` |
| `REGISTRY_ADDRESS` | ProcessRegistry | the Gnosis deployment |
| `START_BLOCK` | Registry deployment block; the log scan starts here | the Gnosis deployment |
| `SEQUENCER_URLS` | Sequencer node APIs, comma-separated (vote status, tracker proofs, raw blobs) | none |
| `BLOCK_EXPLORER_URL` | Block explorer for transaction and address links | `https://gnosisscan.io` |
| `DKG_EXPLORER_URL` | davinci-dkg explorer, for DKG epoch and application links | none |
| `BEACON_PROXY` | `true` serves the beacon same-origin at `/proxy/beacon/` | `false` |
| `SEQUENCER_PROXY` | `true` serves sequencer *n* at `/proxy/sequencer/<n>/` (sequencer nodes send no CORS headers) | `true` |
| `PORT` | Port nginx listens on inside the container | `8080` |

The Gnosis values live only in `public/config.json`; the deployment is due to
be replaced, and that file is the one place to change.

The proxies accept `GET` and `HEAD` only. Everything else is read straight
from the browser: the public Gnosis RPCs and beacon send CORS headers.

## Docker

`Dockerfile` builds the bundle with Node and serves it with nginx. At start,
`docker/render.sh` (installed as an nginx entrypoint script) writes
`/config.json` and the nginx site, including the proxies. `GET /healthz`
answers `ok`, and the image's `HEALTHCHECK` polls it.

```sh
docker build -t davinci-explorer .
docker run --rm -p 8080:8080 davinci-explorer                     # Gnosis defaults
docker run --rm -p 8080:8080 \
  -e CHAIN_ID=31337 -e NETWORK_NAME=anvil -e RPC_URL=http://127.0.0.1:8545 \
  -e REGISTRY_ADDRESS=0x... -e START_BLOCK=0 -e BEACON_URL= -e BLOCK_EXPLORER_URL= \
  davinci-explorer
```

`docker-compose.yml` runs it on its own (`docker compose up`, or
`docker compose up --build` from source) on `EXPLORER_PORT` (8080); it passes
the variables above through when they are set. With `--profile watchtower`
it also runs Watchtower, which pulls each new release of `:latest`; unset
variables follow the release's defaults, so a release that moves the default
deployment moves the explorer with it. `sh docker/render.test.sh` checks the
renderer (needs `jq`).

CI (`.github/workflows/explorer.yml`) runs the checks, the unit tests, the
Playwright suite and the build on every PR and push that touches
`explorer/`, then builds the image. Branch pushes publish
`ghcr.io/vocdoni/davinci-explorer` and `vocdoni/davinci-explorer` under the
branch name; a release tag `vX.Y.Z` publishes `:vX.Y.Z` and moves
`:latest` (prerelease tags don't).

## Demo mode

`?demo=1` (kept for the tab until `?demo=0`), or a build with `VITE_DEMO=1`,
runs the whole app off `src/fixtures/synthetic.ts`: ten processes covering
every status, key mode and census origin, 84 transitions (one of them
spread over four blobs), results from the zkVM and from a DKG committee,
a DKG-locked process waiting for its organizer's reveal, two fake sequencers
and generated blobs. It makes no network request and needs no
`config.json`. The unit tests and the Playwright suite use it.

## Layout

```
src/
├── main.tsx, App.tsx     entry and provider tree (theme → config → query → data → router)
├── styles/index.css      Tailwind entry and the design tokens, dark and light
├── theme/                theme preference (system/light/dark), provider, hook
├── config/               runtime config loader, <ConfigProvider>, useRuntimeConfig()
├── app/                  shell: TopBar, ThemeToggle, ChainPill, GlobalSearch, StatusBanners, Footer
├── routes/               paths.ts (URL table), router.tsx
├── pages/<view>/         one folder per view
├── components/           domain components shared by the pages (badges, links, check marks)
├── kit/                  design-system primitives; kit/charts: SVG charts
├── data/                 data source, services, store hooks, on-demand hooks
├── indexer/              in-browser event indexer, reducers, selectors, persistence
├── protocol/             decoders and clients: publics, blobs, calldata, beacon, sequencer API, releases
├── contracts/            ABIs (copied from the sequencer's sequencer/abi/)
├── fixtures/             the demo network
└── lib/                  format and URL helpers
tests/
├── vectors/              test vectors from the davinci-zkvm Rust SDK and a live Gnosis transition
└── e2e/                  Playwright smoke suite
docker/                   render.sh (entrypoint) and its test
```

Path aliases (`~app`, `~components`, `~config`, `~contracts`, `~data`,
`~fixtures`, `~hooks`, `~indexer`, `~kit`, `~lib`, `~pages`, `~protocol`,
`~routes`, `~theme`) are defined in `tsconfig.paths.json`.
