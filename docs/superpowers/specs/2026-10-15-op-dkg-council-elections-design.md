# DKG, Council and davinci-elections on OP Mainnet; Gnosis retired

Status: owner-approved scope (2026-10-15 15:08-15:30 UTC). Sub-projects 3 and 4 of the OP stack rollout
(`2026-10-13-op-core-stack-design.md`), plus the Gnosis teardown. Executed autonomously with Fable audits
and the independent reviewer gate, as the core stack was.

## Goal

Everything DAVINCI runs in production is on OP Mainnet, released, and Gnosis Chain is shut down:
the sequencer (L1-blob mode) with DKG and Council key modes, the DKG operator network, the Council relayer
and app, the SDK, and davinci-elections at `elections.davinci.vote`.

## Owner decisions

1. Council on OP now, with the `circuits-v1` single-party development setup (`ALLOW_DEV_SETUP=true`), as on Gnosis.
2. DKG operators: the 3 Gnosis operator keys and Railway nodes (`davinci-dkg-gnosis`), repointed to OP.
3. davinci-elections: wipe the database and start fresh on OP; final Gnosis tallies stay on Gnosis.
4. Funding: the owner's OP key `0x470d…` deploys the contracts and tops up operators, relayer and treasury,
   at most 5 mETH in total, every transfer logged in the ledger.

## Facts that shape the design

- The OP registry `0x7bfa…558F` has `dkgAdapter() == councilAdapter() == 0`, and both are immutable
  (constructor-only, `ProcessRegistry.sol:107,112,197-199`). DKG or Council on OP therefore needs a **new
  OP registry**. It reuses the ZiskVerifier, the pins and the grace settings of `0x7bfa` (`PINS_FROM_REGISTRY`)
  and the L1BlobVerifier `0x3B95…7529`. Every preset (node, SDK, DKG, Council, elections) moves to it.
- `0x7bfa` keeps its 34 processes (bench campaigns only). It is retired like `0x8b63` and `0xC5f8`: no
  production node follows it after the switch.
- The DKG manager and Council manager are ownerless after wiring; the DKG adapter reads
  `manager.appManager()` at construction, so the DKG must be fully wired before the registry deploys.
- DKG windows are counted in blocks; Gnosis (5 s) used epoch 17280, selection 36, assembly 12, gap 1,
  inactivity 50400. OP (2 s) keeps the same wall-clock durations: epoch 43200, selection 90, assembly 30,
  gap 1, inactivity 126000; the node's `--decrypt-lookback-blocks` 126000. Thresholds and floors stay as on
  Gnosis.
- Public OP RPCs refuse archive `getLogs` and finalized `eth_call`; DKG nodes, the Council relayer and the
  explorer use the owner's private OP RPC (never sent to browsers).
- The sequencer's DKG and Council flows do not depend on the DA mode, but DKG in L1-blob mode was never run
  end to end (the e2e harness refuses it).
- The SDK still pins ZisK 1.3.0 and has no OP preset; davinci-elections passes no pins and defaults to Gnosis.

## Work, in order (each step gated as stated)

1. **DKG contracts on OP** (davinci-dkg). Add an `optimism` preset (Go `config/networks.go`, TS SDK, UI wagmi,
   explorer config) with the OP parameters above. Rehearse `make solidity-deploy` on an anvil fork of OP, then
   deploy with the owner's key. Gate: parameters read back, verifier bytecode equal to Gnosis, sources
   verified on Optimistic Etherscan if a key is available.
2. **Council contracts on OP** (davinci-dkg-council). Add `scripts/optimism/deploy.sh` from the Gnosis
   template, `ALLOW_DEV_SETUP=true`. Rehearse on a fork, deploy. Gate: code-hash pins match `CouncilRelease.sol`.
3. **New OP registry** (davinci-contracts). Keep `deployments/10.json` as `10-0x7bfa.json`; rehearse on a fork,
   then `deploy_all.sh` with `CHAIN_ID=10`, `PINS_FROM_REGISTRY=0x7bfa…`, `L1_BLOB_VERIFIER=0x3B95…`,
   `EXPECT_DA_MODE=l1`, `EXPECT_GRACE=300,240,600,1800,60`, `DKG_MANAGER`, `COUNCIL_MANAGER`. Gate:
   `verify_deployment.py`, adapters non-zero and bound to the managers, pins equal to `0x7bfa`.
4. **DKG operators on OP**. Repoint the 3 Railway nodes to the `optimism` preset and the private RPC; fund each
   operator. Gate: `activeCount` reaches 3, an epoch goes Live with its pool keys, a `dkgapp` round trip works.
   The DKG explorer (DO App Platform) points at OP.
5. **Council relayer and app on OP**. New Railway services from the Gnosis ones with an OP config
   (`config.optimism.json`, the new registry, a new relayer hot key, an explicit daily budget). Gate: a council
   ceremony on OP reaches Live.
6. **Sequencer**. Preset `OPTIMISM` → the new registry and start block; `docs/deployments.md` lists the DKG and
   Council addresses. Lift the e2e L1-mode DKG guard and run `DAVINCI_E2E_DKG=1` in L1-blob mode (GPU). A live
   OP test of the three key modes (sequencer, DKG, Council) on the new registry. Fable review, then the
   independent reviewer confirms production readiness of the final commit.
7. **SDK v4** (davinci-sdk). An `OPTIMISM` preset (the new registry, start block, public RPCs, Ethereum beacons)
   as the default, Gnosis retired, release pins moved to ZisK 1.3.2 (equal to davinci-zkvm `release.rs`),
   fixtures re-synced, the vendored ABI re-synced to the deployed commit, an OP live e2e, a `major` changeset.
