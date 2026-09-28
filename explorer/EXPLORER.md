# Explorer architecture

How the explorer is put together and what a page builds on. For the dev
loop, the configuration and Docker, see [README.md](README.md). When this
note and the code disagree, the code wins. The protocol is described in the
sequencer's [README](../README.md), the contracts in davinci-contracts
(`src/ProcessRegistry.sol`, `src/libraries/DAVINCITypes.sol`) and the guest
in davinci-zkvm `circuit/CIRCUIT.md`.

## What it is for

Anyone can see what a DAVINCI deployment is doing and check it themselves:

- an **auditor** checks the deployment, the pinned verification keys, every
  state transition and the results;
- an **organizer** watches their processes: parameters, census, key,
  progress, results;
- a **voter** checks that their vote was included and counted.

The data is technical, so every screen says in plain words what a value is,
why it matters and how to verify it. Nothing writes, there is no wallet, and
nothing is hidden: every page has the raw values behind its summary.

## Page ownership

Each view is a folder under `src/pages/`, lazy-loaded by `src/routes/pages.tsx`.
The stubs there show the data hooks working; replace them freely. Keep your
changes inside your folders; if you need a shared piece, add a new file under
`src/components/` (or a hook in a new file under `src/data/`) rather than
changing an existing one, and say so in your report.

| Folder | Route | Owner |
|---|---|---|
| `pages/overview/` | `/` | A |
| `pages/processes/` | `/processes?status=&keyMode=&census=&organizer=&q=` | A |
| `pages/process/` | `/processes/:pid` and `/processes/:pid/:tab` (`overview`, `key`, `transitions`, `votes`, `results`, `raw`); one file per tab in `tabs/` | A |
| `pages/transition/` | `/processes/:pid/transitions/:index`, and `/tx/:hash` (`tx.tsx`, resolves a hash to its transition or process) | B |
| `pages/votes/` | `/votes?pid=&voteId=` and `/votes/:pid/:voteId` | B |
| `pages/contracts/` | `/contracts` | C |
| `pages/sequencers/` | `/sequencers` (the nav item shows only when sequencers are configured) | C |
| `pages/learn/` | `/learn` and `/learn/:topic` | C |
| `pages/kit/` | `/kit`, the design review page | shared |

Every page root carries `data-testid="page-<name>"`; the Playwright suite
(`tests/e2e/smoke.spec.ts`) relies on them and on `process-count`,
`transition-summary` and the `tab-<name>` ids. Keep them, or update the
suite in the same change.

Build every link with `paths` from `~routes/paths` (`paths.process(pid, 'results')`,
`paths.transition(pid, i)`, `paths.vote(pid, voteId)`, `paths.tx(hash)`,
`paths.processes({ organizer })`, ...), never from string literals.

## Data flow

```
viem client ─┐                                      ┌─ store hooks (~data/hooks)
             ├─► Indexer ─► IndexerStore ─► selectors┤
demo fixture ┘   (~indexer)  (~indexer/types)       └─ pages
beacon / sequencers / chain ─► services (~data/services) ─► on-demand hooks (~data/queries)
```

**The indexer** (`src/indexer/indexer.ts`) checks the RPC's chain id against
the config (a mismatch stops it and raises the "Wrong network" banner), scans
every `ProcessRegistry` event from `startBlock` with chunked `getLogs`
(adaptive chunk size, `scan.ts`), folds them into the entity store with the
pure reducers (`reduce.ts`), persists the store in IndexedDB under
`chainId:registry` (`persist.ts`) and polls. It stays 2 blocks behind the
head. When the head moves, it first checks that the last indexed block still
has the hash it was indexed with; a reorg deeper than the lag clears the
cache and reindexes from `startBlock`. Per tick it re-reads `getProcess` (one
multicall at the last indexed block, so the state and the events describe the
same chain) for every process an event touched, reads each process's
`genesisRoot` once, re-reads any registry field whose read failed (they stay
null until then; a zero `dkgAdapter()` is a value, not a failure), and
resolves settlement transactions (receipt, fee, decoded calldata) and block
times in small batches. A transaction whose lookup fails three times leaves
the queue for the session (`status.skippedTx`) and is retried with backoff,
so it cannot hold up the newer ones. An idle poll costs one request.

**The demo fixture** (`src/fixtures/synthetic.ts`) builds the same store by
pushing a generated event stream through the same reducers. `?demo=1` swaps
it in, with demo services behind the same interfaces. Nothing in a page may
branch on demo mode; read `useRuntimeConfig().demo` only to explain things
(the shell already shows a demo banner).

