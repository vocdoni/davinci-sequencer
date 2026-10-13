# OP Core Stack Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Settle 512-vote batches at every field count on OP Mainnet by posting one transition in several Ethereum blob transactions, deploy a registry on the davinci-zkvm v0.2.0 program vks with a grace window that fits the Ethereum leg, retire Gnosis, release davinci-contracts `v0.0.51` and davinci-sequencer `v0.6.0`, and switch this host's production node to OP.

**Architecture:** `davinci-state` takes a per-batch blob cap up to the guest's `MAX_BLOBS`; the node keeps two caps (per transaction from `eth_config`, the chunk size; per batch `MAX_BLOBS` in L1-blob mode). `l1da::with_post` posts a transition's chunks in order through the existing window-1 poster while the proof runs, and `l1da::settle` waits for every chunk, anchors one L1 proof of all chunk transactions (depth from the oldest block, origin and record checks against the newest) and re-posts one chunk or all of them. The actor's seal budget reserves `POST_TIME` per extra chunk and sizes a batch down to fewer chunks. The contracts need no source change: test-only fixtures and forge tests prove a two-transaction settlement with the real `L1BlobVerifier`.

**Tech Stack:** Rust 1.95 (edition 2024), alloy 2.5, tokio, axum 0.7, davinci-zkvm v0.2.0 (`rust-sdk`, `input-gen`, `ziskemu` in `~/.zisk/bin`), Foundry 1.8.3 (`~/.foundry/bin`), Go 1.27 (module `go 1.25.5`) with go-ethereum v1.17.1, Node 22 with the checkout's `node_modules` (hardhat, typechain), abigen v1.17.8, python3 (stdlib, `-I`), `gh` over HTTPS, docker (read-only until the owner's production switch).

**Spec:** `docs/superpowers/specs/2026-10-13-op-core-stack-design.md` (davinci-sequencer). Read every section before Task 1; §3 (node), §4 (registry), §8 (release) and §9 (production) bind the tasks below.

## Global Constraints

- **Grace of the new OP registry**, immutable: `GRACE_DEFAULT=300 GRACE_FLOOR=240 GRACE_CEIL=600 GRACE_MAX_TOTAL=1800 NOTICE_MIN=60`; the deployed `defaultGrace/graceFloor/graceCeil/graceMaxTotal/noticeMin` must read `300/240/600/1800/60` (`DeployAll.s.sol` silently keeps 180/150/600/1800/60 for an unset or misspelled variable).
- **Pins of the new OP registry** (davinci-zkvm v0.2.0): `batchProgramVK` `0xa0ad5c4c7e39d4a7007933158181418a752e6c1563a211af84c579640db435fa`, `resultsProgramVK` `0xe693589453ecaee63eba697c48d4c5321a714bb18c2d84a97437b3f368578f68`; `rootCVadcopFinal` `0x05006517b6ccde5da4d890587ba62845b5af8a307c00e87d4b9d05099b16dc80` and `ballotVKHash` `0xbf1e6590bb1ba883d601c4d7d1c6fa2722a78590716874019db6d68fc776bb0e` unchanged.
- **Reused verifiers on OP**: `ZISK_VERIFIER=0x6781FFF50D615CB3F331CCD3Bdad3416d4ce0d5c`, `L1_BLOB_VERIFIER=0x3B95BC14F43ed8Addb83F84D10A949De74eE7529` (on `L1Block` `0x4200000000000000000000000000000000000015`). The 2026-10-12 registry `0x8b63fAE96948D69CF92Eb40e4EE0fCE40962267d` is retired (v0.1 vks).
- **Two caps.** Per transaction: what `resolve_blob_cap` gives (the L1's `eth_config` blob schedule, `--max-blobs-per-tx`, else 6); it is the chunk size. Per batch: the guest's `MAX_BLOBS` (32) in L1-blob mode, the per-transaction cap in blob mode. `select_batch`, `capacity_estimate` and the prepare-time refresh check use the per-batch cap. The node logs both (`blob cap per transaction`, `blob cap per batch`); the live L1-blob run asserts the per-transaction one (6) in `e2e.rs` (Task 5).
- **Chunks.** Chunk `i` holds blobs `cap*i ..` in order (the last may be shorter); the chunks' versioned hashes concatenated in chunk order are the transition's.
- **POST_TIME = 24 s**: the seal guard requires `l1_settle_lag + (chunks - 1) * POST_TIME + settle_margin <= window`; a window that fits fewer chunks sizes the batch down, never holds it.
- **MAX_REPOSTS = 3 per transition**: re-posting one chunk counts one, re-posting every chunk counts one. Worst case per transition: `chunks + MAX_REPOSTS * chunks` posts, each under `--l1-max-post-fee` (13 blobs in 3 chunks: 12 posts, 0.012 ETH at the default cap).
- **Merge order: contracts first.** davinci-contracts PR merged by rebase and tagged `v0.0.51`, then davinci-sequencer sets `CONTRACTS_REF: v0.0.51`, merges by rebase and tags `v0.6.0`. Before the `v0.6.0` tag, `davinci-sequencer-1` is taken off `:latest`.
- **Branches.** davinci-sequencer and davinci-contracts: `feat/op-core` from `feat/compact-da-stream`, rebased once onto `origin/main` at creation (Tasks 1 and 7). davinci-zkvm: one test-only commit on `main`.
- **Commits.** Titles `type(scope): lowercase title`. No AI attribution and no `Co-Authored-By` (or any other) trailer in commits, PR bodies or release notes. Stage files by name.
- **Never push, merge, tag or release without the owner's explicit confirmation**, step by step (see "Owner checkpoints"). Pushes and PRs go through `gh` and HTTPS remotes (SSH pushes fail on this host).
- **Build output never in `/tmp`.** Cargo targets stay in each repository's `target/`; every shell exports `TMPDIR=$HOME/.cache/op-core/tmp`; e2e logs under `~/.cache/davinci-e2e`; `make e2e` always gets `E2E_LOG=$HOME/.cache/davinci-e2e/<name>.log`.
- **Secrets never printed, logged or committed**: `~/optimism-privkey.txt`, `~/optimism-rpcs.txt`, `~/.config/davinci-l1blob/*.key`. Private keys never sit in an exported environment variable or a container's environment: nodes and the harness get key *files*, and `forge` gets `PRIVATE_KEY` only inside the deployment's subshell. The private OP RPC URL is exported only by `~/.config/davinci-l1blob/env.sh` (as `OP_RPC`, for the deployment and the live harness) and never reaches a container, with one exception chosen by the owner: the production node's `DAVINCI_RPC_URL` (Task 14 Step 3) lists the owner-provided endpoint `$OP_PRIVATE_RPC` first, then the preset's two public ones. Commands read secrets through `env.sh` (`owner_key`, `key_of`) and pipe anything that may echo the OP RPC through `redact`. Deployments set variables in a subshell, never in a `.env`.
- **Shared GPU prover** at `DAVINCI_ZKVM_URL` (default `http://127.0.0.1:8080`, the production prover): before every GPU run `curl -s "$DAVINCI_ZKVM_URL/health"` must show `"queue_len":0`; one GPU workload at a time; never start a run while production jobs are queued.
- **Spending caps** for the whole plan (resolved by the owner as written; `seq-a` keeps what is left for production, no production top-up): the owner's OP account at most 0.0002 ETH (the deployment, organizer transactions), the owner's Ethereum account at most 0.0005 ETH (a top-up of `seq-a.l1` only if its balance is below the run's minimum); `seq-a.l1` at most 0.0003 ETH of blob posts before the production switch. Every spending step has a go/no-go check.
- In `/home/p4u/davinci-zkvm` never read, stage or touch `SILENT-REVOTING.md`, `paper.tex`, `token.overleaf`.
- Strict Fable 5.1 audits: one per task (the reviewer of subagent-driven development), and a final whole-release audit (Task 13 Step 1) before any push or merge.

## Decisions beyond the spec

- **`select_batch_within(pending, max_voters, must_include, max_votes, max_blobs)`** in `davinci-state` sizes a batch below the per-batch cap; `select_batch` calls it with the cap. The seal guard passes `chunks_fit * chunk` (never above the cap); `prepare` keeps checking the per-batch cap, which such a selection never passes.
- **`Posting` carries the mined chunk posts** (`posts: Vec<Posted>`, a prefix in chunk order) and the error that stopped posting (`error`); `L1Da::new` takes the chunk size, so the boot builds `L1Da` after resolving the per-transaction cap.
- **`choose_anchor(oldest, newest, head, origin, recorded)`** also returns `Wait` while OP's origin is below `newest`, so it is correct on its own.
- **The contract fixtures are synthetic and generated by `test/vectors`' `go run .`** (CI's drift check covers them): Prague-shaped headers, blob transactions carrying the fixture transitions' versioned hashes, plus a worst-case proof (64 headers, three transactions, 13 blobs). The real-chain check of two transactions is the `ANVIL=1` test and the GPU runs.
- **The 512 x 16 run on anvil** (`e2e/tests/l1_big.rs`) gives its process the registry's 600 s ceiling grace: a node that has not proved yet estimates 0.8 s a vote at 16 fields, so 512 votes (410 s of estimate plus 30 s base) fit after the end only above the 300 s default. Phase-2 votes are queued while the process is paused, so the batch is sealed by the flush after the end, as the spec asks. The same test runs live in Task 12.
- **The davinci-zkvm fixture is recorded by that test** (`DAVINCI_E2E_RECORD_DIR`): the node does not log job ids and the prover lists none, so the test taps the node's `/prove` calls and fetches the first job's `/snark` and `/publics`. The wire test's tuple of publics follows the recording.
- **Bindings are generated with abigen v1.17.8**, the version the contracts' `bindings-sync` and `release-tag` workflows install today (`go install …/abigen@latest`): v1.16.5 output differs (imports, `ErrEventSignatureMismatch`), so its bindings would make the bot push a regeneration commit and could fail the release's drift check.
- **`deployments/10.json` is renamed `deployments/10-0x8b63.json`** before the deployment writes the new record, so the retired registry keeps its verified record.
- **Compose mounts a second secret** (`sequencer_l1_key`, `SEQUENCER_L1_KEY_FILE`, default `./sequencer-l1.key`) and sets `DAVINCI_L1_PRIVKEY_FILE` itself in `x-sequencer-env` (like `DAVINCI_PRIVKEY_FILE`; `.env.example` keeps only the commented `SEQUENCER_L1_KEY_FILE` line): with `optimism` as the default a signing compose node needs its Ethereum key, and compose refuses to start until `./sequencer-l1.key` exists, so the v0.6.0 notes and the README say to create it before the image moves.
- **`davinci-sequencer-1` is removed, not only stopped**: the host's watchtower runs `--include-stopped --revive-stopped`, so a stopped container that carries the watchtower label is updated to `v0.6.0` and restarted on its Gnosis environment. Its `docker inspect` is saved first; its keys and datadir are bind mounts and stay.
- **The production OP node's OP RPCs** are set in its environment: `DAVINCI_RPC_URL=$OP_PRIVATE_RPC,https://optimism-rpc.publicnode.com,https://mainnet.optimism.io` (the owner's endpoint first, then the `optimism` preset's two public ones). The committed `optimism` preset is not changed. The URL therefore lands in `docker inspect`; the saved inspect file is mode 600 and treated as private. Resolved (owner).
- **`e2e/tests/node_smoke.rs`** drives an already running node through a three-voter CSP election; it is the "small live election" of the production switch.
- **Task 9 also reruns the blob-mode e2e**: the selection and seal code it changes runs in both modes.
- **Task 11 boots the built node as an observer on the filled preset**, and checks that it refuses the retired registry, before anything is committed.
- **Live spend bounds**: go only at Ethereum base plus priority fee at most 0.5 gwei and a blob base fee at most 0.02 gwei (`op_live`) or 0.01 gwei (the live 512 x 16 run), with a stop rule on `seq-a`'s balance during each run.
- **Merges**: `gh pr merge --rebase`; davinci-contracts' `main` requires a code-owner review, the owner decided to merge with `gh pr merge --rebase --admin` after CI is green and the final Fable audit passes, and the PR body says the merge relied on the final Fable audit as its review. Both `feat/op-core` branches stay on GitHub: the rebase merge rewrites commit ids, and `deployments/10.json` names the `feat/op-core` commit the registry was built from.
- **davinci-contracts' CHANGELOG entry sits under the release date** (the file's convention); `package.json` moves to 0.0.51 by the release bot, and `main` is fast-forwarded to the bot's bump commit, as for v0.0.50.
- **The production OP node is a `docker run` container like the Gnosis one** (`--user 1001:1001`, the same ports and prover URL), named `davinci-sequencer-op`, with `seq-a`'s key files mounted read-only from `~/.config/davinci-l1blob/` (not copied) and the datadir `~/.davinci-op/data`. The image is `ghcr.io/vocdoni/davinci-sequencer:latest` **with** the watchtower enable label (owner's choice, resolved): the node follows releases automatically, so a release that changes required configuration must ship with upgrade notes before its tag (a `:latest` move restarts the node mid-flight; a restart re-proves and replaces a pending chunk).

## Review Focus

1. **A fee spike between two chunks** (chunk 0 mined, chunk 1 over `--l1-max-post-fee`): chunk 1 waits at the cap, chunk 0 is never posted again, and the transition lands once fees fall. Test: Task 2 `a_fee_spike_between_chunks_waits_for_the_second`.
2. **Two processes posting at the same time through the one-post window**: chunks go out one at a time at consecutive nonces, and each transition keeps its own chunks in chunk order. Test: Task 2 `two_transitions_interleave_their_chunks`.
3. **A recorded origin, then a single-chunk re-post after a reorg**: the stale record anchors nothing, the proof anchors at the current origin, and nothing else is re-posted. Test: Task 2 `a_single_chunk_repost_drops_the_recorded_origin`.
4. **The largest proof a node sends** (64 headers, three transactions, 13 blobs) must fit OP's 128 KB transaction limit and the gas estimate. Test: Task 7 `test_WorstCaseProof_FitsOP` (measured 47,168 bytes, 3.49 M gas for `verify`).
5. **Boot on the 240 s floor**: a signing node with the L1 defaults, or with a blob-mode 60 s margin carried over, boots; a leg and margin above 240 s refuse. Test: Task 3, `budget_decision` in `sequencer/src/api/mod.rs`.

---

## Shell setup (every task, every new shell)

```bash
mkdir -p $HOME/.cache/op-core/tmp $HOME/.cache/davinci-e2e
export TMPDIR=$HOME/.cache/op-core/tmp
export PATH=$HOME/.zisk/bin:$HOME/.foundry/bin:$PATH
```

Agents whose shell keeps no state prefix every command with these exports. Run verification pipelines through `rtk proxy bash -c '…'` (or a script file): the `rtk` hook filters `cargo`, `go`, `git` and `grep` output, and the `Expected:` lines below are raw output.

## File Structure

davinci-sequencer (`/home/p4u/davinci-sequencer`, branch `feat/op-core`):

- Modify `state/src/process.rs`: `set_blob_cap` up to `MAX_BLOBS`, `select_batch_within`.
- Modify `state/tests/transition.rs`, `state/tests/dryrun.rs`: the cap tests, the nine-blob dry run.
- Modify `sequencer/src/l1da/mod.rs`: `L1Da.chunk`, `chunks`, `Posting.{posts, error}`, `post_chunks`, `with_post` over chunks, `POST_TIME`, `batch_blob_cap`, `chunks_fit`.
- Modify `sequencer/src/l1da/settle.rs`: `choose_anchor(oldest, newest, …)`, the multi-transaction `settle`.
- Modify `sequencer/src/api/mod.rs`: `L1Da` built after the cap, both caps logged, the 240 s budget test.
- Modify `sequencer/src/actor.rs`: the per-batch cap at spawn, the per-chunk seal guard.
- Modify `sequencer/tests/l1da.rs`, `sequencer/tests/actor_l1.rs`, `sequencer/tests/web3_anvil.rs`, `sequencer/tests/common/mod.rs`, `sequencer/tests/common/l1.rs`.
- Modify `sequencer/src/config.rs` (tests), `client/src/networks.rs`: Gnosis retired, `optimism` the default; later the new registry.
- Modify `e2e/src/net.rs`, `e2e/src/chain.rs`, `e2e/tests/e2e.rs`, `e2e/tests/demo.rs`; create `e2e/tests/l1_big.rs`, `e2e/tests/node_smoke.rs`.
- Modify docs: `README.md`, `docs/configuration.md`, `docs/deployments.md`, `docs/lifecycle.md`, `docs/testing.md`, `docs/demo.md`; `docker-compose.yml`, `.env.example`.
- Release: `Cargo.toml`, `Cargo.lock` (workspace 0.6.0), `.github/workflows/main.yml` (`CONTRACTS_REF: v0.0.51`).

davinci-contracts (`/home/p4u/davinci-contracts`, branch `feat/op-core`):

- Create `test/vectors/l1blob.go`; modify `test/vectors/main.go`, `go.mod`, `go.sum`; generate `test/vectors/l1blob/{one_tx,two_tx,two_tx_reposted,worst_case}.json`.
- Modify `test/L1BlobVerifier.t.sol`, `test/TransitionL1.t.sol`.
- Modify `script/verify_deployment.py` (`--grace`), `deploy_all.sh` (`EXPECT_GRACE`), `.env.example`, `docker-compose.yml`, `docs/deployment.md`, `docs/verification.md`, `README.md`, `CHANGELOG.md`.
- Regenerate `artifacts/`, `typechain-types/`, `golang-types/ProcessRegistry.go`.
- Deployment: rename `deployments/10.json` to `deployments/10-0x8b63.json`; create `deployments/10.json`, `broadcast/DeployAll.s.sol/10/run-latest.json`.

davinci-zkvm (`/home/p4u/davinci-zkvm`, `main`): `rust-sdk/testdata/snark_batch.json`, `rust-sdk/testdata/publics_batch.bin`, `rust-sdk/tests/wire.rs`.

Outside the repositories (never committed): `~/.config/davinci-l1blob/op-core.sh`, `~/.cache/davinci-l1blob/bin/fill_op_core.py`, `~/.cache/op-core/wire_fixture.py`, `~/.cache/gobin-latest/abigen`, `~/.cache/op-core/fixture/`, `~/.cache/op-core/observer/`, `~/.davinci-op/` (the production OP node's datadir and its saved `docker inspect`; its keys stay in `~/.config/davinci-l1blob/` and are mounted read-only).

Not changed, checked: `sequencer/src/l1da/post.rs` (one transaction per `post`, records per transaction: a restart replaces whichever chunk is pending), `sequencer/src/storage.rs` (no record shape change, no `SCHEMA_VERSION` bump), `sequencer/src/web3/blobs.rs` (`blobs_for_l1` already groups by block in first-seen order, tested with `[43, 42]`), `sequencer/src/web3/l1proof.rs` (`build` and `verify_local` take any number of transactions), every `.sol` under `src/` in davinci-contracts.

---
### Task 1: Branch, and a per-batch blob cap up to `MAX_BLOBS` in davinci-state

**Files:**
- Modify: `state/src/process.rs:17` (imports), `:136-137` (`blob_cap` doc), `:222-229` (`set_blob_cap`), `:283-338` (`select_batch`)
- Test: `state/tests/transition.rs:15`, `:368-378`, append one test

**Interfaces:**
- Produces: `ProcessState::set_blob_cap(cap)` accepts `1..=MAX_BLOBS`; `ProcessState::select_batch_within(&self, pending: &'a [VerifiedVote], max_voters: u64, must_include: &BTreeSet<u64>, max_votes: usize, max_blobs: usize) -> Result<(Vec<&'a VerifiedVote>, bool), Error>` (cap `max_blobs.min(self.blob_cap)`); `select_batch` keeps its signature and calls it with `self.blob_cap`.

- [ ] **Step 1: Create the branch on top of `origin/main`**

```bash
cd /home/p4u/davinci-sequencer
git status --short | wc -l                 # expect 0
# origin is an SSH URL, which this host cannot use: fetch over HTTPS into the same ref.
git fetch https://github.com/vocdoni/davinci-sequencer.git +refs/heads/main:refs/remotes/origin/main
git rev-parse --short origin/main          # expect f1cc114 (ci(deps): pin davinci-zkvm to v0.1.1)
git checkout feat/compact-da-stream && git checkout -b feat/op-core
git rebase origin/main
```

Expected: the rebase stops once, at `5774bc6 feat(state): recompute the results leaf when following a compact stream`, with `CONFLICT (content)` in `.github/workflows/main.yml` and `.github/workflows/docker-build.yml` (main pinned davinci-zkvm `v0.1.1`, the branch moves it). Keep the branch side and continue:

```bash
git checkout --theirs -- .github/workflows/main.yml .github/workflows/docker-build.yml
git add .github/workflows/main.yml .github/workflows/docker-build.yml
GIT_EDITOR=true git rebase --continue
git diff feat/compact-da-stream --stat | wc -l      # expect 0: the same tree
grep -n "ZKVM_REF:\|ref: v0" .github/workflows/main.yml .github/workflows/docker-build.yml
```

Expected: `Successfully rebased and updated refs/heads/feat/op-core`, `0`, `main.yml:44:  ZKVM_REF: v0.2.0` and `docker-build.yml:36:          ref: v0.2.0`. If `origin/main` moved past `f1cc114`, read its new commits first; conflicts outside the workflow files are not expected: stop and ask.

- [ ] **Step 2: Write the failing tests**

`state/tests/transition.rs:15`:

```rust
use davinci_zkvm_sdk::limits::{MAX_BLOBS, MAX_REFRESH, TX_BLOB_CAP, required_refresh};
```

Replace the head of `select_batch_respects_a_lower_blob_cap` (lines 368-378, from `fn select_batch_respects_a_lower_blob_cap() {` to the second `assert_eq!(st.blob_cap(), TX_BLOB_CAP);`) with:

```rust
fn select_batch_respects_a_lower_blob_cap() {
    // A chain that takes two blobs per transaction: 400 new 16-field votes
    // fit six blobs, not two. The cap goes up to the guest's MAX_BLOBS.
    let env = env(16, 400);
    let mut st = new_state(&env);
    assert_eq!(st.blob_cap(), TX_BLOB_CAP);
    for bad in [0, MAX_BLOBS + 1] {
        let err = st.set_blob_cap(bad).unwrap_err();
        assert!(matches!(err, Error::Invalid(_)), "{err}");
    }
    assert_eq!(st.blob_cap(), TX_BLOB_CAP);
    st.set_blob_cap(MAX_BLOBS).unwrap();
    assert_eq!(st.blob_cap(), MAX_BLOBS);
    st.set_blob_cap(TX_BLOB_CAP).unwrap();
```

Append to `state/tests/transition.rs`:

```rust

#[test]
fn a_512_vote_steady_batch_at_sixteen_fields_takes_nine_blobs_in_l1_mode() {
    // In L1-blob mode the per-batch cap is MAX_BLOBS: the node posts the
    // blobs in several transactions. A steady 512 x 16 batch (one refresh per
    // new vote) is nine blobs, all revotes thirteen: --batch-max 512 stays
    // under MAX_BLOBS at any field count.
    assert_eq!(blob_count(512, 512 + 512, 16), 9);
    assert_eq!(blob_count(512, 512 + 1024, 16), 13);
    let env = env(16, 1024);
    let mut st = committed_state(&env, 512);
    st.set_blob_cap(MAX_BLOBS).unwrap();
    let pending: Vec<VerifiedVote> = (512..1024)
        .map(|i| fake_vote(&env, i, &[1], 60_000 + i as u64))
        .collect();
    let (sel, blocked) = st.select_batch(&pending, u64::MAX, &none(), 512).unwrap();
    assert_eq!((sel.len(), blocked), (512, false));
    // A landing window that fits one six-blob transaction sizes it down to
    // what six blobs hold, as a one-transaction chain does.
    let (six, blocked) = st
        .select_batch_within(&pending, u64::MAX, &none(), 512, TX_BLOB_CAP)
        .unwrap();
    assert_eq!((six.len(), blocked), (379, true));
    // Never above the blob cap.
    let (all, _) = st
        .select_batch_within(&pending, u64::MAX, &none(), 512, MAX_BLOBS + 10)
        .unwrap();
    assert_eq!(all.len(), 512);
    let sel: Vec<VerifiedVote> = sel.into_iter().cloned().collect();
    let b = st
        .prepare(&sel, &mut StdRng::seed_from_u64(18), &none())
        .unwrap();
    assert_eq!(b.blobs.blobs.len(), 9);
    assert_eq!(b.expected.n_blobs, 9);
}
```

- [ ] **Step 3: Run the tests to see them fail**

Run: `rtk proxy bash -c 'cargo test -p davinci-state --test transition 2>&1 | grep -E "^error" -A 3 | head -8'`
Expected: `error[E0599]: no method named \`select_batch_within\` found for struct \`ProcessState\``.

- [ ] **Step 4: Implement**

`state/src/process.rs:17`:

```rust
use davinci_zkvm_sdk::limits::{
    MAX_BATCH_SIZE, MAX_BLOBS, MAX_REFRESH, NUM_FIELDS, SMT_LEVELS, TX_BLOB_CAP,
};
```

`state/src/process.rs:136-137` (the `blob_cap` field):

```rust
    /// Most blobs one transition may use: the chain's per-tx limit, or the
    /// guest's `MAX_BLOBS` when the blobs go out in several transactions.
    pub(crate) blob_cap: usize,
```

`state/src/process.rs:222-229`, the doc comment and `set_blob_cap` up to the end of its `if` block:

```rust
    /// Caps the blobs of every batch `select_batch` and `prepare` size
    /// (`TX_BLOB_CAP` by default): lower for chains that take fewer per
    /// transaction, up to the guest's `MAX_BLOBS` for a node that posts one
    /// transition in several transactions.
    pub fn set_blob_cap(&mut self, cap: usize) -> Result<(), Error> {
        if cap == 0 || cap > MAX_BLOBS {
            return Err(Error::Invalid(format!(
                "blob cap {cap} not in 1..={MAX_BLOBS}"
            )));
        }
```

Replace the doc comment and signature of `select_batch` (lines 283-302, from `/// FIFO batch selection` through `let nf = self.cfg.ballot_mode.num_fields;`) with:

```rust
    /// FIFO batch selection: at most one vote per slot, no reused vote id,
    /// the on-chain `max_voters` cap on distinct voters, at most `max_votes`
    /// (and `MAX_BATCH_SIZE`) votes, and the whole transition (refreshes
    /// included) within the blob cap ([`Self::set_blob_cap`]). Votes that
    /// don't fit are left for a later batch; the flag is true when the blob
    /// or refresh cap turned one away (the transaction is full).
    ///
    /// The refresh count is at least the live `must_include` slots (the
    /// exposed set `prepare` will keep), so a large exposed set shrinks the
    /// batch. If not even one vote fits next to it (more than `MAX_REFRESH`
    /// live slots, or the blob cap), this returns `RefreshOverflow`.
    pub fn select_batch<'a>(
        &self,
        pending: &'a [VerifiedVote],
        max_voters: u64,
        must_include: &BTreeSet<u64>,
        max_votes: usize,
    ) -> Result<(Vec<&'a VerifiedVote>, bool), Error> {
        self.select_batch_within(pending, max_voters, must_include, max_votes, self.blob_cap)
    }

    /// [`Self::select_batch`] within at most `max_blobs` blobs (never above
    /// the blob cap): a node whose landing time fits fewer blob
    /// transactions sizes the batch down to them. `prepare` keeps checking
    /// the blob cap, which a selection made here never passes.
    pub fn select_batch_within<'a>(
        &self,
        pending: &'a [VerifiedVote],
        max_voters: u64,
        must_include: &BTreeSet<u64>,
        max_votes: usize,
        max_blobs: usize,
    ) -> Result<(Vec<&'a VerifiedVote>, bool), Error> {
        let cap = max_blobs.min(self.blob_cap);
        let nf = self.cfg.ballot_mode.num_fields;
```

and in its loop (old line 338):

```rust
            if r > MAX_REFRESH || blob::blob_count(n, n + r, nf) > cap {
```

- [ ] **Step 5: Run the tests**

```bash
rtk proxy bash -c 'cargo test -p davinci-state --test transition 2>&1 | grep -E "^test (select_batch_respects|a_512)|test result"'
rtk proxy bash -c 'cargo test -p davinci-state 2>&1 | grep -E "^test result: FAILED|panicked"; echo "exit ${PIPESTATUS[0]}"'
```

Expected: `select_batch_respects_a_lower_blob_cap ... ok`, `select_batch_respects_blob_cap ... ok`, `a_512_vote_queue_at_sixteen_fields_seals_what_six_blobs_hold ... ok`, `a_512_vote_steady_batch_at_sixteen_fields_takes_nine_blobs_in_l1_mode ... ok`, `test result: ok. 22 passed` (about 9 s); then no failure and `exit 0`.

- [ ] **Step 6: Commit**

```bash
cargo fmt --all -- --check && cargo clippy --workspace --all-targets --locked -- -D warnings
git add state/src/process.rs state/tests/transition.rs
git commit -m "feat(state): per-batch blob cap up to max_blobs" -m "set_blob_cap accepts up to the guest's MAX_BLOBS (32): an L1-blob node posts
one transition in several Ethereum transactions. select_batch_within sizes a
batch below the cap when the landing window fits fewer transactions."
```

---

### Task 2: Post a transition in chunks and settle every chunk in one proof

**Files:**
- Modify: `sequencer/src/l1da/mod.rs` (struct `L1Da`, `Posting`, `with_post`; add `chunks`, `post_chunks`; tests)
- Modify: `sequencer/src/l1da/settle.rs` (module doc, imports, `MAX_REPOSTS` doc, `choose_anchor`, `settle`, tests)
- Modify: `sequencer/src/api/mod.rs:176-212` (`connect_l1`), `:243-290` (`start`)
- Test: `sequencer/tests/l1da.rs`, `sequencer/tests/web3_anvil.rs` (callers), `sequencer/tests/common/mod.rs:2026-2028`

**Interfaces:**
- Consumes: Task 1 nothing (node only).
- Produces: `L1Da { l1, poster, poll, chunk: usize }`, `L1Da::new(l1: Arc<dyn L1Chain>, db: Db, fee_cap: u128, poll: Duration, chunk: usize) -> Result<L1Da, StorageError>` (`chunk` clamped to at least 1); `pub fn chunks(blobs: &TransitionBlobs, per_tx: usize) -> Vec<TransitionBlobs>`; `#[derive(Debug, Default)] pub struct Posting { pub posts: Vec<Posted>, pub error: Option<PostError>, pub recorded: Option<L1Origin> }`; `with_post` and `settle` keep their signatures; `pub fn choose_anchor(oldest: u64, newest: u64, head: u64, origin: L1Origin, recorded: Option<L1Origin>) -> Anchor`. Log line `L1-blob transition sent` gains `l1_oldest` and `l1_txs`.

- [ ] **Step 1: Write the failing unit tests**

In `sequencer/src/l1da/settle.rs`'s `mod tests`, every existing `choose_anchor(n, ` call becomes `choose_anchor(n, n, ` (10 calls; `cargo fmt` rewraps one). Append to that module:

```rust

    // Chunks in blocks 100 (oldest) and 105 (newest).
    #[test]
    fn depth_counts_from_the_oldest_chunk_and_anchors_cover_the_newest() {
        let (old, new) = (100, 105);
        // OP's origin between the two blocks: wait for it to reach the newest.
        assert_eq!(choose_anchor(old, new, 200, o(103), None), Anchor::Wait);
        // At the newest: headers from origin + 2 down to the oldest.
        assert_eq!(
            choose_anchor(old, new, 200, o(105), None),
            Anchor::At {
                top: 107,
                hash: o(105).hash
            }
        );
        // A record at oldest + 1 < newest must not anchor: the verifier would
        // reject the newest transaction above it. The current origin does.
        assert_eq!(
            choose_anchor(old, new, 200, o(150), Some(o(101))),
            Anchor::At {
                top: 152,
                hash: o(150).hash
            }
        );
        // ...and past the header budget nothing anchors: post again.
        assert_eq!(
            choose_anchor(old, new, 300, o(170), Some(o(101))),
            Anchor::Repost
        );
        // A record at the newest anchors, headers 105 down to 100.
        assert_eq!(
            choose_anchor(old, new, 300, o(170), Some(o(105))),
            Anchor::At {
                top: 105,
                hash: o(105).hash
            }
        );
        // The header budget counts from the oldest chunk: 64 headers from
        // origin + 2 down to block 100 is the most, however recent the newest.
        assert_eq!(
            choose_anchor(old, 140, 300, o(161), None),
            Anchor::At {
                top: 163,
                hash: o(161).hash
            }
        );
        assert_eq!(choose_anchor(old, 140, 300, o(162), None), Anchor::Repost);
        // So does MAX_AGE.
        assert_eq!(
            choose_anchor(
                old,
                old + 50,
                old + 8000,
                o(old + L1_MAX_AGE),
                Some(o(old + 50))
            ),
            Anchor::Repost
        );
    }
```

In `sequencer/src/l1da/mod.rs`'s `mod tests`, replace the module head (`use alloy::primitives::Address;` through `use crate::web3::RegistryDa;`) with the following, keeping `the_registry_mode_must_match_the_configuration` below it:

```rust
    use alloy::primitives::Address;
    use davinci_zkvm_sdk::blob::{BLOB_SIZE, Blob};

    use super::*;
    use crate::web3::RegistryDa;

    fn fake_blobs(n: usize) -> TransitionBlobs {
        let blob = |i: usize| -> Blob {
            vec![i as u8; BLOB_SIZE]
                .into_boxed_slice()
                .try_into()
                .unwrap()
        };
        TransitionBlobs {
            blobs: (0..n).map(blob).collect(),
            commitments: (0..n).map(|i| [i as u8; 48]).collect(),
            zs: (0..n).map(|i| [i as u8 ^ 0x10; 32]).collect(),
            ys: (0..n).map(|i| [i as u8 ^ 0x20; 32]).collect(),
            proofs: (0..n).map(|i| [i as u8 ^ 0x30; 48]).collect(),
            versioned_hashes: (0..n).map(|i| [i as u8 ^ 0x40; 32]).collect(),
            digest: [7; 32],
        }
    }

    #[test]
    fn chunks_keep_the_blob_order() {
        let t = fake_blobs(13);
        let c = chunks(&t, 6);
        let sizes = |c: &[TransitionBlobs]| c.iter().map(|c| c.blobs.len()).collect::<Vec<_>>();
        assert_eq!(sizes(&c), [6, 6, 1]);
        for (k, c) in c.iter().enumerate() {
            for j in 0..c.blobs.len() {
                let i = 6 * k + j;
                assert_eq!(c.blobs[j][0], i as u8);
                assert_eq!(c.commitments[j], t.commitments[i]);
                assert_eq!((c.zs[j], c.ys[j]), (t.zs[i], t.ys[i]));
                assert_eq!(c.proofs[j], t.proofs[i]);
                assert_eq!(c.versioned_hashes[j], t.versioned_hashes[i]);
            }
            assert_eq!(c.digest, t.digest);
        }
        // Concatenated in chunk order: the transition's versioned hashes.
        let vhs: Vec<[u8; 32]> = c.iter().flat_map(|c| c.versioned_hashes.clone()).collect();
        assert_eq!(vhs, t.versioned_hashes);
        assert_eq!(sizes(&chunks(&t, 32)), [13]);
        assert_eq!(sizes(&chunks(&fake_blobs(9), 6)), [6, 3]);
        assert_eq!(sizes(&chunks(&fake_blobs(6), 6)), [6]);
        // A zero cap is one blob per transaction.
        assert_eq!(sizes(&chunks(&fake_blobs(2), 0)), [1, 1]);
    }
```

- [ ] **Step 2: Update the integration test callers and write the failing integration tests**

`sequencer/tests/common/mod.rs`, in `start_l1_node_cfg` (lines 2026-2028), the `let da = …` becomes:

```rust
    // Chunks of the per-transaction cap the config names, else six.
    let chunk = cfg
        .max_blobs_per_tx
        .unwrap_or(davinci_zkvm_sdk::limits::TX_BLOB_CAP);
    let da = davinci_sequencer::l1da::L1Da::new(
        Arc::new(l1),
        db.clone(),
        u128::MAX,
        cfg.poll_interval,
        chunk,
    )
    .unwrap();
```

`sequencer/tests/web3_anvil.rs`, in `mod l1_blob_mode`:
- the import line 2159 becomes
  ```rust
      use davinci_sequencer::l1da::{
          Anchor, L1Da, L1Job, Posting, Settled, choose_anchor, settle, with_post,
      };
  ```
- `fn da(&self) -> L1Da` (line 2276) becomes
  ```rust
          /// Posts in chunks of `chunk` blobs.
          fn da(&self, chunk: usize) -> L1Da {
              L1Da::new(
                  self.l1c.clone(),
                  self.db.clone(),
                  u128::MAX,
                  Duration::from_millis(100),
                  chunk,
              )
              .unwrap()
          }
  ```
- the three callers `t.da()` become `t.da(6)` (lines 2433, 2561, 2621);
- line 2525, the anchor of the second transition:
  ```rust
          let Anchor::At { top, hash } =
              choose_anchor(posted2.block, posted2.block, head, origin, Some(rec))
          else {
  ```
- lines 2596-2599, the `Posting` literal:
  ```rust
          let posting = Posting {
              posts: vec![posted],
              ..Posting::default()
          };
  ```

`sequencer/tests/l1da.rs`:

1. Imports (lines 7-18):

```rust
use davinci_sequencer::l1da::{
    GONE_FOR, L1Da, L1Job, MAX_PROOF_RETRIES, MAX_REPOSTS, PROOF_FAIL_FOR, PostError, Posted,
    Poster, Posting, Settled, chunks, settle, with_post,
};
use davinci_sequencer::storage::L1PostRecord;
use davinci_sequencer::web3::L1Chain;
use davinci_sequencer::web3::L1Receipt;
use davinci_sequencer::web3::L1SendError;
use davinci_sequencer::web3::SignedBlobTx;
use davinci_sequencer::web3::l1proof::{L1BlobProof, verify_local};
use davinci_zkvm_sdk::ballot::Ballot;
use davinci_zkvm_sdk::blob::{TransitionData, blob_count, build_blobs};
use davinci_zkvm_sdk::limits::{TX_BLOB_CAP, VOTE_ID_MIN};
```

2. After `fn blobs(seed: u8)`:

```rust

/// A transition of `n` blobs (2 or more): 128 (n - 1) 16-field updates.
fn wide_blobs(n: usize) -> TransitionBlobs {
    let u = 128 * (n - 1);
    assert_eq!(blob_count(0, u, 16), n);
    let t = TransitionData {
        vote_ids: vec![],
        updates: (0..u as u64)
            .map(|k| (0x10 + k, Ballot::identity()))
            .collect(),
        num_fields: 16,
    };
    build_blobs(&t, &Fr::from(n as u64), &[n as u8; 32]).unwrap()
}
```

3. The rig: `struct Rig` gains `db: Db,` after `da: L1Da,`; `rig_with` becomes three functions (replace from `fn rig_with(poll: Duration) -> Rig {` through `let blobs = blobs(9);`):

```rust
fn rig_with(poll: Duration) -> Rig {
    rig_of(poll, blobs(9), TX_BLOB_CAP)
}

/// A transition of `n` blobs, posted in chunks of `chunk`.
fn rig_chunked(poll: Duration, n: usize, chunk: usize) -> Rig {
    rig_of(poll, wide_blobs(n), chunk)
}

fn rig_of(poll: Duration, blobs: TransitionBlobs, chunk: usize) -> Rig {
    let s = setup(2, 8, None);
    let l1 = FakeL1::new();
    s.chain.attach_l1(&l1);
    let (dir, db) = fresh_db();
    let da = L1Da::new(Arc::new(l1.clone()), db.clone(), u128::MAX, poll, chunk).unwrap();
```

and the `Rig { … }` literal at its end gains `db,` after `da,`.

4. `Rig::posting` and a new `Rig::vhs` (replace the old `posting`):

```rust
    /// Every chunk posted, the proof already done (nothing recorded).
    async fn posting(&self, job: &L1Job<'_>) -> Posting {
        with_post(&self.da, &self.s.chain, job, std::future::ready(()))
            .await
            .1
    }

    fn vhs(&self) -> Vec<B256> {
        self.blobs
            .versioned_hashes
            .iter()
            .map(|h| B256::from(*h))
            .collect()
    }
```

5. Old callers: `let posted = *posting.post.as_ref().unwrap();` becomes `let posted = posting.posts[0];`; both `let n = posting.post.as_ref().unwrap().block;` become `let n = posting.posts[0].block;`; in `a_lagging_receipt_does_not_repost`:

```rust
    r.da = L1Da::new(lag.clone(), db.clone(), u128::MAX, poll, TX_BLOB_CAP).unwrap();
    (r.db, r._dir) = (db, dir);
```

6. Append the chunk tests:

```rust

// ---------------------------------------------------------------- chunks

#[tokio::test]
async fn a_chunked_transition_lands_with_every_transaction_in_order() {
    // Three blobs in chunks of two: blobs 0-1 in one transaction, blob 2 in
    // the next, posted once the first is mined.
    let r = rig_chunked(Duration::from_millis(5), 3, 2);
    let job = r.job();
    let posting = r.posting(&job).await;
    let p = posting.posts.clone();
    assert_eq!(p.len(), 2, "{posting:?}");
    assert!(posting.error.is_none());
    assert_eq!((p[0].nonce, p[1].nonce), (0, 1));
    assert!(p[1].block > p[0].block);
    let s = r.l1.sends();
    assert_eq!(s.len(), 2);
    assert!(
        s[1].head >= p[0].block,
        "the second waited for the first: {s:?}"
    );
    let out = settle(&r.da, &r.s.chain, &job, &r.snark, posting).await;
    assert!(matches!(out, Settled::Landed(_)), "{out:?}");
    let last = r.s.chain.l1_last().unwrap();
    assert_eq!(last.vhs, r.vhs(), "versioned hashes in transition order");
    assert_eq!(last.blocks, vec![p[0].block, p[0].block, p[1].block]);
    // One header chain from the anchor down to the oldest chunk's block.
    assert!(r.s.chain.l1_last_headers() as u64 > p[1].block - p[0].block);
    assert_eq!((r.l1.mined_posts(), r.s.chain.l1_records()), (2, 0));
    assert!(r.db.l1_posts().unwrap().is_empty());
}

#[tokio::test]
async fn the_origin_is_recorded_only_once_the_last_chunk_is_mined() {
    // Three blobs, one per transaction.
    let r = rig_chunked(Duration::from_millis(5), 3, 1);
    r.l1.pin_origin(Some(0));
    let job = r.job();
    let (done, proved) = tokio::sync::oneshot::channel::<()>();
    let driver = async {
        wait_until("three chunks mined", async || r.l1.mined_posts() == 3).await;
        let b: Vec<u64> =
            r.l1.sends()
                .iter()
                .map(|x| r.l1.block_of(&x.hash).unwrap())
                .collect();
        // OP's origin at an older chunk's block: a record there could not
        // anchor the newer transactions.
        for n in [b[0], b[1]] {
            r.l1.pin_origin(Some(n));
            tokio::time::sleep(Duration::from_millis(100)).await;
            assert_eq!(r.s.chain.l1_records(), 0, "recorded below the newest chunk");
        }
        r.l1.pin_origin(Some(b[2]));
        wait_until("origin recorded", async || r.s.chain.l1_records() == 1).await;
        // The origin runs 30 blocks ahead before the proof is ready.
        r.l1.mine(30);
        r.l1.pin_origin(Some(b[2] + 30));
        done.send(()).unwrap();
        (b[0], b[2])
    };
    let ((_, posting), (oldest, newest)) = tokio::join!(
        with_post(&r.da, &r.s.chain, &job, async {
            proved.await.ok();
        }),
        driver
    );
    assert_eq!(posting.recorded.map(|o| o.number), Some(newest));
    let out = settle(&r.da, &r.s.chain, &job, &r.snark, posting).await;
    assert!(matches!(out, Settled::Landed(_)), "{out:?}");
    assert_eq!(r.s.chain.l1_records(), 1, "recorded once");
    assert_eq!(
        r.s.chain.l1_last_headers() as u64,
        newest - oldest + 1,
        "anchored at the record: headers from the newest chunk down to the oldest"
    );
}

#[tokio::test(start_paused = true)]
async fn one_chunk_gone_is_posted_again_alone() {
    let r = rig_chunked(Duration::from_secs(1), 3, 2);
    let job = L1Job {
        deadline: later(),
        ..r.job()
    };
    let posting = r.posting(&job).await;
    let p = posting.posts.clone();
    // An Ethereum reorg drops the block with the second chunk only.
    assert_eq!(r.l1.head_number(), p[1].block);
    r.l1.reorg(1);
    assert_eq!(r.l1.mined_posts(), 1);
    let out = settle(&r.da, &r.s.chain, &job, &r.snark, posting).await;
    assert!(matches!(out, Settled::Landed(_)), "{out:?}");
    let s = r.l1.sends();
    assert_eq!(s.len(), 3, "the lost chunk alone: {s:?}");
    assert_eq!(s[2].nonce, 1, "at the freed nonce");
    assert_eq!(s[2].fees, s[1].fees.doubled(), "priced over the reorged tx");
    let b2 = r.l1.block_of(&s[2].hash).unwrap();
    assert_eq!(r.l1.block_of(&s[0].hash), Some(p[0].block), "chunk 0 kept");
    assert_eq!(
        r.s.chain.l1_last().unwrap().blocks,
        vec![p[0].block, p[0].block, b2]
    );
    assert_eq!(r.l1.mined_posts(), 2);
}

#[tokio::test(start_paused = true)]
async fn a_reposted_first_chunk_may_land_after_the_second() {
    // Chunk 0's transaction is gone; chunk 1 is mined. Chunk 0 goes out
    // again and lands in a newer block than chunk 1: the proof still lists
    // the transactions in chunk order.
    let r = rig_chunked(Duration::from_secs(1), 3, 2);
    let job = L1Job {
        deadline: later(),
        ..r.job()
    };
    let c = chunks(&r.blobs, 2);
    let p1 =
        r.da.poster
            .post(&job.pid, &job.root_before, &c[1], job.deadline)
            .await
            .unwrap();
    let lost = Posted {
        tx: B256::repeat_byte(0xdd),
        nonce: 7,
        block: p1.block,
    };
    let posting = Posting {
        posts: vec![lost, p1],
        ..Posting::default()
    };
    let out = settle(&r.da, &r.s.chain, &job, &r.snark, posting).await;
    assert!(matches!(out, Settled::Landed(_)), "{out:?}");
    let s = r.l1.sends();
    assert_eq!(s.len(), 2, "{s:?}");
    let b0 = r.l1.block_of(&s[1].hash).unwrap();
    assert!(b0 > p1.block);
    let last = r.s.chain.l1_last().unwrap();
    assert_eq!(last.vhs, r.vhs());
    assert_eq!(last.blocks, vec![b0, b0, p1.block]);
}

#[tokio::test]
async fn a_proof_too_deep_posts_every_chunk_again() {
    let r = rig_chunked(Duration::from_millis(5), 3, 2);
    let job = r.job();
    let posting = r.posting(&job).await;
    let b0 = posting.posts[0].block;
    r.l1.mine(80);
    r.l1.pin_origin(Some(b0 + 70));
    let driver = async {
        wait_until("re-posted", async || r.l1.mined_posts() == 4).await;
        r.l1.pin_origin(None);
    };
    let (out, ()) = tokio::join!(settle(&r.da, &r.s.chain, &job, &r.snark, posting), driver);
    assert!(matches!(out, Settled::Landed(_)), "{out:?}");
    let s = r.l1.sends();
    assert_eq!(s.len(), 4, "both chunks again: {s:?}");
    let (n0, n1) = (
        r.l1.block_of(&s[2].hash).unwrap(),
        r.l1.block_of(&s[3].hash).unwrap(),
    );
    assert!(n0 > b0 + 70 && n1 > n0);
    assert_eq!(r.s.chain.l1_last().unwrap().blocks, vec![n0, n0, n1]);
}

#[tokio::test]
async fn reposts_of_a_chunked_transition_are_bounded() {
    let r = rig_chunked(Duration::from_millis(5), 3, 2);
    r.s.chain.revert_simulates("TxTooOld", 100);
    let job = r.job();
    let posting = r.posting(&job).await;
    let out = settle(&r.da, &r.s.chain, &job, &r.snark, posting).await;
    assert!(
        matches!(&out, Settled::Transient(e) if e.contains("re-posts")),
        "{out:?}"
    );
    // Two chunks, then MAX_REPOSTS full re-posts of two.
    assert_eq!(r.l1.mined_posts(), 2 * (1 + MAX_REPOSTS as usize));
}

/// An Ethereum whose fees jump above any cap once a post is mined, until
/// `calm`.
struct Spiky {
    inner: FakeL1,
    calm: AtomicBool,
}

#[async_trait]
impl L1Chain for Spiky {
    fn sender(&self) -> Option<Address> {
        self.inner.sender()
    }
    async fn head(&self) -> Result<u64, Web3Error> {
        self.inner.head().await
    }
    async fn nonce(&self) -> Result<u64, Web3Error> {
        self.inner.nonce().await
    }
    async fn fees(&self) -> Result<L1Market, Web3Error> {
        let mut m = self.inner.fees().await?;
        if self.inner.mined_posts() > 0 && !self.calm.load(Ordering::SeqCst) {
            m.base_fee *= 1000;
        }
        Ok(m)
    }
    async fn sign_blob_tx(
        &self,
        nonce: u64,
        fees: &L1Fees,
        blobs: &TransitionBlobs,
    ) -> Result<SignedBlobTx, Web3Error> {
        self.inner.sign_blob_tx(nonce, fees, blobs).await
    }
    async fn send_blob_tx(&self, tx: &SignedBlobTx) -> Result<(), L1SendError> {
        self.inner.send_blob_tx(tx).await
    }
    async fn receipt(&self, tx: B256) -> Result<Option<L1Receipt>, Web3Error> {
        self.inner.receipt(tx).await
    }
    async fn tx_known(&self, tx: B256) -> Result<bool, Web3Error> {
        self.inner.tx_known(tx).await
    }
    async fn build_proof(&self, txs: &[B256], top: u64) -> Result<L1BlobProof, Web3Error> {
        self.inner.build_proof(txs, top).await
    }
}

#[tokio::test]
async fn a_fee_spike_between_chunks_waits_for_the_second() {
    // Chunk 0 goes out; then the base fee jumps a thousandfold, past the
    // cap: chunk 1 waits, retried each poll, and goes out once fees fall.
    // Chunk 0 is never posted again.
    let mut r = rig_chunked(Duration::from_millis(5), 3, 2);
    let spiky = Arc::new(Spiky {
        inner: r.l1.clone(),
        calm: AtomicBool::new(false),
    });
    let cap = L1Fees::offer(&r.l1.market()).cost(2) * 3;
    let (dir, db) = fresh_db();
    r.da = L1Da::new(spiky.clone(), db.clone(), cap, Duration::from_millis(5), 2).unwrap();
    (r.db, r._dir) = (db, dir);
    let job = r.job();
    let driver = async {
        wait_until("chunk 0 mined", async || r.l1.mined_posts() == 1).await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(r.l1.sends().len(), 1, "chunk 1 sent above the cap");
        spiky.calm.store(true, Ordering::SeqCst);
    };
    let ((_, posting), ()) = tokio::join!(
        with_post(&r.da, &r.s.chain, &job, std::future::ready(())),
        driver
    );
    assert_eq!(posting.posts.len(), 2, "{posting:?}");
    let out = settle(&r.da, &r.s.chain, &job, &r.snark, posting).await;
    assert!(matches!(out, Settled::Landed(_)), "{out:?}");
    let s = r.l1.sends();
    assert_eq!(s.iter().map(|x| x.nonce).collect::<Vec<_>>(), [0, 1]);
    assert_eq!(r.l1.mined_posts(), 2);
}

#[tokio::test(start_paused = true)]
async fn a_fee_spike_that_never_calms_stops_at_the_deadline() {
    // Chunk 0 mined, then fees stay above the cap: the retry loop must not
    // outlive the job's deadline (the flight would hold the process's single
    // slot until fees fall, however long after the grace end).
    let mut r = rig_chunked(Duration::from_millis(5), 3, 2);
    let spiky = Arc::new(Spiky {
        inner: r.l1.clone(),
        calm: AtomicBool::new(false), // never set
    });
    let cap = L1Fees::offer(&r.l1.market()).cost(2) * 3;
    let (dir, db) = fresh_db();
    r.da = L1Da::new(spiky.clone(), db.clone(), cap, Duration::from_millis(5), 2).unwrap();
    (r.db, r._dir) = (db, dir);
    let job = L1Job {
        deadline: tokio::time::Instant::now() + Duration::from_secs(2),
        ..r.job()
    };
    let (_, posting) = with_post(&r.da, &r.s.chain, &job, std::future::ready(())).await;
    assert_eq!(posting.posts.len(), 1, "{posting:?}");
    assert!(matches!(posting.error, Some(PostError::Deadline)), "{posting:?}");
    let out = settle(&r.da, &r.s.chain, &job, &r.snark, posting).await;
    assert!(
        matches!(&out, Settled::Transient(e) if e.contains("deadline")),
        "{out:?}"
    );
    assert_eq!(r.l1.sends().len(), 1);
}

#[tokio::test]
async fn two_transitions_interleave_their_chunks() {
    // Two processes' transitions through one Ethereum account at once: the
    // posts go out one at a time at consecutive nonces, and each transition
    // keeps its own chunks, in order.
    let r = rig_chunked(Duration::from_millis(5), 3, 2);
    let other = wide_blobs(4);
    let job_a = r.job();
    let job_b = L1Job {
        pid: [9; 31],
        root_before: [9; 32],
        blobs: &other,
        ..r.job()
    };
    let ((_, pa), (_, pb)) = tokio::join!(
        with_post(&r.da, &r.s.chain, &job_a, std::future::ready(())),
        with_post(&r.da, &r.s.chain, &job_b, std::future::ready(()))
    );
    let mut nonces: Vec<u64> = pa.posts.iter().chain(&pb.posts).map(|p| p.nonce).collect();
    nonces.sort();
    assert_eq!(nonces, [0, 1, 2, 3]);
    let origin = r.l1.origin();
    for (posting, blobs) in [(&pa, &r.blobs), (&pb, &other)] {
        assert_eq!(posting.posts.len(), 2);
        assert!(posting.posts[0].block < posting.posts[1].block);
        let txs: Vec<B256> = posting.posts.iter().map(|p| p.tx).collect();
        let proof = r.l1.build_proof(&txs, origin.number).await.unwrap();
        let (vhs, _) = verify_local(&proof, origin.hash).unwrap();
        let want: Vec<B256> = blobs
            .versioned_hashes
            .iter()
            .map(|h| B256::from(*h))
            .collect();
        assert_eq!(vhs, want);
    }
    let out = settle(&r.da, &r.s.chain, &job_a, &r.snark, pa).await;
    assert!(matches!(out, Settled::Landed(_)), "{out:?}");
}

#[tokio::test(start_paused = true)]
async fn a_single_chunk_repost_drops_the_recorded_origin() {
    // OP's origin reached the second chunk and was recorded; then a reorg
    // drops that chunk's block. The chunk goes out again alone, the stale
    // record anchors nothing, and the proof anchors at the current origin.
    let r = rig_chunked(Duration::from_secs(1), 3, 2);
    let job = L1Job {
        deadline: later(),
        ..r.job()
    };
    let posting = r.posting(&job).await;
    let p = posting.posts.clone();
    r.l1.pin_origin(Some(p[1].block));
    let rec = r.s.chain.record_l1_origin().await.unwrap();
    r.l1.pin_origin(None);
    r.l1.reorg(1);
    let posting = Posting {
        recorded: Some(rec),
        ..posting
    };
    let out = settle(&r.da, &r.s.chain, &job, &r.snark, posting).await;
    assert!(matches!(out, Settled::Landed(_)), "{out:?}");
    let s = r.l1.sends();
    assert_eq!(s.len(), 3, "one chunk again, not all: {s:?}");
    let b1 = r.l1.block_of(&s[2].hash).unwrap();
    assert!(b1 > rec.number);
    assert_eq!(r.s.chain.l1_records(), 1);
    assert_eq!(
        r.s.chain.l1_last().unwrap().blocks,
        vec![p[0].block, p[0].block, b1]
    );
}
```

- [ ] **Step 3: Run to see the build fail**

Run: `rtk proxy bash -c 'cargo test -p davinci-sequencer --test l1da 2>&1 | grep -E "^error" | sort | uniq -c | head'`
Expected: errors such as `unresolved imports \`davinci_sequencer::l1da::chunks\`, \`davinci_sequencer::l1da::with_post\``... (`chunks` does not exist), `this function takes 4 arguments but 5 arguments were supplied`, `struct \`Posting\` has no field named \`posts\``.

- [ ] **Step 4: Implement `l1da/mod.rs`**

Replace `struct L1Da` and its first `impl` (old lines 27-49) with:

```rust
/// The Ethereum side of a node in L1-blob mode.
pub struct L1Da {
    pub l1: Arc<dyn L1Chain>,
    pub poster: Poster,
    /// Pace of the settle loop and the poster (the chain poll interval).
    pub poll: Duration,
    /// Most blobs one Ethereum transaction carries (the per-transaction
    /// cap): a transition goes out in chunks of this many.
    pub chunk: usize,
}

impl L1Da {
    /// `fee_cap`: the most one post may cost at the fees it offers, in wei;
    /// `chunk`: the per-transaction blob cap (at least 1).
    pub fn new(
        l1: Arc<dyn L1Chain>,
        db: Db,
        fee_cap: u128,
        poll: Duration,
        chunk: usize,
    ) -> Result<Self, StorageError> {
        Ok(L1Da {
            poster: Poster::new(l1.clone(), db, fee_cap, poll)?,
            l1,
            poll,
            chunk: chunk.max(1),
        })
    }
}

/// `blobs` split in order into transactions of at most `per_tx` blobs (the
/// last may be shorter), each with its commitments, openings and versioned
/// hashes. Chunk `i` holds blobs `per_tx * i ..`, so the chunks' versioned
/// hashes in chunk order are the transition's: the order the registry opens
/// its commitments in. The per-blob vectors must have the length of `blobs`
/// (`build_blobs` output always does); unequal ones would panic on a slice.
pub fn chunks(blobs: &TransitionBlobs, per_tx: usize) -> Vec<TransitionBlobs> {
    let per_tx = per_tx.max(1);
    let n = blobs.blobs.len();
    debug_assert!(
        [
            blobs.commitments.len(),
            blobs.zs.len(),
            blobs.ys.len(),
            blobs.proofs.len(),
            blobs.versioned_hashes.len()
        ]
        .iter()
        .all(|&l| l == n),
        "per-blob vectors of unequal length"
    );
    (0..blobs.blobs.len())
        .step_by(per_tx)
        .map(|a| {
            let b = (a + per_tx).min(blobs.blobs.len());
            TransitionBlobs {
                blobs: blobs.blobs[a..b].to_vec(),
                commitments: blobs.commitments[a..b].to_vec(),
                zs: blobs.zs[a..b].to_vec(),
                ys: blobs.ys[a..b].to_vec(),
                proofs: blobs.proofs[a..b].to_vec(),
                versioned_hashes: blobs.versioned_hashes[a..b].to_vec(),
                digest: blobs.digest,
            }
        })
        .collect()
}
```

Replace `struct Posting` (old lines 72-78):

```rust
/// What [`with_post`] leaves for [`settle`].
#[derive(Debug, Default)]
pub struct Posting {
    /// The chunks mined while proving, in chunk order (the first ones).
    pub posts: Vec<Posted>,
    /// Why posting stopped before the last chunk, if it did.
    pub error: Option<PostError>,
    /// OP's origin recorded with `record()` while the proof was not ready.
    pub recorded: Option<L1Origin>,
}
```

Replace `with_post` and its doc comment (old lines 91-155) with:

```rust
/// Posts `chunks` in order, one transaction at a time: each waits until the
/// one before is mined (the window-1 poster). Fee-cap and L1 trouble are
/// retried each poll; any other error, or the flight's end, stops the run.
/// Returns the mined posts and the error that stopped it early.
async fn post_chunks(
    da: &L1Da,
    job: &L1Job<'_>,
    chunks: &[TransitionBlobs],
) -> (Vec<Posted>, Option<PostError>) {
    let mut posts = Vec::with_capacity(chunks.len());
    for c in chunks {
        loop {
            match da
                .poster
                .post(&job.pid, &job.root_before, c, job.deadline)
                .await
            {
                Ok(p) => {
                    posts.push(p);
                    break;
                }
                // Fee cap, L1 trouble: wait and post again while the proof runs.
                Err(e @ (PostError::FeeCap { .. } | PostError::L1(_))) => {
                    warn!(pid = %hex::encode(job.pid), error = %e, "L1 post failed; retrying");
                    // A fee spike between chunks must not hold the flight past
                    // the job's deadline; `settle` then posts the missing chunk
                    // and gets `Deadline` from its own `timeout_at`.
                    if tokio::time::Instant::now() >= job.deadline {
                        return (posts, Some(PostError::Deadline));
                    }
                    if !da.pause(&job.cancel).await {
                        return (posts, Some(e));
                    }
                }
                Err(e) => return (posts, Some(e)),
            }
        }
    }
    (posts, None)
}

/// Runs `prove` while posting the batch's blobs, in chunks of the
/// per-transaction cap. Once the last chunk is mined, if OP's origin reaches
/// its block before the proof is ready, records that origin on OP once, so
/// a late proof still anchors close to the blobs.
pub async fn with_post<T>(
    da: &L1Da,
    chain: &dyn Chain,
    job: &L1Job<'_>,
    prove: impl Future<Output = T>,
) -> (T, Posting) {
    let chunks = chunks(job.blobs, da.chunk);
    let post = post_chunks(da, job, &chunks);
    tokio::pin!(prove);
    tokio::pin!(post);
    let mut posted: Option<(Vec<Posted>, Option<PostError>)> = None;
    let mut recorded = None;
    let mut record_tried = false;
    let proved = loop {
        // The newest chunk's block, once every chunk is mined: a recorded
        // origin below it could not anchor the transactions above it.
        let newest = match &posted {
            Some((p, None)) if !record_tried => p.iter().map(|p| p.block).max(),
            _ => None,
        };
        tokio::select! {
            out = &mut prove => break out,
            p = &mut post, if posted.is_none() => posted = Some(p),
            _ = tokio::time::sleep(da.poll), if newest.is_some() => {
                if let Some(n) = newest
                    && let Ok(o) = chain.l1_origin().await
                    && o.number >= n
                {
                    record_tried = true;
                    match chain.record_l1_origin().await {
                        Ok(r) => {
                            info!(pid = %hex::encode(job.pid), origin = r.number,
                                "OP's origin reached the blobs before the proof; origin recorded");
                            recorded = Some(r);
                        }
                        Err(e) => warn!(pid = %hex::encode(job.pid), error = %e,
                            "record() failed; settling without it"),
                    }
                }
            }
        }
    };
    let (posts, error) = match posted {
        Some(p) => p,
        None => post.await,
    };
    (
        proved,
        Posting {
            posts,
            error,
            recorded,
        },
    )
}
```

- [ ] **Step 5: Implement `l1da/settle.rs`**

Module doc (lines 1-4):

```rust
//! Settling a proved batch whose blobs are on Ethereum, in one or more blob
//! transactions (chunks): wait for every chunk's block and for OP's origin,
//! anchor the L1 proof where OP accepts it, check it locally, simulate and
//! send. A chunk whose transaction is gone is posted again alone; a proof
//! that would be too deep posts every chunk again.
```

Line 13: `use super::{L1Da, L1Job, PostError, Posted, Posting, Settled, chunks};`

`MAX_REPOSTS` doc (lines 40-41):

```rust
/// Re-posts in one settlement before the batch goes back to the queue: one
/// chunk posted again counts one, every chunk posted again counts one.
pub const MAX_REPOSTS: u32 = 3;
```

Replace `choose_anchor` and its doc (lines 59-90):

```rust
/// The anchor of a proof whose blob transactions sit in blocks `oldest`
/// to `newest`. Depth counts from `oldest`: the current origin while that
/// takes at most [`CURRENT_MAX_HEADERS`] headers (up to [`TOP_ABOVE_ORIGIN`]
/// above it, within our L1 head), else a recorded origin at or above
/// `newest` (the verifier rejects a transaction above its anchor), else the
/// current origin up to [`MAX_L1_PROOF_HEADERS`]; deeper, or [`L1_MAX_AGE`]
/// behind, a re-post. Waits while OP's origin or our L1 head is below
/// `newest`.
pub fn choose_anchor(
    oldest: u64,
    newest: u64,
    head: u64,
    origin: L1Origin,
    recorded: Option<L1Origin>,
) -> Anchor {
    let behind = origin.number.saturating_sub(oldest);
    if behind >= L1_MAX_AGE {
        return Anchor::Repost;
    }
    let current = behind + 1 + TOP_ABOVE_ORIGIN;
    if current > CURRENT_MAX_HEADERS
        && let Some(r) = recorded
        && r.number >= newest
        && r.number - oldest < MAX_L1_PROOF_HEADERS
    {
        return Anchor::At {
            top: r.number,
            hash: r.hash,
        };
    }
    if current > MAX_L1_PROOF_HEADERS {
        return Anchor::Repost;
    }
    if origin.number < newest || head < origin.number {
        return Anchor::Wait;
    }
    Anchor::At {
        top: head.min(origin.number + TOP_ABOVE_ORIGIN),
        hash: origin.hash,
    }
}
```

Replace `settle` and its doc (lines 122-319) with:

```rust
/// Settles `job`'s proved batch on OP. Posts the chunks [`with_post`]
/// left unposted, then waits (within the deadline) for every chunk's
/// transaction to have [`REORG_MARGIN`] blocks on top and for OP's origin to
/// reach the newest, and sends the transition with one L1 proof of every
/// chunk transaction, in chunk order, anchored by [`choose_anchor`]. A chunk
/// whose transaction vanished is posted again alone; a too-deep proof, one
/// that fails to build or anchor for [`PROOF_FAIL_FOR`], or `TxTooOld` posts
/// every chunk again; each counts one of [`MAX_REPOSTS`].
/// `L1OriginMismatch` rebuilds the proof from fresh reads, up to
/// [`MAX_PROOF_RETRIES`] times. Other reverts end the job with their name,
/// as in blob mode.
///
/// [`with_post`]: super::with_post
pub async fn settle(
    da: &L1Da,
    chain: &dyn Chain,
    job: &L1Job<'_>,
    snark: &PlonkSnark,
    posting: Posting,
) -> Settled {
    let pid = hex::encode(job.pid);
    let chunks = chunks(job.blobs, da.chunk);
    let want: Vec<B256> = job
        .blobs
        .versioned_hashes
        .iter()
        .map(|h| B256::from(*h))
        .collect();
    if let Some(e) = &posting.error {
        warn!(%pid, error = %e, "L1 post during proving failed; posting again");
    }
    // One entry per chunk, in chunk order; `None` is (re)posted first.
    let mut posts: Vec<Option<Posted>> = (0..chunks.len())
        .map(|i| posting.posts.get(i).copied())
        .collect();
    let mut recorded = posting.recorded;
    let mut retries = 0u32;
    let mut reposts = 0u32;
    // Since when each chunk's receipt reads as unknown.
    let mut gone: Vec<Option<Instant>> = vec![None; chunks.len()];
    // Since when the proof of these posts fails to build or anchor.
    let mut proof_failing: Option<Instant> = None;
    loop {
        if job.cancel.is_cancelled() {
            return Settled::Transient("flight aborted".into());
        }
        if Instant::now() >= job.deadline {
            return Settled::Transient("L1 settlement past the batch deadline".into());
        }
        if let Some(i) = posts.iter().position(Option::is_none) {
            match da
                .poster
                .post(&job.pid, &job.root_before, &chunks[i], job.deadline)
                .await
            {
                Ok(p) => {
                    posts[i] = Some(p);
                    // A recorded origin below the new post anchors nothing.
                    recorded = None;
                    proof_failing = None;
                }
                Err(e @ (PostError::Deadline | PostError::NoSigner | PostError::Storage(_))) => {
                    return Settled::Transient(e.to_string());
                }
                // Fee cap, L1 trouble: wait and post again within the deadline.
                Err(e) => {
                    warn!(%pid, chunk = i, error = %e, "L1 post failed; retrying");
                    pause_or_abort!(da, job);
                }
            }
            continue;
        }
        let txs: Vec<Posted> = posts.iter().flatten().copied().collect();
        let mut blocks = Vec::with_capacity(txs.len());
        let mut lost = None;
        for (i, p) in txs.iter().enumerate() {
            match da.l1.receipt(p.tx).await {
                Ok(Some(rc)) => {
                    gone[i] = None;
                    blocks.push(rc.block);
                }
                Ok(None) if gone[i].is_none_or(|t| t.elapsed() < GONE_FOR) => {
                    if gone[i].is_none() {
                        gone[i] = Some(Instant::now());
                        warn!(%pid, chunk = i, tx = %p.tx, "L1 blob transaction not found; re-checking");
                    }
                }
                Ok(None) => {
                    lost = Some(i);
                    break;
                }
                Err(e) => warn!(%pid, error = %e, "L1 receipt read failed"),
            }
        }
        if let Some(i) = lost {
            gone[i] = None;
            repost!(reposts, "a blob transaction is gone");
            warn!(%pid, chunk = i, tx = %txs[i].tx,
                "L1 blob transaction gone (dropped or reorged out); posting its chunk again");
            posts[i] = None;
            continue;
        }
        // A receipt still unknown or unread: look again next poll.
        if blocks.len() < txs.len() {
            pause_or_abort!(da, job);
            continue;
        }
        let (Some(&oldest), Some(&newest)) = (blocks.iter().min(), blocks.iter().max()) else {
            return Settled::Failed("a transition without blobs".into());
        };
        let (head, origin) = match (da.l1.head().await, chain.l1_origin().await) {
            (Ok(h), Ok(o)) => (h, o),
            (Err(e), _) | (_, Err(e)) => {
                warn!(%pid, error = %e, "L1 head or OP origin read failed");
                pause_or_abort!(da, job);
                continue;
            }
        };
        if head < newest + REORG_MARGIN || origin.number < newest {
            pause_or_abort!(da, job);
            continue;
        }
        let (top, anchor) = match choose_anchor(oldest, newest, head, origin, recorded) {
            Anchor::At { top, hash } => (top, hash),
            Anchor::Wait => {
                pause_or_abort!(da, job);
                continue;
            }
            Anchor::Repost => {
                repost!(reposts, "no anchor close enough");
                warn!(%pid, oldest, origin = origin.number,
                    "the L1 proof would be too deep; posting every chunk again");
                posts.fill(None);
                continue;
            }
        };
        let hashes: Vec<B256> = txs.iter().map(|p| p.tx).collect();
        let built = match da.l1.build_proof(&hashes, top).await {
            Ok(proof) => match verify_local(&proof, anchor) {
                Ok((vhs, _)) if vhs == want => Ok(proof),
                Ok(_) => {
                    return Settled::Failed("the L1 blob transactions carry other blobs".into());
                }
                Err(e) => Err(format!("the L1 proof does not anchor: {e}")),
            },
            Err(e) => Err(format!("the L1 proof build failed: {e}")),
        };
        let proof = match built {
            Ok(proof) => {
                proof_failing = None;
                proof
            }
            // A lagging or pruning endpoint: retry each poll; past the
            // window new blob blocks usually help.
            Err(e) if proof_failing.is_some_and(|t| t.elapsed() >= PROOF_FAIL_FOR) => {
                repost!(reposts, "the L1 proof could not be built or anchored");
                warn!(%pid, error = %e, "the L1 proof kept failing; posting every chunk again");
                posts.fill(None);
                continue;
            }
            Err(e) => {
                proof_failing.get_or_insert_with(Instant::now);
                warn!(%pid, error = %e, "retrying the L1 proof");
                pause_or_abort!(da, job);
                continue;
            }
        };
        let refused = match chain
            .simulate_transition_l1(&job.pid, snark, job.blobs, &proof)
            .await
        {
            Ok(()) => match chain
                .submit_transition_l1(&job.pid, snark, job.blobs, &proof)
                .await
            {
                Ok(rc) => {
                    info!(%pid, l1_block = newest, l1_oldest = oldest, l1_txs = txs.len(),
                        headers = proof.headers.len(), tx = %rc.tx_hash, "L1-blob transition sent");
                    return Settled::Landed(rc);
                }
                Err(Web3Error::Revert(RevertReason::Revert { name, data })) => named(name, &data),
                Err(e @ Web3Error::Lost { .. }) => return Settled::Lost(e.to_string()),
                Err(e) => return Settled::Transient(e.to_string()),
            },
            Err(RevertReason::Revert { name, data }) => named(name, &data),
            Err(RevertReason::Rpc(e)) => {
                retries += 1;
                if retries > MAX_PROOF_RETRIES {
                    return Settled::Transient(e);
                }
                warn!(%pid, error = %e, "pre-flight RPC failure; retrying");
                pause_or_abort!(da, job);
                continue;
            }
        };
        match refused.as_str() {
            // The origin moved past the proof, or our L1 RPC lags it: rebuild.
            "L1OriginMismatch" => {
                retries += 1;
                if retries > MAX_PROOF_RETRIES {
                    return Settled::Transient(format!(
                        "L1OriginMismatch after {MAX_PROOF_RETRIES} rebuilds"
                    ));
                }
                info!(%pid, "OP's origin moved past the proof; rebuilding");
                pause_or_abort!(da, job);
            }
            "TxTooOld" => {
                repost!(reposts, "TxTooOld");
                warn!(%pid, oldest, "a blob transaction is too old for the verifier; posting every chunk again");
                posts.fill(None);
            }
            _ => return Settled::Reverted(refused),
        }
    }
}
```

- [ ] **Step 6: Build `L1Da` after the cap at boot (`sequencer/src/api/mod.rs`)**

`connect_l1` (lines 176-212) loses `db` and returns the RPC only:

```rust
/// L1-blob mode: the Ethereum side, checked against the network's.
async fn connect_l1(
    cfg: &Config,
    l1_block: alloy::primitives::Address,
    signing: bool,
) -> anyhow::Result<Arc<L1Rpc>> {
```

and its tail (from `let rpc = Arc::new(rpc);` to `Ok((rpc, Arc::new(da)))`) becomes `Ok(Arc::new(rpc))`. In `start`: the call becomes `Some(connect_l1(&cfg, l1_block, contracts.signer().is_some()).await?)`; the cap match arm `Some((rpc, _)) =>` becomes `Some(rpc) =>`; after `tracing::info!(cap, "blob cap per transaction");` insert

```rust
    // L1-blob mode posts a transition in chunks of the per-transaction cap.
    let l1 = match l1 {
        Some(rpc) => Some(Arc::new(L1Da::new(
            rpc,
            db.clone(),
            cfg.l1_max_post_fee,
            cfg.poll_interval,
            cap,
        )?)),
        None => None,
    };
```

and in `Deps { … }` the field `l1: l1.map(|(_, da)| da),` becomes `l1,`.

- [ ] **Step 7: Run the tests**

```bash
rtk proxy bash -c 'cargo test -p davinci-sequencer --lib l1da 2>&1 | grep -E "^test |test result"'
rtk proxy bash -c 'cargo test -p davinci-sequencer --test l1da 2>&1 | grep -E "test result|FAILED|panicked"'
rtk proxy bash -c 'cargo test -p davinci-sequencer 2>&1 | tee $TMPDIR/seq.log | grep -E "^test result: FAILED|panicked"; echo "suites ok $(grep -c "^test result: ok" $TMPDIR/seq.log), exit ${PIPESTATUS[0]}"'
```

Expected: the four unit tests `the_anchor_follows_the_origin_then_the_record`, `depth_counts_from_the_oldest_chunk_and_anchors_cover_the_newest`, `chunks_keep_the_blob_order`, `the_registry_mode_must_match_the_configuration` ok (`4 passed`); `l1da`: `test result: ok. 32 passed` in about 2 s; the whole crate with no failure, `exit 0` (`web3_anvil` prints its skip lines). Run the `l1da` suite three more times: it must pass every time (the chunk tests race real tasks against 5 ms polls).

- [ ] **Step 8: Commit**

```bash
cargo fmt --all -- --check && cargo clippy --workspace --all-targets --locked -- -D warnings
git add sequencer/src/l1da/mod.rs sequencer/src/l1da/settle.rs sequencer/src/api/mod.rs \
  sequencer/tests/l1da.rs sequencer/tests/web3_anvil.rs sequencer/tests/common/mod.rs
git commit -m "feat(l1da): post a transition in several blob transactions and settle them in one proof" -m "The blobs go out in chunks of the per-transaction cap, one transaction at a
time through the window-1 poster, and the settlement proves every chunk
transaction in chunk order. The anchor's depth counts from the oldest chunk
block; OP's origin and a recorded origin must reach the newest, and record()
waits for the last chunk. A vanished chunk is posted again alone; a proof too
deep posts every chunk again; each counts one re-post."
```

---

### Task 3: Two caps in the node and a per-chunk seal budget

**Files:**
- Modify: `sequencer/src/l1da/mod.rs` (imports, `POST_TIME`, `batch_blob_cap`, `chunks_fit`, test)
- Modify: `sequencer/src/actor.rs:919-922` (`spawn_actor`), `:1729-1736` (seal guard), `:1752-1766` (selection), `:1797-1806` (belt)
- Modify: `sequencer/src/api/mod.rs` (the per-batch cap log, `budget_decision` test)
- Test: `sequencer/tests/actor_l1.rs` (append), `sequencer/tests/common/l1.rs` (`limit_mined`)
- Docs: `docs/lifecycle.md`, `docs/configuration.md`

**Interfaces:**
- Consumes: Task 1 `select_batch_within`; Task 2 `L1Da.chunk`.
- Produces: `pub const POST_TIME: u64 = 24;`, `pub fn batch_blob_cap(l1_mode: bool, per_tx: Option<usize>) -> usize`, `pub fn chunks_fit(window: u64, lag: u64, margin: u64) -> Option<usize>` (all in `davinci_sequencer::l1da`); log line `blob cap per batch` (`cap=32` in L1-blob mode); `FakeL1::limit_mined(Option<usize>)` in the test harness.

- [ ] **Step 1: Write the failing tests**

Append to `mod tests` in `sequencer/src/l1da/mod.rs` (before `chunks_keep_the_blob_order`):

```rust
    #[test]
    fn each_chunk_after_the_first_needs_post_time() {
        // OP defaults: a 110 s leg and a 30 s margin.
        assert_eq!(chunks_fit(139, 110, 30), None);
        assert_eq!(chunks_fit(140, 110, 30), Some(1));
        assert_eq!(chunks_fit(163, 110, 30), Some(1));
        assert_eq!(chunks_fit(164, 110, 30), Some(2));
        assert_eq!(chunks_fit(188, 110, 30), Some(3));
        // The 240 s floor while voting is open: five chunks, 30 blobs.
        assert_eq!(chunks_fit(240, 110, 30), Some(5));
        assert_eq!(chunks_fit(600, u64::MAX, 30), None);
        assert_eq!(batch_blob_cap(true, Some(6)), MAX_BLOBS);
        assert_eq!(batch_blob_cap(false, Some(2)), 2);
        assert_eq!(batch_blob_cap(false, None), TX_BLOB_CAP);
    }
```

In `sequencer/src/api/mod.rs`, at the end of `budget_decision`:

```rust
        // The OP registry's 240 s floor: the L1-blob defaults fit, and so does a
        // blob-mode 60 s margin carried over; a leg and margin above it do not.
        assert!(budget_error(d(110), d(30), 240, true).is_none());
        assert!(budget_error(d(110), d(60), 240, true).is_none());
        assert!(budget_error(d(110), d(131), 240, true).unwrap().0);
```

`sequencer/tests/common/l1.rs`: in `struct L1Inner` after `hold: bool,`

```rust
    /// Blocks take pooled transactions only while fewer than this many are
    /// mined (any sender); the rest stay pending.
    mine_limit: Option<usize>,
```

in `fn mine`, replace the `let txs: Vec<FakeTx> = if i.hold { … };` block with

```rust
    let room = i.mine_limit.map_or(usize::MAX, |m| {
        m.saturating_sub(i.blocks.iter().map(|b| b.txs.len()).sum())
    });
    let txs: Vec<FakeTx> = if i.hold {
        Vec::new()
    } else {
        let keys: Vec<(Address, u64)> = i.pool.keys().take(room).copied().collect();
        keys.iter().filter_map(|k| i.pool.remove(k)).collect()
    };
```

in `FakeL1::new` after `hold: false,` add `mine_limit: None,`, and after `set_auto_mine`:

```rust

    /// Mines pooled transactions only up to `n` mined in all; `None` lifts it.
    pub fn limit_mined(&self, n: Option<usize>) {
        self.lock().mine_limit = n;
    }
```

Append to `sequencer/tests/actor_l1.rs`:

```rust

// ------------------------------------------------- several transactions

/// A 16-field node whose Ethereum takes one blob per transaction: a batch
/// of 127 or more fresh votes needs two blobs, so two transactions.
fn chunked_cfg(dir: &TempDir, s: &TestSetup, batch_max: usize, extra: &[&str]) -> Config {
    let mut args = vec!["--max-blobs-per-tx", "1"];
    args.extend_from_slice(extra);
    test_config_with(dir.path(), &s.census_dir, batch_max, "0s", &args)
}

async fn chunked_node(
    s: &TestSetup,
    l1: &FakeL1,
    prover: Arc<FakeProver>,
    dir: &TempDir,
    cfg: Config,
    shutdown: &CancellationToken,
) -> (Node, ActorHandle) {
    let node = start_l1_node_cfg(
        Db::open_in(dir.path()).unwrap(),
        s.chain.clone(),
        l1.clone(),
        prover,
        cfg,
        shutdown.clone(),
    )
    .await;
    let h = handle(&node).await;
    (node, h)
}

#[tokio::test]
async fn l1_a_two_blob_batch_goes_out_in_two_transactions() {
    let s = setup(16, 140, None);
    let l1 = FakeL1::new();
    let dir = TempDir::new().unwrap();
    let shutdown = CancellationToken::new();
    let prover = FakeProver::open();
    let cfg = chunked_cfg(&dir, &s, 140, &[]);
    let (node, h) = chunked_node(&s, &l1, prover.clone(), &dir, cfg, &shutdown).await;
    let vids = submit(&h, &s, 0..140, 2000).await;
    wait_until("settled", async || all_settled(&h, &vids).await).await;
    // One batch (the per-batch cap is MAX_BLOBS), two blobs, two posts.
    assert_eq!(prover.sizes(), vec![140]);
    let sends = l1.sends();
    assert_eq!(sends.iter().map(|x| x.nonce).collect::<Vec<_>>(), [0, 1]);
    let blocks: Vec<u64> = sends
        .iter()
        .map(|x| l1.block_of(&x.hash).unwrap())
        .collect();
    assert!(blocks[1] > blocks[0]);
    assert_eq!(s.chain.l1_last().unwrap().blocks, blocks);
    assert_eq!(s.chain.voters(), 140);
    assert!(node.db.l1_posts().unwrap().is_empty());
    shutdown.cancel();
}

/// The fake registry's 150 s floor, a 140 s Ethereum leg and no margin:
/// the leg of a second transaction (24 s more) does not fit while voting is
/// open, so the batch is sized down to one blob, not held.
#[tokio::test]
async fn l1_the_seal_budget_sizes_a_batch_down_to_fewer_transactions() {
    let s = setup(16, 140, None);
    let l1 = FakeL1::new();
    let dir = TempDir::new().unwrap();
    let shutdown = CancellationToken::new();
    let prover = FakeProver::open();
    let cfg = chunked_cfg(
        &dir,
        &s,
        140,
        &["--l1-settle-lag", "140s", "--batch-time", "100s"],
    );
    let (_node, h) = chunked_node(&s, &l1, prover.clone(), &dir, cfg, &shutdown).await;
    let vids = submit(&h, &s, 0..140, 3000).await;
    // One blob holds 126 fresh 16-field votes.
    wait_until("one batch settled", async || s.chain.voters() == 126).await;
    assert_eq!(prover.sizes(), vec![126]);
    assert_eq!(l1.mined_posts(), 1);
    s.chain.advance_time(115);
    wait_until("settled", async || all_settled(&h, &vids).await).await;
    assert_eq!(prover.sizes(), vec![126, 14]);
    assert_eq!(l1.mined_posts(), 2);
    shutdown.cancel();
}

/// A lost race exposes the batch's slots; the requeue lands in a short
/// window that fits one blob only. A window-limited `RefreshOverflow` is
/// transient: no vote errors and the exposed set stays until a window that
/// fits the refreshes opens (the grace after the end: two blobs).
#[tokio::test]
async fn l1_a_short_window_waits_for_an_exposed_set_that_needs_more_blobs() {
    let s = setup(16, 140, None);
    let l1 = FakeL1::new();
    let dir = TempDir::new().unwrap();
    let shutdown = CancellationToken::new();
    let prover = FakeProver::open();
    // The fake registry's 150 s floor and no margin: one chunk of one blob
    // while voting is open, two blobs in the 180 s default grace.
    let cfg = chunked_cfg(
        &dir,
        &s,
        140,
        &["--l1-settle-lag", "140s", "--batch-time", "100s"],
    );
    let (node, h) = chunked_node(&s, &l1, prover.clone(), &dir, cfg, &shutdown).await;
    s.chain.revert_simulates("InvalidStateRoot", 1);
    let vids = submit(&h, &s, 0..126, 3500).await;
    wait_until("exposed set recorded", async || {
        node.db.exposed(&pid_fr()).unwrap().is_some()
    })
    .await;
    let slots = node.db.exposed(&pid_fr()).unwrap().unwrap();
    // A few seal ticks in the short window: nothing errors, nothing clears.
    for _ in 0..5 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        for vid in &vids {
            assert_ne!(
                vote_status(&h, *vid).await,
                Some(VoteStatus::Error),
                "a window-limited overflow errored a vote"
            );
        }
        assert_eq!(node.db.exposed(&pid_fr()).unwrap().as_ref(), Some(&slots));
    }
    s.chain.end_process();
    s.chain.advance_time(5);
    wait_until("settled", async || all_settled(&h, &vids).await).await;
    // The settlement after the end carries two blobs in two transactions.
    assert_eq!(s.chain.l1_last().unwrap().blocks.len(), 2);
    shutdown.cancel();
}

#[tokio::test]
async fn l1_restart_with_one_chunk_pending_replaces_it() {
    let s = setup(16, 140, None);
    let l1 = FakeL1::new();
    // The first chunk is mined, the second stays in the pool.
    l1.limit_mined(Some(1));
    let dir = TempDir::new().unwrap();
    let shutdown = CancellationToken::new();
    let cfg = chunked_cfg(&dir, &s, 140, &[]);
    let (node, h) = chunked_node(&s, &l1, FakeProver::open(), &dir, cfg, &shutdown).await;
    let vids = submit(&h, &s, 0..140, 4000).await;
    wait_until("second chunk pending", async || {
        l1.sends().len() == 2 && node.db.l1_posts().unwrap().len() == 1
    })
    .await;
    let rec = node.db.l1_posts().unwrap().remove(0);
    assert_eq!((rec.nonce, l1.mined_posts()), (1, 1));
    shutdown.cancel();
    drop(h);
    drop(node);

    let db = reopen_db(dir.path()).await;
    l1.limit_mined(None);
    let shutdown2 = CancellationToken::new();
    let cfg = chunked_cfg(&dir, &s, 140, &[]);
    let node2 = start_l1_node_cfg(
        db,
        s.chain.clone(),
        l1.clone(),
        FakeProver::open(),
        cfg,
        shutdown2.clone(),
    )
    .await;
    let h2 = handle(&node2).await;
    // Proved again; its first chunk replaces the pending one at nonce 1
    // with every fee doubled, its second follows at nonce 2.
    wait_until("recovered votes settled", async || {
        all_settled(&h2, &vids).await
    })
    .await;
    let sends = l1.sends();
    assert_eq!(
        sends.iter().map(|x| x.nonce).collect::<Vec<_>>(),
        [0, 1, 1, 2]
    );
    assert!(sends[2].fees.max_fee >= 2 * rec.fees.max_fee);
    assert!(sends[2].fees.tip >= 2 * rec.fees.tip);
    assert!(sends[2].fees.blob_fee >= 2 * rec.fees.blob_fee);
    assert_eq!(
        l1.block_of(&rec.tx_hash),
        None,
        "the pending chunk was replaced"
    );
    // The first run's mined chunk is wasted; the settlement carries the new pair.
    assert_eq!(l1.mined_posts(), 3);
    let (b2, b3) = (
        l1.block_of(&sends[2].hash).unwrap(),
        l1.block_of(&sends[3].hash).unwrap(),
    );
    assert_eq!(s.chain.l1_last().unwrap().blocks, vec![b2, b3]);
    assert!(node2.db.l1_posts().unwrap().is_empty());
    shutdown2.cancel();
}
```

Hardening note (optional): the fake L1 mines a block per `head()` call and the harness polls every 20 ms, so the restarted poster replaces the pending chunk after about three polls, and the `[0, 1, 1, 2]` assertion needs the first shutdown to land inside that 60 ms window. It passed in every run so far. If it ever flakes, call `l1.set_auto_mine(false)` once the second send is seen (`wait_until("second chunk pending"` above) instead of racing the shutdown, and `set_auto_mine(true)` before the restart.

- [ ] **Step 2: Run to see them fail**

Run: `rtk proxy bash -c 'cargo test -p davinci-sequencer --lib l1da 2>&1 | grep -E "^error" | head -3'`
Expected: `cannot find function \`chunks_fit\` in this scope`. (The `actor_l1` tests compile once the harness change is in, and fail on the old actor: the per-batch cap is still the per-transaction one, so the first waits out its 15 s `wait_until` with a batch of 126.)

- [ ] **Step 3: Implement the caps and the guard**

`sequencer/src/l1da/mod.rs`: after `use davinci_zkvm_sdk::blob::TransitionBlobs;` add `use davinci_zkvm_sdk::limits::{MAX_BLOBS, TX_BLOB_CAP};`, and after the `pub use settle::{…};` block:

```rust

/// Seconds each chunk after the first adds to the Ethereum leg: its post
/// waits until the one before is mined (about two L1 blocks).
pub const POST_TIME: u64 = 24;

/// Most blobs one transition may use: in L1-blob mode the guest's
/// `MAX_BLOBS`, posted in chunks of the per-transaction cap; otherwise the
/// per-transaction cap (`TX_BLOB_CAP` when unknown).
pub fn batch_blob_cap(l1_mode: bool, per_tx: Option<usize>) -> usize {
    if l1_mode {
        MAX_BLOBS
    } else {
        per_tx.unwrap_or(TX_BLOB_CAP)
    }
}

/// Most chunks whose Ethereum leg lands within `window` seconds:
/// `lag + (chunks - 1) * POST_TIME + margin <= window`. `None` when not
/// even one does.
pub fn chunks_fit(window: u64, lag: u64, margin: u64) -> Option<usize> {
    let spare = window.checked_sub(lag.saturating_add(margin))?;
    usize::try_from(1 + spare / POST_TIME).ok()
}
```

`sequencer/src/actor.rs`, `spawn_actor` (lines 919-922):

```rust
    let mut state = open_state(&deps.db, &pid, &pcfg)?;
    state
        .set_blob_cap(crate::l1da::batch_blob_cap(
            deps.l1.is_some(),
            cfg.max_blobs_per_tx,
        ))
        .map_err(ierr)?;
```

`maybe_seal`, the L1 guard (lines 1729-1736):

```rust
        // L1-blob mode: the Ethereum leg runs during the proof, so a landing
        // takes max(proof, leg) + margin; the leg must fit as well. Each
        // chunk after the first adds a post to it, so a short window takes
        // a batch of fewer chunks.
        let max_blobs = match &self.deps.l1 {
            Some(l1) => {
                let window = deadline.saturating_sub(now);
                let Some(chunks) =
                    crate::l1da::chunks_fit(window, self.l1_settle_lag, self.settle_margin)
                else {
                    debug!(pid = %hex::encode(self.pid31), deadline, now, "no time to land an L1-blob batch");
                    return;
                };
                chunks.saturating_mul(l1.chunk).min(self.state.blob_cap())
            }
            None => self.state.blob_cap(),
        };
```

the sized selection inside `let selected: Vec<VerifiedVote> = loop {` (lines 1752-1766):

```rust
            let sized: Vec<VerifiedVote> = match self.state.select_batch_within(
                &self.pending,
                max_voters,
                &self.exposed,
                cap,
                max_blobs,
            ) {
                Ok((refs, _)) => refs.into_iter().cloned().collect(),
                // The exposed set needs more blob transactions than this
                // window fits; the full-cap selection above passed, so wait
                // for a window that does (the votes and the set stay). Only
                // an overflow at the full cap is permanent.
                Err(StateError::RefreshOverflow { .. }) if max_blobs < self.state.blob_cap() => {
                    debug!(pid = %hex::encode(self.pid31), max_blobs, "the exposed set does not fit the blob transactions the window allows");
                    return;
                }
                Err(StateError::RefreshOverflow { .. }) => {
                    self.refresh_overflow();
                    return;
                }
                Err(e) => {
                    error!(pid = %hex::encode(self.pid31), %e, "select_batch");
                    return;
                }
            };
```

and the belt (lines 1797-1806):

```rust
        // Belt: select_batch sized the batch for the cap, so this never
        // fires; a batch above it could never land.
        if prepared.blobs.blobs.len() > max_blobs {
            error!(
                pid = %hex::encode(self.pid31),
                blobs = prepared.blobs.blobs.len(),
                cap = max_blobs,
                "prepared batch exceeds the blob cap; not sealing"
            );
```

`sequencer/src/api/mod.rs`, after `tracing::info!(cap, "blob cap per transaction");`:

```rust
    tracing::info!(
        cap = crate::l1da::batch_blob_cap(l1.is_some(), Some(cap)),
        "blob cap per batch"
    );
```

- [ ] **Step 4: Run the tests, and check that the guard test bites**

```bash
rtk proxy bash -c 'cargo test -p davinci-sequencer --lib -- l1da budget_decision 2>&1 | grep -E "test result|panicked"'
rtk proxy bash -c 'cargo test -p davinci-sequencer --test actor_l1 2>&1 | grep -E "^test l1_(a_two|the_seal|restart_with)|test result"'
```

Expected: unit `6 passed` (the filter matches by substring, so the quoted form `"l1da|budget_decision"` runs nothing); `actor_l1`: the four new tests ok, `test result: ok. 17 passed` (about 3 s).

Mutation check: in `maybe_seal` replace `chunks.saturating_mul(l1.chunk).min(self.state.blob_cap())` with `{ let _ = (chunks, l1); self.state.blob_cap() }`, run `cargo test -p davinci-sequencer --test actor_l1 l1_the_seal_budget`; expected `FAILED` with `timeout waiting for one batch settled` (one batch of 140 went out). Revert the line and re-run: ok.

Second mutation check: drop the new `if max_blobs < self.state.blob_cap()` arm of the sized selection and run `cargo test -p davinci-sequencer --test actor_l1 l1_a_short_window`; expected `FAILED` on `a window-limited overflow errored a vote` (the old handler sets the exposed votes to `Error("exposed refresh set too large, recast")` and clears the set). Restore it and re-run: ok.

- [ ] **Step 5: Docs**

Run this from the repository root (it fails loudly if an anchor moved):

```bash
python3 -I - <<'EOF'
def edit(p, pairs):
    s = open(p).read()
    for old, new in pairs:
        assert s.count(old) == 1, (p, old[:70])
        s = s.replace(old, new)
    open(p, "w").write(s)

edit("docs/lifecycle.md", [
("""to the registry's floor. In L1-blob mode the Ethereum leg (`--l1-settle-lag`) runs during the
proof, so it plus `--settle-margin` must fit before that deadline as well. When not even one vote
fits, nothing seals and the votes wait.""",
"""to the registry's floor. In L1-blob mode the Ethereum leg (`--l1-settle-lag`) runs during the
proof, so it plus `--settle-margin` must fit before that deadline as well, plus 24 s for each blob
transaction after the first (they go out one after another): a short window takes a batch of
fewer transactions. When not even one vote fits, nothing seals and the votes wait."""),
("""1. takes votes in arrival order, the oldest per slot, within `maxVoters`, the size above and the
   per-transaction blob limit (counting the refreshes the circuit will require);""",
"""1. takes votes in arrival order, the oldest per slot, within `maxVoters`, the size above and the
   blob limit, counting the refreshes the circuit will require: the chain's per-transaction limit
   in blob mode, the circuit's 32 blobs in L1-blob mode (at most the transactions the time left
   allows);"""),
("""2. Posts the blobs on Ethereum while it is proved: one type-3 transaction from the node's
   Ethereum account to itself, EIP-7594 sidecar, at most six blobs. The account sends one
   transaction at a time; a later post waits until the earlier one is mined. The tip is at least""",
"""2. Posts the blobs on Ethereum while it is proved, in type-3 transactions from the node's
   Ethereum account to itself (EIP-7594 sidecar): blobs 0-5 in the first, 6-11 in the second and
   so on, at Ethereum's per-transaction limit (six). A 512-vote batch at 16 fields takes 9 blobs
   in two transactions, 13 in three when every vote is a revote. The account sends one
   transaction at a time; the next waits until the one before is mined. The tip is at least"""),
("""3. If OP's L1 origin (`L1Block.number()`) reaches the blob block before the proof is ready, the
   node calls `record()` on the verifier once, which stores that origin on OP.
4. With the proof ready, waits until the blob transaction has one Ethereum block on top and OP's
   origin reached it, then builds the L1 proof: Ethereum headers from at most two blocks above the
   origin down to the blob block while that is at most 16 headers; else from the recorded origin
   down; else up to 64 headers from the current origin. Deeper than that, or 7,200 blocks behind
   the origin, it posts the blobs again.""",
"""3. If OP's L1 origin (`L1Block.number()`) reaches the block of the last transaction before the
   proof is ready, the node calls `record()` on the verifier once, which stores that origin on OP.
4. With the proof ready, waits until every blob transaction has one Ethereum block on top and
   OP's origin reached the newest, then builds one L1 proof of all of them, in blob order:
   Ethereum headers from at most two blocks above the origin down to the oldest blob block while
   that is at most 16 headers; else from a recorded origin at or above the newest blob block
   down; else up to 64 headers from the current origin. Deeper than that, or 7,200 blocks behind
   the origin, it posts every transaction again."""),
("""6. A blob transaction that disappears (dropped, or reorged out of Ethereum) is posted again. A
   missing receipt counts as gone only after 36 s, since a lagging endpoint reads the same. A
   proof that cannot be built or anchored for 60 s also posts the blobs again. Re-posts are
   capped at 3 per batch; then the votes go back to the queue.""",
"""6. A blob transaction that disappears (dropped, or reorged out of Ethereum) is posted again
   alone, at the account's next nonce, so it may land after the transactions that follow it. A
   missing receipt counts as gone only after 36 s, since a lagging endpoint reads the same. A
   proof that cannot be built or anchored for 60 s also posts every transaction again. Re-posts
   (one transaction or all) are capped at 3 per batch; then the votes go back to the queue."""),
("""In L1-blob mode the node records each Ethereum blob transaction, with the sending account, before
sending it and drops the record once it is mined. After a restart the batch is proved again (its
secrets died with the process) and its first post replaces a still-pending transaction at its
nonce with doubled fees instead of queueing behind it. Records of another account (a rotated L1
key) are dropped at start. A transaction mined before the crash is wasted.""",
"""In L1-blob mode the node records each Ethereum blob transaction, with the sending account, before
sending it and drops the record once it is mined. After a restart the batch is proved again (its
secrets died with the process) and its first post replaces a still-pending transaction at its
nonce with doubled fees instead of queueing behind it, whichever of the batch's transactions that
was. Records of another account (a rotated L1 key) are dropped at start. Transactions mined
before the crash are wasted."""),
])
edit("docs/configuration.md", [
("""| `--l1-settle-lag` | `DAVINCI_L1_SETTLE_LAG` | `110s` | L1-blob mode: the Ethereum leg of a landing (blob inclusion, OP's L1 origin lag, the reorg margin). It runs during the proof, so a batch seals only if this plus `--settle-margin` fits before the window closes too. Lower it only for a faster L1, such as a local anvil. |""",
"""| `--l1-settle-lag` | `DAVINCI_L1_SETTLE_LAG` | `110s` | L1-blob mode: the Ethereum leg of a landing (blob inclusion, OP's L1 origin lag, the reorg margin). It runs during the proof, so a batch seals only if this plus `--settle-margin`, plus 24 s for each blob transaction after the first, fits before the window closes too. Lower it only for a faster L1, such as a local anvil. |"""),
("""| `--max-blobs-per-tx` | `DAVINCI_MAX_BLOBS_PER_TX` | from the chain | Most blobs one settlement carries (1 to 6). See [Chain limits](#chain-limits). |""",
"""| `--max-blobs-per-tx` | `DAVINCI_MAX_BLOBS_PER_TX` | from the chain | Most blobs one blob transaction carries (1 to 6): the most per batch in blob mode; in L1-blob mode a batch takes up to 32, posted in transactions of this many. See [Chain limits](#chain-limits). |"""),
("""At boot the node checks the Ethereum chain id and the verifier's `L1Block` address against the
network (the `optimism` preset expects chain 1 and `0x4200000000000000000000000000000000000015`),
and sizes batches with Ethereum's blob limit. Observers need no Ethereum key.""",
"""At boot the node checks the Ethereum chain id and the verifier's `L1Block` address against the
network (the `optimism` preset expects chain 1 and `0x4200000000000000000000000000000000000015`),
and reads Ethereum's per-transaction blob limit. A batch may take up to the circuit's 32 blobs:
the node posts them in transactions of that limit, one after another, and proves all of them in
one settlement. It logs both caps (`blob cap per transaction`, `blob cap per batch`). Observers
need no Ethereum key."""),
("""covers the OP leg. A batch seals only while both the estimated proof and the Ethereum leg, each
plus the margin, end before the window can close. While the election is open that is""",
"""covers the OP leg. A batch seals only while both the estimated proof and the Ethereum leg, each
plus the margin, end before the window can close; each blob transaction after the first adds 24 s
to the leg, so a short window takes a batch of fewer transactions. While the election is open that is"""),
("""- `--l1-max-post-fee` bounds what one post may cost at the fees it offers (twice the base fee and
  twice the blob base fee); a replacement doubles them. A one-blob post cost about 2 µETH at
  0.08 gwei and 0.004 gwei; the 0.001 ETH default leaves room for spikes and replacements.""",
"""- `--l1-max-post-fee` bounds what one post (one blob transaction) may cost at the fees it offers
  (twice the base fee and twice the blob base fee); a replacement doubles them. A one-blob post
  cost about 2 µETH at 0.08 gwei and 0.004 gwei; the 0.001 ETH default leaves room for spikes and
  replacements. A batch of three transactions, re-posted at most three times, sends at most 12."""),
])
print("docs: done")
EOF
```

Expected: `docs: done`.

- [ ] **Step 6: Run the whole workspace and commit**

```bash
cargo fmt --all -- --check && cargo clippy --workspace --all-targets --locked -- -D warnings
rtk proxy bash -c 'cargo test --workspace 2>&1 | tee $TMPDIR/ws.log | grep -E "^test result: FAILED|panicked"; echo "suites ok $(grep -c "^test result: ok" $TMPDIR/ws.log), exit ${PIPESTATUS[0]}"'
git add sequencer/src/l1da/mod.rs sequencer/src/actor.rs sequencer/src/api/mod.rs \
  sequencer/tests/actor_l1.rs sequencer/tests/common/l1.rs docs/lifecycle.md docs/configuration.md
git commit -m "feat(actor): a per-batch blob cap of max_blobs and a per-chunk seal budget in l1-blob mode" -m "In L1-blob mode a batch may take the guest's 32 blobs; the per-transaction cap
from eth_config is the chunk size, and the node logs both. The seal budget
reserves 24 s per chunk after the first and sizes a batch down to the chunks
the window fits instead of holding it. A refresh set that needs more blobs than
a short window allows waits for a larger window; it no longer errors the
exposed votes and clears the set. Blob mode keeps one transaction per batch."
```

Expected: no failure line, about 40 suites ok, `exit 0`.

---

### Task 4: A settlement over two Ethereum transactions on two anvils, and a nine-blob dry run

**Files:**
- Test: `sequencer/tests/web3_anvil.rs` (`mod l1_blob_mode`: a helper and one test)
- Test: `state/tests/dryrun.rs` (imports, one test)

**Interfaces:**
- Consumes: Task 2 `L1Da::new(.., chunk)`, `with_post`, `settle`; Task 1 `set_blob_cap(MAX_BLOBS)`.

Both tests are gated (`ANVIL=1`, `ZISKEMU=1`) and pass on Tasks 1-3; they would not build before Task 2 (the anvil test) and fail before Task 1 (`set_blob_cap(32)` refused).

- [ ] **Step 1: The two-transaction anvil test**

In `sequencer/tests/web3_anvil.rs`, `mod l1_blob_mode`, after `fn transition(...)` (it ends with `(blobs, snark)` and `}` before `/// Ethereum produces a block every 400 ms`), add:

```rust
    /// A 500-vote transition from `before`: two blobs at NF 4.
    fn two_blob_transition(
        pid: &[u8; 31],
        before: [u8; 32],
        after: [u8; 32],
    ) -> (TransitionBlobs, PlonkSnark) {
        let n = 500u64;
        let t = TransitionData {
            vote_ids: (0..n).map(|i| VOTE_ID_MIN + 5000 + i).collect(),
            updates: (0..n)
                .map(|i| (0x400 + i, Ballot([Ciphertext::IDENTITY; 16])))
                .collect(),
            num_fields: NF,
        };
        let blobs = build_blobs(&t, &pid_fr(pid), &before).unwrap();
        assert_eq!(blobs.blobs.len(), 2);
        let snark = batch_snark(
            &Batch {
                before,
                after,
                voters: n as u32,
                overwrites: 0,
                occupied_before: 0,
            },
            &CENSUS_ROOT,
            &blobs,
        );
        (blobs, snark)
    }
```

and before `async fn l1_settle_loop_posts_again_after_an_l1_reorg()` (with its `#[tokio::test(flavor = "multi_thread")]`):

```rust
    // One blob per Ethereum transaction: a two-blob transition goes out in
    // two transactions, in two blocks, and settles with one L1 proof of both.
    #[tokio::test(flavor = "multi_thread")]
    async fn l1_settlement_spans_two_transactions() {
        if !enabled() {
            eprintln!("skipped: set ANVIL=1");
            return;
        }
        let t = two_chains().await;
        // Initialises the mode cell: events() pairs L1 events strictly from here on.
        t.c.da_mode().await.unwrap();
        let da = t.da(1);
        let (pid, p, _pk) = t.process().await;
        let (blobs, snark) = two_blob_transition(&pid, p.state_root, [0x93; 32]);
        let want: Vec<B256> = blobs
            .versioned_hashes
            .iter()
            .map(|h| B256::from(*h))
            .collect();
        let stop = CancellationToken::new();
        let relay = tokio::spawn(relay_origin(
            t.l1_read.clone(),
            t.relay.clone(),
            t.l1_block,
            stop.clone(),
        ));
        let job = L1Job {
            pid,
            root_before: p.state_root,
            blobs: &blobs,
            deadline: tokio::time::Instant::now() + Duration::from_secs(120),
            cancel: CancellationToken::new(),
        };
        let (_, posting) = with_post(&da, &t.c, &job, std::future::ready(())).await;
        let posts = posting.posts.clone();
        assert_eq!(posts.len(), 2, "{posting:?}");
        assert_eq!(posts[1].nonce, posts[0].nonce + 1);
        assert!(posts[1].block > posts[0].block);
        let out = settle(&da, &t.c, &job, &snark, posting).await;
        stop.cancel();
        relay.await.unwrap();
        let Settled::Landed(rc) = out else {
            panic!("{out:?}");
        };
        let evs = t.c.events(rc.block, rc.block).await.unwrap();
        let EventKind::StateTransitioned {
            l1: Some(b),
            n_blobs,
            new_root,
            ..
        } = &evs[0].kind
        else {
            panic!("{evs:?}");
        };
        assert_eq!((*n_blobs, *new_root), (2, [0x93; 32]));
        assert_eq!(b.blocks, vec![posts[0].block, posts[1].block]);
        assert_eq!(b.vhs, want);
        // A follower reads them back from the two Ethereum blocks.
        let got = AnvilBlobs::new(std::slice::from_ref(&t.l1_url))
            .blobs_for_l1(&b.blocks, &b.vhs)
            .await
            .unwrap();
        assert_eq!(got, blobs.blobs);
        assert_eq!(t.c.process(&pid).await.unwrap().state_root, [0x93; 32]);
        assert!(t.db.l1_posts().unwrap().is_empty());
    }
```

- [ ] **Step 2: Run it against the real `L1BlobVerifier` on two anvils**

```bash
cd /home/p4u/davinci-contracts && git branch --show-current && forge build 2>&1 | tail -1
ls out/L1BlobVerifier.sol/L1BlobVerifier.json out/MockL1Block.sol/MockL1Block.json
cd /home/p4u/davinci-sequencer
rtk proxy bash -c 'ANVIL=1 cargo test -p davinci-sequencer --test web3_anvil l1_ 2>&1 | grep -E "^test |test result|skipped"'
```

Expected: the contracts on `feat/compact-da-stream` (or `feat/op-core` once Task 7 created it), a clean build, both artifacts listed; then
```
test l1_blob_mode::l1_post_replaces_a_dropped_transaction ... ok
test l1_blob_mode::l1_settlement_spans_two_transactions ... ok
test l1_blob_mode::l1_settlement_on_two_anvils ... ok
test l1_blob_mode::l1_settle_loop_posts_again_after_an_l1_reorg ... ok
test result: ok. 4 passed; 0 failed; 0 ignored; 0 measured; 10 filtered out
```
in about 45 s, no skip line.

- [ ] **Step 3: Commit**

```bash
cargo fmt --all -- --check && cargo clippy --workspace --all-targets --locked -- -D warnings
git add sequencer/tests/web3_anvil.rs
git commit -m "test(web3): an l1-blob settlement over two ethereum transactions on two anvils"
```

- [ ] **Step 4: The nine-blob dry run**

`state/tests/dryrun.rs:22`: `use davinci_zkvm_sdk::limits::{MAX_BLOBS, required_refresh};`. Before `fn guest_rejects_tampered_requests()` (with its `#[test]`):

```rust
#[test]
fn a_512_vote_steady_batch_at_sixteen_fields_in_nine_blobs_accepted() {
    if !gated() {
        return;
    }
    // Above one transaction's six blobs: an L1-blob node posts these nine
    // in two transactions. 512 committed voters (fake proofs) make it
    // steady: 512 new votes (cached proofs) and 512 refreshes,
    // T = 1 + 384 + 32 * 1024 = 33,153 cells.
    let env = env(16, 1024);
    let mut st = new_state(&env);
    let mut rng = StdRng::seed_from_u64(30);
    let fields: Vec<u64> = (0..16).map(|i| i % 6).collect();
    let first: Vec<_> = (512..1024)
        .map(|i| fake_vote(&env, i, &fields, 900 + i as u64))
        .collect();
    let b = st.prepare(&first, &mut rng, &none()).unwrap();
    st.commit(&b).unwrap();

    st.set_blob_cap(MAX_BLOBS).unwrap();
    let pending: Vec<_> = (0..512)
        .map(|i| real_vote(&env, i, &fields, 60 + i as u64))
        .collect();
    let (sel, blocked) = st.select_batch(&pending, u64::MAX, &none(), 512).unwrap();
    assert_eq!((sel.len(), blocked), (512, false));
    let sel: Vec<VerifiedVote> = sel.into_iter().cloned().collect();
    let b = st.prepare(&sel, &mut rng, &none()).unwrap();
    assert_eq!(b.refresh_slots().len(), 512);
    assert_eq!(b.blobs.blobs.len(), 9);
    assert_eq!(b.expected.n_blobs, 9);
    assert_eq!(b.request.kzg.as_ref().unwrap().commitments.len(), 9);
    assert_accepted(&b);
    assert_blob_count_bound(&b);
}
```

- [ ] **Step 5: Run every dry run on the emulator (CPU only)**

```bash
ls ~/.zisk/bin/ziskemu ../davinci-zkvm/circuit/elf/circuit.elf ../davinci-circom/artifacts/ballot_proof.wasm
rtk proxy bash -c 'ZISKEMU=1 cargo test -p davinci-state --test dryrun -- --test-threads=1 2>&1 | grep -E "^test |test result|mask|panicked"'
```

Expected: nine tests `ok`, among them `a_512_vote_steady_batch_at_sixteen_fields_in_nine_blobs_accepted ... ok` (about 4.5 minutes on a cold proof cache: 385 ballot proofs are made from `../davinci-circom/artifacts` and cached in `state/tests/cache`; then three emulator runs of about 397 M steps, the accepted batch and the 8- and 10-commitment rejections), `test result: ok. 9 passed`. No skip line.

- [ ] **Step 6: Commit**

```bash
git add state/tests/dryrun.rs
git commit -m "test(state): dry run of a nine-blob 512 x 16 steady batch"
```

---

### Task 5: Gnosis retired in the node, the client, the e2e harness and the docs

**Files:**
- Modify: `client/src/networks.rs` (`GNOSIS` gone, `NETWORKS`, `DEFAULT`, tests), `sequencer/src/config.rs` (tests)
- Modify: `e2e/src/net.rs`, `e2e/tests/e2e.rs`, `e2e/tests/demo.rs`; comments only: `client/src/organizer.rs`, `e2e/src/demo.rs`, `e2e/src/dkg.rs`, `docs/agm.md` (`docs/benchmarks.md` keeps its dated Gnosis figures)
- Modify: `README.md`, `docs/configuration.md`, `docs/deployments.md`, `docs/lifecycle.md`, `docs/testing.md`, `docs/demo.md`, `docker-compose.yml`, `.env.example`

**Interfaces:**
- Produces: `davinci_client::networks::{NETWORKS == [OPTIMISM], DEFAULT == &OPTIMISM}`, no `GNOSIS`; the node's `--network` defaults to `optimism` (it reads `networks::DEFAULT.name`); `davinci_e2e::net::preset()` defaults to `optimism`; compose secret `sequencer_l1_key` (`SEQUENCER_L1_KEY_FILE`, default `./sequencer-l1.key`). Chain data stays: `web3/contracts.rs` known-chain tables, `known_chain_blob_cap(100)`, the RPC notes in `tx.rs`.

- [ ] **Step 1: Save the edit script and see the old default**

The script changes the tests in `client/src/networks.rs`, `sequencer/src/config.rs` and `e2e/src/net.rs` together with the code they pin (the new `lookup` and `optimism_is_the_default_network` fail on today's tree, which defaults to `gnosis`). Save it as `~/.cache/op-core/gnosis_code.py`:

```python
"""Gnosis retirement in the client, the node's config tests and the e2e harness.
Run from the davinci-sequencer root: python3 -I gnosis_code.py"""


def edit(p, pairs):
    s = open(p).read()
    for old, new in pairs:
        assert s.count(old) == 1, (p, old[:70])
        s = s.replace(old, new)
    open(p, "w").write(s)


edit("client/src/networks.rs", [
("""/// Gnosis Chain, registry R2 (the production beta): DKG and Council key
/// modes.
pub const GNOSIS: Network = Network {
    name: "gnosis",
    chain_id: 100,
    registry: address!("20b96e465CA7C3536B9C733571ec1eCf42b2eA21"),
    start_block: 48_633_301,
    rpc_urls: &[
        "https://gnosis-rpc.publicnode.com",
        "https://gnosis-rpc.blockreq.com/v1/rpc/public",
        "https://rpc.gnosischain.com",
        "https://gnosis.drpc.org",
    ],
    blob_source: "beacon:https://rpc-gbc.gnosischain.com",
    confirmations: 3,
    da: NetworkDa::Blobs,
};

""", ""),
("""/// Every known deployment.
pub const NETWORKS: &[Network] = &[GNOSIS, OPTIMISM];

/// The deployment a node and the tooling use unless told otherwise.
pub const DEFAULT: &Network = &GNOSIS;""",
"""/// Every known deployment. The Gnosis registries are retired (their
/// program vks are v0.1's): see docs/deployments.md.
pub const NETWORKS: &[Network] = &[OPTIMISM];

/// The deployment a node and the tooling use unless told otherwise.
pub const DEFAULT: &Network = &OPTIMISM;"""),
("""    #[test]
    fn lookup() {
        assert_eq!(by_name(" Gnosis "), Some(&GNOSIS));
        assert_eq!(by_name("custom"), None);
        assert_eq!(by_name(DEFAULT.name), Some(DEFAULT));
        assert_eq!(
            GNOSIS.beacon_urls(),
            ["https://rpc-gbc.gnosischain.com"].to_vec()
        );
    }

    #[test]
    fn gnosis_preset_is_deployed() {
        assert_ne!(GNOSIS.registry, Address::ZERO, "gnosis registry unset");
        assert_ne!(GNOSIS.start_block, 0, "gnosis start block unset");
    }
""",
"""    #[test]
    fn lookup() {
        assert_eq!(by_name(" Optimism "), Some(&OPTIMISM));
        assert_eq!(by_name("custom"), None);
        // Gnosis is retired.
        assert_eq!(by_name("gnosis"), None);
        assert_eq!(DEFAULT, &OPTIMISM);
        assert_eq!(by_name(DEFAULT.name), Some(DEFAULT));
        assert_eq!(
            OPTIMISM.beacon_urls(),
            ["https://ethereum-beacon-api.publicnode.com"].to_vec()
        );
    }
"""),
("""        // Followers read the blobs from Ethereum beacon APIs.
        assert!(!OPTIMISM.beacon_urls().is_empty());
        assert_eq!(GNOSIS.da, NetworkDa::Blobs);
    }""",
"""        // Followers read the blobs from Ethereum beacon APIs.
        assert!(!OPTIMISM.beacon_urls().is_empty());
    }"""),
])

edit("sequencer/src/config.rs", [
("""    #[test]
    fn gnosis_is_the_default_network() {
        let g = &networks::GNOSIS;
        let c = Config::parse_args(["davinci-sequencer"]).unwrap();
        assert_eq!(c.network, NetworkChoice::Known(g));
        assert_eq!(c.registry, g.registry);
        assert!(!c.registry_explicit());
        assert_eq!(c.start_block, Some(g.start_block));
        assert_eq!(c.confirmations, 3);
        let rpcs: Vec<&str> = c.rpc_url.iter().map(|u| u.as_str()).collect();
        assert_eq!(
            rpcs,
            [
                "https://gnosis-rpc.publicnode.com/",
                "https://gnosis-rpc.blockreq.com/v1/rpc/public",
                "https://rpc.gnosischain.com/",
                "https://gnosis.drpc.org/",
            ]
        );
        assert_eq!(c.blob_source, g.blob_source.parse().unwrap());
        assert_eq!(
            Config::parse_args(["davinci-sequencer", "--network", "GNOSIS"])
                .unwrap()
                .network,
            NetworkChoice::Known(g)
        );
    }""",
"""    #[test]
    fn optimism_is_the_default_network() {
        let o = &networks::OPTIMISM;
        let c = Config::parse_args(["davinci-sequencer"]).unwrap();
        assert_eq!(c.network, NetworkChoice::Known(o));
        assert_eq!(c.registry, o.registry);
        assert!(!c.registry_explicit());
        assert_eq!(c.start_block, Some(o.start_block));
        assert_eq!(c.confirmations, 1);
        let rpcs: Vec<&str> = c.rpc_url.iter().map(|u| u.as_str()).collect();
        assert_eq!(
            rpcs,
            [
                "https://optimism-rpc.publicnode.com/",
                "https://mainnet.optimism.io/",
            ]
        );
        assert_eq!(c.blob_source, o.blob_source.parse().unwrap());
        // L1-blob mode, with the L1-blob margin.
        assert!(c.l1_mode());
        assert_eq!(c.settle_margin, Duration::from_secs(30));
        assert_eq!(
            Config::parse_args(["davinci-sequencer", "--network", "OPTIMISM"])
                .unwrap()
                .network,
            NetworkChoice::Known(o)
        );
        // Gnosis is retired.
        assert!(Config::parse_args(["davinci-sequencer", "--network", "gnosis"]).is_err());
    }"""),
("""        assert_eq!(c.start_block, None);
        assert_eq!(c.rpc_url.len(), networks::GNOSIS.rpc_urls.len());
        assert_eq!(c.confirmations, 3);""",
"""        assert_eq!(c.start_block, None);
        assert_eq!(c.rpc_url.len(), networks::OPTIMISM.rpc_urls.len());
        assert_eq!(c.confirmations, 1);"""),
("""        assert_eq!(c.registry, networks::GNOSIS.registry);
        assert_eq!(c.start_block, Some(7));""",
"""        assert_eq!(c.registry, networks::OPTIMISM.registry);
        assert_eq!(c.start_block, Some(7));"""),
("""        let c = Config::parse_args(["davinci-sequencer"]).unwrap();
        assert!(c.check_chain_id(100).is_ok());
        let e = c.check_chain_id(31337).unwrap_err().to_string();
        assert!(e.contains("31337") && e.contains("gnosis"), "{e}");""",
"""        let c = Config::parse_args(["davinci-sequencer"]).unwrap();
        assert!(c.check_chain_id(10).is_ok());
        let e = c.check_chain_id(31337).unwrap_err().to_string();
        assert!(e.contains("31337") && e.contains("optimism"), "{e}");"""),
("""        assert_eq!(c.start_block, None);
        // Gnosis stays in blob mode.
        assert!(!Config::parse_args(["davinci-sequencer"]).unwrap().l1_mode());
        // The zero address is refused as an explicit registry too.""",
"""        assert_eq!(c.start_block, None);
        // The zero address is refused as an explicit registry too."""),
])

edit("e2e/src/net.rs", [
("""//! The chain a run uses: a fresh anvil with the contracts, or with
//! `DAVINCI_E2E_LIVE=1` an existing deployment (Gnosis by default). Key""",
"""//! The chain a run uses: a fresh anvil with the contracts, or with
//! `DAVINCI_E2E_LIVE=1` an existing deployment (OP Mainnet by default). Key"""),
("""//! by `DAVINCI_E2E_NETWORK` (default `gnosis`). `DAVINCI_E2E_MIN_BALANCE`""",
"""//! by `DAVINCI_E2E_NETWORK` (default `optimism`). `DAVINCI_E2E_MIN_BALANCE`"""),
("""use davinci_client::networks::{self, GNOSIS, Network, NetworkDa, OP_L1_BLOCK};""",
"""use davinci_client::networks::{self, Network, NetworkDa, OP_L1_BLOCK, OPTIMISM};"""),
("""/// Least balance each funded account needs for a live run (0.05 xDAI).""",
"""/// Least balance each funded account needs, unless `DAVINCI_E2E_MIN_BALANCE`
/// or an L1-blob preset says otherwise (0.05 native)."""),
("""/// Organizer and harness RPC on Gnosis: the network's last (publicnode, its
/// first, rate-limits organizer bursts).
#[cfg(test)]
const GNOSIS_RPC: &str = GNOSIS.rpc_urls[GNOSIS.rpc_urls.len() - 1];

""", ""),
("""#[cfg(test)]
fn gnosis_node_rpcs() -> String {
    node_rpcs_of(&GNOSIS)
}

/// The live deployment's preset: `DAVINCI_E2E_NETWORK` (default `gnosis`).
pub fn preset() -> Result<&'static Network> {
    let name = env_or("DAVINCI_E2E_NETWORK", GNOSIS.name);""",
"""/// The live deployment's preset: `DAVINCI_E2E_NETWORK` (default `optimism`).
pub fn preset() -> Result<&'static Network> {
    let name = env_or("DAVINCI_E2E_NETWORK", OPTIMISM.name);"""),
("""        let live = node_lists(&gnosis_node_rpcs());
        assert_eq!(live.len(), 4);
        assert!(
            live.iter()
                .all(|l| l.split(',').count() == 2 && !l.contains(' '))
        );
        // The three signing sequencers start on different endpoints.
        let first: std::collections::HashSet<_> =
            live[..3].iter().map(|l| l.split(',').next()).collect();
        assert_eq!(first.len(), 3);
        assert!(GNOSIS.rpc_urls.contains(&GNOSIS_RPC));""",
"""        let live = node_lists(&node_rpcs_of(&OPTIMISM));
        assert_eq!(live.len(), 4);
        assert!(
            live.iter()
                .all(|l| l.split(',').count() == 2 && !l.contains(' '))
        );
        // The signing sequencers start on different endpoints, as far as
        // the preset lists them.
        let first: std::collections::HashSet<_> =
            live[..3].iter().map(|l| l.split(',').next()).collect();
        assert_eq!(first.len(), OPTIMISM.rpc_urls.len().min(3));"""),
])

edit("e2e/tests/e2e.rs", [
("""//! `DAVINCI_E2E_LIVE=1` runs the same scenario on an existing deployment
//! (Gnosis by default, see `davinci_e2e::net`): the registry pins are checked""",
"""//! `DAVINCI_E2E_LIVE=1` runs the same scenario on an existing blob-mode
//! deployment (`DAVINCI_E2E_NETWORK`, see `davinci_e2e::net`; the L1-blob
//! default has its compact `op_live` run instead): the registry pins are checked"""),
("""    // The nodes size transitions from eth_config: 2 blobs on Gnosis.
    for n in &nodes {
        match n.logged_blob_cap() {
            Some(c) if net.live && net.l1.is_none() => {
                ensure!(c == 2, "{}: blob cap {c}, want 2", n.name)
            }
            Some(c) => say!("{}: blob cap {c}", n.name),
            None => say!("{}: no blob cap in its log", n.name),
        }
    }""",
"""    // The nodes size transitions from eth_config.
    for n in &nodes {
        match n.logged_blob_cap() {
            // The OP preset's Ethereum schedule gives 6 blobs per transaction.
            Some(c) if net.live && net.l1.is_some() => {
                ensure!(c == 6, "{}: blob cap {c}, want 6", n.name)
            }
            Some(c) => say!("{}: blob cap {c}", n.name),
            None => say!("{}: no blob cap in its log", n.name),
        }
    }"""),
])

edit("e2e/tests/demo.rs", [
("""//! Demo elections on a live deployment (Gnosis by default), gated by""",
"""//! Demo elections on a live deployment (OP Mainnet by default), gated by"""),
("""/// Process `pid` as the registry has it, riding out RPC rate limits: every
/// public Gnosis RPC answers 429 to a busy host for a minute or two.""",
"""/// Process `pid` as the registry has it, riding out RPC rate limits: public
/// RPCs answer 429 to a busy host for a minute or two."""),
("""                let uri = if self.chain_id == 100 {
                    format!("https://gnosisscan.io/address/{a}")
                } else {""",
"""                let uri = if self.chain_id == 10 {
                    format!("https://optimistic.etherscan.io/address/{a}")
                } else {"""),
])
edit("client/src/organizer.rs", [
("""/// Retries of a rate-limited request, a second apart: the public Gnosis RPCs""",
"""/// Retries of a rate-limited request, a second apart: public RPCs"""),
])

edit("e2e/src/demo.rs", [
("""            // Past the move, the last votes go in well inside Gnosis'
            // 60 s noticeMin, and one comes after the new end.""",
"""            // Past the move, the last votes go in well inside the registry's
            // 60 s noticeMin, and one comes after the new end."""),
])

edit("e2e/src/dkg.rs", [
("""/// nodes open one on their own, Live about two minutes later on Gnosis.""",
"""/// nodes open one on their own, Live about two minutes later on a public chain."""),
])

edit("docs/agm.md", [
("""The figures below are for one RTX 5090 prover and a Gnosis-style chain with 2 blobs per""",
"""The figures below are for one RTX 5090 prover and a chain with 2 blobs per"""),
])

print("gnosis code: done")
```

Before running it, confirm the old default the tests replace:

```bash
grep -n "pub const DEFAULT" client/src/networks.rs     # expect: = &GNOSIS;
rtk proxy bash -c 'cargo test -p davinci-sequencer --lib config::tests::gnosis_is_the_default_network 2>&1 | grep -E "test result"'
```

Expected: `pub const DEFAULT: &Network = &GNOSIS;` and `1 passed`.

- [ ] **Step 2: Apply the code changes and run the tests**

```bash
python3 -I ~/.cache/op-core/gnosis_code.py && cargo fmt --all
rtk proxy bash -c 'cargo test -p davinci-client --lib networks 2>&1 | grep -E "^test |test result"'
rtk proxy bash -c 'cargo test -p davinci-sequencer --lib config 2>&1 | grep -E "optimism_is_the_default|chain_id_must_match|test result"'
rtk proxy bash -c 'cargo test -p davinci-e2e --lib net 2>&1 | grep -E "^test |test result"'
grep -rn "GNOSIS\b\|gnosis_node_rpcs\|GNOSIS_RPC" --include=*.rs client sequencer e2e state | wc -l
```

Expected: `gnosis code: done`; networks `lookup`, `names_are_unique_and_lowercase`, `optimism_preset_is_deployed`, `optimism_posts_its_blobs_on_ethereum` ok, `4 passed`; config `optimism_is_the_default_network ... ok`, `chain_id_must_match_the_network ... ok`, `test result: ok. 16 passed`; e2e net `2 passed`; `0` references left.

- [ ] **Step 3: Docs, compose and `.env.example`**

Save as `~/.cache/op-core/gnosis_docs.py` and `~/.cache/op-core/gnosis_deployments.py`, then run both from the repository root:

```python
"""Gnosis retirement: docs, compose and .env.example. Run from the davinci-sequencer root:
python3 -I gnosis_docs.py"""
import re


def edit(p, pairs):
    s = open(p).read()
    for old, new in pairs:
        assert s.count(old) == 1, (p, old[:70])
        s = s.replace(old, new)
    open(p, "w").write(s)


edit("README.md", [
("""Run a node on Gnosis with Docker Compose. You need Docker, a Gnosis account holding a little xDAI
for the settlement transactions, and a davinci-zkvm prover (or a local NVIDIA GPU, see the
profiles below). Nothing else needs configuring: the defaults follow the Gnosis deployment in
[docs/deployments.md](docs/deployments.md).""",
"""Run a node on OP Mainnet with Docker Compose. You need Docker, an OP Mainnet account holding a
little ETH for the settlement transactions, a second, dedicated Ethereum account holding a little
ETH for the blob transactions, and a davinci-zkvm prover (or a local NVIDIA GPU, see the profiles
below). Nothing else needs configuring: the defaults follow the OP Mainnet deployment in
[docs/deployments.md](docs/deployments.md), whose blobs go to Ethereum."""),
("""# The key file must be readable by the container user (uid 10001).
sudo install -o 10001 -m 0400 /path/to/hex-private-key sequencer.key""",
"""# The key files must be readable by the container user (uid 10001): the OP
# settlement key and the dedicated Ethereum key that posts the blobs.
sudo install -o 10001 -m 0400 /path/to/hex-private-key sequencer.key
sudo install -o 10001 -m 0400 /path/to/hex-ethereum-key sequencer-l1.key"""),
("""`http://host.docker.internal:8080`, is a prover published on the same host). `/info` should show
`"chainId": 100`, the `ProcessRegistry` of [docs/deployments.md](docs/deployments.md) as
`processRegistry`, and your account as `sequencerAddress`. An empty `sequencer.key`
(`sudo install -o 10001 -m 0400 /dev/null sequencer.key`) runs an observer, which follows and
checks every election and needs neither xDAI nor a prover.""",
"""`http://host.docker.internal:8080`, is a prover published on the same host). `/info` should show
`"chainId": 10`, the `ProcessRegistry` of [docs/deployments.md](docs/deployments.md) as
`processRegistry`, and your account as `sequencerAddress`. Empty key files
(`sudo install -o 10001 -m 0400 /dev/null sequencer.key`, the same for `sequencer-l1.key`) run an
observer, which follows and checks every election and needs neither ETH nor a prover. Nothing
but the node may send from the Ethereum account (see
[configuration.md](docs/configuration.md#l1-blob-mode)).

Upgrading a compose node from v0.5.0: create `sequencer-l1.key` before the image moves
(`sudo install -o 10001 -m 0400 /dev/null sequencer-l1.key` for an observer, or the funded
Ethereum key for a signing node), or compose will not start the service: the file is a secret's
source. A signing node without a key in it refuses to boot."""),
("""DAVINCI_PRIVKEY_FILE=/etc/davinci/sequencer.key \\
DAVINCI_PROVER_URL=http://127.0.0.1:8080 \\
./target/release/davinci-sequencer
```

The `gnosis` network preset (the default) supplies the registry, its deployment block, the RPCs,
the beacon API and the confirmation depth, so a Gnosis node needs only the key and the prover. At
boot the node checks that the registry and its verifier pin the proof programs this release
proves, and refuses to start otherwise.

The `optimism` preset runs a node on OP Mainnet in L1-blob mode: transitions settle on OP and
their blobs go to Ethereum. Such a node also needs a dedicated Ethereum key for its blob posts
(`DAVINCI_L1_PRIVKEY_FILE`, funded on Ethereum, used by nothing else) and Ethereum endpoints (the
preset lists public ones); see [configuration.md](docs/configuration.md#l1-blob-mode) and
[deployments.md](docs/deployments.md#op-mainnet).""",
"""DAVINCI_PRIVKEY_FILE=/etc/davinci/sequencer.key \\
DAVINCI_L1_PRIVKEY_FILE=/etc/davinci/sequencer-l1.key \\
DAVINCI_PROVER_URL=http://127.0.0.1:8080 \\
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
is retired: this release has no `gnosis` preset."""),
("""| `DAVINCI_NETWORK` | `--network` | `gnosis` | Network preset, or `custom` with explicit chain settings. |""",
"""| `DAVINCI_NETWORK` | `--network` | `optimism` | Network preset, or `custom` with explicit chain settings. |"""),
("""- [docs/deployments.md](docs/deployments.md): contract addresses and pinned values of the Gnosis
  deployment.""",
"""- [docs/deployments.md](docs/deployments.md): contract addresses and pinned values of the OP
  Mainnet deployment, and the retired ones."""),
])

edit("docs/configuration.md", [
("""| `gnosis` | 100 | `https://gnosis-rpc.publicnode.com`, `https://gnosis-rpc.blockreq.com/v1/rpc/public`, `https://rpc.gnosischain.com` | `beacon:https://rpc-gbc.gnosischain.com` | 3 |
""", ""),
("""The `optimism` preset is in L1-blob mode: its transitions settle on OP Mainnet and its blobs go to Ethereum, see [L1-blob mode](#l1-blob-mode) and [deployments.md](deployments.md#op-mainnet).""",
"""The `optimism` preset, the default, is in L1-blob mode: its transitions settle on OP Mainnet and
its blobs go to Ethereum, see [L1-blob mode](#l1-blob-mode) and
[deployments.md](deployments.md#op-mainnet). The `gnosis` preset is gone: the Gnosis registries
pin the previous release's program vks (see [deployments.md](deployments.md#retired-deployments))."""),
("""| `--network` | `DAVINCI_NETWORK` | `gnosis` | Preset that fills every chain setting not given, or `custom`. |""",
"""| `--network` | `DAVINCI_NETWORK` | `optimism` | Preset that fills every chain setting not given, or `custom`. |"""),
])

edit("docs/testing.md", [
("""With `DAVINCI_E2E_LIVE=1` the same scenario runs on an existing deployment, by default the
`gnosis` preset. It needs key files in `DAVINCI_E2E_ORGANIZER_KEY` and""",
"""With `DAVINCI_E2E_LIVE=1` the same scenario runs on an existing blob-mode deployment
(`DAVINCI_E2E_NETWORK` names its preset; the default, `optimism`, takes the compact run below).
It needs key files in `DAVINCI_E2E_ORGANIZER_KEY` and"""),
("""The full scenario does not run live in L1-blob mode. With `DAVINCI_E2E_NETWORK=optimism`
(an L1-blob preset), `DAVINCI_E2E_OP_LIVE=1 DAVINCI_E2E_LIVE=1` runs a compact one instead""",
"""The full scenario does not run live in L1-blob mode. On the `optimism` preset (the default,
in L1-blob mode), `DAVINCI_E2E_OP_LIVE=1 DAVINCI_E2E_LIVE=1` runs a compact one instead"""),
])

edit("docs/demo.md", [
("""`DAVINCI_E2E_REGISTRY`, defaulting to the `gnosis` preset. It records its progress in a state""",
"""`DAVINCI_E2E_REGISTRY`, defaulting to the `optimism` preset. It records its progress in a state"""),
])

edit("docs/lifecycle.md", [
("""A background job proves the batch. The node accepts the proof only when the circuit reports
success, every public output equals the node's own value, and the program verification key and
`rootCVadcopFinal` equal the release pins. It then simulates `submitStateTransition` and sends the
blob transaction. A node keeps at most one batch in flight per process; processes run in
parallel.""",
"""A background job proves the batch. The node accepts the proof only when the circuit reports
success, every public output equals the node's own value, and the program verification key and
`rootCVadcopFinal` equal the release pins. It then simulates `submitStateTransition` and sends the
blob transaction. On OP Mainnet (the `optimism` preset) the registry is in L1-blob mode and the
blobs go to Ethereum instead, see [L1-blob mode](#l1-blob-mode-op-stack). A node keeps at most
one batch in flight per process; processes run in parallel."""),
])

edit("docker-compose.yml", [
("""# Configuration goes in .env (see .env.example). The node follows the
# DAVINCI_NETWORK deployment (default gnosis), so a Gnosis node needs only the
# key and a prover. The signing key is read from the file SEQUENCER_KEY_FILE
# (default ./sequencer.key) and mounted as a secret; it never goes in .env.
# The image runs as uid 10001, which must be able to read that file. An empty
# file runs the node as an observer.""",
"""# Configuration goes in .env (see .env.example). The node follows the
# DAVINCI_NETWORK deployment (default optimism: OP Mainnet, blobs on
# Ethereum), so it needs only its keys and a prover. The OP signing key is
# read from the file SEQUENCER_KEY_FILE (default ./sequencer.key), the
# dedicated Ethereum key that posts the blobs from SEQUENCER_L1_KEY_FILE
# (default ./sequencer-l1.key); both are mounted as secrets and never go in
# .env. The image runs as uid 10001, which must be able to read them. Empty
# files run the node as an observer."""),
("""  secrets:
    - sequencer_key
  volumes:
    - data:/data""",
"""  secrets:
    - sequencer_key
    - sequencer_l1_key
  volumes:
    - data:/data"""),
("""  DAVINCI_NETWORK: ${DAVINCI_NETWORK:-gnosis}
  DAVINCI_DATADIR: /data
  DAVINCI_PRIVKEY_FILE: /run/secrets/sequencer_key""",
"""  DAVINCI_NETWORK: ${DAVINCI_NETWORK:-optimism}
  DAVINCI_DATADIR: /data
  DAVINCI_PRIVKEY_FILE: /run/secrets/sequencer_key
  DAVINCI_L1_PRIVKEY_FILE: /run/secrets/sequencer_l1_key"""),
("""secrets:
  sequencer_key:
    file: ${SEQUENCER_KEY_FILE:-./sequencer.key}""",
"""secrets:
  sequencer_key:
    file: ${SEQUENCER_KEY_FILE:-./sequencer.key}
  sequencer_l1_key:
    file: ${SEQUENCER_L1_KEY_FILE:-./sequencer-l1.key}"""),
])

edit(".env.example", [
("""# The deployment the node follows. gnosis (the default) sets the registry,
# its start block, the RPCs, the blob source and the confirmations, and a
# release that moves it to a new registry needs no change here. custom uses
# only the settings below and needs DAVINCI_REGISTRY, DAVINCI_RPC_URL and
# DAVINCI_BLOB_SOURCE.
# DAVINCI_NETWORK=gnosis

# Each setting below overrides the network's value; leave them commented out
# on Gnosis. An explicit registry pins the node to it, so releases no longer
# move it.""",
"""# The deployment the node follows. optimism (the default: OP Mainnet, its
# blobs on Ethereum) sets the registry, its start block, the OP and Ethereum
# RPCs, the blob source and the confirmations, and a release that moves it to
# a new registry needs no change here. custom uses only the settings below and
# needs DAVINCI_REGISTRY, DAVINCI_RPC_URL and DAVINCI_BLOB_SOURCE.
# DAVINCI_NETWORK=optimism

# Each setting below overrides the network's value; leave them commented out
# on OP Mainnet. An explicit registry pins the node to it, so releases no
# longer move it."""),
("""# Blocks behind head treated as final. Default: the network's (3 on Gnosis),
# 2 on custom.
# DAVINCI_CONFIRMATIONS=

# Most blobs per settlement transaction (1 to 6). Unset: from eth_config.
# DAVINCI_MAX_BLOBS_PER_TX=""",
"""# Blocks behind head treated as final. Default: the network's (1 on OP
# Mainnet), 2 on custom.
# DAVINCI_CONFIRMATIONS=

# Most blobs per blob transaction (1 to 6). Unset: from eth_config. In L1-blob
# mode a batch takes up to 32 blobs, posted in transactions of this many.
# DAVINCI_MAX_BLOBS_PER_TX=

# L1-blob mode (OP Mainnet): Ethereum JSON-RPCs, and endpoints for the blob
# sends when those refuse 1.5 MB bodies. Default: the network's.
# DAVINCI_L1_RPC_URL=
# DAVINCI_L1_SEND_RPC_URL="""),
("""# DAVINCI_PRIVKEY_FILE is set by compose. DAVINCI_PRIVKEY (the key itself in
# the environment) is the node's alternative; don't set it here, the node
# refuses both at once.""",
"""# DAVINCI_PRIVKEY_FILE is set by compose. DAVINCI_PRIVKEY (the key itself in
# the environment) is the node's alternative; don't set it here, the node
# refuses both at once.

# L1-blob mode: host path of the file holding the dedicated Ethereum key that
# posts the blobs (nothing else may send from that account). Compose mounts it
# as a secret and sets DAVINCI_L1_PRIVKEY_FILE for the container, like the key
# above. A signing node needs it; an empty file is no key.
# SEQUENCER_L1_KEY_FILE=./sequencer-l1.key

# Most one blob post may cost at the fees it offers (wei, or a gwei/eth amount).
# DAVINCI_L1_MAX_POST_FEE=0.001eth"""),
])
print("gnosis docs: done")
```

```python
"""Gnosis retirement: docs/deployments.md keeps Gnosis as a retired history entry.
Run from the davinci-sequencer root: python3 -I gnosis_deployments.py"""
p = "docs/deployments.md"
s = open(p).read()
a = s.index("Pins are the release values listed under Gnosis Chain below.")
b = s.index("`L1BlobVerifier` reads OP's L1 origin from the `L1Block` predeploy")
s = s[:a] + """Pins are the v0.1 release values listed under [Retired deployments](#retired-deployments). No
DKG or Council adapter: those key modes are not on OP Stack chains yet. The grace window is the
production default (180 s, floor 150 s, ceiling 600 s, at most 1800 s past the end; 60 s shorten
notice).

""" + s[b:]
a = s.index("## Gnosis Chain")
s = s[:a] + """## Retired deployments

These registries stay on chain, but no preset follows them. They pin the v0.1 program vks
(davinci-zkvm v0.1.1), so a node of this release, which proves under v0.2.0, refuses them at boot.
Their datadir directories stay untouched next to the current one.

### Gnosis Chain

Chain id 100. The `gnosis` preset followed registry R2 (the production beta) from release v0.5.0
to v0.5.x, and R1 before; release v0.6.0 removed it. All contracts are source-verified on
gnosisscan.io; davinci-contracts' `deployments/100.json` holds R2's verified record.

| Registry | ProcessRegistry | Block | Adapters | DKGManager |
|---|---|---|---|---|
| R2 | `0x20b96e465CA7C3536B9C733571ec1eCf42b2eA21` | 48633301 | DKG `0xB74270Af067Bd75e30cC0cc6D24786EbD6816919`, Council `0x33e91518521Feb5D2A14928563dc6930Bc2F0755` (manager `0x2f5b110864cbad4017fe8ac59111812278f5f71f`) | `0xC6Fb38c42ed3FB35D363a702218746d5C7Da36BF` |
| R1 | `0x6702e0141B6b72bCF8C1bdff20A82A35C5502E7D` | 48504090 | DKG `0xE9559c78E7ff8c19937A0657a092A221E90CCBC3` | `0x9999f38ff8bf959e98ddd5d4551f82775219c01b` |

Both use `ZiskVerifier` `0x150547716bD6f15D872508b66b2ae7ce17677C9C` (block 48504089). A node
for R2 runs release v0.5.0 (its default `gnosis` preset) with a davinci-zkvm v0.1.1 prover; for R1
it also sets `DAVINCI_REGISTRY=0x6702e0141B6b72bCF8C1bdff20A82A35C5502E7D` and
`DAVINCI_START_BLOCK=48504090`.

### v0.1 pins

| Value | Where | Hash |
|---|---|---|
| batch program vk | registry `batchProgramVK` | `0x6cfc89d562d0b22f04478a5c15b390433eb52f1b03147030b183076260da7a10` |
| results program vk | registry `resultsProgramVK` | `0x7bc8c5e9235548386a44b1885732a2a7ffb1badddc8c7fba599d07ece47be794` |
| `rootCVadcopFinal` | registry and verifier | `0x05006517b6ccde5da4d890587ba62845b5af8a307c00e87d4b9d05099b16dc80` |
| ballot VK hash | registry `ballotVKHash`, state leaf `0x07` | `0xbf1e6590bb1ba883d601c4d7d1c6fa2722a78590716874019db6d68fc776bb0e` |
| verifier code keccak256 | `ZiskVerifier` runtime code | `0x82385a405b7301345d7e246017846ca3228aaea349cb68b116d76e0e77056566` |

## Verifying a deployment

Three checks refuse a registry that pins other programs: the node's startup check,
`davinci_client::organizer::verify_registry` for clients, and davinci-contracts'
`script/verify_deployment.py`, which compares the deployed runtime code with a local build (of
the commit in the deployment record) and reads back every pin, the grace settings
(`--grace`) and, in L1-blob mode, the verifier's `L1Block`. Rebuilding a zkVM program changes its
verification key, which needs a new registry.
"""
open(p, "w").write(s)
print("deployments.md: done")
```

```bash
python3 -I ~/.cache/op-core/gnosis_docs.py && python3 -I ~/.cache/op-core/gnosis_deployments.py
grep -n -i "gnosis" README.md docker-compose.yml .env.example docs/configuration.md docs/testing.md docs/demo.md docs/lifecycle.md
docker compose -f docker-compose.yml --env-file .env.example config 2>&1 | grep -E "sequencer_l1_key|DAVINCI_NETWORK|error" | head -5
docker compose -f docker-compose.yml --env-file .env.example config 2>&1 | grep -c DAVINCI_L1_PRIVKEY_FILE
```

Expected: `gnosis docs: done`, `deployments.md: done`; the remaining Gnosis mentions are only: README "Gnosis is retired…" (2 lines), `docs/configuration.md` "The `gnosis` preset is gone…" and the chain-limits text ("2 on Gnosis and Chiado…", chain data that stays). The compose render (read-only, starts nothing) lists `- sequencer_l1_key` and `DAVINCI_NETWORK: optimism` and no `error`; the `grep -c DAVINCI_L1_PRIVKEY_FILE` prints `1` per service rendered (compose sets it in `x-sequencer-env`, not in `.env`, so a `.env` that predates this release still gets it).

- [ ] **Step 4: Commit**

```bash
cargo fmt --all -- --check && cargo clippy --workspace --all-targets --locked -- -D warnings
rtk proxy bash -c 'cargo test --workspace 2>&1 | tee $TMPDIR/ws.log | grep -E "^test result: FAILED|panicked"; echo "suites ok $(grep -c "^test result: ok" $TMPDIR/ws.log), exit ${PIPESTATUS[0]}"'
git add client/src/networks.rs sequencer/src/config.rs e2e/src/net.rs e2e/tests/e2e.rs e2e/tests/demo.rs \
  README.md docker-compose.yml .env.example docs/configuration.md docs/deployments.md docs/lifecycle.md \
  docs/testing.md docs/demo.md
git commit -m "feat(net): retire gnosis, optimism is the default network" -m "A v0.2.0 node cannot settle on the Gnosis registries (they pin the v0.1
program vks), so the gnosis preset goes and optimism becomes the default for
the node, the client tooling and the live e2e. Compose mounts the dedicated
Ethereum key as a second secret. The Gnosis deployments stay in
docs/deployments.md as retired history."
```

Expected: no failure, `exit 0`.

---

### Task 6: A 512 x 16 transition on a production-grace anvil registry, and a running-node smoke test

**Files:**
- Modify: `e2e/src/chain.rs:210-299` (`grace_args`, `deploy_with`)
- Create: `e2e/tests/l1_big.rs`, `e2e/tests/node_smoke.rs`
- Docs: `docs/testing.md`

**Interfaces:**
- Consumes: Tasks 2-3 (the node); `davinci_e2e::{net::Net, node::{Node, NodeConfig, WorkDir}, proxy::{Proxy, Tap, Exchange}, l1::{posts, L1Post}, fixture as fx, wait}`; `Organizer::{set_process_grace, pause_process, end_process, results, cancel_open}`.
- Produces: `davinci_e2e::chain::grace_args() -> Result<(u32, u32, u32, u32, u32)>` (env `DAVINCI_E2E_GRACE`); env `DAVINCI_E2E_L1_BIG`, `DAVINCI_E2E_RECORD_DIR`, `DAVINCI_E2E_NODE_URL`; the recorded `snark_batch.json`/`publics_batch.bin` (Task 10 consumes them) and the stderr line `recorded job <id> in <dir>: voters V overwrites W n_blobs B occupied_before O nproofs P n_public Q log_n L`.

- [ ] **Step 1: The anvil registry's grace from the environment**

`e2e/src/chain.rs`, after `pub const GRACE_ARGS …;` (line 210):

```rust

/// The anvil registry's grace args: `DAVINCI_E2E_GRACE`
/// (`default,floor,ceil,max_total,notice`, seconds; `300,240,600,1800,60` is
/// the OP Mainnet registry's), else [`GRACE_ARGS`].
pub fn grace_args() -> Result<(u32, u32, u32, u32, u32)> {
    let Ok(v) = std::env::var("DAVINCI_E2E_GRACE") else {
        return Ok(GRACE_ARGS);
    };
    let n: Vec<u32> = v
        .split(',')
        .map(|x| x.trim().parse())
        .collect::<std::result::Result<_, _>>()
        .context("DAVINCI_E2E_GRACE")?;
    let [default, floor, ceil, max_total, notice] = n[..] else {
        bail!("DAVINCI_E2E_GRACE: default,floor,ceil,max_total,notice");
    };
    Ok((default, floor, ceil, max_total, notice))
}
```

In the doc of `deploy` (`[`GRACE_ARGS`]` becomes `[`grace_args`]`); in `deploy_with`, after `let vk_hash = B256::from(ballot_vk_hash()?);` add `let grace = grace_args()?;`, and the two uses of `GRACE_ARGS` there become `grace` (`code.extend_from_slice(&grace.abi_encode_params());` and `) == grace`).

Run: `rtk proxy bash -c 'DAVINCI_E2E_GRACE=300,240,600,1800,60 DAVINCI_E2E_SETUP=1 DAVINCI_E2E_DA=l1-blobs cargo test -p davinci-e2e --test e2e setup_without_nodes -- --nocapture 2>&1 | grep -E "pins ok|L1-blob mode|test result"'`
Expected: `chain 31337 registry 0xCf7Ed3AccA5a467e9e704C703E8D87F634fB0Fc9 verifier 0x5FbDB2315678afecb367f032d93F642f64180aa3 (pins ok)`, the two `L1-blob mode` lines and `test result: ok. 1 passed` (the deployment's own grace read-back passed with 300/240/600/1800/60). With `DAVINCI_E2E_GRACE=1,2` the same command fails with `DAVINCI_E2E_GRACE: default,floor,ceil,max_total,notice`.

- [ ] **Step 2: `e2e/tests/l1_big.rs`**

```rust
//! One 512-vote, 16-field transition in L1-blob mode, with the real prover.
//! Gated by `DAVINCI_E2E_L1_BIG=1`. On two anvils it needs
//! `DAVINCI_E2E_DA=l1-blobs` and the OP Mainnet registry's grace window,
//! `DAVINCI_E2E_GRACE=300,240,600,1800,60`; live (`DAVINCI_E2E_LIVE=1`) it
//! runs on the `optimism` preset with the `op_live` environment.
//!
//! One node with the L1-blob defaults (`--l1-settle-lag 110s`,
//! `--settle-margin 30s`, `--prove-base 30s`) and `--batch-max 512`. 512
//! first votes settle in whatever batches the seal budget allows. Then the
//! organizer pauses the process, 512 new voters vote, and the organizer ends
//! it: the flush seals all 512 votes with 512 silent refreshes, nine blobs,
//! which go out as two Ethereum blob transactions and settle in one proof
//! within the grace. The process takes the registry's ceiling grace (600 s),
//! so the node's cold proving estimate (0.8 s a vote at 16 fields) fits 512
//! votes after the end. The node then publishes the results, which must
//! equal the tally.
//!
//! With `DAVINCI_E2E_RECORD_DIR`, the first batch job's snark and register
//! dump are written there as `snark_batch.json` and `publics_batch.bin`, the
//! format of davinci-zkvm's `rust-sdk/testdata`. About 30 minutes (half of
//! it the grace window); the shared prover's queue must be empty first.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use davinci_client::api::{ProcessId, ProcessStatus, VoteRequest, VoteStatus};
use davinci_client::organizer::{KeyMode, NewProcess, Organizer, RegistryReader};
use davinci_client::prover::BallotProver;
use davinci_e2e::fixture as fx;
use davinci_e2e::l1::{self, L1Post};
use davinci_e2e::net::{self, Net};
use davinci_e2e::node::{self, Node, WorkDir};
use davinci_e2e::proxy::{Exchange, Proxy, Tap};
use davinci_e2e::wait;
use davinci_zkvm_sdk::client::ProverClient;
use davinci_zkvm_sdk::publics::BatchPublics;
use davinci_zkvm_sdk::types::JobId;

/// The OP Mainnet registry's grace window.
const OP_GRACE: (u32, u32, u32, u32, u32) = (300, 240, 600, 1800, 60);
const N: usize = 512;
const NF: u8 = 16;

/// The job ids of the node's `/prove` calls, in order.
fn prove_tap(jobs: Arc<Mutex<Vec<String>>>) -> Tap {
    Arc::new(move |x: &Exchange<'_>| {
        if x.method != "POST" || x.path != "/prove" {
            return;
        }
        let id = serde_json::from_slice::<serde_json::Value>(x.response)
            .ok()
            .and_then(|v| v["job_id"].as_str().map(String::from));
        if let (Some(id), Ok(mut j)) = (id, jobs.lock()) {
            j.push(id);
        }
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn l1_big_batch() -> Result<()> {
    if std::env::var("DAVINCI_E2E_L1_BIG").as_deref() != Ok("1") {
        eprintln!("DAVINCI_E2E_L1_BIG not set, skipping");
        return Ok(());
    }
    let prover_url =
        std::env::var("DAVINCI_ZKVM_URL").unwrap_or_else(|_| "http://127.0.0.1:8080".into());
    let health = ProverClient::new(&prover_url).health().await?;
    ensure!(
        health.status == "ok" && health.queue_len == 0,
        "the shared prover is busy or down: {health:?}"
    );
    let mut dir = WorkDir::new("davinci-l1-big-")?;
    let ballot_prover = tokio::task::spawn_blocking(fx::load_prover);
    let net = Net::open(dir.path()).await?;
    ensure!(
        net.l1.is_some(),
        "DAVINCI_E2E_L1_BIG runs in L1-blob mode: DAVINCI_E2E_DA=l1-blobs on anvil"
    );
    net.check_balances(1).await?;
    let org = net.organizer()?;
    let g = org.grace_params().await?;
    ensure!(
        (
            g.default_grace,
            g.grace_floor,
            g.grace_ceil,
            g.grace_max_total,
            g.notice_min
        ) == OP_GRACE,
        "the registry's grace is {g:?}: on anvil set DAVINCI_E2E_GRACE=300,240,600,1800,60"
    );
    let r = run(&net, &org, &prover_url, dir.path(), ballot_prover).await;
    match &r {
        Ok(()) => eprintln!("PASSED"),
        Err(e) => eprintln!("FAILED: {e:#}"),
    }
    let c = org.cancel_open().await;
    dir.ok = r.is_ok();
    r?;
    c.context("cancel the open processes")?;
    Ok(())
}

async fn run(
    net: &Net,
    org: &Organizer,
    prover_url: &str,
    dir: &Path,
    ballot_prover: tokio::task::JoinHandle<Result<BallotProver>>,
) -> Result<()> {
    let bin = tokio::task::spawn_blocking(node::sequencer_bin).await??;
    let census_dir = dir.join("census");
    std::fs::create_dir_all(&census_dir)?;
    let census_dir = census_dir.canonicalize()?;
    let jobs: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let proxy = Proxy::tapped(prover_url, prove_tap(jobs.clone())).await?;
    let mut c = net.node_config("node-big", Some(0), 0, &proxy.url, &census_dir, dir)?;
    // The node's own L1-blob budget: 110 s Ethereum leg, 30 s margin, 30 s
    // prove base, 3 m flush horizon.
    (
        c.l1_settle_lag,
        c.settle_margin,
        c.prove_base,
        c.flush_horizon,
    ) = (None, None, None, None);
    c.batch_max = N;
    c.batch_time = "30s".into();
    let node = Node::start(&bin, &c).await?;
    eprintln!("node at {} log {}", node.url, node.log.display());

    let voters = fx::bench_voters(2 * N);
    let (census, tree) = fx::merkle_census_of(&voters)?;
    let uri = fx::write_census(&census_dir, "big.json", &census)?;
    let (metadata, metadata_hash) = fx::write_metadata(&census_dir, "big-metadata.json", "big")?;
    let next = org.next_process_id().await?;
    let pk = node.api.new_key(&next).await?;
    let pid = org
        .create_process(&NewProcess {
            process_id: next,
            start_time: 0,
            duration: 2 * 3600,
            max_voters: 2 * N as u64,
            ballot_mode: fx::ballot_mode_nf(NF),
            census_origin: 1,
            census_root: tree.root(),
            census_contract: [0; 20],
            census_uri: uri,
            metadata,
            metadata_hash,
            key_mode: KeyMode::Sequencer(pk),
        })
        .await?
        .pid;
    org.set_process_grace(&pid, OP_GRACE.2).await?;
    eprintln!("process {} with a {} s grace", ProcessId(pid), OP_GRACE.2);

    let p = RegistryReader::connect(&net.rpc, net.registry)?
        .process(&pid)
        .await?;
    let mut list = Vec::new();
    for (v, voter) in voters.iter().enumerate() {
        list.push(fx::Job {
            label: format!("voter {v}"),
            key: voter.key.clone(),
            process: p.clone(),
            fields: fx::choices_nf(v, NF as usize),
            census: fx::merkle_witness(&tree, v)?,
            weight: fx::WEIGHT,
            k: fx::vote_k(0x51, v, 1),
        });
    }
    let prover = ballot_prover.await??;
    let t = Instant::now();
    let reqs = tokio::task::block_in_place(|| fx::prove_all(&prover, &list))?;
    eprintln!(
        "{} ballots proved in {:.0} s",
        reqs.len(),
        t.elapsed().as_secs_f64()
    );
    let last = voters.last().context("no voters")?.address();
    wait::until(
        "the node to serve the census",
        net::scaled(Duration::from_secs(180)),
        Duration::from_secs(1),
        || async {
            node.check_alive()?;
            Ok(node.api.participant(&pid, &last).await.ok().map(|_| ()))
        },
    )
    .await?;
    let l1 = net.l1.as_ref().context("L1-blob mode")?;
    let scan_from = net.start_block.unwrap_or(net.from_block);
    let ours = async || -> Result<Vec<L1Post>> {
        let all = l1::posts(&net.rpc, l1.read_rpc(), net.registry, scan_from).await?;
        Ok(all.into_iter().filter(|p| p.pid == pid).collect())
    };

    // Phase 1: the first 512 voters, in the batches the seal budget allows.
    let phase1 = Instant::now();
    submit(&node, &reqs[..N]).await?;
    settled(&node, &pid, &reqs[..N]).await?;
    let first = ours().await?;
    eprintln!("first 512 votes settled in {} transitions", first.len());
    // What the node measured: the benchmarks sub-project decides from this
    // whether a warmed node fits 512 x 16 inside the 300 s default grace.
    eprintln!(
        "phase 1: {:.2} s per vote from submit to settled ({:.0} s for {N}, including the Ethereum legs)",
        phase1.elapsed().as_secs_f64() / N as f64,
        phase1.elapsed().as_secs_f64()
    );
    if let Some(out) = std::env::var_os("DAVINCI_E2E_RECORD_DIR") {
        let id = jobs.lock().ok().and_then(|j| j.first().cloned());
        record(
            prover_url,
            &id.context("no /prove call seen")?,
            Path::new(&out),
        )
        .await?;
    }

    // Phase 2: 512 new voters while paused, then the end; the flush seals them all.
    org.pause_process(&pid).await?;
    wait_status(&node, &pid, ProcessStatus::Paused).await?;
    submit(&node, &reqs[N..]).await?;
    org.end_process(&pid).await?;
    let ended = Instant::now();
    settled(&node, &pid, &reqs[N..]).await?;
    eprintln!(
        "512 x 16 settled {:.0} s after the end",
        ended.elapsed().as_secs_f64()
    );
    let posts = ours().await?;
    ensure!(
        posts.len() == first.len() + 1,
        "{} settlements after the end, want one",
        posts.len() - first.len()
    );
    let big = posts.last().context("no L1 settlement")?;
    ensure!(big.vhs.len() == 9, "{} blobs, want 9", big.vhs.len());
    ensure!(
        big.l1_txs.len() == 2,
        "{} Ethereum transactions, want 2",
        big.l1_txs.len()
    );
    let sealed = node.log_lines(&["batch sealed"]);
    let line = sealed.last().context("no seal logged")?;
    ensure!(
        line.contains("votes=512") && line.contains("reason=flush"),
        "the last seal: {line}"
    );
    let settled_p = RegistryReader::connect(&net.rpc, net.registry)?
        .process(&pid)
        .await?;
    ensure!(
        settled_p.voters_count == 2 * N as u64,
        "{} voters",
        settled_p.voters_count
    );
    eprintln!(
        "nine blobs in Ethereum blocks {:?} ({} transactions), OP tx {}",
        big.l1_blocks,
        big.l1_txs.len(),
        big.op_tx
    );

    // The node holds the key: after the grace, the results equal the tally.
    let ballots: Vec<Vec<u64>> = (0..2 * N).map(|v| fx::choices_nf(v, NF as usize)).collect();
    let want = fx::expected_tally(&ballots);
    let got = wait::until(
        "the results",
        net::scaled(Duration::from_secs(25 * 60)),
        Duration::from_secs(10),
        || async {
            node.check_alive()?;
            Ok(org.results(&pid).await?)
        },
    )
    .await?;
    ensure!(got == want, "results {got:?}, tally {want:?}");
    eprintln!("results on-chain = tally");
    drop(node);
    Ok(())
}

async fn submit(node: &Node, reqs: &[VoteRequest]) -> Result<()> {
    for r in reqs {
        node.api
            .submit_vote(r)
            .await
            .with_context(|| format!("vote {}", r.vote_id))?;
    }
    Ok(())
}

/// Waits until every vote of `reqs` is settled (30 minutes at most).
async fn settled(node: &Node, pid: &[u8; 31], reqs: &[VoteRequest]) -> Result<()> {
    let mut open: Vec<u64> = reqs.iter().map(|r| r.vote_id).collect();
    let limit = net::scaled(Duration::from_secs(30 * 60));
    let t = Instant::now();
    while !open.is_empty() {
        ensure!(
            t.elapsed() < limit,
            "{} votes not settled after {limit:?}",
            open.len()
        );
        tokio::time::sleep(Duration::from_secs(5)).await;
        node.check_alive()?;
        let mut still = Vec::new();
        for vid in open {
            let s = node.api.vote_status_full(pid, vid).await?;
            match s.status {
                VoteStatus::Settled => {}
                VoteStatus::Error => bail!("vote {vid}: {}", s.error.unwrap_or_default()),
                _ => still.push(vid),
            }
        }
        open = still;
    }
    Ok(())
}

/// Waits until the node reports the process with `status`.
async fn wait_status(node: &Node, pid: &[u8; 31], status: ProcessStatus) -> Result<()> {
    wait::until(
        "the node to see the status",
        net::scaled(Duration::from_secs(60)),
        Duration::from_secs(1),
        || async { Ok((node.api.process(pid).await?.status == status).then_some(())) },
    )
    .await
}

/// Writes job `id`'s snark (the key order of davinci-zkvm's testdata) and
/// its 256-byte register dump into `out`.
async fn record(prover_url: &str, id: &str, out: &Path) -> Result<()> {
    let pc = ProverClient::new(prover_url);
    let id = JobId::parse(id)?;
    let s = pc.snark(&id).await?;
    let regs = pc.publics(&id).await?;
    ensure!(regs.len() == 256, "{} register bytes", regs.len());
    std::fs::create_dir_all(out)?;
    let json = format!(
        "{{\n  \"program_vk\": \"0x{}\",\n  \"root_c_vadcop_final\": \"0x{}\",\n  \"public_values\": \"0x{}\",\n  \"proof_bytes\": \"0x{}\"\n}}\n",
        hex::encode(s.program_vk),
        hex::encode(s.root_c_vadcop_final),
        hex::encode(&s.public_values),
        hex::encode(&s.proof_bytes)
    );
    std::fs::write(out.join("snark_batch.json"), json)?;
    std::fs::write(out.join("publics_batch.bin"), &regs)?;
    let p = BatchPublics::parse(&regs)?;
    eprintln!(
        "recorded job {id} in {}: voters {} overwrites {} n_blobs {} occupied_before {} \
         nproofs {} n_public {} log_n {}",
        out.display(),
        p.voters,
        p.overwrites,
        p.n_blobs,
        p.occupied_before,
        p.nproofs,
        p.n_public,
        p.log_n
    );
    Ok(())
}
```

- [ ] **Step 3: `e2e/tests/node_smoke.rs`**

```rust
//! A small election through a node that already runs (a production node
//! after a switch), gated by `DAVINCI_E2E_NODE_URL`. On the live deployment
//! of `davinci_e2e::net::live_target` (`DAVINCI_E2E_NETWORK`, default
//! `optimism`; `DAVINCI_E2E_RPC`, `DAVINCI_E2E_REGISTRY`), organized with the
//! key file `DAVINCI_E2E_ORGANIZER_KEY`: a CSP process with three voters and
//! the node's election key; the organizer ends it once the votes are queued,
//! the node seals them in its flush, settles them within the grace window
//! and publishes the tally. Spawns nothing. About 10 minutes, most of it the
//! registry's default grace.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use davinci_client::SequencerClient;
use davinci_client::api::{ProcessId, VoteStatus};
use davinci_client::organizer::{KeyMode, NewProcess, Organizer, RegistryReader, verify_registry};
use davinci_e2e::fixture as fx;
use davinci_e2e::net;
use davinci_e2e::node::WorkDir;
use davinci_e2e::wait;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn node_smoke() -> Result<()> {
    let Ok(url) = std::env::var("DAVINCI_E2E_NODE_URL") else {
        eprintln!("DAVINCI_E2E_NODE_URL not set, skipping");
        return Ok(());
    };
    let (rpc, registry, _) = net::live_target()?;
    let pins = verify_registry(&rpc, registry)
        .await
        .context("registry pins")?;
    let key_file = std::env::var("DAVINCI_E2E_ORGANIZER_KEY")
        .context("DAVINCI_E2E_ORGANIZER_KEY (a key file) is required")?;
    let org = Organizer::connect(&rpc, net::read_key(Path::new(&key_file))?, registry)?;
    let node = SequencerClient::new(&url);
    let info = node.info().await.context("node /info")?;
    ensure!(
        info.chain_id == pins.chain_id && info.process_registry == registry.0.0 && !info.observer,
        "{url}: chain {}, registry or observer mode differ",
        info.chain_id
    );
    let dir = WorkDir::new("davinci-node-smoke-")?;
    let csp = fx::csp_key();
    let (metadata, metadata_hash) = fx::write_metadata(dir.path(), "smoke.json", "node smoke")?;
    let next = org.next_process_id().await?;
    let pk = node.new_key(&next).await?;
    let pid = org
        .create_process(&NewProcess {
            process_id: next,
            start_time: 0,
            duration: 3600,
            max_voters: fx::N_CSP as u64,
            ballot_mode: fx::ballot_mode(),
            census_origin: 4,
            census_root: fx::csp_root(&csp)?,
            census_contract: [0; 20],
            census_uri: "csp://davinci-node-smoke".into(),
            metadata,
            metadata_hash,
            key_mode: KeyMode::Sequencer(pk),
        })
        .await?
        .pid;
    eprintln!("process {}", ProcessId(pid));
    let r = vote(&org, &node, &rpc, registry, &pid, &csp).await;
    if r.is_err() {
        let _ = org.cancel_process(&pid).await;
    }
    r
}

async fn vote(
    org: &Organizer,
    node: &SequencerClient,
    rpc: &str,
    registry: alloy::primitives::Address,
    pid: &[u8; 31],
    csp: &k256::ecdsa::SigningKey,
) -> Result<()> {
    let p = RegistryReader::connect(rpc, registry)?.process(pid).await?;
    let prover = tokio::task::spawn_blocking(fx::load_prover).await??;
    let mut jobs = Vec::new();
    for (i, voter) in fx::csp_voters().iter().enumerate() {
        jobs.push(fx::Job {
            label: format!("voter {i}"),
            key: voter.key.clone(),
            process: p.clone(),
            fields: fx::choices(i, 1),
            census: fx::csp_witness(csp, &p, voter, i as u64),
            weight: fx::WEIGHT,
            k: fx::vote_k(0x60, i, 1),
        });
    }
    let reqs = tokio::task::block_in_place(|| fx::prove_all(&prover, &jobs))?;
    wait::until(
        "the node to list the process",
        net::scaled(Duration::from_secs(120)),
        Duration::from_secs(2),
        || async { Ok(node.process(pid).await.ok().map(|_| ())) },
    )
    .await?;
    for r in &reqs {
        node.submit_vote(r)
            .await
            .with_context(|| format!("vote {}", r.vote_id))?;
    }
    // Queued: the end flushes them.
    org.end_process(pid).await?;
    for r in &reqs {
        wait::until(
            &format!("vote {} to settle", r.vote_id),
            net::scaled(Duration::from_secs(600)),
            Duration::from_secs(5),
            || async {
                let s = node.vote_status_full(pid, r.vote_id).await?;
                ensure!(
                    s.status != VoteStatus::Error,
                    "vote {}: {}",
                    r.vote_id,
                    s.error.unwrap_or_default()
                );
                Ok((s.status == VoteStatus::Settled).then_some(()))
            },
        )
        .await?;
    }
    eprintln!("3 votes settled");
    let last: Vec<Vec<u64>> = (0..fx::N_CSP).map(|i| fx::choices(i, 1)).collect();
    let want = fx::expected_tally(&last);
    let got = wait::until(
        "the results",
        net::scaled(Duration::from_secs(20 * 60)),
        Duration::from_secs(10),
        || async { Ok(org.results(pid).await?) },
    )
    .await?;
    ensure!(got == want, "results {got:?}, tally {want:?}");
    eprintln!("results on-chain = tally {want:?}");
    Ok(())
}
```

- [ ] **Step 4: Docs**

In `docs/testing.md`, add a table row after the `DAVINCI_E2E_L1_BLOCK_TIME`, `DAVINCI_E2E_L1_LAG` row:

```markdown
| `DAVINCI_E2E_GRACE` | The anvil registry's grace window, `default,floor,ceil,max_total,notice` in seconds (default `davinci_e2e::chain::GRACE_ARGS`). |
```

and before `## Docker runner`:

````markdown
### One 512 x 16 transition in L1-blob mode

`e2e/tests/l1_big.rs`, gated by `DAVINCI_E2E_L1_BIG=1`, settles one transition of 512 votes at 16
fields with the real prover, on two anvils whose registry has the OP Mainnet grace window
(`DAVINCI_E2E_GRACE=300,240,600,1800,60`), or live with the `op_live` environment. One node runs
the L1-blob defaults (`--l1-settle-lag 110s`, `--settle-margin 30s`, `--prove-base 30s`) with
`--batch-max 512`. 512 first votes settle as the seal budget allows; then 512 new voters vote while
the process is paused, the organizer ends it, and the flush seals all of them with 512 silent
refreshes: nine blobs, posted as two Ethereum transactions and settled in one proof. The process
takes the registry's 600 s ceiling grace, so a fresh node's proving estimate fits the batch after
the end; the node then publishes the results. `DAVINCI_E2E_RECORD_DIR` writes the first batch
job's snark and register dump in davinci-zkvm's `rust-sdk/testdata` format. A run takes about 30
minutes, half of it the grace window, and wants the shared prover's queue empty:

```bash
DAVINCI_E2E_L1_BIG=1 DAVINCI_E2E_DA=l1-blobs DAVINCI_E2E_GRACE=300,240,600,1800,60 \
  systemd-run --user --scope -p MemoryMax=16G -- \
  cargo test -p davinci-e2e --test l1_big -- --nocapture
```

### A small election through a running node

`e2e/tests/node_smoke.rs`, gated by `DAVINCI_E2E_NODE_URL`, drives a node that already runs (a
production node after an upgrade) on the live deployment of the preset: a CSP process with three
voters and the node's election key, created with the organizer key file
`DAVINCI_E2E_ORGANIZER_KEY`. The organizer ends it once the votes are queued; the node seals them
in its flush, settles them and publishes the tally, which must match. It spawns nothing and takes
about 10 minutes, most of it the grace window.

````

- [ ] **Step 5: Build, lint and check the gates skip**

```bash
cargo fmt --all -- --check && cargo clippy --workspace --all-targets --locked -- -D warnings
rtk proxy bash -c 'cargo test -p davinci-e2e --test l1_big --test node_smoke 2>&1 | grep -E "skipping|test result"'
rtk proxy bash -c 'cargo test -p davinci-e2e 2>&1 | grep -E "^test result" | sort | uniq -c'
```

Expected: no warnings; `DAVINCI_E2E_L1_BIG not set, skipping`, `DAVINCI_E2E_NODE_URL not set, skipping`, each `test result: ok. 1 passed`; every suite of the crate `ok`.

- [ ] **Step 6: Commit**

```bash
git add e2e/src/chain.rs e2e/tests/l1_big.rs e2e/tests/node_smoke.rs docs/testing.md
git commit -m "test(e2e): one 512 x 16 transition over two ethereum transactions, and a running-node smoke test" -m "l1_big settles 512 votes at 16 fields with 512 refreshes (nine blobs, two
Ethereum transactions) on a registry with the OP grace window, on anvil or
live, and can record the first batch job for davinci-zkvm's wire test.
node_smoke runs a three-voter CSP election through a node that already runs.
DAVINCI_E2E_GRACE sets the anvil registry's grace."
```

---

### Task 7: davinci-contracts: one transition settled from two Ethereum transactions

**Files:**
- Create: `test/vectors/l1blob.go`
- Modify: `test/vectors/main.go:3-9` (doc), `:77` (call), `test/vectors/go.mod`, `test/vectors/go.sum`
- Generate: `test/vectors/l1blob/one_tx.json`, `two_tx.json`, `two_tx_reposted.json`, `worst_case.json`
- Test: `test/L1BlobVerifier.t.sol` (five tests), `test/TransitionL1.t.sol` (imports, contract `TransitionL1TwoTxTest`)

**Interfaces:**
- Consumes: the fixture's two transitions (`test/vectors/transition.json`: transition 0 one blob, transition 1 two blobs, 1000 voters).
- Produces: fixtures in the format of `test/vectors/l1blob/recent.json` (`headers`, `header_hashes`, `header_numbers`, `txs[].{header_index,key,proof}`, `expected_vhs`, `expected_tx_blocks` one per hash, `tx_type`, `name`): `one_tx` (transition 0's blob, block 1000, headers 1002..1000), `two_tx` (transition 1's blobs in blocks 2000 and 2002, headers 2003..2000), `two_tx_reposted` (the same hashes, the first in block 2002), `worst_case` (13 synthetic hashes in three transactions, blocks 3000, 3023, 3061, headers 3063..3000).

No contract source changes in this task or the next.

- [ ] **Step 1: Branch on top of `origin/main`**

```bash
cd /home/p4u/davinci-contracts
git status --short | wc -l          # expect 0
test -e .env && echo ".env PRESENT" || echo "no .env"
git fetch origin
git rev-parse --short origin/main   # expect 7c7b0b2 (ci(deps): pin davinci-zkvm to v0.1.1)
git checkout feat/compact-da-stream && git checkout -b feat/op-core
git rebase origin/main
```

Expected: `0`, `no .env`, then the rebase stops once at `db89888 test(vectors): compact da stream fixtures` with `CONFLICT (content): Merge conflict in .github/workflows/test.yml`. Keep the branch side:

```bash
git checkout --theirs -- .github/workflows/test.yml && git add .github/workflows/test.yml
GIT_EDITOR=true git rebase --continue
git diff feat/compact-da-stream --stat | wc -l     # expect 0
grep -n "ref: v0" .github/workflows/test.yml       # expect: ref: v0.2.0
```

The remote `feat/l1-blob-da` carries two commits this branch does not (`dbbbfa8` pin to v0.1.1, `3366446` a bot bindings commit); both are superseded here (Task 8 regenerates the bindings) and that branch is not merged.

- [ ] **Step 2: Write the failing forge tests**

`test/L1BlobVerifier.t.sol`, before `function test_RevertWhen_HeaderIndexOutOfRange()`:

```solidity
    /// One transition in two transactions two blocks apart (synthetic headers, test/vectors).
    function test_TwoTxsInTwoBlocks() public {
        Fx memory fx = _load("two_tx");
        l1.set(uint64(fx.blocks[1]) + 1, fx.hashes[0]);
        (bytes32[] memory vhs, uint256[] memory blocks) = v.verify(fx.p);
        assertEq(vhs, fx.vhs);
        assertEq(blocks, fx.blocks);
        assertEq(blocks[0], 2000);
        assertEq(blocks[1], 2002);
    }

    /// The first transaction was posted again and landed after the second: the proof still
    /// lists it first, and the hashes come back in proof order.
    function test_TwoTxs_RepostedFirstInTheNewerBlock() public {
        Fx memory fx = _load("two_tx_reposted");
        l1.set(2003, fx.hashes[0]);
        (bytes32[] memory vhs, uint256[] memory blocks) = v.verify(fx.p);
        assertEq(vhs, _load("two_tx").vhs);
        assertEq(blocks, fx.blocks);
        assertEq(blocks[0], 2002);
        assertEq(blocks[1], 2000);
    }

    /// A recorded origin between the two blocks anchors the older transaction only.
    function test_RevertWhen_TwoTxs_RecordBelowTheNewer() public {
        Fx memory fx = _load("two_tx");
        assertEq(_number(fx.p.headers[2]), 2001);
        l1.set(2001, fx.hashes[2]);
        v.record();
        l1.set(2011, keccak256("later"));
        vm.expectRevert(abi.encodeWithSelector(IL1BlobVerifier.TxAboveOrigin.selector, 1));
        v.verify(fx.p);
        fx = _load("two_tx_reposted");
        l1.set(2001, fx.hashes[2]);
        v.record();
        l1.set(2011, keccak256("later"));
        vm.expectRevert(abi.encodeWithSelector(IL1BlobVerifier.TxAboveOrigin.selector, 0));
        v.verify(fx.p);
    }

    /// A record at the newer block anchors both.
    function test_TwoTxs_RecordAtTheNewer() public {
        Fx memory fx = _load("two_tx");
        l1.set(2002, fx.hashes[1]);
        v.record();
        l1.set(2040, keccak256("later"));
        (bytes32[] memory vhs, uint256[] memory blocks) = v.verify(fx.p);
        assertEq(vhs, fx.vhs);
        assertEq(blocks, fx.blocks);
    }

    /// The largest proof a node sends: 64 headers and three transactions (13 blobs, a 512-vote
    /// 16-field batch of revotes). It must leave the settlement well inside OP's 128 KB transaction
    /// limit and the node's gas estimate.
    function test_WorstCaseProof_FitsOP() public {
        Fx memory fx = _load("worst_case");
        assertEq(fx.p.headers.length, 64);
        assertEq(fx.p.txs.length, 3);
        l1.set(_number(fx.p.headers[0]), fx.hashes[0]);
        uint256 g = gasleft();
        (bytes32[] memory vhs, uint256[] memory blocks) = v.verify(fx.p);
        uint256 used = g - gasleft();
        assertEq(vhs, fx.vhs);
        assertEq(blocks, fx.blocks);
        uint256 size = abi.encode(fx.p).length;
        emit log_named_uint("verify gas, 64 headers and 3 transactions", used);
        emit log_named_uint("proof calldata bytes", size);
        assertLt(size, 64_000);
        assertLt(used, 8_000_000);
    }
```

`test/TransitionL1.t.sol`: the imports become

```solidity
import {stdJson} from "forge-std/StdJson.sol";
import {RegistryTestBase} from "./RegistryTestBase.sol";
import {IProcessRegistry} from "../src/interfaces/IProcessRegistry.sol";
import {IL1BlobVerifier} from "../src/interfaces/IL1BlobVerifier.sol";
import {L1BlobVerifier} from "../src/L1BlobVerifier.sol";
import {DAVINCITypes} from "../src/libraries/DAVINCITypes.sol";
import {MockL1BlobVerifier} from "./mocks/MockL1BlobVerifier.sol";
import {MockL1Block} from "./mocks/MockL1Block.sol";
```

and append:

```solidity

/// @dev L1-blob mode with the real L1BlobVerifier: the fixture transitions' blobs in synthetic
///      Ethereum blob transactions (test/vectors/l1blob, written by `go run .`). The second
///      transition's two blobs went out in two transactions, as a node posts a transition
///      above one transaction's blob limit; the proof lists them in blob order.
contract TransitionL1TwoTxTest is RegistryTestBase {
    using stdJson for string;

    MockL1Block internal l1;

    function _l1BlobVerifier() internal override returns (address) {
        l1 = new MockL1Block();
        return address(new L1BlobVerifier(address(l1)));
    }

    /// @dev The L1 proof of fixture `name`, with OP's origin set at its top header.
    function _proof(string memory name)
        internal
        returns (IL1BlobVerifier.L1BlobProof memory p, uint256[] memory blocks)
    {
        string memory j = vm.readFile(string.concat("test/vectors/l1blob/", name, ".json"));
        p.headers = j.readBytesArray(".headers");
        uint256 nt = abi.decode(vm.parseJson(j, ".txs"), (bytes[])).length;
        p.txs = new IL1BlobVerifier.L1TxProof[](nt);
        for (uint256 i = 0; i < nt; ++i) {
            string memory b = string.concat(".txs[", vm.toString(i), "]");
            p.txs[i].headerIndex = j.readUint(string.concat(b, ".header_index"));
            p.txs[i].key = j.readBytes(string.concat(b, ".key"));
            p.txs[i].proof = j.readBytesArray(string.concat(b, ".proof"));
        }
        blocks = j.readUintArray(".expected_tx_blocks");
        l1.set(uint64(j.readUintArray(".header_numbers")[0]), j.readBytes32Array(".header_hashes")[0]);
    }

    function _submitL1(bytes31 pid, Transition memory t, IL1BlobVerifier.L1BlobProof memory p) internal {
        registry.submitStateTransitionL1(pid, t.publicValues, t.proofBytes, t.commitments, t.ys, t.kzgProofs, p);
    }

    /// @dev The first transition, from its one-transaction proof.
    function _first() internal returns (bytes31 pid) {
        pid = _fixtureProcess();
        (IL1BlobVerifier.L1BlobProof memory p,) = _proof("one_tx");
        _submitL1(pid, _transition(0), p);
    }

    function _settledTwoTx(bytes31 pid, string memory name, uint256 b0, uint256 b1) internal {
        Transition memory t = _transition(1);
        (IL1BlobVerifier.L1BlobProof memory p, uint256[] memory blocks) = _proof(name);
        assertEq(t.commitments.length, 2);
        assertEq(blocks[0], b0);
        assertEq(blocks[1], b1);
        vm.expectEmit(true, false, false, true, address(registry));
        emit IProcessRegistry.ProcessStateTransitionedL1(pid, blocks, t.versionedHashes);
        _submitL1(pid, t, p);
        DAVINCITypes.Process memory proc = registry.getProcess(pid);
        assertEq(proc.latestStateRoot, t.rootAfter);
        assertEq(proc.batchNumber, 2);
        assertEq(proc.votersCount, 1000);
    }

    function test_SubmitL1_TwoTransactionsInChunkOrder() public {
        _settledTwoTx(_first(), "two_tx", 2000, 2002);
    }

    function test_SubmitL1_RepostedFirstChunkInTheNewerBlock() public {
        _settledTwoTx(_first(), "two_tx_reposted", 2002, 2000);
    }

    /// The same two transactions listed newest blob first: the openings follow the proof order,
    /// so commitment 0 meets the second blob's hash.
    function test_RevertWhen_L1_ChunksOutOfOrder() public {
        bytes31 pid = _first();
        Transition memory t = _transition(1);
        (IL1BlobVerifier.L1BlobProof memory p,) = _proof("two_tx");
        (p.txs[0], p.txs[1]) = (p.txs[1], p.txs[0]);
        vm.expectRevert(abi.encodeWithSelector(IProcessRegistry.InvalidBlobOpening.selector, 0));
        _submitL1(pid, t, p);
    }
}
```

- [ ] **Step 3: Run them to see them fail**

Run: `forge test --match-contract "L1BlobVerifier|TransitionL1" 2>&1 | grep -E "FAIL|Suite result" | head -12`
Expected: the five new `L1BlobVerifierTest` tests and the three `TransitionL1TwoTxTest` tests `FAIL` (`vm.readFile: … test/vectors/l1blob/two_tx.json … No such file or directory`); every older test passes.

- [ ] **Step 4: The fixture generator**

Create `test/vectors/l1blob.go`:

```go
package main

import (
	"math/big"
	"os"
	"path/filepath"

	"github.com/ethereum/go-ethereum/common"
	"github.com/ethereum/go-ethereum/core/rawdb"
	"github.com/ethereum/go-ethereum/core/types"
	"github.com/ethereum/go-ethereum/crypto"
	"github.com/ethereum/go-ethereum/rlp"
	"github.com/ethereum/go-ethereum/trie"
	"github.com/ethereum/go-ethereum/triedb"
	"github.com/holiman/uint256"
)

// --- L1-blob mode: one transition in several Ethereum transactions ------------

// l1TxJSON and l1ProofJSON are the shape of test/vectors/l1blob/*.json.
type l1TxJSON struct {
	HeaderIndex int      `json:"header_index"`
	Key         string   `json:"key"`
	Proof       []string `json:"proof"`
}

type l1ProofJSON struct {
	ExpectedTxBlocks []uint64   `json:"expected_tx_blocks"`
	ExpectedVhs      []string   `json:"expected_vhs"`
	HeaderHashes     []string   `json:"header_hashes"`
	HeaderNumbers    []uint64   `json:"header_numbers"`
	Headers          []string   `json:"headers"`
	Name             string     `json:"name"`
	TxType           int        `json:"tx_type"`
	Txs              []l1TxJSON `json:"txs"`
}

// l1Post is one blob transaction of the proof: the versioned hashes it carries
// and how many blocks below the top header it sits.
type l1Post struct {
	vhs   []common.Hash
	depth int
}

// keyProof records the trie nodes Trie.Prove writes, root first.
type keyProof struct{ nodes [][]byte }

func (k *keyProof) Put(_, v []byte) error {
	k.nodes = append(k.nodes, common.CopyBytes(v))
	return nil
}

func (k *keyProof) Delete([]byte) error { return nil }

// l1Envelope is a type-3 transaction from a fixed sender to itself carrying vhs
// (unsigned: the verifier reads only the versioned hashes).
func l1Envelope(nonce uint64, vhs []common.Hash) []byte {
	to := common.HexToAddress("0x00000000000000000000000000000000000000b1")
	tx := types.NewTx(&types.BlobTx{
		ChainID:    uint256.NewInt(1),
		Nonce:      nonce,
		GasTipCap:  uint256.NewInt(1_000_000),
		GasFeeCap:  uint256.NewInt(2_000_000_000),
		Gas:        21_000,
		To:         to,
		Value:      uint256.NewInt(0),
		BlobFeeCap: uint256.NewInt(2),
		BlobHashes: vhs,
		V:          uint256.NewInt(0),
		R:          uint256.NewInt(1),
		S:          uint256.NewInt(1),
	})
	b, err := tx.MarshalBinary()
	check(err, "blob envelope")
	return b
}

// l1Block is a header over envs, with the transactions-trie proof of each one.
// It carries every field of a Prague header, so it is as long as a mainnet one.
func l1Block(parent common.Hash, number uint64, envs [][]byte) (*types.Header, [][][]byte) {
	tr := trie.NewEmpty(triedb.NewDatabase(rawdb.NewMemoryDatabase(), nil))
	for i, e := range envs {
		k, err := rlp.EncodeToBytes(uint64(i))
		check(err, "tx key")
		check(tr.Update(k, e), "trie update")
	}
	proofs := make([][][]byte, len(envs))
	for i := range envs {
		k, _ := rlp.EncodeToBytes(uint64(i))
		var p keyProof
		check(tr.Prove(k, &p), "trie proof")
		proofs[i] = p.nodes
	}
	withdrawals, requests := types.EmptyWithdrawalsHash, types.EmptyRequestsHash
	beacon := crypto.Keccak256Hash([]byte("beacon"), new(big.Int).SetUint64(number).Bytes())
	blobGas, excess := uint64(131_072*len(envs)), uint64(0)
	h := &types.Header{
		ParentHash:       parent,
		UncleHash:        types.EmptyUncleHash,
		Coinbase:         common.HexToAddress("0x00000000000000000000000000000000000000c0"),
		Root:             crypto.Keccak256Hash([]byte("state"), new(big.Int).SetUint64(number).Bytes()),
		TxHash:           tr.Hash(),
		ReceiptHash:      crypto.Keccak256Hash([]byte("receipts"), new(big.Int).SetUint64(number).Bytes()),
		Difficulty:       big.NewInt(0),
		Number:           new(big.Int).SetUint64(number),
		GasLimit:         45_000_000,
		GasUsed:          21_000 * uint64(len(envs)),
		Time:             1_760_000_000 + 12*number,
		Extra:            []byte("davinci l1blob vector"),
		BaseFee:          big.NewInt(1_000_000_000),
		WithdrawalsHash:  &withdrawals,
		BlobGasUsed:      &blobGas,
		ExcessBlobGas:    &excess,
		ParentBeaconRoot: &beacon,
		RequestsHash:     &requests,
	}
	return h, proofs
}

// l1ProofFixture builds `top - bottom + 1` chained headers, newest first, with
// each post's transaction (after one filler blob transaction in its block) at
// its depth, and the proof listing the posts in the given order.
func l1ProofFixture(name string, bottom, top uint64, posts []l1Post) l1ProofJSON {
	type placed struct{ depth, index int }
	blocks := make(map[uint64][][]byte)
	where := make([]placed, len(posts))
	for i, p := range posts {
		n := top - uint64(p.depth)
		if blocks[n] == nil {
			// A filler, so the trie has a branch and the key is not 0x80.
			blocks[n] = [][]byte{l1Envelope(100+n, []common.Hash{{0x01, 0xf1}})}
		}
		where[i] = placed{p.depth, len(blocks[n])}
		blocks[n] = append(blocks[n], l1Envelope(uint64(i), p.vhs))
	}
	var (
		parent  common.Hash
		headers = make([]*types.Header, 0, top-bottom+1)
		tproofs = make(map[uint64][][][]byte)
	)
	for n := bottom; n <= top; n++ {
		h, proofs := l1Block(parent, n, blocks[n])
		headers = append(headers, h)
		tproofs[n] = proofs
		parent = h.Hash()
	}
	out := l1ProofJSON{Name: name, TxType: 3}
	for i := len(headers) - 1; i >= 0; i-- {
		h := headers[i]
		enc, err := rlp.EncodeToBytes(h)
		check(err, "header rlp")
		if crypto.Keccak256Hash(enc) != h.Hash() {
			panic("header hash")
		}
		out.Headers = append(out.Headers, hex0x(enc))
		out.HeaderHashes = append(out.HeaderHashes, h.Hash().Hex())
		out.HeaderNumbers = append(out.HeaderNumbers, h.Number.Uint64())
	}
	for i, p := range posts {
		n := top - uint64(where[i].depth)
		k, _ := rlp.EncodeToBytes(uint64(where[i].index))
		tx := l1TxJSON{HeaderIndex: where[i].depth, Key: hex0x(k)}
		for _, node := range tproofs[n][where[i].index] {
			tx.Proof = append(tx.Proof, hex0x(node))
		}
		out.Txs = append(out.Txs, tx)
		for _, vh := range p.vhs {
			out.ExpectedVhs = append(out.ExpectedVhs, vh.Hex())
			out.ExpectedTxBlocks = append(out.ExpectedTxBlocks, n)
		}
	}
	return out
}

// syntheticVhs are n versioned hashes (0x01 || keccak(tag, i)[1:]).
func syntheticVhs(tag string, n int) []common.Hash {
	out := make([]common.Hash, n)
	for i := range out {
		out[i] = crypto.Keccak256Hash([]byte(tag), []byte{byte(i)})
		out[i][0] = 0x01
	}
	return out
}

// l1BlobFixtures writes l1blob/two_tx.json and two_tx_reposted.json: the
// fixture's second transition (two blobs) posted in two transactions, two
// blocks apart, under four headers. In two_tx the first blob's transaction is
// the older; in two_tx_reposted it was posted again and is the newer, while
// the proof still lists it first. one_tx.json proves the first transition's
// blob, so a registry can settle both in order. worst_case.json is the
// largest proof a node sends, for the size and gas bound.
func l1BlobFixtures(dir string, fx fixtureFile) {
	vhs := func(t int) []common.Hash {
		var out []common.Hash
		for _, s := range fx.Transitions[t].VersionedHashes {
			out = append(out, common.HexToHash(s))
		}
		return out
	}
	t0, t1 := vhs(0), vhs(1)
	if len(t0) != 1 || len(t1) != 2 {
		panic("the fixture transitions are no longer one and two blobs")
	}
	dir = filepath.Join(dir, "l1blob")
	check(os.MkdirAll(dir, 0o755), "create l1blob directory")
	writeJSON(dir, "one_tx.json", l1ProofFixture("one_tx", 1000, 1002,
		[]l1Post{{vhs: t0, depth: 2}}))
	writeJSON(dir, "two_tx.json", l1ProofFixture("two_tx", 2000, 2003,
		[]l1Post{{vhs: t1[:1], depth: 3}, {vhs: t1[1:], depth: 1}}))
	writeJSON(dir, "two_tx_reposted.json", l1ProofFixture("two_tx_reposted", 2000, 2003,
		[]l1Post{{vhs: t1[:1], depth: 1}, {vhs: t1[1:], depth: 3}}))
	// The largest proof a node sends: 64 headers, three transactions of a
	// 13-blob transition (6 + 6 + 1), the oldest at the bottom.
	vh := syntheticVhs("worst case", 13)
	writeJSON(dir, "worst_case.json", l1ProofFixture("worst_case", 3000, 3063,
		[]l1Post{{vhs: vh[:6], depth: 63}, {vhs: vh[6:12], depth: 40}, {vhs: vh[12:], depth: 2}}))
}
```

`test/vectors/main.go`, the file doc after the `publics.json` line:

```go
//	l1blob/one_tx.json, l1blob/two_tx.json, l1blob/two_tx_reposted.json,
//	l1blob/worst_case.json
//	                 synthetic Ethereum headers and blob transactions carrying the
//	                 transitions' versioned hashes, for the L1-blob mode tests
```

and after `writeJSON(*out, "transition.json", fx)`:

```go
	l1BlobFixtures(*out, fx)
```

- [ ] **Step 5: Generate, twice**

```bash
cd /home/p4u/davinci-contracts/test/vectors
GOTOOLCHAIN=local go mod tidy && gofmt -l . && GOTOOLCHAIN=local go vet . && GOTOOLCHAIN=local go run . 2>&1 | tail -1
cd ../.. && git status --short test/vectors
sha256sum test/vectors/l1blob/one_tx.json test/vectors/l1blob/two_tx.json test/vectors/l1blob/two_tx_reposted.json test/vectors/l1blob/worst_case.json
(cd test/vectors && GOTOOLCHAIN=local go run . >/dev/null 2>&1) && git status --short test/vectors | wc -l
```

Expected: `go.mod` moves `github.com/holiman/uint256 v1.3.2` into the direct block and adds the trie's indirect modules (`VictoriaMetrics/fastcache`, `cespare/xxhash/v2`, `go-ole/go-ole`, `gofrs/flock`, `golang/snappy`, `shirou/gopsutil`, `tklauser/go-sysconf`, `tklauser/numcpus`, `yusufpapurcu/wmi`); no file from `gofmt -l`; `rust-sdk genesis vector: 6 cases agree with go-sdk chain.NewState`; status `M test/vectors/go.mod`, `M test/vectors/go.sum`, `M test/vectors/main.go`, `?? test/vectors/l1blob.go`, `?? test/vectors/l1blob/one_tx.json`, `two_tx.json`, `two_tx_reposted.json`, `worst_case.json` (no other vector moves); checksums

```
7ff22790f6dbf8a2de0c1a20c9e27c49e07634256ef35e92270e2c60230b4826  test/vectors/l1blob/one_tx.json
56e097c7ad8dd5a5b4f3f38f434ed620f3de16eafa00d5358b97980dc51f4aa9  test/vectors/l1blob/two_tx.json
ed21d0bb11d64bb30e7e5dfccdcb41bc4b151491fd0ce9fdd943032b9ce6e640  test/vectors/l1blob/two_tx_reposted.json
2ef6fb38c7d5c9fcc3da483e3633eb0d1ed1a76bef691ed42cdf778bbafab587  test/vectors/l1blob/worst_case.json
```

and the second run changes nothing (CI's drift check).

- [ ] **Step 6: Run the forge tests**

```bash
forge test --match-contract "L1BlobVerifier|TransitionL1" -vv 2>&1 | grep -E "FAIL|TwoTx|WorstCase|verify gas|calldata bytes|Suite result"
forge fmt --check test/L1BlobVerifier.t.sol test/TransitionL1.t.sol && echo fmt-ok
forge test 2>&1 | grep -E "FAIL|Ran .* test suites"
```

Expected: every new test `[PASS]`; `verify gas, 64 headers and 3 transactions: 3491169`, `proof calldata bytes: 47168`; `fmt-ok`; `Ran 27 test suites …: 370 tests passed, 0 failed, 0 skipped (370 total tests)`.

- [ ] **Step 7: Commit**

```bash
git add test/L1BlobVerifier.t.sol test/TransitionL1.t.sol test/vectors/go.mod test/vectors/go.sum \
  test/vectors/main.go test/vectors/l1blob.go test/vectors/l1blob/one_tx.json test/vectors/l1blob/two_tx.json \
  test/vectors/l1blob/two_tx_reposted.json test/vectors/l1blob/worst_case.json
git commit -m "test(l1blob): one transition settled from two ethereum transactions" -m "Synthetic Ethereum headers and blob transactions from test/vectors carry the
fixture transitions' versioned hashes: L1BlobVerifier.verify returns them in
proof order across two blocks, also when the first transaction was posted
again into the newer block, and a registry with the real verifier settles the
two-blob transition from two transactions. A worst-case proof (64 headers,
three transactions, 13 blobs) is 47 KB and 3.5 M gas. No source change."
```

---

### Task 8: davinci-contracts: the grace check, defaults on the v0.2.0 vks, regenerated bindings

**Files:**
- Modify: `script/verify_deployment.py` (doc, `--grace`, the grace loop), `deploy_all.sh:97-99` (`EXPECT_GRACE`)
- Modify: `docker-compose.yml:63-64`, `docs/deployment.md`, `docs/verification.md`, `.env.example`
- Regenerate: `artifacts/`, `typechain-types/`, `golang-types/ProcessRegistry.go`

**Interfaces:**
- Produces: `verify_deployment.py --grace DEFAULT,FLOOR,CEIL,MAX_TOTAL,NOTICE` (one `OK`/`FAIL registry.<name> == --grace <v> s` line per value; a malformed value exits 2 with `--grace takes five non-negative integers: …`); `deploy_all.sh` forwards `EXPECT_GRACE`; Go bindings with `SubmitStateTransitionL1`, `L1BlobVerifier`, `ProcessStateTransitionedL1`.

- [ ] **Step 1: `--grace` in `script/verify_deployment.py`**

Module doc, the paragraph and usage (lines 4-13):

```python
Compares the on-chain runtime code of ZiskVerifier, ProcessRegistry and both adapters
with `forge build` output (immutable slots masked), then reads every immutable back and
compares it with the expected pins, verifier and managers. The grace and notice settings
are compared with --pins-from and --grace, and printed otherwise. Exit status 1 on any
mismatch. --record writes what was verified as JSON, only when every check passes.

    python3 script/verify_deployment.py --rpc URL --registry 0x... \
        --batch-vk 0x... --results-vk 0x... --root-c 0x... --ballot-vk-hash 0x... \
        [--grace 300,240,600,1800,60]
```

After the `--l1-block` argument:

```python
    ap.add_argument(
        "--grace",
        metavar="DEFAULT,FLOOR,CEIL,MAX_TOTAL,NOTICE",
        help="expected defaultGrace, graceFloor, graceCeil, graceMaxTotal and noticeMin in seconds "
        "(DeployAll keeps its default for a GRACE_* variable that is unset or misspelled)",
    )
```

After `a = ap.parse_args()`:

```python
    expect_grace = None
    if a.grace is not None:
        try:
            values = [int(x) for x in a.grace.split(",")]
        except ValueError:
            values = []
        if len(values) != len(GRACE) or min(values) < 0:
            ap.error("--grace takes five non-negative integers: DEFAULT,FLOOR,CEIL,MAX_TOTAL,NOTICE")
        expect_grace = dict(zip(GRACE, values))
```

In the grace loop, the `if want_grace is not None: … else: print(…)` block becomes:

```python
        if want_grace is not None:
            check(f"registry.{name} == {a.pins_from}.{name}", grace[name] == want_grace[name], f"{grace[name]} s")
        if expect_grace is not None:
            check(f"registry.{name} == --grace", grace[name] == expect_grace[name], f"{grace[name]} s")
        if want_grace is None and expect_grace is None:
            print(f"{name:13} {'n/a' if grace[name] is None else str(grace[name]) + ' s'}")
```

`deploy_all.sh`, after the `--dkg-manager … --council-manager …` line:

```bash
# EXPECT_GRACE (DEFAULT,FLOOR,CEIL,MAX_TOTAL,NOTICE in seconds): the grace settings the registry
# must read back. DeployAll keeps its default for a GRACE_* variable that is unset or misspelled.
if [ -n "${EXPECT_GRACE:-}" ]; then
    VERIFY_DEPLOYMENT_ARGS+=(--grace "$EXPECT_GRACE")
fi
```

- [ ] **Step 2: Check it against a local L1-blob deployment with the production grace**

A throwaway anvil on port 18545 with anvil's public dev key 0 (no real key, nothing kept: `broadcast/*/31337/` and `deployments/31337.json` are gitignored):

```bash
cd /home/p4u/davinci-contracts
anvil --port 18545 --chain-id 31337 --hardfork osaka > $TMPDIR/anvil-18545.log 2>&1 &
ANVIL_PID=$!; sleep 2
K=0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80
MOCK=$(forge create test/mocks/MockL1Block.sol:MockL1Block --rpc-url http://127.0.0.1:18545 --private-key $K --broadcast 2>&1 | awk '/Deployed to:/{print $3}')
( export CHAIN_ID=31337 RPC_URL=http://127.0.0.1:18545 PRIVATE_KEY=$K L1_BLOCK=$MOCK \
    BATCH_PROGRAM_VK=0xa0ad5c4c7e39d4a7007933158181418a752e6c1563a211af84c579640db435fa \
    RESULTS_PROGRAM_VK=0xe693589453ecaee63eba697c48d4c5321a714bb18c2d84a97437b3f368578f68 \
    ROOT_C_VADCOP_FINAL=0x05006517b6ccde5da4d890587ba62845b5af8a307c00e87d4b9d05099b16dc80 \
    BALLOT_VK_HASH=0xbf1e6590bb1ba883d601c4d7d1c6fa2722a78590716874019db6d68fc776bb0e \
    GRACE_DEFAULT=300 GRACE_FLOOR=240 GRACE_CEIL=600 GRACE_MAX_TOTAL=1800 NOTICE_MIN=60 \
    EXPECT_GRACE=300,240,600,1800,60
  ./deploy_all.sh ) > $TMPDIR/deploy-local.log 2>&1; echo "deploy_all exit $?"
grep -E "^  Grace|^OK|^FAIL|^record|^l1 blob" $TMPDIR/deploy-local.log
python3 script/verify_deployment.py --rpc http://127.0.0.1:18545 --chain-id 31337 \
  --registry 0xCf7Ed3AccA5a467e9e704C703E8D87F634fB0Fc9 \
  --batch-vk 0xa0ad5c4c7e39d4a7007933158181418a752e6c1563a211af84c579640db435fa \
  --results-vk 0xe693589453ecaee63eba697c48d4c5321a714bb18c2d84a97437b3f368578f68 \
  --root-c 0x05006517b6ccde5da4d890587ba62845b5af8a307c00e87d4b9d05099b16dc80 \
  --ballot-vk-hash 0xbf1e6590bb1ba883d601c4d7d1c6fa2722a78590716874019db6d68fc776bb0e \
  --dkg-manager 0x0 --council-manager 0x0 --l1-block "$MOCK" --grace 180,150,600,1800,60 > $TMPDIR/wrong-grace.txt; echo "exit $?"
grep -E "grace" $TMPDIR/wrong-grace.txt
python3 script/verify_deployment.py --rpc http://127.0.0.1:18545 --registry 0x00 --batch-vk 0x0 --results-vk 0x0 \
  --root-c 0x0 --ballot-vk-hash 0x0 --grace 300,240 2>&1 | tail -1
kill $ANVIL_PID; git status --short
```

Expected: `deploy_all exit 0`; `Grace (s): default 300, floor 240, ceil 600, max total 1800; notice min 60`; every line `OK` (runtime code of `ProcessRegistry`, `ZiskVerifier` and `L1BlobVerifier`, the four pins, `registry.chainID`, both managers disabled, `l1BlobVerifier.l1Block == --l1-block 0x5fbdb2315678afecb367f032d93f642f64180aa3`, and `registry.defaultGrace == --grace 300 s` … `registry.noticeMin == --grace 60 s`), `record   deployments/31337.json`; then with the wrong `--grace`: `exit 1`, and among the grace lines `FAIL registry.defaultGrace == --grace 300 s`, `FAIL registry.graceFloor == --grace 240 s` and three `OK`; then `verify_deployment.py: error: --grace takes five non-negative integers: DEFAULT,FLOOR,CEIL,MAX_TOTAL,NOTICE`; and only `deploy_all.sh` and `script/verify_deployment.py` modified (`git status`).

- [ ] **Step 3: Defaults and docs on the v0.2.0 vks**

Save as `~/.cache/op-core/contracts_docs.py` and run it from `/home/p4u/davinci-contracts`:

```python
"""davinci-contracts: deploy defaults and docs on the v0.2.0 vks, and the --grace check.
Run from the davinci-contracts root: python3 -I contracts_docs.py"""


def edit(p, pairs):
    s = open(p).read()
    for old, new in pairs:
        assert s.count(old) >= 1, (p, old[:70])
        s = s.replace(old, new)
    open(p, "w").write(s)


V1_BATCH = "0x6cfc89d562d0b22f04478a5c15b390433eb52f1b03147030b183076260da7a10"
V1_RESULTS = "0x7bc8c5e9235548386a44b1885732a2a7ffb1badddc8c7fba599d07ece47be794"
V2_BATCH = "0xa0ad5c4c7e39d4a7007933158181418a752e6c1563a211af84c579640db435fa"
V2_RESULTS = "0xe693589453ecaee63eba697c48d4c5321a714bb18c2d84a97437b3f368578f68"

edit("docker-compose.yml", [
    (f'BATCH_PROGRAM_VK: "{V1_BATCH}"', f'BATCH_PROGRAM_VK: "{V2_BATCH}"'),
    (f'RESULTS_PROGRAM_VK: "{V1_RESULTS}"', f'RESULTS_PROGRAM_VK: "{V2_RESULTS}"'),
])

edit("docs/deployment.md", [
    (f"""export PRIVATE_KEY=0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80 CHAIN_ID=31337 \\
  BATCH_PROGRAM_VK={V1_BATCH} \\
  RESULTS_PROGRAM_VK={V1_RESULTS} \\""",
     f"""export PRIVATE_KEY=0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80 CHAIN_ID=31337 \\
  BATCH_PROGRAM_VK={V2_BATCH} \\
  RESULTS_PROGRAM_VK={V2_RESULTS} \\"""),
    ("""settings, commit, deployer, block and transaction hashes. A failed check stops the script before
it regenerates `golang-types/addresses.go` from the broadcast logs and leaves no record. It needs
`jq` and `python3` besides Foundry.""",
     """settings, commit, deployer, block and transaction hashes. A failed check stops the script before
it regenerates `golang-types/addresses.go` from the broadcast logs and leaves no record. It needs
`jq` and `python3` besides Foundry. `DeployAll` keeps its default for a grace variable that is
unset or misspelled, so set `EXPECT_GRACE` (`DEFAULT,FLOOR,CEIL,MAX_TOTAL,NOTICE`, for example
`300,240,600,1800,60`) as well: the script then also checks the deployed grace settings against
it, and writes no record on a difference."""),
])

edit("docs/verification.md", [
    ("""It prints the grace and notice settings (`defaultGrace`, `graceFloor`, `graceCeil`,
`graceMaxTotal`, `noticeMin`) without judging them, except with `--pins-from <registry>`: then
the pins, the verifier and the grace settings are expected to equal that registry's (a
deployment made with `PINS_FROM_REGISTRY`), and any pin flag also given must agree with it. Each""",
     """It prints the grace and notice settings (`defaultGrace`, `graceFloor`, `graceCeil`,
`graceMaxTotal`, `noticeMin`) without judging them, except with `--grace
DEFAULT,FLOOR,CEIL,MAX_TOTAL,NOTICE` (seconds), which expects those values, or with
`--pins-from <registry>`: then the pins, the verifier and the grace settings are expected to
equal that registry's (a deployment made with `PINS_FROM_REGISTRY`), and any pin flag also given
must agree with it. In L1-blob mode it compares the `L1BlobVerifier`'s code with the local build
and its `l1Block()` with `--l1-block` (default the OP predeploy). Each"""),
])

edit(".env.example", [
    ("""# ProcessRegistry grace window and shorten notice, in seconds (unset keeps these defaults).
# GRACE_DEFAULT=180
# GRACE_FLOOR=150
# GRACE_CEIL=600
# GRACE_MAX_TOTAL=1800
# NOTICE_MIN=60""",
     """# ProcessRegistry grace window and shorten notice, in seconds (unset keeps these defaults).
# GRACE_DEFAULT=180
# GRACE_FLOOR=150
# GRACE_CEIL=600
# GRACE_MAX_TOTAL=1800
# NOTICE_MIN=60
# The values deploy_all.sh must read back (DEFAULT,FLOOR,CEIL,MAX_TOTAL,NOTICE): a misspelled
# variable above silently keeps its default.
# EXPECT_GRACE=180,150,600,1800,60"""),
])
print("contracts docs: done")
```

```bash
python3 -I ~/.cache/op-core/contracts_docs.py
grep -rn "6cfc89d5\|7bc8c5e9" docker-compose.yml docs/deployment.md docs/verification.md README.md | cut -c1-90
```

Expected: `contracts docs: done`; the v0.1 vks remain only where they describe the Gnosis registries and the first OP registry (`docs/deployment.md` "OP Mainnet" section, rewritten in Task 11; `docs/verification.md`'s two Gnosis commands; the README pins table of the Gnosis registries).

- [ ] **Step 4: Commit the tooling**

```bash
git add script/verify_deployment.py deploy_all.sh docker-compose.yml docs/deployment.md docs/verification.md .env.example
git commit -m "feat(deploy): check the deployed grace window, defaults on the v0.2.0 vks" -m "verify_deployment.py --grace (EXPECT_GRACE in deploy_all.sh) checks the five
grace settings: DeployAll keeps its default for a misspelled variable. The
local compose deployment and the docs pin the davinci-zkvm v0.2.0 program
vks."
```

- [ ] **Step 5: Regenerate the bindings with the abigen CI uses**

`bindings-sync.yml` and `release-tag.yml` run `go install github.com/ethereum/go-ethereum/cmd/abigen@latest` (v1.17.8 on 2026-10-13; v1.16.5 output differs). Install that version outside the repositories and check the Node toolchain (the checkout's `node_modules` holds hardhat; nothing is installed globally; if `node_modules` is missing, `npm ci` in the checkout installs the locked versions):

```bash
GOBIN=$HOME/.cache/gobin-latest go install github.com/ethereum/go-ethereum/cmd/abigen@v1.17.8
$HOME/.cache/gobin-latest/abigen --version
gh run list -R vocdoni/davinci-contracts --workflow bindings-sync.yml -L 1 --json databaseId --jq '.[0].databaseId' \
  | xargs -I{} gh run view {} -R vocdoni/davinci-contracts --log 2>/dev/null | grep -m1 "go: downloading github.com/ethereum/go-ethereum v"
node --version && ls node_modules/.bin/hardhat
cd /home/p4u/davinci-contracts && PATH=$HOME/.cache/gobin-latest:$PATH ./build_all.sh > $TMPDIR/build_all.log 2>&1; echo "build_all exit $?"
git status --short | grep -v "^?? node_modules" | awk '{print $2}' | cut -d/ -f1 | sort | uniq -c
grep -c "SubmitStateTransitionL1\|ProcessStateTransitionedL1\|L1BlobVerifier" golang-types/ProcessRegistry.go
forge test 2>&1 | grep -E "FAIL|Ran .* test suites"
```

Expected: `abigen version 1.17.8-stable`; the last CI run downloaded `go-ethereum v1.17.8` (if a newer version shows, install that one instead and say so in the PR); `v22…` and the hardhat link; `build_all exit 0`; changes under `artifacts`, `golang-types` (`ProcessRegistry.go` only) and `typechain-types` (new `L1BlobVerifier` and `IL1BlobVerifier` factories); a count above 40; `370 tests passed, 0 failed`. `TMPDIR` keeps the helper's `mktemp` files out of `/tmp`.

- [ ] **Step 6: Commit the bindings**

```bash
git add artifacts typechain-types golang-types
git status --short | grep -v "^?? node_modules" | wc -l     # expect 0
git commit -m "chore(bindings): regenerate for the l1-blob mode" -m "build_all.sh with abigen v1.17.8: the bindings carry
submitStateTransitionL1, l1BlobVerifier() and ProcessStateTransitionedL1."
```

---

### Task 9: GPU validation on two anvils: the L1-blob scenario, blob mode, and one 512 x 16 transition

**Files:** none (a failing run leads to a `fix(<scope>): ...` commit with its regression test, a new binary and the run again).

**Interfaces:**
- Consumes: Tasks 1-6 (the node, `e2e/tests/l1_big.rs`); the shared prover at `DAVINCI_ZKVM_URL` (default `http://127.0.0.1:8080`, davinci-zkvm v0.2.0 since 2026-10-13 12:45 UTC).
- Produces: the binary `~/.cache/davinci-l1blob/bin/davinci-sequencer-<commit>`; the logs `~/.cache/davinci-e2e/{e2e-l1.log,e2e-blobs.log,l1-big.log}`; the fixture `~/.cache/op-core/fixture/{snark_batch.json,publics_batch.bin}` and its `recorded job` line in `l1-big.log` (Task 10).

Nothing here spends: both chains are anvils. The three runs use the shared production prover one after another, never together.

- [ ] **Step 1: Build the node binary of this commit**

```bash
cd /home/p4u/davinci-sequencer
git branch --show-current && git status --short | wc -l
export DAVINCI_SEQUENCER_BIN=$HOME/.cache/davinci-l1blob/bin/davinci-sequencer-$(git rev-parse --short HEAD)
cargo build --release -p davinci-sequencer --bin davinci-sequencer 2>&1 | tail -1 && cp target/release/davinci-sequencer "$DAVINCI_SEQUENCER_BIN"
"$DAVINCI_SEQUENCER_BIN" --version
```

Expected: `feat/op-core`, `0`, `Finished `release` profile [optimized] target(s) in ...`, `davinci-sequencer 0.5.0` (the version moves in Task 13). Every later run of this task uses this `DAVINCI_SEQUENCER_BIN`, so a tree change during a run cannot swap the binary.

- [ ] **Step 2: Go/no-go on the shared prover (before each of Steps 3, 4 and 5)**

```bash
curl -s "${DAVINCI_ZKVM_URL:-http://127.0.0.1:8080}/health"; echo
docker ps -a --filter name=davinci-sequencer --format '{{.Names}} {{.Image}} {{.Status}}'
```

Go: `"status":"ok"` and `"queue_len":0`, and `davinci-sequencer-1 ghcr.io/vocdoni/davinci-sequencer:latest Exited (0) ...` (the Gnosis node has been stopped since 2026-10-13 13:27 UTC; its v0.1 pins cannot settle v0.2.0 proofs). No-go if the queue is not empty or a production node runs: wait, and ask the owner. Before each of Steps 3, 4 and 5 (each runs for up to an hour on the production GPU) the controller tells the owner that the run is starting and for how long; no confirmation is needed while the queue is empty; never stop or start a container from this plan before Task 13.

- [ ] **Step 3: The L1-blob scenario (about 60 minutes)**

```bash
cd /home/p4u/davinci-sequencer
DAVINCI_E2E_DA=l1-blobs DAVINCI_E2E_NEGATIVE=1 TMPDIR=$HOME/.cache/davinci-e2e \
  make e2e E2E_LOG=$HOME/.cache/davinci-e2e/e2e-l1.log
grep -E "L1-blob|node-b (killed|replaced)|restart with a pending post|negative checks|  L1 |blob-mode entry|PASSED|FAILED|test result" ~/.cache/davinci-e2e/e2e-l1.log
```

Expected (excerpt):
```
L1-blob mode: Ethereum anvil http://127.0.0.1:<port> (chain 1, 2 s blocks), OP origin 6 blocks behind
L1-blob mode: L1BlobVerifier 0x9fE46736679d2D9a65F0992F2272dE9f3c7fa6e0 on L1Block 0xe7f1725E7734CE288F8367e1Bb143E90bb3F0512
[e2e ...] node-b killed with 0x... pending at L1 nonce <n>
[e2e ...] node-b replaced 0x... with 0x... at L1 nonce <n>, every fee doubled
[e2e ...] 0x...: restart with a pending post ok; OP origin at Ethereum block <m>, 0 failed relay steps
negative checks on 0x... (transition 0x... at <N>, results 0x... at <R>):
  L1 transition control at N-1       ok                           PASS
  L1 replay at N                     reverted InvalidStateRoot    PASS
  blob-mode entry point              reverted WrongDAMode         PASS
  L1 proof byte flipped at N-1       reverted InvalidProof        PASS
  L1 header chain broken at N-1      reverted BrokenHeaderChain   PASS
  L1 tx proof of another trie at N-1 reverted MerkleTrie: invalid root hash PASS
  L1 commitment swapped at N-1       reverted InvalidBlobsDigest  PASS
  L1 extra blob in the arrays at N-1 reverted BlobCountMismatch   PASS
[e2e ...] PASSED in <~60> min
test e2e ... ok
```
and the gas bill with `L1 blobs 0x...` lines. The scenario's batches are at most 8 votes, so every transition is one chunk here; Step 5 covers several. If the run fails (`FAILED: logs and datadirs kept in <dir>`): keep the work directory it prints, read the node logs with superpowers:systematic-debugging, fix the node in a `fix(l1da): ...` (or the harness in a `fix(e2e): ...`) commit with a regression test, rebuild (Step 1) and run again. A grace-phase failure that is pure timing is rerun alone with `DAVINCI_E2E_GRACE_ONLY=1 DAVINCI_E2E_L1_LAG=2` and reported.

- [ ] **Step 4: Blob mode unchanged (about 45 minutes)**

Go/no-go: Step 2 again.

```bash
DAVINCI_E2E_NEGATIVE=1 TMPDIR=$HOME/.cache/davinci-e2e \
  make e2e E2E_LOG=$HOME/.cache/davinci-e2e/e2e-blobs.log
grep -E "registry 0x|negative checks|PASSED|FAILED|test result" ~/.cache/davinci-e2e/e2e-blobs.log
```

Expected: `chain 31337 registry 0xe7f1725E7734CE288F8367e1Bb143E90bb3F0512 ...`, the blob-mode negative checks all `PASS`, `PASSED in <~45> min`, `test e2e ... ok`. The per-batch cap in blob mode is the per-transaction cap, so nothing in the seal or selection path may differ from v0.5.0's behaviour here.

- [ ] **Step 5: One 512 x 16 transition on a production-grace registry, recording the fixture (about 30 minutes)**

Go/no-go: Step 2 again.

```bash
mkdir -p $HOME/.cache/op-core/fixture
DAVINCI_E2E_L1_BIG=1 DAVINCI_E2E_DA=l1-blobs DAVINCI_E2E_GRACE=300,240,600,1800,60 \
  DAVINCI_E2E_RECORD_DIR=$HOME/.cache/op-core/fixture TMPDIR=$HOME/.cache/davinci-e2e \
  systemd-run --user --scope -p MemoryMax=16G -- \
  cargo test -p davinci-e2e --test l1_big -- --nocapture > ~/.cache/davinci-e2e/l1-big.log 2>&1
grep -E "pins ok|L1-blob mode|node at|s grace|ballots proved|transitions|phase 1|recorded job|after the end|nine blobs|results on-chain|PASSED|FAILED|test result" ~/.cache/davinci-e2e/l1-big.log
```

Expected:
```
chain 31337 registry 0xCf7Ed3AccA5a467e9e704C703E8D87F634fB0Fc9 verifier 0x5FbDB2315678afecb367f032d93F642f64180aa3 (pins ok)
L1-blob mode: Ethereum anvil http://127.0.0.1:<port> (chain 1, 2 s blocks), OP origin 6 blocks behind
L1-blob mode: L1BlobVerifier 0x... on L1Block 0x...
node at http://127.0.0.1:<port> log <work dir>/node-big.log
process 0x... with a 600 s grace
1024 ballots proved in <n> s
first 512 votes settled in <k> transitions
phase 1: <p> s per vote from submit to settled (<t> s for 512, including the Ethereum legs)
recorded job <id> in /home/p4u/.cache/op-core/fixture: voters <v> overwrites <w> n_blobs <b> occupied_before <o> nproofs <p> n_public <q> log_n <l>
512 x 16 settled <s> s after the end
nine blobs in Ethereum blocks [<a>, <b>] (2 transactions), OP tx 0x...
results on-chain = tally
PASSED
test l1_big ... ok
```
with `<s>` under 600 (the process's grace) and `<k>` at least 2 (a fresh node's estimate, 0.8 s a vote, keeps the first batches within the 180 s open-window budget). The test itself asserts what the review needs: one settlement after the end, nine versioned hashes from two Ethereum transactions (six and three blobs at the per-transaction cap of 6), the last seal `votes=512 ... reason=flush`, 1024 voters on-chain and the tally. The work directory, with the node log, is deleted on success and kept on failure; a failure is debugged as in Step 3. Keep `~/.cache/davinci-e2e/l1-big.log` and the fixture directory for Task 10 and the report.

- [ ] **Step 6: Record the results**

No commit (nothing changed unless a fix was needed). Note for the final report: the three run times, `<k>`, `<p>` (the measured seconds per vote: a figure under 0.5 s a vote at 16 fields says a warmed node fits 512 x 16 in the 300 s default grace; the benchmarks sub-project decides whether the default needs raising), `<s>` and the `recorded job` line.

---

### Task 10: davinci-zkvm: the batch wire fixture from a v0.2.0 proof

**Files (davinci-zkvm `main`):**
- Replace: `rust-sdk/testdata/snark_batch.json`, `rust-sdk/testdata/publics_batch.bin`
- Modify: `rust-sdk/tests/wire.rs:189-192` (the recorded job's publics), `:207-211` (restore the `BATCH_PROGRAM_VK` assertion)

**Interfaces:**
- Consumes: Task 9 Step 5's fixture and its `recorded job` line.
- Produces: one test-only commit on davinci-zkvm `main` (pushed in Task 13 after the owner confirms; no release, no tag: a push to `main` publishes only the `:main` prover image, never `:latest`).

- [ ] **Step 1: Preflight**

```bash
cd /home/p4u/davinci-zkvm
git branch --show-current
git status --short
# origin is an SSH URL, which this host cannot use: fetch over HTTPS into the same ref.
git fetch https://github.com/vocdoni/davinci-zkvm.git +refs/heads/main:refs/remotes/origin/main
git rev-parse --short HEAD
git rev-parse --short origin/main
```

Expected: `main`; exactly `?? SILENT-REVOTING.md`, `?? paper.tex`, `?? token.overleaf` (never read, staged or touched: every `git add` below names its files); `439ab65` twice (tag `v0.2.0`). If `origin/main` moved, `git merge --ff-only origin/main` first and read the new commits.

- [ ] **Step 2: Check the fixture before using it**

```bash
python3 -I - $HOME/.cache/op-core/fixture <<'EOF'
import json, pathlib, sys
d = pathlib.Path(sys.argv[1])
s = json.load(open(d / "snark_batch.json"))
print(list(s))
print(s["program_vk"], s["root_c_vadcop_final"])
print(len(bytes.fromhex(s["public_values"][2:])), (d / "publics_batch.bin").stat().st_size)
EOF
```

Expected: `['program_vk', 'root_c_vadcop_final', 'public_values', 'proof_bytes']`, `0xa0ad5c4c7e39d4a7007933158181418a752e6c1563a211af84c579640db435fa 0x05006517b6ccde5da4d890587ba62845b5af8a307c00e87d4b9d05099b16dc80`, `512 256`. No-go on another program vk (the prover was not v0.2.0): stop and report.

- [ ] **Step 3: Copy the fixture and point the wire test at it**

Save as `~/.cache/op-core/wire_fixture.py`:

```python
"""davinci-zkvm: point the batch wire test at the recorded v0.2.0 job.
python3 -I wire_fixture.py <l1_big log> <rust-sdk/tests/wire.rs>"""
import re
import sys

log, wire = sys.argv[1], sys.argv[2]
m = None
for line in open(log, errors="replace"):
    m = re.search(
        r"recorded job \S+ in \S+: voters (\d+) overwrites (\d+) n_blobs (\d+) "
        r"occupied_before (\d+) nproofs (\d+) n_public (\d+) log_n (\d+)", line) or m
assert m, "no 'recorded job' line in the log"
v = m.groups()
s = open(wire).read()
pairs = [
    ("""        (p.voters, p.overwrites, p.n_blobs, p.occupied_before),
        (3, 3, 1, 24)
    );
    assert_eq!((p.nproofs, p.n_public, p.log_n), (3, 3, 1));""",
     f"""        (p.voters, p.overwrites, p.n_blobs, p.occupied_before),
        ({v[0]}, {v[1]}, {v[2]}, {v[3]})
    );
    assert_eq!((p.nproofs, p.n_public, p.log_n), ({v[4]}, {v[5]}, {v[6]}));"""),
    ("""    // Recorded on the v0.1 batch ELF; the publics layout did not change since.
    assert_eq!(
        s(&snark["program_vk"]),
        "0x6cfc89d562d0b22f04478a5c15b390433eb52f1b03147030b183076260da7a10"
    );""",
     """    assert_eq!(
        s(&snark["program_vk"]),
        format!("0x{}", hex::encode(BATCH_PROGRAM_VK))
    );"""),
]
for old, new in pairs:
    assert s.count(old) == 1, old[:60]
    s = s.replace(old, new)
open(wire, "w").write(s)
print("wire.rs: voters overwrites n_blobs occupied_before =", v[:4], "nproofs n_public log_n =", v[4:])
```

```bash
cd /home/p4u/davinci-zkvm
cp $HOME/.cache/op-core/fixture/snark_batch.json $HOME/.cache/op-core/fixture/publics_batch.bin rust-sdk/testdata/
python3 -I ~/.cache/op-core/wire_fixture.py ~/.cache/davinci-e2e/l1-big.log rust-sdk/tests/wire.rs
cargo fmt --all -- --check && echo "fmt ok"
git status --short
```

Expected: the seven numbers of the `recorded job` line; `fmt ok`; ` M rust-sdk/testdata/publics_batch.bin`, ` M rust-sdk/testdata/snark_batch.json`, ` M rust-sdk/tests/wire.rs` besides the three untracked files. (Checked on a scratch clone: with the old v0.1 fixture and its numbers the patched test compiles and fails only at the restored assertion, `left: "0x6cfc89d5…7a10"`, `right: "0xa0ad5c4c…35fa"`.)

- [ ] **Step 4: Run the SDK tests**

```bash
rtk proxy bash -c 'cargo test -p davinci-zkvm-sdk --test wire 2>&1 | grep -E "panicked|test result"'
rtk proxy bash -c 'cargo test -p davinci-zkvm-sdk 2>&1 | grep -E "^test result" | sort | uniq -c'
cargo clippy -p davinci-zkvm-sdk --all-targets -- -D warnings 2>&1 | tail -1
```

Expected: `test result: ok. 9 passed; 0 failed`; every suite of the crate `ok`; no clippy warning.

- [ ] **Step 5: Commit (no push here)**

```bash
git add rust-sdk/testdata/snark_batch.json rust-sdk/testdata/publics_batch.bin rust-sdk/tests/wire.rs
git status --short      # only the three untracked files remain
git commit -m "test(sdk): batch snark fixture from a v0.2.0 proof" -m "Recorded from the first batch job of the sequencer's 512 x 16 L1-blob run on
the v0.2.0 prover. The wire test checks its program vk against
BATCH_PROGRAM_VK again."
git log --oneline -2
```

Expected: the new commit on top of `439ab65 chore(service): v0.2.0`.

---

### Task 11: Deploy the OP Mainnet registry and fill in the preset

**Files:**
- davinci-contracts: rename `deployments/10.json` to `deployments/10-0x8b63.json`; create `deployments/10.json`; modify `broadcast/DeployAll.s.sol/10/run-latest.json`, `README.md`, `docs/deployment.md`, `docs/verification.md`.
- davinci-sequencer: `client/src/networks.rs` (`OPTIMISM.registry`, `start_block`, the doc date), `docs/deployments.md`, `docs/configuration.md`.
- Outside the repositories: `~/.config/davinci-l1blob/op-core.sh`, `~/.cache/davinci-l1blob/bin/fill_op_core.py`, `~/.cache/davinci-l1blob/deploy-op-core.log`, `~/.cache/op-core/observer/`.

**Interfaces:**
- Consumes: Task 8 (`--grace`, `EXPECT_GRACE`); `~/.config/davinci-l1blob/env.sh` (`OWNER`, `OP_RPC`, `OP_PUB`, `L1_READ`, `CAST`, `FORGE`, `KEYS`, `ZISK`, `L1BLOCK`, `ROOT_C_VADCOP_FINAL`, `BALLOT_VK_HASH`, `owner_key`, `key_of`, `lc`, `redact`).
- Produces: the registry `REG` at block `BLOCK` (from `deployments/10.json`); `OPTIMISM` on it; `op-core.sh` for Tasks 12 and 14.

**Spend: owner checkpoint 1** (Step 6 only): one contract creation on OP from the owner's account, at most 0.0001 ETH (the 2026-10-12 deployment of two contracts estimated 9.2 M gas and 0.00000003 ETH of L2 fee, plus the L1 data fee).

- [ ] **Step 1: `op-core.sh` (no secret in it)**

```bash
cat > ~/.config/davinci-l1blob/op-core.sh <<'EOF'
# Sourced by the OP core steps (Tasks 11, 12 and 14). Holds no secret: env.sh
# reads the keys and the OP RPC from their files when used, and never echoes them.
. ~/.config/davinci-l1blob/env.sh
# davinci-zkvm v0.2.0 program vks (env.sh keeps the v0.1 ones of 2026-10-12).
export BATCH_PROGRAM_VK=0xa0ad5c4c7e39d4a7007933158181418a752e6c1563a211af84c579640db435fa
export RESULTS_PROGRAM_VK=0xe693589453ecaee63eba697c48d4c5321a714bb18c2d84a97437b3f368578f68
export L1V=0x3B95BC14F43ed8Addb83F84D10A949De74eE7529
export OLD_REG=0x8b63fAE96948D69CF92Eb40e4EE0fCE40962267d
export GRACE=300,240,600,1800,60
export TMPDIR=$HOME/.cache/op-core/tmp PATH=$HOME/.zisk/bin:$HOME/.foundry/bin:$PATH
EOF
chmod 600 ~/.config/davinci-l1blob/op-core.sh
. ~/.config/davinci-l1blob/op-core.sh
[ "$("$CAST" wallet address --private-key "$(owner_key)" 2>/dev/null | lc)" = "$(echo "$OWNER" | lc)" ] && echo "owner key ok" || echo "owner key MISMATCH"
"$CAST" chain-id --rpc-url "$OP_RPC" 2>&1 | redact
```

Expected: `owner key ok`, `10`.

- [ ] **Step 2: Contracts preflight (read-only)**

```bash
. ~/.config/davinci-l1blob/op-core.sh
cd /home/p4u/davinci-contracts
git branch --show-current; git status --short | grep -v "^?? node_modules" | wc -l
test -e .env && echo ".env PRESENT" || echo "no .env"
"$FORGE" build 2>&1 | tail -1
echo "ZiskVerifier codehash $("$CAST" keccak "$("$CAST" code $ZISK --rpc-url "$OP_PUB")") rootC $("$CAST" call $ZISK 'getRootCVadcopFinal()(bytes32)' --rpc-url "$OP_PUB")"
echo "L1BlobVerifier l1Block $("$CAST" call $L1V 'l1Block()(address)' --rpc-url "$OP_PUB") MAX_AGE $("$CAST" call $L1V 'MAX_AGE()(uint256)' --rpc-url "$OP_PUB")"
python3 script/verify_deployment.py --rpc "$OP_PUB" --chain-id 10 --registry $OLD_REG --verifier $ZISK \
  --batch-vk 0x6cfc89d562d0b22f04478a5c15b390433eb52f1b03147030b183076260da7a10 \
  --results-vk 0x7bc8c5e9235548386a44b1885732a2a7ffb1badddc8c7fba599d07ece47be794 \
  --root-c "$ROOT_C_VADCOP_FINAL" --ballot-vk-hash "$BALLOT_VK_HASH" --dkg-manager 0x0 --council-manager 0x0 \
  --l1-block "$L1BLOCK" --grace 180,150,600,1800,60 > $TMPDIR/old-registry.txt; echo "exit $?"
grep -c "^OK" $TMPDIR/old-registry.txt; grep -c "^FAIL" $TMPDIR/old-registry.txt
echo "owner OP nonce $("$CAST" nonce "$OWNER" --rpc-url "$OP_PUB") balance $("$CAST" balance "$OWNER" --ether --rpc-url "$OP_PUB")"
```

Expected: `feat/op-core`, `0`, `no .env` (`deploy_all.sh` sources one), a successful build; codehash `0x82385a405b7301345d7e246017846ca3228aaea349cb68b116d76e0e77056566`, rootC `0x05006517b6ccde5da4d890587ba62845b5af8a307c00e87d4b9d05099b16dc80`; `l1Block 0x4200000000000000000000000000000000000015 MAX_AGE 7200`; `exit 0`, `19` OK lines and `0` FAIL lines: the retired registry and the reused `L1BlobVerifier` match this build (checked read-only on 2026-10-13 against a scratch build of this branch, including `L1BlobVerifier runtime code == local build (immutables masked) ok`); the owner's OP balance at least 0.0005 ETH. Write down the nonce and the balance (`N0`, `B0`). No-go on any difference.

- [ ] **Step 3: Simulate the deployment (no broadcast)**

```bash
. ~/.config/davinci-l1blob/op-core.sh
cd /home/p4u/davinci-contracts
echo "expected registry $("$CAST" compute-address "$OWNER" --nonce "$("$CAST" nonce "$OWNER" --rpc-url "$OP_PUB")")"
( export PRIVATE_KEY="$(owner_key)" RPC_URL="$OP_RPC" CHAIN_ID=10 ZISK_VERIFIER=$ZISK L1_BLOB_VERIFIER=$L1V L1_BLOCK= \
    BATCH_PROGRAM_VK RESULTS_PROGRAM_VK ROOT_C_VADCOP_FINAL BALLOT_VK_HASH \
    GRACE_DEFAULT=300 GRACE_FLOOR=240 GRACE_CEIL=600 GRACE_MAX_TOTAL=1800 NOTICE_MIN=60
  "$FORGE" script script/DeployAll.s.sol:DeployAllScript --chain 10 --rpc-url "$RPC_URL" \
    --optimize --optimizer-runs 200 ) 2>&1 | redact \
  | grep -E "Deployer|reused|code hash|deployed at|L1 b|program vk|rootC|Ballot vk|Grace|Estimated|SIMULATION|Error|error"
```

Expected:
```
expected registry 0x<B>
  Deployer address: 0x470D1dDc7DdCeb0d818751D171d40366f010a3cc
  ZiskVerifier reused at: 0x6781FFF50D615CB3F331CCD3Bdad3416d4ce0d5c
  ZiskVerifier code hash: 0x82385a405b7301345d7e246017846ca3228aaea349cb68b116d76e0e77056566
  ProcessRegistry deployed at: 0x<B>
  DavinciDKGAdapter deployed at: 0x0000000000000000000000000000000000000000
  CouncilAdapter deployed at: 0x0000000000000000000000000000000000000000
  L1 blob verifier: 0x3B95BC14F43ed8Addb83F84D10A949De74eE7529
  L1 block: 0x4200000000000000000000000000000000000015
  Batch program vk: 0xa0ad5c4c7e39d4a7007933158181418a752e6c1563a211af84c579640db435fa
  Results program vk: 0xe693589453ecaee63eba697c48d4c5321a714bb18c2d84a97437b3f368578f68
  rootCVadcopFinal: 0x05006517b6ccde5da4d890587ba62845b5af8a307c00e87d4b9d05099b16dc80
  Ballot vk hash: 0xbf1e6590bb1ba883d601c4d7d1c6fa2722a78590716874019db6d68fc776bb0e
  Grace (s): default 300, floor 240, ceil 600, max total 1800; notice min 60
Estimated total gas used for script: <about 7,000,000>
Estimated amount required: <well under 0.00001> ETH
SIMULATION COMPLETE. To broadcast these transactions, add --broadcast ...
```
No `L1BlobVerifier deployed at` line (the verifier is reused). Go: exactly these values, the same `0x<B>` twice, no error, estimated amount at most 0.00001 ETH. No-go otherwise: in particular a `Grace (s): default 180, floor 150` line means a variable did not reach the script.

- [ ] **Step 4: Keep the retired registry's record**

```bash
cd /home/p4u/davinci-contracts
git mv deployments/10.json deployments/10-0x8b63.json
git commit -m "chore(deploy): keep the first op registry's record as 10-0x8b63.json" -m "The new OP Mainnet registry's deployment writes deployments/10.json."
git status --short | grep -v "^?? node_modules" | wc -l     # expect 0: the deployment runs from a clean commit
```

- [ ] **Step 5: Ask the owner (checkpoint 1)**

Show the owner Step 3's output, `N0`, `B0` and the spend bound (at most 0.0001 ETH on OP), and wait for an explicit "deploy".

- [ ] **Step 6: Deploy**

```bash
. ~/.config/davinci-l1blob/op-core.sh
cd /home/p4u/davinci-contracts
( export PRIVATE_KEY="$(owner_key)" RPC_URL="$OP_RPC" CHAIN_ID=10 VERIFY_MODE=false ZISK_VERIFIER=$ZISK L1_BLOB_VERIFIER=$L1V L1_BLOCK= \
    BATCH_PROGRAM_VK RESULTS_PROGRAM_VK ROOT_C_VADCOP_FINAL BALLOT_VK_HASH \
    GRACE_DEFAULT=300 GRACE_FLOOR=240 GRACE_CEIL=600 GRACE_MAX_TOTAL=1800 NOTICE_MIN=60 EXPECT_GRACE=$GRACE
  ./deploy_all.sh; echo "deploy_all exit $?" ) 2>&1 | redact > ~/.cache/davinci-l1blob/deploy-op-core.log
grep -E "reused at|deployed at|^OK|^FAIL|l1 blob|^record|Hash:|Block:|Paid:|deploy_all exit|Error|error" ~/.cache/davinci-l1blob/deploy-op-core.log
```

Expected: the addresses of Step 3; forge's `Hash: 0x...`, `Block: ...`, `Paid: ... ETH` lines for the one creation; every `verify_deployment.py` line `OK` (runtime code of `ProcessRegistry`, `ZiskVerifier` and `L1BlobVerifier` equal to the local build, the four pins, `registry.chainID == rpc chain id`, both managers disabled, `l1BlobVerifier.l1Block == --l1-block 0x4200000000000000000000000000000000000015`, `registry.defaultGrace == --grace 300 s` … `registry.noticeMin == --grace 60 s`); `l1 blob  0x3b95bc14f43ed8addb83f84d10a949de74ee7529 (L1Block 0x4200000000000000000000000000000000000015)`; `record   deployments/10.json`; `deploy_all exit 0`. `VERIFY_MODE=false`: no explorer key (source verification stays an open item, as for the first OP registry). If any line is `FAIL`, no record is written: stop and report; the registry exists but nothing points at it.

- [ ] **Step 7: Post-deployment checks and the secret scan**

```bash
. ~/.config/davinci-l1blob/op-core.sh
cd /home/p4u/davinci-contracts
REG=$(jq -r .processRegistry deployments/10.json); BLOCK=$(jq -r .block deployments/10.json); COMMIT=$(jq -r .commit deployments/10.json)
echo "registry $REG block $BLOCK commit $COMMIT"
jq -c '{pins, grace, l1BlobVerifier, l1Block, ziskVerifier}' deployments/10.json
"$CAST" call "$REG" 'l1BlobVerifier()(address)' --rpc-url "$OP_PUB"
python3 script/verify_deployment.py --rpc "$OP_PUB" --chain-id 10 --registry "$REG" --verifier "$ZISK" \
  --batch-vk "$BATCH_PROGRAM_VK" --results-vk "$RESULTS_PROGRAM_VK" --root-c "$ROOT_C_VADCOP_FINAL" \
  --ballot-vk-hash "$BALLOT_VK_HASH" --dkg-manager 0x0 --council-manager 0x0 --l1-block "$L1BLOCK" --grace "$GRACE" > $TMPDIR/new-registry.txt; echo "exit $?"
grep -c "^FAIL" $TMPDIR/new-registry.txt
python3 -I - "$HOME/optimism-privkey.txt" "$HOME/optimism-rpcs.txt" broadcast/DeployAll.s.sol/10/run-latest.json deployments/10.json ~/.cache/davinci-l1blob/deploy-op-core.log <<'EOF'
import sys
key = open(sys.argv[1]).read().strip().lower().removeprefix("0x")
rpc = open(sys.argv[2]).readline().strip()
for f in sys.argv[3:]:
    t = open(f).read()
    print(f, "key" if key in t.lower() else "no key", "rpc" if rpc and rpc in t else "no rpc")
EOF
git status --short | grep -v "^?? node_modules"
echo "owner OP nonce $("$CAST" nonce "$OWNER" --rpc-url "$OP_PUB") balance $("$CAST" balance "$OWNER" --ether --rpc-url "$OP_PUB")"
```

Expected: `deployments/10.json` holds `chainId 10`, the v0.2.0 pins, `"grace": {"defaultGrace": 300, "graceFloor": 240, "graceCeil": 600, "graceMaxTotal": 1800, "noticeMin": 60}`, `l1BlobVerifier` `0x3b95…7529`, `l1Block` `0x4200…0015`, `ziskVerifier` `0x6781…0d5c`, `commit` the Step 4 commit; `l1BlobVerifier()` `0x3B95BC14F43ed8Addb83F84D10A949De74eE7529`; `exit 0` and `0`; all three files `no key no rpc`; `git status` shows ` M broadcast/DeployAll.s.sol/10/run-latest.json` and `?? deployments/10.json` only (`helpers/parse_deployall_runs.sh` skips chain 10, so `golang-types/addresses.go` does not change; if it does, `git checkout golang-types/addresses.go`). Spend: `B0` minus the new balance, at most 0.0001 ETH; nonce `N0 + 1`.

- [ ] **Step 8: The fill script**

Save as `~/.cache/davinci-l1blob/bin/fill_op_core.py` (it asserts the record before it writes; every replaced text must occur exactly once):

````python
"""Fills in the new OP Mainnet registry after its deployment.
python3 -I fill_op_core.py <davinci-sequencer> <davinci-contracts> <YYYY-MM-DD> <registry, EIP-55>
Reads <davinci-contracts>/deployments/10.json (the verified record); prints what it wrote."""
import json
import sys

seq, con, day = sys.argv[1], sys.argv[2], sys.argv[3]
rec = json.load(open(f"{con}/deployments/10.json"))
# The record holds lowercase addresses; the caller passes the registry's EIP-55
# form (cast to-check-sum-address) as the fourth argument.
reg = sys.argv[4]
assert reg.lower() == rec["processRegistry"].lower(), (reg, rec["processRegistry"])
l1v, block, commit = rec["l1BlobVerifier"], int(rec["block"]), rec["commit"]
assert rec["chainId"] == 10, rec["chainId"]
assert rec["ziskVerifier"].lower() == "0x6781fff50d615cb3f331ccd3bdad3416d4ce0d5c", rec["ziskVerifier"]
assert rec["grace"] == {"defaultGrace": 300, "graceFloor": 240, "graceCeil": 600,
                        "graceMaxTotal": 1800, "noticeMin": 60}, rec["grace"]
assert rec["pins"]["batchProgramVK"] == "0xa0ad5c4c7e39d4a7007933158181418a752e6c1563a211af84c579640db435fa"
assert rec["pins"]["resultsProgramVK"] == "0xe693589453ecaee63eba697c48d4c5321a714bb18c2d84a97437b3f368578f68"
assert l1v.lower() == "0x3b95bc14f43ed8addb83f84d10a949de74ee7529", l1v


def edit(p, pairs):
    s = open(p).read()
    for old, new in pairs:
        assert s.count(old) == 1, (p, old[:70])
        s = s.replace(old, new)
    open(p, "w").write(s)


def section(p, start, end, new):
    s = open(p).read()
    a, b = s.index(start), s.index(end)
    open(p, "w").write(s[:a] + new + s[b:])


# --- davinci-sequencer: the preset ---
edit(f"{seq}/client/src/networks.rs", [
    ("""/// Ethereum (deployed 2026-10-12, see docs/deployments.md). Blob sends need an""",
     f"""/// Ethereum (deployed {day}, see docs/deployments.md). Blob sends need an"""),
    ("""    registry: address!("8b63fAE96948D69CF92Eb40e4EE0fCE40962267d"),
    start_block: 158_125_136,""",
     f"""    registry: address!("{reg[2:]}"),
    start_block: {block:_},"""),
])

# --- davinci-sequencer: docs/deployments.md ---
section(f"{seq}/docs/deployments.md", "## OP Mainnet", "## Retired deployments", f"""## OP Mainnet

Chain id 10, preset `optimism`, L1-blob mode: transitions settle on OP Mainnet and their blobs
live in Ethereum blob transactions. Deployed on {day} from davinci-contracts commit `{commit}`
(the contract sources of release v0.0.51); davinci-contracts' `deployments/10.json` holds the
verified record. Not source-verified on an explorer yet.

| Contract | Address | Block |
|---|---|---|
| ZiskVerifier | `0x6781FFF50D615CB3F331CCD3Bdad3416d4ce0d5c` (reused; code hash the pinned `0x82385a40…5566`) | |
| L1BlobVerifier | `0x3B95BC14F43ed8Addb83F84D10A949De74eE7529` (reused from the first OP registry) | 158125135 |
| ProcessRegistry | `{reg}` | {block} |

Pinned values, equal to the SDK release pins (davinci-zkvm v0.2.0):

| Value | Where | Hash |
|---|---|---|
| batch program vk | registry `batchProgramVK` | `0xa0ad5c4c7e39d4a7007933158181418a752e6c1563a211af84c579640db435fa` |
| results program vk | registry `resultsProgramVK` | `0xe693589453ecaee63eba697c48d4c5321a714bb18c2d84a97437b3f368578f68` |
| `rootCVadcopFinal` | registry and verifier | `0x05006517b6ccde5da4d890587ba62845b5af8a307c00e87d4b9d05099b16dc80` |
| ballot VK hash | registry `ballotVKHash`, state leaf `0x07` | `0xbf1e6590bb1ba883d601c4d7d1c6fa2722a78590716874019db6d68fc776bb0e` |
| verifier code keccak256 | `ZiskVerifier` runtime code | `0x82385a405b7301345d7e246017846ca3228aaea349cb68b116d76e0e77056566` |

The grace window fits the Ethereum leg: 300 s by default, floor 240 s, ceiling 600 s, at most
1800 s past the end, 60 s shorten notice. No DKG or Council adapter: those key modes are not on
OP Stack chains yet.

`L1BlobVerifier` reads OP's L1 origin from the `L1Block` predeploy
`0x4200000000000000000000000000000000000015`. Consumers of OP transitions (SDKs, explorers) read
`ProcessStateTransitionedL1` for the Ethereum blocks and versioned hashes, one block per blob: a
transition above six blobs comes in several Ethereum transactions. There is no `BLOBHASH` on OP.
The preset reads Ethereum from `https://ethereum-rpc.publicnode.com` and `https://eth.drpc.org`:
publicnode refuses request bodies above about 1 MB, dRPC takes the 1.5 MB six-blob sends, and the
node sends a blob transaction to every endpoint. Followers fetch blobs from
`https://ethereum-beacon-api.publicnode.com`, which served full blobs for recent and day-old
slots when the preset was filled in; production nodes should add a beacon node of their own that
keeps full blobs.

""")
edit(f"{seq}/docs/deployments.md", [
    ("""### Gnosis Chain

Chain id 100.""",
     """### OP Mainnet, first registry

`ProcessRegistry` `0x8b63fAE96948D69CF92Eb40e4EE0fCE40962267d` (block 158125136), deployed on
2026-10-12 from davinci-contracts `e7de916` on the same `ZiskVerifier` and `L1BlobVerifier`,
followed by the `optimism` preset of release v0.5.x builds only. Its grace floor (150 s) left too
little room for the Ethereum leg, and it pins the v0.1 vks.

### Gnosis Chain

Chain id 100."""),
])

# --- davinci-sequencer: docs/configuration.md, the OP operator notes ---
edit(f"{seq}/docs/configuration.md", [
    ("""`now + graceFloor`, so `--l1-settle-lag` plus `--settle-margin` must stay under the registry's
grace floor (150 s on OP); otherwise nothing could seal while a window is open, so a signing
node refuses to boot (an observer only warns).""",
     """`now + graceFloor`, so `--l1-settle-lag` plus `--settle-margin` must stay under the registry's
grace floor (240 s on OP); otherwise nothing could seal while a window is open, so a signing
node refuses to boot (an observer only warns)."""),
    ("""- Keep the L1-blob defaults: `--settle-margin 30s` and `--l1-settle-lag 110s`. Do not carry a
  blob-mode `DAVINCI_SETTLE_MARGIN=60s` into an `optimism` node (110 + 60 > 150), and do not lower
  `--l1-settle-lag` below about 100 s: a post settled about 72 s after it was sent in the live run,
  and the rest is headroom for a missed slot or a wider origin lag.
- Organizers should keep the grace at the 180 s default or above, not the 150 s floor: after the
  end, a follow-up batch can seal only within `grace - 140 s` of the previous landing.""",
     """- Keep the L1-blob defaults: `--settle-margin 30s` and `--l1-settle-lag 110s`. They leave a batch
  up to 240 - 30 - 30 = 180 s of estimated proof while an election is open, and five blob
  transactions of Ethereum leg. Do not lower `--l1-settle-lag` below about 100 s: a post settled
  about 72 s after it was sent in the live run, and the rest is headroom for a missed slot or a
  wider origin lag.
- Organizers should keep the grace at the 300 s default or above, not the 240 s floor: after the
  end, a follow-up batch can seal only within `grace - 140 s` of the previous landing. A 512-vote
  batch at 16 fields after the end wants the 600 s ceiling on a node that has not proved yet (its
  estimate starts at 0.8 s a vote)."""),
])

# --- davinci-contracts: README deployments ---
section(f"{con}/README.md", "### OP Mainnet, L1-blob mode", "### Gnosis Chain production beta", f"""### OP Mainnet, L1-blob mode

OP Mainnet (chain id 10), deployed {day} from commit `{commit}` at block {block}, on the
davinci-zkvm v0.2.0 program vks. The registry settles transitions on OP while their blobs live in
Ethereum blob transactions, checked by its `L1BlobVerifier` against OP's `L1Block` predeploy
([settlement.md](docs/settlement.md#l1-blob-mode)). [`deployments/10.json`](deployments/10.json)
holds the verified record. Not source-verified on an explorer yet.

| Contract | Address |
|---|---|
| `ProcessRegistry` | `{reg}` |
| `L1BlobVerifier` | `0x3B95BC14F43ed8Addb83F84D10A949De74eE7529` (reads `L1Block` `0x4200000000000000000000000000000000000015`; reused) |
| `ZiskVerifier` | `0x6781FFF50D615CB3F331CCD3Bdad3416d4ce0d5c` (reused; same code hash as Gnosis') |

| Immutable | Value |
|---|---|
| `batchProgramVK` | `0xa0ad5c4c7e39d4a7007933158181418a752e6c1563a211af84c579640db435fa` |
| `resultsProgramVK` | `0xe693589453ecaee63eba697c48d4c5321a714bb18c2d84a97437b3f368578f68` |
| `rootCVadcopFinal` | `0x05006517b6ccde5da4d890587ba62845b5af8a307c00e87d4b9d05099b16dc80` |
| `ballotVKHash` | `0xbf1e6590bb1ba883d601c4d7d1c6fa2722a78590716874019db6d68fc776bb0e` |
| `defaultGrace` / `graceFloor` / `graceCeil` | 300 / 240 / 600 s |
| `graceMaxTotal` | 1800 s |
| `noticeMin` | 60 s |

No DKG or Council adapter.

### OP Mainnet, first registry

`ProcessRegistry` `0x8b63fAE96948D69CF92Eb40e4EE0fCE40962267d` (block 158125136), deployed
2026-10-12 from commit `e7de916` with the same verifiers, the Gnosis pins below and grace 180 /
150 / 600 / 1800 / 60 s. Retired: its v0.1 vks and 150 s floor; its record is
[`deployments/10-0x8b63.json`](deployments/10-0x8b63.json).

""")
edit(f"{con}/README.md", [
    ("""### Gnosis Chain production beta

Gnosis Chain (chain id 100), deployed 2026-10-07 from commit `4e4f6c8` at block 48,633,301.""",
     """### Gnosis Chain production beta

Gnosis Chain (chain id 100), deployed 2026-10-07 from commit `4e4f6c8` at block 48,633,301.
Retired on {day}: sequencers from v0.6.0 on prove under the v0.2.0 vks and follow OP Mainnet.""".format(day=day)),
    ("""Both registries are pinned to:""", """Both Gnosis registries (and the first OP one) are pinned to:"""),
])

# --- davinci-contracts: docs/deployment.md, the OP Mainnet section ---
section(f"{con}/docs/deployment.md", "## OP Mainnet", "## Council support", f"""## OP Mainnet

Deployed on {day} reusing the `ZiskVerifier` and the `L1BlobVerifier` of the first OP registry
(the verifier is stateless apart from its permissionless origin record), on the davinci-zkvm v0.2.0
pins and a grace window that fits the Ethereum leg, from the owner's account, with no `.env`
(variables set in a subshell; the RPC and the key never written to disk):

```bash
CHAIN_ID=10 VERIFY_MODE=false \\
ZISK_VERIFIER=0x6781FFF50D615CB3F331CCD3Bdad3416d4ce0d5c \\
L1_BLOB_VERIFIER=0x3B95BC14F43ed8Addb83F84D10A949De74eE7529 \\
BATCH_PROGRAM_VK=0xa0ad5c4c7e39d4a7007933158181418a752e6c1563a211af84c579640db435fa \\
RESULTS_PROGRAM_VK=0xe693589453ecaee63eba697c48d4c5321a714bb18c2d84a97437b3f368578f68 \\
ROOT_C_VADCOP_FINAL=0x05006517b6ccde5da4d890587ba62845b5af8a307c00e87d4b9d05099b16dc80 \\
BALLOT_VK_HASH=0xbf1e6590bb1ba883d601c4d7d1c6fa2722a78590716874019db6d68fc776bb0e \\
GRACE_DEFAULT=300 GRACE_FLOOR=240 GRACE_CEIL=600 GRACE_MAX_TOTAL=1800 NOTICE_MIN=60 \\
EXPECT_GRACE=300,240,600,1800,60 \\
RPC_URL=... PRIVATE_KEY=... ./deploy_all.sh
```

The script ran `forge script` once without `--broadcast` first. The first OP registry
(2026-10-12, `L1_BLOCK` instead of `L1_BLOB_VERIFIER`, the v0.1 pins, the default grace) is
retired; its record is `deployments/10-0x8b63.json`. Addresses are in the
[deployments table](../README.md#deployments).

""")

# --- davinci-contracts: docs/verification.md, the OP example ---
edit(f"{con}/docs/verification.md", [
    ("""For the previous Gnosis registry:""",
     f"""For the OP Mainnet registry (`deployments/10.json`), with `RPC` an OP Mainnet JSON-RPC endpoint:

```bash
git checkout {commit}
forge build
python3 script/verify_deployment.py --rpc "$RPC" --chain-id 10 \\
    --registry {reg} \\
    --verifier 0x6781FFF50D615CB3F331CCD3Bdad3416d4ce0d5c \\
    --batch-vk 0xa0ad5c4c7e39d4a7007933158181418a752e6c1563a211af84c579640db435fa \\
    --results-vk 0xe693589453ecaee63eba697c48d4c5321a714bb18c2d84a97437b3f368578f68 \\
    --root-c 0x05006517b6ccde5da4d890587ba62845b5af8a307c00e87d4b9d05099b16dc80 \\
    --ballot-vk-hash 0xbf1e6590bb1ba883d601c4d7d1c6fa2722a78590716874019db6d68fc776bb0e \\
    --dkg-manager 0x0 --council-manager 0x0 \\
    --l1-block 0x4200000000000000000000000000000000000015 --grace 300,240,600,1800,60
```

For the previous Gnosis registry:"""),
])
print(f"filled: registry {reg} block {block} commit {commit} ({day})")
````

```bash
. ~/.config/davinci-l1blob/op-core.sh
cd /home/p4u/davinci-contracts
REG=$(jq -r .processRegistry deployments/10.json)
python3 -I ~/.cache/davinci-l1blob/bin/fill_op_core.py /home/p4u/davinci-sequencer /home/p4u/davinci-contracts \
  "$(date -u +%F)" "$("$CAST" to-check-sum-address "$REG")"
git -C /home/p4u/davinci-sequencer diff --stat; git diff --stat
grep -n "registry: address\|start_block:" /home/p4u/davinci-sequencer/client/src/networks.rs | head -2
```

Expected: `filled: registry 0x<B> block <BLOCK> commit <COMMIT> (<date>)`; davinci-sequencer `client/src/networks.rs`, `docs/configuration.md`, `docs/deployments.md` changed; davinci-contracts `README.md`, `docs/deployment.md`, `docs/verification.md` changed (plus Step 6's files); the preset reads the new registry and `start_block: <BLOCK with _ separators>,`. (The script was run against a scratch tree of Tasks 1-8 with a made-up record and reverted: every anchor matched.)

- [ ] **Step 9: The node boots on the preset as an observer, and refuses the retired registry**

```bash
. ~/.config/davinci-l1blob/op-core.sh
cd /home/p4u/davinci-sequencer
rtk proxy bash -c 'cargo test -p davinci-client --lib networks 2>&1 | grep -E "test result"; cargo test -p davinci-sequencer --lib config 2>&1 | grep -E "test result"'
cargo build --release -p davinci-sequencer --bin davinci-sequencer 2>&1 | tail -1
mkdir -p $HOME/.cache/op-core/observer
timeout 90 target/release/davinci-sequencer --network optimism --datadir $HOME/.cache/op-core/observer \
  --api-host 127.0.0.1 --api-port 19090 > $HOME/.cache/op-core/observer.log 2>&1 &
sleep 60; curl -s http://127.0.0.1:19090/info; echo; wait
grep -E "observer|registry grace window|deployment|L1-blob mode|blob cap per|API listening|ERROR|WARN" $HOME/.cache/op-core/observer.log | cut -c1-260
timeout 60 target/release/davinci-sequencer --network optimism --registry "$OLD_REG" --start-block 158125136 \
  --datadir $HOME/.cache/op-core/observer-old --api-host 127.0.0.1 --api-port 19091 2>&1 | grep -m1 -o "registry is not the pinned release.*" | cut -c1-200
ls $HOME/.cache/op-core/observer-old 2>/dev/null | wc -l
```

Expected: both `test result: ok`; the build; `/info` with `"chain_id":10`, `"process_registry":"0x<b, lowercase>"`, `"batch_program_vk":"0xa0ad5c4c…35fa"`, `"results_program_vk":"0xe6935894…8f68"`, `"observer":true`; in the log `no signing key: running as an observer (never settles)`, `registry grace window grace=GraceParams { default_grace: 300, grace_floor: 240, grace_ceil: 600, grace_max_total: 1800, notice_min: 60 }`, `deployment network=optimism chain_id=10 registry=0x<B> start_block=Some(<BLOCK>) datadir=/home/p4u/.cache/op-core/observer/10-0x<b>`, `L1-blob mode: blobs go to Ethereum l1_chain_id=1 sender=None l1_block=0x4200000000000000000000000000000000000015 ...`, `blob cap per transaction cap=6`, `blob cap per batch cap=32`, `API listening addr=127.0.0.1:19090`, no `ERROR` and no budget `WARN` (110 + 30 ≤ 240). Then `registry is not the pinned release: registry.batchProgramVK: expected 0xa0ad5c4c…, got 0x6cfc89d5…; registry.resultsProgramVK: …` and `0` (the check runs before any database is opened). Only public endpoints are used (the preset's), so nothing here needs `redact`.

- [ ] **Step 10: Commit both repositories**

```bash
. ~/.config/davinci-l1blob/op-core.sh
cd /home/p4u/davinci-contracts
REG=$(jq -r .processRegistry deployments/10.json); BLOCK=$(jq -r .block deployments/10.json); COMMIT=$(jq -r .commit deployments/10.json)
REG_CS=$("$CAST" to-check-sum-address "$REG")
git add deployments/10.json broadcast/DeployAll.s.sol/10/run-latest.json README.md docs/deployment.md docs/verification.md
git commit -m "chore(deploy): op mainnet registry on the v0.2.0 vks with a 240 s grace floor" -m "ProcessRegistry $REG_CS at block $BLOCK, built from $COMMIT, on
ZiskVerifier 0x6781...0d5c and L1BlobVerifier 0x3B95...7529 (both reused),
grace 300/240/600/1800/60 s, checked by verify_deployment.py --grace. The
first OP registry 0x8b63...267d is retired (v0.1 vks, 150 s floor)."
cd /home/p4u/davinci-sequencer
cargo fmt --all -- --check && echo "fmt ok"
git add client/src/networks.rs docs/deployments.md docs/configuration.md
git commit -m "feat(net): optimism preset on the v0.2.0 registry" -m "ProcessRegistry $REG_CS from block $BLOCK: davinci-zkvm v0.2.0
program vks and a 240 s grace floor that fits the Ethereum leg. The
2026-10-12 registry 0x8b63...267d is retired."
git -C /home/p4u/davinci-contracts status --short | grep -v "^?? node_modules" | wc -l; git status --short | wc -l
```

Expected: `fmt ok`; two commits; `0` and `0`.

---

### Task 12: Live on OP Mainnet: `op_live`, then one 512 x 16 transition

**Files:** none (a failure leads to a fix commit with its regression test, Task 9's anvil runs that cover it, then the live run again).

**Interfaces:**
- Consumes: Task 11 (the preset on the new registry, `op-core.sh`); the funded keys `~/.config/davinci-l1blob/seq-a.l1.key` (Ethereum blob posts) and `seq-a.op.key` (OP settlements); the organizer key file `~/optimism-privkey.txt`.
- Produces: `~/.cache/davinci-e2e/{op-live-dry.log,op-live.log,l1-big-live.log}` (redacted); the spend figures for the report.

**Spend: owner checkpoints 2-4.** `seq-a.l1` at most 0.0003 ETH of blob posts for both runs together; the owner's OP account the organizer's transactions (well under 0.0001 ETH); a top-up of `seq-a.l1` from the owner's Ethereum account (at most 0.0005 ETH) only if its balance is below a run's 0.0002 ETH minimum.

- [ ] **Step 1: Live environment (no secret in it) and the binary of this commit**

Every later command of this task runs in a shell set up with:

```bash
. ~/.config/davinci-l1blob/op-core.sh
cd /home/p4u/davinci-sequencer
export DAVINCI_E2E_LIVE=1 DAVINCI_E2E_NETWORK=optimism DAVINCI_E2E_POLL=2s \
  DAVINCI_E2E_RPC="$OP_RPC" \
  DAVINCI_E2E_NODE_RPCS="$OP_RPC,https://optimism-rpc.publicnode.com;$OP_RPC,https://mainnet.optimism.io" \
  DAVINCI_E2E_ORGANIZER_KEY=$HOME/optimism-privkey.txt \
  DAVINCI_E2E_SEQUENCER_KEYS=$KEYS/seq-a.op.key DAVINCI_E2E_L1_KEYS=$KEYS/seq-a.l1.key \
  DAVINCI_E2E_L1_SEND_RPC=$L1_SEND DAVINCI_E2E_BEACON=$ETH_BEACON \
  DAVINCI_SEQUENCER_BIN=$HOME/.cache/davinci-l1blob/bin/davinci-sequencer-$(git rev-parse --short HEAD) \
  TMPDIR=$HOME/.cache/davinci-e2e
SEQ_A_L1=$("$CAST" wallet address --private-key "$(key_of "$KEYS/seq-a.l1.key")")
SEQ_A_OP=$("$CAST" wallet address --private-key "$(key_of "$KEYS/seq-a.op.key")")
balances() { echo "seq-a Ethereum $("$CAST" balance "$SEQ_A_L1" --ether --rpc-url "$L1_READ") (nonce $("$CAST" nonce "$SEQ_A_L1" --rpc-url "$L1_READ")) OP $("$CAST" balance "$SEQ_A_OP" --ether --rpc-url "$OP_PUB"); owner OP $("$CAST" balance "$OWNER" --ether --rpc-url "$OP_PUB") Ethereum $("$CAST" balance "$OWNER" --ether --rpc-url "$L1_READ")"; }
fees() { echo "Ethereum base fee $("$CAST" base-fee --rpc-url "$L1_READ") wei, priority fee $("$CAST" to-dec "$("$CAST" rpc eth_maxPriorityFeePerGas --rpc-url "$L1_READ" | tr -d '"')") wei, blob base fee $("$CAST" to-dec "$("$CAST" rpc eth_blobBaseFee --rpc-url "$L1_READ" | tr -d '"')") wei"; }
```

Once, after Task 11's commit:

```bash
git status --short | wc -l
cargo build --release -p davinci-sequencer --bin davinci-sequencer 2>&1 | tail -1 && cp target/release/davinci-sequencer "$DAVINCI_SEQUENCER_BIN"
balances; fees
```

Expected: `0`; the build; the balances (write them down as the starting point `S`); fees. (Key *files* only; the OP RPC reaches the harness and its nodes through the environment and is redacted from every output below.)

- [ ] **Step 2: Zero-cost dry check: balances are checked before anything is spent**

Save as `~/.cache/op-core/scan_secrets.py` (it reads the files itself: no secret ever appears in a process argument, shell history or a command audit):

```python
"""Secret scan by file. python3 -I scan_secrets.py <key files, comma separated> <rpcs file> <path>...
Directories are walked. Prints the files that hold a key or an RPC URL and a total; exit 1 if any."""
import os
import sys

keys = [open(p).read().strip().lower().removeprefix("0x") for p in sys.argv[1].split(",")]
rpcs = [l.strip() for l in open(sys.argv[2]) if l.strip()]
files = []
for a in sys.argv[3:]:
    if os.path.isdir(a):
        files += [os.path.join(d, f) for d, _, fs in os.walk(a) for f in fs]
    else:
        files.append(a)
bad = 0
for f in sorted(files):
    t = open(f, errors="replace").read()
    k = any(x and x in t.lower() for x in keys)
    r = any(x in t for x in rpcs)
    if k or r:
        bad += 1
        print(f, "key" if k else "no key", "rpc" if r else "no rpc")
print(f"{len(files)} files, {bad} with a secret")
sys.exit(1 if bad else 0)
```

```bash
"$CAST" nonce "$OWNER" --rpc-url "$OP_PUB"
DAVINCI_E2E_OP_LIVE=1 DAVINCI_E2E_L1_MIN_BALANCE=1000 cargo test -p davinci-e2e --test e2e op_live -- --nocapture 2>&1 | redact > ~/.cache/davinci-e2e/op-live-dry.log
grep -E "pins ok|L1-blob mode|balance|below|FAILED|test result" ~/.cache/davinci-e2e/op-live-dry.log
python3 -I ~/.cache/op-core/scan_secrets.py "$KEYS/seq-a.l1.key,$KEYS/seq-a.op.key,$HOME/optimism-privkey.txt" "$HOME/optimism-rpcs.txt" ~/.cache/davinci-e2e/op-live-dry.log
"$CAST" nonce "$OWNER" --rpc-url "$OP_PUB"
```

Expected: `chain 10 registry 0x<B> verifier 0x6781FFF50D615CB3F331CCD3Bdad3416d4ce0d5c (pins ok)`, `L1-blob mode: L1BlobVerifier 0x3B95BC14F43ed8Addb83F84D10A949De74eE7529 on L1Block 0x4200000000000000000000000000000000000015`, the balance lines, then `FAILED: below 1000 ETH on Ethereum: sequencer 1 0x<SEQ_A_L1>` and `test result: FAILED`; the scanner prints `1 files, 0 with a secret`; the owner's OP nonce unchanged.

- [ ] **Step 3: Go/no-go for `op_live` (owner checkpoints 2 and 3)**

```bash
curl -s "${DAVINCI_ZKVM_URL:-http://127.0.0.1:8080}/health"; echo
balances; fees
```

Go: `"queue_len":0`; Ethereum base fee plus priority fee at most 0.5 gwei (500000000 wei; the node pays the RPC's suggested tip, at least 0.001 gwei) and blob base fee at most 0.02 gwei (20000000 wei), which bounds this run at about 0.0001 ETH (up to 8 posts of 21,000 gas and 8 blobs of 131,072 blob gas); `seq-a` at least 0.0002 ETH on Ethereum and 0.00005 ETH on OP. If `seq-a` Ethereum is below 0.0002 ETH, propose a top-up to the owner (checkpoint 2: `"$CAST" send --rpc-url "$L1_READ" --private-key "$(owner_key)" --value 0.0005ether "$SEQ_A_L1" --json`, at most 0.0005 ETH, gas at the base fee above) and run it only on their explicit "top up". (That `cast send --private-key "$(owner_key)"` puts the owner's key in a process argument for the length of one transaction: the same one-off as the 2026-10-12 precedent, under the owner's eyes, accepted; nothing else in this plan passes a key or the private RPC URL as an argument.) Then show the owner the expected spend (about 0.00004 to 0.0001 ETH of blob posts, a few OP settlements) and that the run occupies the production prover for 30 to 45 minutes, and wait for an explicit "run op_live" (checkpoint 3).

- [ ] **Step 4: The live run (about 30 to 45 minutes)**

```bash
DAVINCI_E2E_OP_LIVE=1 DAVINCI_E2E_NEGATIVE=1 \
  systemd-run --user --scope -p MemoryMax=16G -- cargo test -p davinci-e2e --test e2e op_live -- --nocapture 2>&1 \
  | redact > ~/.cache/davinci-e2e/op-live.log
grep -E "prover ok|pins ok|L1-blob|node-a (at|killed)|round [12]|after the restart|transitions|local tree|results on-chain|closed at|  [A-Za-z1]|gas bill|L1 blobs|PASSED|FAILED|test result" ~/.cache/davinci-e2e/op-live.log
```

Expected:
```
[e2e ...] prover ok, queue_len 0
chain 10 registry 0x<B> verifier 0x6781FFF50D615CB3F331CCD3Bdad3416d4ce0d5c (pins ok)
L1-blob mode: L1BlobVerifier 0x3B95BC14F43ed8Addb83F84D10A949De74eE7529 on L1Block 0x4200000000000000000000000000000000000015
[e2e ...] node-a at http://127.0.0.1:<port> log ...
[e2e ...] round 1 submitted: 11 votes
[e2e ...] node-a killed with an Ethereum blob post sent (nonce <n>)
[e2e ...] round 1: all 11 votes settled in <s> s
[e2e ...] after the restart: post record picked up true; mined posts at nonces [...] (...)
[e2e ...] round 2: all 3 votes settled in <s> s
[e2e ...] node-obs: local tree of 0x... at the chain root (and localStateRoot)
[e2e ...] transitions <4-6>; node-a: settled_by_self=<k> ...; node-obs: settled_by_self=0 synced_from_others=<k> ...
[e2e ...] 0x...: results on-chain [...] = expected tally
negative checks on 0x... : the L1 checks PASS or INCONCLUSIVE (an OP RPC without old state)
[e2e ...] gas bill, chain 10 and Ethereum: <n> txs, <total> total
  L1 blobs 0x...  block <L1 block> gas 21000 @ <wei> wei, blobs 1 @ <wei> wei, <ETH> 0x...
[e2e ...] PASSED in <m> min
test op_live ... ok
```
The run's batches are small, so each transition is one Ethereum transaction; what changed under it is the registry (v0.2.0 vks, 240 s floor) and the node (chunked posting with one chunk). Stop rule: if `seq-a`'s Ethereum balance falls more than 0.0002 ETH below `S` during the run (`balances` in a second shell set up as in Step 1), stop the run (Ctrl-C) and report. A FAILED run keeps its work directory; cancel leftover processes with `DAVINCI_E2E_CANCEL=<pids> cargo test -p davinci-e2e --test e2e cancel_leftovers` (same environment).

- [ ] **Step 5: Secrets and spend after `op_live`**

```bash
python3 -I ~/.cache/op-core/scan_secrets.py "$KEYS/seq-a.l1.key,$KEYS/seq-a.op.key,$HOME/optimism-privkey.txt" "$HOME/optimism-rpcs.txt" ~/.cache/davinci-e2e
balances; fees
```

Expected: `<n> files, 0 with a secret` (the scanner walks `~/.cache/davinci-e2e`; a hit names the file, which is then redacted or deleted and the cause fixed); `seq-a` Ethereum spend (against `S`) consistent with the gas bill, about 0.00004 to 0.0001 ETH.

- [ ] **Step 6: Go/no-go for the live 512 x 16 transition (owner checkpoint 4)**

The run settles 512 first votes in several transitions (about 5 to 8 blobs in all), then one transition of nine blobs in two Ethereum transactions: about 8 to 12 blob posts and 15 to 17 blobs in total. At 0.5 gwei of base plus priority fee and a blob base fee of 0.01 gwei that is at most about 0.00015 ETH (12 x 21,000 gas and 17 x 131,072 blob gas); each post is also capped at the harness's live `--l1-max-post-fee` (0.00015 ETH).

Go: Step 3's prover check; Ethereum base fee plus priority fee at most 500000000 wei (0.5 gwei) and blob base fee at most 10000000 wei (0.01 gwei); `seq-a` Ethereum spend so far plus 0.00015 ETH at most 0.0003 ETH, and its balance at least 0.0002 ETH (a top-up as in Step 3 only within the 0.0005 ETH cap, and only if the spend bound still holds). Otherwise skip the live 512 x 16 run, say so in the report (Task 9 Step 5 covers it on anvil), and go to Step 9. With a go, show the owner the numbers and that the run occupies the production prover for about an hour, and wait for an explicit "run l1_big live". (Owner decision: run it whenever the fee gates pass; skip only on fees, and say so in the report.)

- [ ] **Step 7: The live run (about 60 minutes, 10 of them the 600 s grace)**

```bash
DAVINCI_E2E_L1_BIG=1 systemd-run --user --scope -p MemoryMax=16G -- \
  cargo test -p davinci-e2e --test l1_big -- --nocapture 2>&1 | redact > ~/.cache/davinci-e2e/l1-big-live.log
grep -E "pins ok|L1-blob mode|node at|s grace|ballots proved|transitions|phase 1|after the end|nine blobs|results on-chain|PASSED|FAILED|test result" ~/.cache/davinci-e2e/l1-big-live.log
```

Expected: `chain 10 registry 0x<B> ... (pins ok)`, the L1-blob line of Step 4, then the lines of Task 9 Step 5 without `recorded job` (`DAVINCI_E2E_RECORD_DIR` is unset): `first 512 votes settled in <k> transitions`, `phase 1: <p> s per vote ...`, `512 x 16 settled <s> s after the end` (under 600), `nine blobs in Ethereum blocks [<a>, <b>] (2 transactions), OP tx 0x...`, `results on-chain = tally`, `PASSED`, `test l1_big ... ok`. The same stop rule as Step 4 applies (0.00015 ETH below the balance at Step 6). On failure the test cancels its open process; keep the work directory and debug as in Task 9 Step 3.

- [ ] **Step 8: Secrets and spend after the live 512 x 16 run**

Step 5's commands again (the scanner over `~/.cache/davinci-e2e`). Expected: `<n> files, 0 with a secret`; `seq-a` Ethereum spend for both runs together at most 0.0003 ETH.

- [ ] **Step 9: Report figures**

Write down for the final report: both run times, the `transitions` and `gas bill` lines, `<k>`, `<s>`, the two Ethereum blocks of the nine-blob transition, and the spend per account against `S` (`seq-a` Ethereum and OP, the owner's OP and, after a top-up, Ethereum).

---

### Task 13: Merge and release: davinci-contracts v0.0.51, then davinci-sequencer v0.6.0

**Files:**
- davinci-contracts: `CHANGELOG.md` (`package.json` is bumped to 0.0.51 by the release bot).
- davinci-sequencer: `.github/workflows/main.yml:30,45` (`CONTRACTS_REF: v0.0.51`), `Cargo.toml:6`, `Cargo.lock` (workspace 0.6.0).
- Outside the repositories: `~/.cache/op-core/{changelog_v0051.py,notes.py,pr-contracts.md,pr-sequencer.md,notes-v0.0.51.md,notes-v0.6.0.md}`, `~/.davinci-gnosis/sequencer1/container-inspect-v0.5.0.json`.

**Interfaces:**
- Consumes: Tasks 1-12 (all green), the final audit; `deployments/10.json`.
- Produces: tags `v0.0.51` (davinci-contracts, by the release bot) and `v0.6.0` (davinci-sequencer, annotated, by the owner's account), their GitHub releases, `ghcr.io/vocdoni/davinci-sequencer:v0.6.0` and `:latest`; davinci-zkvm `main` with Task 10's commit.

Every push, merge, tag, release and container change below is an owner checkpoint (5 to 13): show the exact command and wait for an explicit yes. The remote `feat/op-core` branches stay after the merges (no `--delete-branch`): GitHub's rebase merge rewrites commit ids, and `deployments/10.json` names the `feat/op-core` commit the registry was built from.

- [ ] **Step 1: Final whole-release audit**

Dispatch the strict Fable 5.1 reviewer over `git diff origin/main...feat/op-core` in davinci-sequencer and davinci-contracts and Task 10's davinci-zkvm commit, with the spec, this plan's Review Focus and the run logs of Tasks 9 and 12. Fix every finding in a commit with its test (and, for node changes, the affected GPU run again) before Step 2. Expected: APPROVE.

- [ ] **Step 2: davinci-contracts CHANGELOG**

Save as `~/.cache/op-core/changelog_v0051.py`:

```python
"""davinci-contracts: the CHANGELOG entry of v0.0.51. Run from the repository root:
python3 -I changelog_v0051.py <YYYY-MM-DD> <registry, EIP-55>"""
import json
import sys

day, reg = sys.argv[1], sys.argv[2]
rec = json.load(open("deployments/10.json"))
assert reg.lower() == rec["processRegistry"].lower(), (reg, rec["processRegistry"])
block = int(rec["block"])
entry = f"""## {day}

- L1-blob mode, for OP Stack chains where `BLOBHASH` sees no Ethereum blob: a registry built with
  an `IL1BlobVerifier` (constructor argument `_l1BlobVerifier` after `_noticeMin`; zero keeps blob
  mode) settles through `submitStateTransitionL1(processId, publicValues, proofBytes, commitments,
  ys, kzgProofs, l1)`, where `l1` proves that Ethereum blob transactions carrying the transition's
  versioned hashes are in Ethereum's canonical chain. Each entry point reverts `WrongDAMode` in the
  other mode. Settlements emit `ProcessStateTransitionedL1(processId, l1BlockNumbers,
  versionedHashes)`, one Ethereum block per blob; `l1BlobVerifier()` exposes the verifier.
- `L1BlobVerifier(l1Block)`: RLP headers newest first, chained by parent hash and anchored at OP's
  current L1 origin (`L1Block.hash()`) or at an origin recorded earlier with the permissionless
  `record()` (`OriginRecorded`, `recorded(number)`); transactions-trie proofs of type-3
  transactions at or below the anchor and at most `MAX_AGE` (7200) blocks behind the origin. The
  versioned hashes are concatenated in `txs` order, so one transition may come in several
  transactions from different blocks. Errors `L1OriginMismatch`, `BrokenHeaderChain`, `BadHeader`,
  `TxAboveOrigin`, `NotBlobTx`, `BadVersionedHash`, `TxTooOld`.
- OP Mainnet registry `{reg}` (block {block:,}) on the davinci-zkvm v0.2.0 program vks (batch
  `0xa0ad5c4c…35fa`, results `0xe6935894…8f68`), grace 300 / 240 / 600 / 1800 / 60 s, reusing
  `ZiskVerifier` `0x6781FFF50D615CB3F331CCD3Bdad3416d4ce0d5c` and `L1BlobVerifier`
  `0x3B95BC14F43ed8Addb83F84D10A949De74eE7529`; record in `deployments/10.json`. The first OP
  registry `0x8b63fAE96948D69CF92Eb40e4EE0fCE40962267d` (v0.1 vks, 150 s floor) is retired, its
  record is `deployments/10-0x8b63.json`; the Gnosis registries are retired too.
- Deployment: `DeployAll` reads `L1_BLOB_VERIFIER` (reuse a verifier) or `L1_BLOCK` (deploy one on
  that `L1Block`). `verify_deployment.py` checks the verifier's code and `l1Block()`
  (`--l1-block`, default the OP predeploy) and, with `--grace` (`EXPECT_GRACE` in
  `deploy_all.sh`), the five grace settings, since `DeployAll` keeps its default for a misspelled
  variable. The compose deployment and the docs default to the v0.2.0 vks.
- Test vectors for davinci-zkvm v0.2.0's compact DA stream, and L1-blob fixtures: one transition
  in two transactions (also re-posted, its first transaction in the newer block) and the largest
  proof a node sends (64 headers, three transactions, 13 blobs: 47,168 bytes, 3.5 M gas).

"""
p = "CHANGELOG.md"
s = open(p).read()
head = "# Changelog\n\n"
assert s.startswith(head) and f"## {day}\n" not in s
open(p, "w").write(head + entry + s[len(head):])
print(f"CHANGELOG: {day}, registry {reg} block {block}")
```

```bash
. ~/.config/davinci-l1blob/op-core.sh
cd /home/p4u/davinci-contracts
python3 -I ~/.cache/op-core/changelog_v0051.py "$(date -u +%F)" "$("$CAST" to-check-sum-address "$(jq -r .processRegistry deployments/10.json)")"
head -12 CHANGELOG.md
git add CHANGELOG.md
git commit -m "docs(changelog): l1-blob mode and the op mainnet registry" -m "The entry of the v0.0.51 release: submitStateTransitionL1 and the
L1BlobVerifier, the OP Mainnet registry on the v0.2.0 vks, the deployment
checks and the test vectors."
```

Expected: `CHANGELOG: <date>, registry 0x<B> block <BLOCK>`; the new section on top.

- [ ] **Step 3: Push davinci-contracts `feat/op-core` and open the pull request (owner checkpoint 5)**

```bash
cd /home/p4u/davinci-contracts
git fetch origin
git rev-parse --short origin/main                       # expect 7c7b0b2, else stop and rebase (Task 7 Step 1)
git merge-base --is-ancestor origin/main feat/op-core && echo "on top of main"
git push -u origin feat/op-core
cat > ~/.cache/op-core/pr-contracts.md <<'EOF'
L1-blob mode for OP Stack chains and the OP Mainnet registry of the davinci-zkvm v0.2.0 release.

Review: this repository's `main` requires a code-owner review; the merge (by rebase, with `--admin`) relied on the final Fable 5.1 audit of the whole release as its review.

- `submitStateTransitionL1` and `L1BlobVerifier`: a transition's blobs live in one or several Ethereum blob transactions, proven against OP's L1 origin (`L1Block`) or a recorded one.
- The OP Mainnet registry on the v0.2.0 program vks with grace 300/240/600/1800/60 s (`deployments/10.json`); the first OP registry is retired (`deployments/10-0x8b63.json`).
- Deployment: `L1_BLOB_VERIFIER` / `L1_BLOCK`; `verify_deployment.py --grace` (`EXPECT_GRACE`); compose and docs on the v0.2.0 vks; bindings regenerated with abigen v1.17.8.
- Tests: compact DA stream vectors, two-transaction fixtures (also re-posted) and the worst-case proof size, forge `TransitionL1TwoTxTest` with the real verifier.

No contract source changed since the 2026-10-12 OP deployment; see CHANGELOG.md.
EOF
gh pr create -R vocdoni/davinci-contracts --base main --head feat/op-core \
  --title "feat(l1blob): op mainnet registry in l1-blob mode on the v0.2.0 vks" --body-file ~/.cache/op-core/pr-contracts.md
```

Expected: `on top of main`; the branch pushed; the PR URL. Then wait for CI and the bindings bot:

```bash
gh pr checks feat/op-core -R vocdoni/davinci-contracts --watch
gh run list -R vocdoni/davinci-contracts --workflow bindings-sync.yml --branch feat/op-core -L 1 --json conclusion,status --jq '.[0]'
git ls-remote origin refs/heads/feat/op-core | cut -c1-7; git rev-parse --short feat/op-core
```

Expected: every check passes; the bindings run `success` (or none: no `.sol` changed in the pushed range); the remote head equals the local one (the bot pushed nothing, the bindings match abigen v1.17.8). If the bot pushed a regeneration commit, `git pull --ff-only`, read it, and say so in the PR.

- [ ] **Step 4: Merge davinci-contracts (owner checkpoint 6)**

`main` requires one approving code-owner review. The owner decided to bypass it as admin, once CI is green and Step 1's final Fable audit has approved; the PR body (Step 3) says so:

```bash
gh pr merge feat/op-core -R vocdoni/davinci-contracts --rebase --admin
```

```bash
git fetch origin
git diff feat/op-core origin/main --stat | wc -l          # expect 0: the same tree
gh run list -R vocdoni/davinci-contracts --branch main -L 3 --json workflowName,conclusion,status --jq '.[]'
```

Expected: `0`; the `main` runs green, and no bindings commit on `main` after them (`git fetch origin && git log -1 --format=%an origin/main` is not `github-actions[bot]`).

- [ ] **Step 5: Release v0.0.51 through the `release` branch (owner checkpoint 7)**

```bash
cd /home/p4u/davinci-contracts
git merge-base --is-ancestor origin/release origin/main && echo "fast-forward"
git push origin origin/main:refs/heads/release
gh run list -R vocdoni/davinci-contracts --workflow release-tag.yml -L 1 --json conclusion,status --jq '.[0]'
```

Expected: `fast-forward` (`release` is `0f00014 bump: v0.0.50`, an ancestor of `main`); the push; the "Bump version & tag" run ends `success` (it rebuilds the bindings with abigen `@latest`, fails on any drift, then `npm version 0.0.51 -m "bump: v0.0.51"` and pushes the commit and the tag). If it fails on drift (abigen moved past v1.17.8), regenerate the bindings with that version on a branch, open a PR, merge it, and push `release` again. Then:

```bash
git fetch origin --tags
git log -1 --format='%h %an %s' origin/release
git rev-parse --short 'v0.0.51^{commit}'
git push origin origin/release:refs/heads/main            # fast-forward main to the bump commit
gh run list -R vocdoni/davinci-contracts --workflow publish.yml -L 1 --json conclusion,headBranch --jq '.[0]'
gh release view v0.0.51 -R vocdoni/davinci-contracts --json name --jq .name
```

Expected: `<sha> github-actions[bot] bump: v0.0.51`, the same `<sha>`; `main` moved to it (an admin push of a fast-forward, as for v0.0.50); the publish run for `v0.0.51` `success` (npm package and a GitHub release named `v0.0.51` with generated notes). Then replace the generated notes:

```bash
python3 -I ~/.cache/op-core/notes.py contracts /home/p4u/davinci-contracts/deployments/10.json "$("$CAST" to-check-sum-address "$(jq -r .processRegistry deployments/10.json)")" > ~/.cache/op-core/notes-v0.0.51.md
gh release edit v0.0.51 -R vocdoni/davinci-contracts --notes-file ~/.cache/op-core/notes-v0.0.51.md
```

`~/.cache/op-core/notes.py` (both releases' notes, in the style of v0.0.50 and v0.5.0):

```python
"""Release notes of davinci-contracts v0.0.51 or davinci-sequencer v0.6.0.
python3 -I notes.py contracts|sequencer <davinci-contracts>/deployments/10.json <registry, EIP-55>"""
import json
import sys

which, rec_path, reg = sys.argv[1], sys.argv[2], sys.argv[3]
rec = json.load(open(rec_path))
assert reg.lower() == rec["processRegistry"].lower()
block = int(rec["block"])
CONTRACTS = f"""OP Mainnet in L1-blob mode, on the davinci-zkvm v0.2.0 program vks.

- **New registry on OP Mainnet (chain 10)**: `ProcessRegistry` `{reg}` (block {block:,}) in L1-blob mode on `L1BlobVerifier` `0x3B95BC14F43ed8Addb83F84D10A949De74eE7529` (OP's `L1Block` predeploy), reusing `ZiskVerifier` `0x6781FFF50D615CB3F331CCD3Bdad3416d4ce0d5c`. Batch vk `0xa0ad5c4c7e39d4a7007933158181418a752e6c1563a211af84c579640db435fa`, results vk `0xe693589453ecaee63eba697c48d4c5321a714bb18c2d84a97437b3f368578f68`; grace 300 s by default, 240 s floor, 600 s ceiling, at most 1800 s past the end, 60 s notice. Checked by `verify_deployment.py --grace`; record in `deployments/10.json`. Not source-verified on an explorer yet.
- **L1-blob mode**: `submitStateTransitionL1` settles a transition whose blobs are in one or several Ethereum blob transactions (`L1BlobVerifier.verify`, anchored at OP's L1 origin or a `record()`ed one) and emits `ProcessStateTransitionedL1`; `WrongDAMode` guards each entry point.
- **Retired**: the first OP registry `0x8b63fAE96948D69CF92Eb40e4EE0fCE40962267d` (v0.1 vks, record `deployments/10-0x8b63.json`) and the Gnosis registries.
- Deployment tooling: `L1_BLOB_VERIFIER` / `L1_BLOCK`, `--grace` / `EXPECT_GRACE`; compose and docs on the v0.2.0 vks; Go bindings and TypeChain types with the L1-blob interface.

See [CHANGELOG.md](https://github.com/vocdoni/davinci-contracts/blob/v0.0.51/CHANGELOG.md) for the interface changes.

**Full Changelog**: https://github.com/vocdoni/davinci-contracts/compare/v0.0.50...v0.0.51
"""
SEQUENCER = f"""OP Mainnet, and Gnosis retired. The node settles on OP Mainnet with its blobs on Ethereum, proves under davinci-zkvm v0.2.0, batches up to 512 votes at every field count, and opens a new database for the new registry.

## OP Mainnet in L1-blob mode

- **The default network is `optimism`**: `ProcessRegistry` [`{reg}`](https://optimistic.etherscan.io/address/{reg}) from block {block:,}, on the davinci-zkvm v0.2.0 program vks, grace 300 s by default with a 240 s floor. Addresses are in [docs/deployments.md](https://github.com/vocdoni/davinci-sequencer/blob/v0.6.0/docs/deployments.md).
- **Blobs go to Ethereum.** OP cannot read Ethereum blobs, so the node posts each transition's blobs in Ethereum blob transactions while the proof runs, then settles on OP with a proof that those transactions are in Ethereum's chain. A signing node needs a dedicated Ethereum key (`DAVINCI_L1_PRIVKEY_FILE`) that sends nothing else, besides its OP key; observers need neither. Defaults: `--l1-settle-lag 110s`, `--settle-margin 30s`, `--l1-max-post-fee 0.001eth` per post. The node refuses to boot when the Ethereum leg and the margin do not fit the registry's grace floor.
- Followers rebuild the state from Ethereum beacons (the preset uses `https://ethereum-beacon-api.publicnode.com`); production nodes should add a beacon of their own that keeps full blobs.

## 512-vote batches at every field count

- `--batch-max` is 512. In L1-blob mode a transition may take up to 32 blobs (the guest's limit), posted as several Ethereum transactions of at most the per-transaction cap (6 today), one at a time, and settled with one proof. A 512-vote batch at 16 fields with 512 silent refreshes is nine blobs in two transactions.
- The seal budget reserves 24 s for each extra transaction and sizes a batch down when the window fits fewer, instead of holding it. A transition is re-posted at most three times (one transaction or all of them), each post under the fee cap.
- The boot logs both caps: `blob cap per transaction` and `blob cap per batch`.

## davinci-zkvm v0.2.0

- The compact DA stream: smaller blobs for the same data; followers decode it and recompute the results leaf. The program vks change (`ROOT_C_VADCOP_FINAL` and the ballot vk do not), so this release needs a v0.2.0 prover and a registry pinned to its vks, and refuses any other at boot.

## Gnosis retired

- The `gnosis` preset is gone: the Gnosis registries pin the v0.1 vks, which a v0.2.0 prover cannot serve. `davinci-client` and the e2e default to `optimism`.

## Upgrading

- **Nodes that follow `latest` move to OP Mainnet.** A signing node also needs `DAVINCI_L1_PRIVKEY_FILE` (and Ether on Ethereum for its blob posts), or it refuses to boot. A node that must stay on Gnosis pins `ghcr.io/vocdoni/davinci-sequencer:v0.5.0` and a v0.1 prover.
- The new registry opens `<datadir>/10-{reg.lower()}`; other databases are left alone.
- `docker-compose.yml` mounts the Ethereum key as a second secret (`SEQUENCER_L1_KEY_FILE`, default `./sequencer-l1.key`) and sets `DAVINCI_L1_PRIVKEY_FILE` itself. **Compose nodes must create `sequencer-l1.key` before the image moves** (`sudo install -o 10001 -m 0400 /dev/null sequencer-l1.key` for an observer; the funded Ethereum key for a signing node), or compose will not start the service. An empty file is no key: an observer runs, a signing node refuses to boot.

**Full Changelog**: https://github.com/vocdoni/davinci-sequencer/compare/v0.5.0...v0.6.0
"""
print({"contracts": CONTRACTS, "sequencer": SEQUENCER}[which], end="")
```

Expected: `gh release view v0.0.51 -R vocdoni/davinci-contracts --json body --jq .body | head -3` shows the new text.

- [ ] **Step 6: davinci-sequencer: pin the contracts release and set the version**

```bash
cd /home/p4u/davinci-sequencer
git fetch https://github.com/vocdoni/davinci-sequencer.git +refs/heads/main:refs/remotes/origin/main
git rev-parse --short origin/main                      # expect f1cc114, else stop and rebase (Task 1 Step 1)
git merge-base --is-ancestor origin/main feat/op-core && echo "on top of main"
sed -i 's|^  CONTRACTS_REF: main$|  CONTRACTS_REF: v0.0.51|; s|^#   davinci-contracts/                vocdoni/davinci-contracts@main (submodules)$|#   davinci-contracts/                vocdoni/davinci-contracts@v0.0.51 (submodules)|' .github/workflows/main.yml
grep -n "davinci-contracts@\|CONTRACTS_REF:" .github/workflows/main.yml
git add .github/workflows/main.yml
git commit -m "ci(deps): pin davinci-contracts to v0.0.51" -m "The release with submitStateTransitionL1 and the OP Mainnet registry."
sed -i 's/^version = "0.5.0"$/version = "0.6.0"/' Cargo.toml
cargo update --workspace --offline 2>&1 | grep Updating
git diff --stat
```

Expected: `on top of main`; `30:#   davinci-contracts/                vocdoni/davinci-contracts@v0.0.51 (submodules)` and `45:  CONTRACTS_REF: v0.0.51`; five `Updating <crate> v0.5.0 (...) -> v0.6.0` lines (`arbo`, `davinci-client`, `davinci-e2e`, `davinci-sequencer`, `davinci-state`); `Cargo.lock | 10`, `Cargo.toml | 2`. Then the checks CI runs, plus the anvil settlement tests against the sibling contracts (the `v0.0.51` sources):

```bash
git -C /home/p4u/davinci-contracts checkout v0.0.51 && (cd /home/p4u/davinci-contracts && forge build 2>&1 | tail -1)
cargo fmt --all -- --check && cargo clippy --workspace --all-targets --locked -- -D warnings 2>&1 | tail -1
rtk proxy bash -c 'cargo test --workspace 2>&1 | tee $TMPDIR/ws.log | grep -E "^test result: FAILED|panicked"; echo "suites ok $(grep -c "^test result: ok" $TMPDIR/ws.log), exit ${PIPESTATUS[0]}"'
rtk proxy bash -c 'ANVIL=1 cargo test -p davinci-sequencer --test web3_anvil 2>&1 | grep -E "test result"'
git -C /home/p4u/davinci-contracts checkout feat/op-core
git add Cargo.toml Cargo.lock
git commit -m "chore(release): workspace version 0.6.0" -m "OP Mainnet in L1-blob mode with several Ethereum blob transactions per
transition, davinci-zkvm v0.2.0, and Gnosis retired."
```

Expected: no warnings; no failed suite and `exit 0`; `test result: ok. 14 passed` (the file runs every test without a filter: 13 today plus Task 4's); two commits.

- [ ] **Step 7: Push davinci-sequencer `feat/op-core` and open the pull request (owner checkpoint 8)**

```bash
cd /home/p4u/davinci-sequencer
git push https://github.com/vocdoni/davinci-sequencer.git feat/op-core:refs/heads/feat/op-core
cat > ~/.cache/op-core/pr-sequencer.md <<'EOF'
The node of the OP Mainnet release: L1-blob DA, the compact DA stream of davinci-zkvm v0.2.0, and several Ethereum blob transactions per transition.

- L1-blob mode: blobs posted on Ethereum (window-1 poster, fee cap, doubled replacements, records before sends) while the proof runs; settlement on OP with an L1 inclusion proof; followers fetch blobs from Ethereum beacons.
- Two caps: per transaction (the chunk size, from the L1's `eth_config`) and per batch (32 in L1-blob mode). A transition is posted in chunks and settled with one proof over every chunk transaction; the seal budget reserves 24 s per extra chunk.
- davinci-zkvm v0.2.0 (compact DA stream), 512-vote batches by default.
- The `optimism` preset on the new registry (davinci-contracts v0.0.51); Gnosis retired.
- Tests: unit and fake-chain tests of the chunking and the settle loop, a nine-blob ZISKEMU dry run, a two-transaction ANVIL settlement, GPU e2e in both DA modes, one 512 x 16 transition on a production-grace anvil registry, and live runs on OP Mainnet.
EOF
gh pr create -R vocdoni/davinci-sequencer --base main --head feat/op-core \
  --title "feat(l1da): op mainnet core stack, several ethereum blob transactions per transition" --body-file ~/.cache/op-core/pr-sequencer.md
gh pr checks feat/op-core -R vocdoni/davinci-sequencer --watch
```

Expected: the PR URL; every check passes (with `CONTRACTS_REF: v0.0.51`).

- [ ] **Step 8: Merge davinci-sequencer (owner checkpoint 9)**

```bash
gh pr merge feat/op-core -R vocdoni/davinci-sequencer --rebase
git fetch https://github.com/vocdoni/davinci-sequencer.git +refs/heads/main:refs/remotes/origin/main
git diff feat/op-core origin/main --stat | wc -l          # expect 0
```

- [ ] **Step 9: Push davinci-zkvm `main` (owner checkpoint 10)**

```bash
cd /home/p4u/davinci-zkvm
git status --short          # only the three untracked files
git push https://github.com/vocdoni/davinci-zkvm.git main:refs/heads/main
gh run list -R vocdoni/davinci-zkvm --branch main -L 1 --json conclusion,status --jq '.[0]'
```

Expected: the fast-forward push of Task 10's commit; the run `success`. It publishes the `:main` prover image only; the production prover follows `:latest`, which moves only with a release tag.

- [ ] **Step 10: Take the Gnosis node off `latest` (owner checkpoint 11, before the tag)**

The host's watchtower (`--label-enable --include-stopped --revive-stopped`, every 300 s) would pull `v0.6.0` into the stopped `davinci-sequencer-1` and start it on its Gnosis environment (no L1 key: a crash loop). Its keys and datadir are bind mounts and stay where they are.

```bash
docker inspect davinci-sequencer-1 > ~/.davinci-gnosis/sequencer1/container-inspect-v0.5.0.json && chmod 600 ~/.davinci-gnosis/sequencer1/container-inspect-v0.5.0.json
docker rm davinci-sequencer-1
docker ps -a --filter label=com.centurylinklabs.watchtower.enable=true --format '{{.Names}} {{.Image}} {{.Status}}'
ls ~/.davinci-gnosis/sequencer1/data
```

Expected: the inspect saved (it holds key file paths, no key); `davinci-sequencer-1` removed; only `davinci-zkvm-davinci-zkvm-1 ghcr.io/vocdoni/davinci-zkvm:latest Up ...` follows watchtower; the Gnosis databases still there.

- [ ] **Step 11: Tag v0.6.0 and publish the release (owner checkpoint 12)**

```bash
. ~/.config/davinci-l1blob/op-core.sh
cd /home/p4u/davinci-sequencer
git tag -a v0.6.0 -m "v0.6.0: op mainnet in l1-blob mode, several ethereum blob transactions per transition, davinci-zkvm v0.2.0, gnosis retired" origin/main
git push https://github.com/vocdoni/davinci-sequencer.git refs/tags/v0.6.0
gh run list -R vocdoni/davinci-sequencer --branch v0.6.0 -L 1 --json conclusion,status --jq '.[0]'
docker buildx imagetools inspect ghcr.io/vocdoni/davinci-sequencer:v0.6.0 | grep -m1 Digest
docker buildx imagetools inspect ghcr.io/vocdoni/davinci-sequencer:latest | grep -m1 Digest
python3 -I ~/.cache/op-core/notes.py sequencer /home/p4u/davinci-contracts/deployments/10.json "$("$CAST" to-check-sum-address "$(jq -r .processRegistry /home/p4u/davinci-contracts/deployments/10.json)")" > ~/.cache/op-core/notes-v0.6.0.md
gh release create v0.6.0 -R vocdoni/davinci-sequencer --verify-tag --latest --title v0.6.0 --notes-file ~/.cache/op-core/notes-v0.6.0.md
```

Expected: the tag pushed; the tag's CI run `success` (it publishes `:v0.6.0` and `:latest`); both digests equal; the release URL. The tag message, the notes and the PR bodies carry no attribution trailer.

---

### Task 14: The production node on this host switches to OP Mainnet

**Files:** none in the repositories. On the host: `~/.davinci-op/data/` (a fresh datadir), `~/.davinci-op/container-inspect-v0.6.0.json`; the container `davinci-sequencer-op`.

**Interfaces:**
- Consumes: Task 13 (`ghcr.io/vocdoni/davinci-sequencer:v0.6.0`, `davinci-sequencer-1` removed); the funded keys `~/.config/davinci-l1blob/seq-a.op.key` and `seq-a.l1.key` (mounted read-only, never copied); the prover at `http://10.200.0.26:8080` (the Gnosis node's `DAVINCI_PROVER_URL`); `e2e/tests/node_smoke.rs`.
- Produces: the production OP node on `127.0.0.1:9090` and `10.200.0.26:9090` (the Gnosis node's bindings, so the public endpoint keeps its route).

How the Gnosis node ran (read with `docker inspect`, values without secrets): `docker run`, not compose; image `ghcr.io/vocdoni/davinci-sequencer:latest` with the watchtower label; `--user 1001:1001`, `--restart unless-stopped`; ports `127.0.0.1:9090` and `10.200.0.26:9090`; `DAVINCI_PRIVKEY_FILE=/run/secrets/sequencer_key` (bind of `~/.davinci-gnosis/keys/seq1.key`), `DAVINCI_DATADIR=/data` (bind of `~/.davinci-gnosis/sequencer1/data`), `DAVINCI_PROVER_URL=http://10.200.0.26:8080`, no network or RPC variable (the `gnosis` default).

From now on `seq-a`'s keys belong to production: a later live test needs keys of its own (an account's blob posts must come from one sender, or nonces collide).

- [ ] **Step 1: Preconditions (read-only)**

```bash
. ~/.config/davinci-l1blob/op-core.sh
docker buildx imagetools inspect ghcr.io/vocdoni/davinci-sequencer:v0.6.0 | grep -m1 Digest
docker ps -a --filter name=davinci-sequencer --format '{{.Names}} {{.Status}}'
curl -s http://10.200.0.26:8080/health; echo
SEQ_A_L1=$("$CAST" wallet address --private-key "$(key_of "$KEYS/seq-a.l1.key")"); SEQ_A_OP=$("$CAST" wallet address --private-key "$(key_of "$KEYS/seq-a.op.key")")
echo "seq-a Ethereum $SEQ_A_L1 $("$CAST" balance "$SEQ_A_L1" --ether --rpc-url "$L1_READ") nonce $("$CAST" nonce "$SEQ_A_L1" --rpc-url "$L1_READ") pending $("$CAST" nonce "$SEQ_A_L1" --block pending --rpc-url "$L1_READ")"
echo "seq-a OP $SEQ_A_OP $("$CAST" balance "$SEQ_A_OP" --ether --rpc-url "$OP_PUB")"
stat -c '%a %u %n' "$KEYS/seq-a.op.key" "$KEYS/seq-a.l1.key"
```

Expected: a digest; no `davinci-sequencer*` container; `"status":"ok"`; the two nonces equal (no blob post of the test runs still pending); `600 1001` for both keys (the container runs as `1001:1001`). The Ethereum balance funds production posts (about 0.00002 ETH a post at 0.5 gwei): below 0.0005 ETH, tell the owner before Step 3; the owner decided there is no production top-up (`seq-a` keeps what is left), so a low balance is reported, not fixed.

- [ ] **Step 2: The owner's decisions (owner checkpoint 13)**

Decided by the owner: the image is `ghcr.io/vocdoni/davinci-sequencer:latest` **with** `--label com.centurylinklabs.watchtower.enable=true`, so the node follows releases automatically, as the Gnosis node did; every later release reaches production within the watchtower interval. Operator caution: a release that changes required configuration (a new mandatory key or variable, as v0.6.0's `DAVINCI_L1_PRIVKEY_FILE` was) must ship with upgrade notes before its tag, because the tag is what moves the node. Still to ask: the go for the switch, accepting the outage of `sequencer2.davinci.vote`'s Gnosis beta. Wait for an explicit "switch".

- [ ] **Step 3: Start the OP node**

```bash
mkdir -p ~/.davinci-op/data && chmod 700 ~/.davinci-op ~/.davinci-op/data
IMAGE=ghcr.io/vocdoni/davinci-sequencer:latest       # the owner's choice (Step 2): follows releases
docker pull "$IMAGE"
docker run -d --name davinci-sequencer-op --restart unless-stopped --user 1001:1001 \
  -p 127.0.0.1:9090:9090 -p 10.200.0.26:9090:9090 \
  -v $HOME/.config/davinci-l1blob/seq-a.op.key:/run/secrets/sequencer_key:ro \
  -v $HOME/.config/davinci-l1blob/seq-a.l1.key:/run/secrets/sequencer_l1_key:ro \
  -v $HOME/.davinci-op/data:/data \
  -e DAVINCI_NETWORK=optimism -e DAVINCI_DATADIR=/data \
  -e DAVINCI_PRIVKEY_FILE=/run/secrets/sequencer_key \
  -e DAVINCI_L1_PRIVKEY_FILE=/run/secrets/sequencer_l1_key \
  -e DAVINCI_PROVER_URL=http://10.200.0.26:8080 \
  -e DAVINCI_RPC_URL=$OP_PRIVATE_RPC,https://optimism-rpc.publicnode.com,https://mainnet.optimism.io \
  --label com.centurylinklabs.watchtower.enable=true "$IMAGE"
```

`DAVINCI_RPC_URL` lists the owner-provided endpoint first, then the `optimism` preset's two public ones (the committed preset is not changed); the Ethereum endpoints are the preset's. The URL is in `docker inspect`, so the saved inspect file stays mode 600.

- [ ] **Step 4: Boot checks**

```bash
sleep 60
docker ps --filter name=davinci-sequencer-op --format '{{.Names}} {{.Image}} {{.Status}}'
docker logs davinci-sequencer-op 2>&1 | grep -E "observer|registry grace window|deployment|L1-blob mode|blob cap per|API listening|ERROR|WARN" | cut -c1-260
curl -s http://127.0.0.1:9090/info; echo
ls ~/.davinci-op/data
```

Expected: `davinci-sequencer-op <image> Up ...` (not restarting); `registry grace window grace=GraceParams { default_grace: 300, grace_floor: 240, grace_ceil: 600, grace_max_total: 1800, notice_min: 60 }`, `deployment network=optimism chain_id=10 registry=0x<B> start_block=Some(<BLOCK>) datadir=/data/10-0x<b>`, `L1-blob mode: blobs go to Ethereum l1_chain_id=1 sender=Some(0x<SEQ_A_L1>) l1_block=0x4200000000000000000000000000000000000015 ...`, `blob cap per transaction cap=6`, `blob cap per batch cap=32`, `API listening addr=0.0.0.0:9090`; no `observer`, `ERROR` or budget `WARN` line; `/info` with `"chain_id":10`, `"process_registry":"0x<b>"`, `"sequencer_address":"0x<SEQ_A_OP, lowercase>"`, `"batch_program_vk":"0xa0ad5c4c…35fa"`, `"observer":false`; `10-0x<b>` in the datadir. A crash loop (`Restarting`) means a check refused the boot: `docker logs davinci-sequencer-op 2>&1 | tail -5`, then `docker stop davinci-sequencer-op` and report.

- [ ] **Step 5: No secret in the container's configuration or logs, and the record**

```bash
. ~/.config/davinci-l1blob/op-core.sh
docker inspect davinci-sequencer-op > $TMPDIR/op-inspect.json; docker logs davinci-sequencer-op > $TMPDIR/op-logs.txt 2>&1
python3 -I - "$KEYS/seq-a.op.key" "$KEYS/seq-a.l1.key" "$HOME/optimism-privkey.txt" "$HOME/optimism-rpcs.txt" $TMPDIR/op-inspect.json $TMPDIR/op-logs.txt <<'EOF'
import sys
keys = [open(p).read().strip().lower().removeprefix("0x") for p in sys.argv[1:4]]
# The three endpoints of the container's DAVINCI_RPC_URL are expected in the inspect; any other listed one is a leak.
ok = ("$OP_PRIVATE_RPC", "https://optimism-rpc.publicnode.com", "https://mainnet.optimism.io")
rpcs = [l.strip() for l in open(sys.argv[4]) if l.strip() and l.strip() not in ok]
for f in sys.argv[5:]:
    t = open(f, errors="replace").read()
    print(f.rsplit("/", 1)[1], "key" if any(k in t.lower() for k in keys) else "no key", "rpc" if any(r in t for r in rpcs) else "no rpc")
EOF
install -m 600 $TMPDIR/op-inspect.json ~/.davinci-op/container-inspect-v0.6.0.json && rm $TMPDIR/op-inspect.json $TMPDIR/op-logs.txt
```

Expected: `op-inspect.json no key no rpc`, `op-logs.txt no key no rpc` (the three `DAVINCI_RPC_URL` endpoints are in the inspect by design and are not counted).

- [ ] **Step 6: A small live election through the production node**

Spend: the owner's OP account for create and end (well under 0.0001 ETH), `seq-a` for one blob post and two OP transactions.

```bash
. ~/.config/davinci-l1blob/op-core.sh
cd /home/p4u/davinci-sequencer
DAVINCI_E2E_NODE_URL=http://127.0.0.1:9090 DAVINCI_E2E_LIVE=1 DAVINCI_E2E_NETWORK=optimism \
  DAVINCI_E2E_RPC=$OP_PUB DAVINCI_E2E_ORGANIZER_KEY=$HOME/optimism-privkey.txt TMPDIR=$HOME/.cache/davinci-e2e \
  cargo test -p davinci-e2e --test node_smoke -- --nocapture 2>&1 | redact > ~/.cache/davinci-e2e/node-smoke.log
grep -E "process 0x|settled|results on-chain|panicked|test result" ~/.cache/davinci-e2e/node-smoke.log
docker logs davinci-sequencer-op 2>&1 | grep -E "batch sealed|blobs posted on L1|L1-blob transition sent" | tail -3 | cut -c1-220
```

Expected (about 10 to 15 minutes, most of it the 300 s default grace): `process 0x...`, `3 votes settled`, `results on-chain = tally [...]`, `test node_smoke ... ok`; the node's `batch sealed ... votes=3 ... reason=flush`, `blobs posted on L1 ... blobs=1`, `L1-blob transition sent ... l1_txs=1`. On failure the test cancels its process; read the container log and report.

- [ ] **Step 7: The public endpoint**

```bash
curl -s https://sequencer2.davinci.vote/info; echo
```

Expected: the same `/info` as Step 4: the owner confirmed `sequencer2.davinci.vote` routes to this host (`10.200.0.26:9090`), so the public endpoint is checked after the switch. Anything else is reported to the owner as is.

- [ ] **Step 8: Report**

Container name, image and update policy (`:latest` with the watchtower label: it follows releases), datadir, the boot lines, the smoke election's process id and times, the spend of Steps 1-6, and the reminder that `seq-a`'s keys now belong to production.

---

## Owner checkpoints

**Decisions already taken by the owner (resolved, no open questions remain):** production OP node on `ghcr.io/vocdoni/davinci-sequencer:latest` with the watchtower label (it follows releases; a release that changes required configuration must ship with upgrade notes before its tag); production `DAVINCI_RPC_URL=$OP_PRIVATE_RPC,https://optimism-rpc.publicnode.com,https://mainnet.optimism.io` (the committed preset unchanged); the contracts PR merged with `gh pr merge --rebase --admin` after green CI and the final Fable audit (the PR body says the merge relied on it); spending caps as stated, `seq-a` keeps what is left for production (no production top-up); the blob-mode GPU e2e is rerun (Task 9 Step 4); `sequencer2.davinci.vote` routes to this host and Task 14 checks it; the live 512 x 16 runs whenever the fee gates pass (skipped only on fees, said so in the report); `l1_big` keeps the 600 s ceiling grace and prints its measured seconds per vote after phase 1; the controller tells the owner before each hour-long GPU run (Tasks 9 and 12).

In order; each needs the owner's explicit yes on the command shown, and nothing proceeds past one without it.

1. **Deploy the OP registry** (Task 11 Step 5): one creation from the owner's account on OP, at most 0.0001 ETH; simulation output, nonce and balance shown first.
2. **Top up `seq-a.l1`** (Task 12 Step 3 or 6, only if its balance is below 0.0002 ETH): at most 0.0005 ETH from the owner's Ethereum account.
3. **Run `op_live`** (Task 12 Step 3): about 0.00004 to 0.0001 ETH of blob posts, organizer transactions on OP; fee and balance go/no-go first.
4. **Run the live 512 x 16 transition** (Task 12 Step 6): at most about 0.00015 ETH of blob posts; skipped if fees or the 0.0003 ETH total for `seq-a.l1` do not allow it.
5. **Push davinci-contracts `feat/op-core` and open its PR** (Task 13 Step 3), after the final audit.
6. **Merge the davinci-contracts PR by rebase with `--admin`** (Task 13 Step 4), after CI is green and the final Fable audit passes (resolved: the owner chose the admin bypass; the PR body says the merge relied on that audit).
7. **Release davinci-contracts v0.0.51** (Task 13 Step 5): push `main` to `release` (the bot bumps and tags), fast-forward `main` to the bump commit, replace the generated release notes.
8. **Push davinci-sequencer `feat/op-core` with `CONTRACTS_REF: v0.0.51` and version 0.6.0, and open its PR** (Task 13 Steps 6-7).
9. **Merge the davinci-sequencer PR by rebase** (Task 13 Step 8).
10. **Push davinci-zkvm `main`** with the fixture commit (Task 13 Step 9; `:main` image only).
11. **Remove `davinci-sequencer-1`** (Task 13 Step 10), before the tag: its inspect saved, keys and data kept.
12. **Tag and push davinci-sequencer v0.6.0, publish its release** (Task 13 Step 11): moves `ghcr.io/vocdoni/davinci-sequencer:latest`.
13. **Switch production** (Task 14 Step 2): the go for the outage (the image is decided: `:latest` with the watchtower label); then the start and the small live election (organizer and `seq-a` spend, well under 0.0001 ETH each).

The GPU runs of Tasks 9 and 12 use the shared production prover; they need no confirmation while the Gnosis node is stopped and the queue is empty, but the controller tells the owner before each hour-long run (Task 9 Steps 3-5, Task 12 Steps 4 and 7) and stops for the owner otherwise.

## Self-review

**Spec coverage.**

| Spec | Tasks |
|---|---|
| §1 goal; §2 decisions (production node, grace 300/240/600/1800/60, Gnosis retired, 512-vote batches) | 3, 5, 11, 13, 14 |
| §3 node: two caps, chunking, window-1 posting of chunks, per-chunk seal guard, depth from the oldest, origin and record against the newest, single-chunk and full re-posts, `MAX_REPOSTS` per transition, restart with one chunk pending | 1, 2, 3 (tests in 2, 3 and 4) |
| §4 registry: reused verifiers, v0.2.0 pins, grace, simulate, `.env` check, `deploy_all.sh`, `verify_deployment.py --grace`, secret scan, preset and docs, `0x8b63…` retired, boot budget 110 + 30 ≤ 240 | 8, 11 (budget test in 3) |
| §5 Gnosis retired (default `optimism`, preset removed, docs, tests, compose, e2e, client `DEFAULT`) | 5 |
| §6 leftovers: davinci-zkvm fixture and `BATCH_PROGRAM_VK` assertion; contracts defaults, docs and bindings; CI pins | 8, 10 (CI pins already on the branches, kept through the rebases of Tasks 1 and 7) |
| §7 validation: unit and fake-chain tests, contract tests (two transactions, re-posted order, `TransitionL1`), ZISKEMU dry run above 6 blobs, `ANVIL=1` two-anvil test, GPU e2e plus 512 x 16 on a production-grace anvil, `op_live` and live 512 x 16, Fable audits | 2, 3, 4, 7, 9, 12, 13 Step 1 |
| §8 merge and release: PRs, rebase merges, contracts first and tagged v0.0.51 with CHANGELOG and package.json, `CONTRACTS_REF: v0.0.51`, v0.6.0, Gnosis node off `latest` before the tag, release notes, owner confirmations | 13 |
| §9 production switch: how the node runs, the change prepared without secrets, fresh datadir, applied on confirmation, boot lines, `/info`, a small live election | 14 |
| §10 risks: fee spike between chunks, origin far behind, tight floor, `:latest` moving, interleaved posts, data loss | Review Focus 1-3 and 5 (Tasks 2-3), Task 13 Step 10, Task 14 (fresh datadir, Gnosis data untouched) |

**Placeholder scan.** Values known only at run time are named once and read from their source by the commands: the new registry and block (`deployments/10.json`, read by `fill_op_core.py`, `changelog_v0051.py`, `notes.py` and the commit-message commands), the recorded job's publics (read from the log by `wire_fixture.py`), PRs (addressed by branch). Expected outputs show them as `0x<B>`, `<BLOCK>`, `<k>`, `<s>`.

**Consistency.** `select_batch_within(pending, max_voters, must_include, max_votes, max_blobs)` (Task 1) is what Task 3's actor calls; `L1Da::new(l1, db, fee_cap, poll, chunk)`, `chunks`, `Posting { posts, error, recorded }`, `post_chunks` (Task 2) are what `settle`, the boot (Task 3) and the tests use; `choose_anchor(oldest, newest, head, origin, recorded)` has five arguments in `settle.rs`, `l1da.rs` and `web3_anvil.rs`; `POST_TIME`, `batch_blob_cap`, `chunks_fit` (Task 3) are the names the actor, the boot log and the docs use; `grace_args` and the `DAVINCI_E2E_GRACE`, `DAVINCI_E2E_L1_BIG`, `DAVINCI_E2E_RECORD_DIR`, `DAVINCI_E2E_NODE_URL` variables (Task 6) are what Tasks 9, 12 and 14 set; the `recorded job` line format (Task 6) is what `wire_fixture.py` parses.


**Checked while writing (scratch clones under `~/.cache`, real trees untouched).** Tasks 1-8 applied in order to clones of both branches: `davinci-state` 22 transition tests, `l1da` 32 and `actor_l1` 16 sequencer tests, the workspace suites, `ANVIL=1 web3_anvil` (filtered to `l1_blob_mode`: 4 passed, including the two-transaction settlement), the nine-blob `ZISKEMU=1` dry run (275 s cold), forge 370 tests (worst-case proof 47,168 bytes, 3,491,169 gas), the Go vector generator's checksums, `verify_deployment.py --grace` on a local deployment, `build_all.sh` with abigen v1.17.8, the Gnosis retirement and docs scripts reproducing the scratch commits, `fill_op_core.py` on a made-up record, `wire_fixture.py` on a clone of davinci-zkvm (it fails only at the restored assertion with the v0.1 fixture), `changelog_v0051.py` and `notes.py` on the retired record, the CI pin `sed`, the workspace version bump (`cargo update --workspace --offline`: five crates), the `davinci-client` `networks` and `davinci-sequencer` `config` unit tests, and a read-only `verify_deployment.py` of the retired OP registry against this branch's build (19 OK, 0 FAIL). Not checked: anything that needs the GPU prover, a transaction on OP or Ethereum, a push, or a container change (Tasks 9-14).
