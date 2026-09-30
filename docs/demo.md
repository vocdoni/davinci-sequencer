# Demo elections

`e2e/tests/demo.rs` fills a live deployment with demo elections covering every ballot kind,
census origin, key mode and organizer action. It spawns nothing: it drives nodes that already run
(`DAVINCI_DEMO_NODES`, default `http://127.0.0.1:9090,http://127.0.0.1:9091`), their provers and
the deployment's DKG committee. It is gated by `DAVINCI_E2E_DEMO`, which names one phase per run:

| Phase | What it does |
|---|---|
| `prepare` | Draws the voter keys, the CSP key and the seed behind every ballot into a private directory (`DAVINCI_DEMO_DIR`, default `~/.davinci-demo`, mode 0700), and writes the public census and metadata files into `e2e/demo/`. Run again, it reuses the keys and rewrites the same files. |
| `check` | Proves every planned ballot against the circuit, offline. |
| `run` | Checks that the published files are served unchanged, creates the elections with their census and metadata URIs under `DAVINCI_DEMO_BASE_URL` and the SHA-256 of each `metadata.json` as its metadata hash, casts the votes, runs the organizer actions, waits for every result and prints a report. |

`run` needs the organizer key file in `DAVINCI_DEMO_ORGANIZER_KEY` (required, there is no default),
the circom artifacts and the built census contract project. The chain comes from `DAVINCI_E2E_RPC` and
`DAVINCI_E2E_REGISTRY`, defaulting to the `gnosis` preset. It records its progress in a state
file next to the keys and resumes from it without duplicates. With
`DAVINCI_DEMO_EXPLORER_URL` the report links every election to that explorer.

`e2e/bench.sh` runs any phase in Docker:

```bash
DAVINCI_E2E_DEMO=prepare e2e/bench.sh
DAVINCI_E2E_DEMO=check e2e/bench.sh
# commit and push e2e/demo, then:
DAVINCI_E2E_DEMO=run \
  DAVINCI_DEMO_BASE_URL=https://raw.githubusercontent.com/vocdoni/davinci-sequencer/<commit>/e2e/demo \
  DAVINCI_DEMO_ORGANIZER_KEY=/path/to/organizer.key \
  e2e/bench.sh
```

## Waves

`DAVINCI_DEMO_WAVE` picks the election set. Each wave keeps its own keys, state and public
directory.

| Wave | Files | Elections |
|---|---|---|
| 1 (default) | `voters.json`, `state.json`, `e2e/demo/` | Eight elections, one of every ballot kind, census origin, key mode and lifecycle. |
| 2 | `voters-wave2.json`, `state-wave2.json`, `e2e/demo/wave2/` | Nineteen elections, most in English, Spanish and Catalan: every davinci-sdk ballot preset, every census origin with updates during voting, keys from both nodes and the DKG, every organizer control, and votes the nodes must refuse. |
| 3 | `voters-wave3.json`, `state-wave3.json`, `e2e/demo/wave3/` | A live meeting: three votes, one after another, each closed by the organizer while the nodes still hold votes, so every vote settles in the grace window. |

Wave 2 also exercises the edge cases: a reweighted member whose pending vote errors with
`census changed, recast` and who votes again, a 400-voter approval split over several
transitions, two elections without votes, a metadata hash that does not match its document, and
refused votes checked by status and code. Against nodes on the default batching policy and a
180 s grace, it takes two to three and a half hours. Wave 3 takes 30 to 45 minutes.

## Running again on a new registry

Move the `state*.json` files aside, keep the `voters*.json` files (they hold the keys), and run
with the same `DAVINCI_DEMO_BASE_URL`. The census and metadata files already published at that
commit are reused, since neither depends on the registry.