**Services** (`src/data/services.ts`) are the reads that do not belong in the
store: blob bytes, sequencer APIs, DKG application state, metadata
documents. Pages reach them through the hooks in `~data/queries`.

## Store entities

`src/indexer/types.ts` has the full shapes. Blocks, counters and unix times
are numbers; field elements, tallies and wei are `bigint`; hex is lowercase.

| Entity | Key | Holds |
|---|---|---|
| `ProcessEntity` | pid (bytes31) | organizer, creation block/tx/time, `state` (the normalised `getProcess`: status, key mode, ballot mode, census, key, times, counters, root, result, DKG info) and the block it was read at, `genesisRoot`, transition keys, status / duration / max-voters / census changes, `results` (`ProcessResultsSet`), `decryptionRequest` (`ResultsDecryptionRequested`), event indices |
| `TransitionEntity` | `pid:index` | 0-based index (the sequencer API's), block, tx, time, sender, roots before and after, process totals after, `newVoters` and `overwrites` of this batch, `nBlobs` |
| `TxDetails` | tx hash | from, status, gas, blob gas, `fee` (gas·price + blob gas·blob price), `blobVersionedHashes` (null when the RPC omits it), calldata size, the decoded `publicValues`, `proofBytes`, `commitments`, `ys`, `kzgProofs`; `initialCensusRoot` for `newProcess` |
| `ChainMeta` | | chain id, network name, registry, start block, head block and time, block time, `registry` (immutables: program vks, `rootCVadcopFinal`, `ballotVKHash`, `ziskVerifier` and its runtime code hash, `dkgAdapter` → DKG manager and app manager, `chainID`, `pidPrefix`, `processCount`) |

A process's on-chain status stays `ready` after its end time until someone
ends it or posts results. `processPhase` (below) combines status and clock:
`upcoming`, `open`, `paused`, `closed` (ready but past the end), `ended`,
`canceled`, `results`, and `loading` before the first read.

## Hooks

Store hooks (`~data/hooks`), all memoised on the store snapshot, all safe to
call with `undefined` (they return `null` or `[]`):

| Hook | Returns |
|---|---|
| `useIndexer()` | `{ status, kind, loading, scanning, headBlock, lastBlock, progress, refresh, clearCache }`; `status.chainMismatch`, `status.errors`, `status.skippedTx` |
| `useChain()` / `useChainNow()` | `ChainMeta`; the head block's unix time (use it as "now") |
| `useNetworkStats()` | processes by status / phase / key mode / census origin, organizers, voters, overwrites, ballots, transitions, blobs, processes with results, last activity |
| `useProcesses(filter)` | `ProcessRow[]`, newest first; `filter = { status (a status or a phase), keyMode, censusOrigin, organizer, query }` |
| `useProcess(pid)` | `{ process, row, transitions, rootChain }`; also asks for a fresh `getProcess` |
| `useTransitions(pid)` | `TransitionRow[]` in index order, with gas, fee and `continuous` (root continuity) |
| `useTransition(pid, index)` | `TransitionDetail`: entity, row, process, previous/next, `tx`, decoded `publics`, and `checks` (below); asks for the tx details first |
| `useTransitionByTx(hash)` | the transition a transaction settled |
| `useActivityFeed(limit, pid?)` | `FeedEntry[]`, newest first, each with a label and an `href` |
| `useVotesPerDay(days)` | `DayBucket[]` (UTC days, oldest first): ballots, new voters, overwrites, transitions |
| `useReleaseCheck()` | the deployment's pins against the known releases (`release`, `closest`, per-pin `checks`, `complete`) |
| `useStoreSearch(query)` | `SearchHit[]` the store knows about |

On-demand hooks (`~data/queries`, TanStack Query; `data`, `isLoading`,
`error` as usual):

| Hook | Returns |
|---|---|
| `useTransitionBlobs(pid, index)` | the blobs (`FetchedBlob[]`: bytes, versioned hash, commitment, how it was bound, source) and `decoded` (`TransitionData`: vote ids, slot updates, accumulator) or `decodeError`. Beacon first, then each sequencer; `attempts` lists what failed. Waits for the settlement transaction, the block's exact time and the process's field count |
| `useVoteInclusion(pid, voteId)` | searches the process's transitions newest first for the blob that lists the vote id: `{ state, transitionIndex, checked, total, errors }`; a transition whose transaction the indexer skipped is an entry in `errors` |
| `useVoteStatus(pid, voteId)` | per configured sequencer: `pending` → `aggregated` → `processed` → `settled`, or `error` (polls until final) |
| `useTrackerProof(pid, voteId)` | the first tracker proof a sequencer serves, checked in the browser: `valid` (the path from the requested vote id's leaf reaches its root; an answer naming another vote id or process is invalid and sets `otherVote`) and `rootOnChain` (that root is one the registry held for the process); `null` when no sequencer knows the vote |
| `useSequencers()` | per configured sequencer: its `/info` and process list, polled |
| `useDkgApplication(pid)` | DKG-mode processes: epoch, aid, pool index and key, organizer key and whether its secret was revealed, the application key, and each submitted ciphertext's combine state |
| `useJsonDocument(url)` | a JSON document, e.g. `state.metadataURI` (`ipfs://` goes through a public gateway) |
| `useDeploymentDetails()` (`~data/deployment`) | the reads the indexer doesn't make: the verifier's root, the DKG manager's immutables, verifiers and cadence, the newest epoch, the adapter and app-manager links, DKG registry counts |
| `useSequencerProcessViews(...)` (`~data/sequencer-processes`) | each configured sequencer's `GET /processes/{pid}`, one page at a time |

## Selectors and decoders

Pure functions, unit-tested; use them directly when a hook does not fit.

- `~indexer/selectors`: `processRow(s)`, `processPhase`, `transitionRows`,
  `rootChain` (genesis → every transition → the registry root, with `gaps`
  and `headMatches`), `transitionDetail`, `transitionByTx`, `networkStats`,
  `activityFeed`, `votesPerDay`, `blockTimestamp` (exact or estimated from the
  head), `deploymentPins`, `releaseCheck`, `onchainRoots`, `searchStore`.
- `transitionDetail(...).checks` recomputes, from public data, what
  `submitStateTransition` enforced: the guest passed (`ok`, fail mask), root
  continuity, the proven root is the new root, the census root (the current
  one, any `CensusUpdated` root or the creation root; an on-chain census
  is `unknown`, the registry asked the census contract), `occupied_before`
  equals the previous voters count, vote counts match the event, the blob
  count, each commitment hashes to the transaction's versioned hash, and
  `sha256(commitment ‖ y …)` equals the publics' blob digest. Each is
  `pass`, `fail` or `unknown` (not read yet, or, for the two blob-hash
  checks, the RPC left out `blobVersionedHashes`). The PLONK itself and the
  KZG openings are verified on-chain; the explorer shows the program vk and
  `rootCVadcopFinal` they were checked against (the registry immutables).
- `~protocol/publics`: `decodeBatchPublicValues` (the 512-byte on-chain
  `publicValues`), `decodeBatchPublicsRegisters` (the prover's 256-byte
  view), the results guest equivalents, `failBits`, and `BATCH_REGISTERS`, a
  table of every register with its name, span, meaning and who reads it (the
  contract, the fold guest, or nobody). Ported from davinci-zkvm
  `rust-sdk/src/publics.rs`.
- `~protocol/blob`: `decodeTransitionBlobs(blobs, numFields)` (the DA cell
  layout, `CIRCUIT.md` §8), `unpackBallot(cells)` (decompress one slot
  update's ciphertexts: slow, about 0.3 ms a point, so do it on demand; the
  decoder leaves update points packed unless `{ points: 'full' }`),
  `versionedHash`, `blobEvaluationPoint`, `blobsDigest`, `blobCount`,
  `formatVoteId` / `parseVoteId`. Ported from `rust-sdk/src/blob.rs`. New
  votes, overwrites and silent refreshes are all "slot updates": the blob
  cannot tell them apart, on purpose.
- `~protocol/calldata`: `decodeRegistryCall` / `decodeStateTransitionCall`.
- `~protocol/tracker`: `verifyTracker` (port of the sequencer client's).
- `~protocol/babyjubjub`: point arithmetic, packing, and the DKG key forms
  (`reducedToCircom`, `circomToReduced`: the registry stores a DKG key in
  circomlib form, the DKG returns it reduced, as `BjjFormLib` converts).
- `~protocol/process-id`: `parseProcessId` (organizer, registry prefix,
  nonce), `processIdPrefix`, `computeProcessId`.
- `~protocol/types`: the enums with a label and a one-line explanation each
  (`PROCESS_STATUS_INFO`, `CENSUS_ORIGIN_INFO`, `KEY_MODE_INFO`).
- `~protocol/releases`: `KNOWN_RELEASES` (the davinci-zkvm pins from
  `rust-sdk/src/release.rs`: both program vks, `rootCVadcopFinal`, the
  verifier code hash, the ballot VK hash) and `matchRelease`. Add a row per
  release, newest first.
- `~protocol/limits`: protocol constants (`NUM_FIELDS`, `MAX_BLOBS`,
  `TX_BLOB_CAP`, vote-id and slot namespaces, the refresh rule).

## Blobs

A transition's blobs come from the beacon API. The slot is
`(block time − genesis time) / SECONDS_PER_SLOT` (the exact block time, never
an estimate). `blob_sidecars/{slot}` goes first: it returns the block's blobs
with their commitments, and a sidecar belongs to the transaction when its
commitment hashes to one of the transaction's versioned hashes
(`binding: 'commitment'`). Any other answer (beacons past Fulu may drop
sidecars) falls back to `blobs/{slot}?versioned_hashes=…`, taken only when it
returns exactly one blob per hash (`binding: 'beacon-filter'`: the beacon
matched them). Beacons prune blobs after about 15 days on Gnosis Chain; then
the hook falls back to each configured sequencer's
`/processes/{pid}/transitions/{i}/blobs` (`binding: 'sequencer'`, tied by
position only). The explorer does not recompute KZG commitments from the
bytes, so show the binding next to decoded content; a `kzg-wasm` check would
close that gap.

## Kit and components

`~kit` is the davinci-dkg kit with theme-aware tokens: `Card`, `CardHeader`,
`CardBody`, `Panel`, `KeyValue`, `Stat` / `StatRow` / `StatCell`,
`DataTable` (sortable, `virtualized` above ~50 rows), `Tabs`, `Timeline` /
`TimelineRow`, `Badge`, `Callout`, `Tooltip`, `CopyButton`, `Address`,
`Hash`, `TxCell`, `BlockCell`, `Input`, `Select`, `Toggle`, `Button` /
`ButtonLink` / `buttonClasses`, `Dialog`, `Popover`, `ProgressBar`,
`Pagination`, `Skeleton`, `EmptyState`, `PageContainer`, `SectionHeader`,
`Stack`, icons. Charts in `~kit/charts`: `StackedBars`, `Sparkline`, `Donut`
(with `ChartFrame` and the scale helpers). `/kit` renders all of them; check
it in both themes after a design change.

`~components` holds the domain pieces: `ProcessPhaseBadge`, `KeyModeBadge`,
`CensusOriginBadge` (each with its explanation on hover), `ProcessIdLink`,
`TxLink` (in-app `/tx/:hash` plus the block explorer), `Timestamp` (UTC, and
"5 min ago" against the chain head), `NativeAmount` (wei in xDAI/ETH),
`CheckMark` (pass / fail / unknown), `Explain` (the "what is this" info
glyph), `MissingEntity` (skeleton until the first poll, then "not found"),
`CodeBlock` (a command with a copy button) and `HashLink`. Link to a section
of the page with `HashLink`, never a plain `href="#id"`: the router's scroll
restoration sends a plain fragment link to the top of the page.

## Design rules

- Tokens are surface levels, not colours: `obsidian` canvas, `carbon` cards,
  `onyx` hover, `charcoal` hairlines, `ghost` / `silver` / `pewter` / `ash`
  text from strong to quiet, `emerald` the one accent, `amber` and `red` the
  two semantic colours, `field` for form outlines, `on-accent` for text on an
  emerald fill. Both themes set them (`src/styles/index.css`); never use raw
  hex in a component, or the other theme breaks.
- Inter for text, JetBrains Mono for every address, hash and number in a
  table (`font-mono tnum`). Labels are `label-caps`.
- Borders, not shadows; `shadow-pop` only for popovers and tooltips.
- Shorten hashes with copy and a link; the full value is always in the
  tooltip. Every value gets an explanation, inline or through `Explain`.
- Wide content scrolls inside its panel; the page never scrolls sideways.
- Memoise on the store snapshot or on anything in it: every publish copies
  the collections and entities, so their identities change with the data.

## Demo fixture

`demoFixture()` (from `~fixtures/demo`) returns the fixture with a
`featured` set for tests and docs: `openProcess` (40 transitions, sequencer
key, on-chain census), `resultsProcess` (zkVM results), `awaitingReveal`
(DKG-locked, tally submitted, secret not revealed), `multiBlob` (a
transition over four blobs), `settledVote` (a vote id with a tracker proof
that verifies) and `pendingVote`. Roots after each transition are the roots
of a vote-id tree (`smt.ts`), so tracker proofs verify with the real
verifier; publics, digests and versioned hashes are consistent, so every
settlement check passes; KZG commitments are random bytes. Demo sequencer 0
settles, sequencer 1 is an observer.