8. **davinci-elections on OP**. SDK v4; chain-neutral config names (`CHAIN_ID`, `RPC_URLS`, a separate browser
   RPC list), the committee registry fallback fixed, top-up defaults in ETH, Gnosis/xDAI copy replaced, key modes
   discovered from the registry. Gate: e2e journeys against OP, `smoke:live`.
9. **Releases**: davinci-zkvm v0.3.0 (artifacts to DO Spaces `zkvm/v0.3.0/`), davinci-contracts (the new OP record),
   davinci-dkg and davinci-dkg-council (OP presets), davinci-sequencer v0.6.0, davinci-sdk v4.
10. **Production switch**: `sequencer2.davinci.vote` → the OP node (`:latest` with watchtower, seq-a keys,
    `http://10.200.0.26:8080` prover); davinci-elections deployed on Railway with OP config and a wiped database;
    a live organizer + voter journey on `elections.davinci.vote` in each key mode.
11. **Gnosis teardown**, stop first, delete after the OP stack is verified: the Railway `davinci-dkg-council-gnosis`
    project, any remaining Gnosis-only services, the elections Gnosis config; xDAI balances swept to the owner if
    the owner asks. Done already: the z6 container `davinci-sequencer-1` (removed 2026-10-15 15:25, config kept).

## Non-goals

A Council multi-party setup; DKG parameter retuning beyond the wall-clock rescale; keeping Gnosis history in the
elections app; moving the 34 `0x7bfa` processes.

## Risks

- A wrong DKG window or threshold is permanent for that manager (ownerless): rehearse on a fork and read back.
- A new registry address must reach every consumer before the switch; a mismatch fails closed (pins check,
  `DeploymentPinError`), never silently.
- The private OP RPC must never reach a browser (elections CSP header, `/api/public/config`).
- Council `circuits-v1` is a development setup on mainnet, by the owner's decision.

## Revisions after Fable's validation (2026-10-15 15:40 UTC)

1. **Council app RPCs.** The Council SDK reads at an explicit finalized block number and needs two agreeing RPCs
   (`sdk/src/client.ts:168-232`, `scripts/render-ui-config.sh:106-108`). Browser RPCs for the Council app on OP:
   `https://mainnet.optimism.io` and `https://optimism.drpc.org` (publicnode refuses ~600-block-old state). Each
   ceremony transition waits for OP finality, measured ~19 min behind the head; documented for users. Gate for step 5:
   the UI's finalized anchor and authenticated reads work against those two.
2. **Elections data.** Before the wipe, `pg_dump` the database (kept with the ledger). The documents table is content
   addressed and chain-agnostic (`apps/api/src/services/documents.ts:7-8`, the on-chain `metadataURI`): it is kept, so
   Gnosis processes' metadata stays readable; every other table is wiped.
3. **Gnosis DKG and Council before repointing.** Before step 4 (and before the Gnosis relayer stops): enumerate R2
   (`0x20b9…`) processes with `dkgResultsRequested && status != Results` and ceremonies on `0x2f5b…` with open requests;
   proceed on zero, otherwise ask the owner. The Railway volumes stay (node state is per `<datadir>/<chainid>-<manager>`).
4. **Order.** SDK v4 is published (or an rc) before davinci-elections is built on it; davinci-zkvm v0.3.0 precedes the
   sequencer tag (CI ref, pins); the contracts tag precedes the sequencer tag (`CONTRACTS_REF`).
5. **Production pinning.** The production node runs `DAVINCI_SEQUENCER_TAG=v0.6.0` with `DAVINCI_REGISTRY` and the
   start block set explicitly: a later release never re-points or crash-loops it through watchtower. The prover moves
   from `release-zisk-1.3.2` to `v0.3.0` explicitly, between proving jobs.
6. **`sequencer2.davinci.vote`.** Its proxy is not on z6 and expects the upstream `10.200.0.26:9090`, which the removed
   Gnosis container served. Right after step 3 the OP node (release candidate, the new registry, seq-a keys) comes up on
   that address and port, so the hostname serves again without touching the proxy; it moves to the v0.6.0 image at the
   release.
7. **Council relayer on OP.** `COUNCIL_STATE_GAS=false`; the daily budget counts `gasUsed × effectiveGasPrice` only, so it
   is sized with OP's L1 data fee in mind; the 30-min `SENT_TIMEOUT_MS` against the ~19-min finality lag is accepted.
8. **DKG explorer and defaults.** `optimism` from `viem/chains` in `ui/src/app/wagmi.ts` (else 12-s block countdowns);
   `DefaultNetwork` and the SDK `NODE_DEFAULT_NETWORK` become `optimism`, Gnosis kept only as history, as in the sequencer.
9. **DKG node RPCs.** The private OP RPC first, public OP RPCs as failover (the node pools endpoints).
10. **DKG in L1-blob mode, the e2e.** The lifted guard must run DKG automatic and locked processes to Results in L1-blob mode,
    with a non-settling follower that rebuilt its state from Ethereum blobs confirming the same `requestResultsDecryption`
    inputs. There is no Council e2e: Council is covered by the live OP test only.
11. **Risk.** The DKG lottery seed is an OP block hash (`Sizes.sol:156`, `DKGManager.sol:378,426-437`): the OP sequencer
    builds every block and picks the block `createEpoch` lands in, so it can bias the committee lottery (not extract keys)
    and can censor `claimSlot` for 90 blocks to abort an epoch. With three owner-run operators the threshold saturates and
    the lottery is a no-op today; it matters once the operator set opens. Documented in `docs/security.md`.
12. **Registry deploy.** Rename `deployments/10.json` to `10-0x7bfa.json` before the run (the script overwrites it and
    regenerates `golang-types/addresses.go`); verify the sources on Optimistic Etherscan (needs an Etherscan v2 key).
