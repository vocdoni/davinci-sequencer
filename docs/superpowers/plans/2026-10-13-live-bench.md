# Live Benchmark Campaign Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Benchmark DAVINCI live on OP Mainnet in L1-blob mode, with two competing sequencers (A on
z6's prover, B on z7's). The benchmark covers:

- realistic and overload elections;
- the throughput ceiling, by nf and batch size;
- forced races;
- chaos;
- a no-vote-lost audit;
- a TypeScript SDK bench.

Reports are written continuously.

**Architecture:** A `davinci-bench` binary in the e2e crate. Its steps are `keygen`, `fund`, `gate`,
`stand-in`, `plan`, `gen`, `merge`, `run`, `audit`, `serve`, `cancel` and `dry`.

- **Ballots.** They come from a campaign seed and are proved on both hosts in shards. They are merged
  on z6.
- **Rig.** It runs the real node binary as A, B and an observer, on fixed ports with persistent
  datadirs. Every upstream URL sits behind a loopback proxy.
- **Driver.** A scheduler and submitter write an append-only ledger. The audit classifies every vote
  against vote ids decoded from the chain's blobs, once the observer holds every transition and
  neither signing node has a pending vote.
- **Node.** The rig runs a `davinci-sequencer` that carries Ruling 20 (spec §11, "How the node errors
  a vote"): a prover-side job failure requeues and bisects proof-first, node-side mismatches hold the
  votes, and a failed results proof is retried.

**Tech Stack:** Rust 1.95 (edition 2024), tokio, alloy 2.5, clap 4, sha2, davinci-zkvm `rust-sdk`
(ZisK 1.3.2 pins from Track A), circom artifacts in `/home/p4u/davinci-circom/artifacts`, Foundry
(`~/.foundry/bin`), systemd user scopes. The TS bench uses Node 22, tsx and the SDK in `~/davinci-sdk`.
Python 3 (stdlib, run with `-I`) is for the secret scan.

**Spec:** `docs/superpowers/specs/2026-10-13-live-bench-design.md`. Read it all before Task 1. §5
(ballots), §8 (the S cells), §9 (races), §11 (audit) and §13 (spend) bind the tasks.

## Global Constraints

- **Order.** Tasks 1-2, 6-11, 16, 19 and 22 start now. Every task that runs a prover or the chain
  (3-5 for the artifacts; 12-15, 17-18, 20-21, 23-24) starts once Track A is done (the 1.3.2
  provers on z6 and z7, the new OP registry, the pins) **and** the node binary carries Ruling 20
  (prover-side `Failed` → requeue, proof-first bisection, `Held` for check mismatches; its
  actor tests pass). `DAVINCI_SEQUENCER_BIN` is that binary; `gate` prints its sha256. The
  registry address and its block come from `/home/p4u/davinci-contracts/deployments/10.json` and
  become `DAVINCI_E2E_REGISTRY` and `DAVINCI_E2E_FROM_BLOCK`. Track A (ZisK 1.3.2) is still in
  progress, so the new registry and verifier addresses are not known yet: nothing hard-codes them,
  and `Net::open` prints the verifiers it reads from the registry, which must equal the deployment
  file's.
- **Provers.** Node A proves on z6's prover (`http://127.0.0.1:8080`), node B on z7's
  (`http://10.200.0.27:8080`), which runs a CI-built davinci-zkvm image. `ZISK_MINIMAL_MEMORY_FROM`
  is 256 on both (2048 OOM-killed nf-16 batches on z6). The owner allowed changes to z7's compose file
  and `.env`; z6's production prover is not changed by this plan.
- **Branches.** davinci-sequencer gets `feat/live-bench` from `feat/op-core`'s head now (the tasks
  that start now need neither Track A nor Ruling 20). Before the first task that waits (Task 3) it is
  rebased on `feat/op-core` once that carries Track A's last commit and the Ruling 20 change
  (`git merge-base --is-ancestor <ruling-20 commit> HEAD`, the commit recorded in `progress.md`).
  davinci-sdk gets `feat/op-bench` from its `HEAD`. Commits are local only: nothing is pushed, merged
  or tagged, and nothing needs a push, because z7 gets a copied binary.
- **Commits.** Titles are `type(scope): lowercase title`, e.g. `feat(e2e): ...`, `test(e2e): ...`,
  `feat(bench): ...` in davinci-sdk. No AI attribution and no trailers. Stage files by name.
- **Build output never in `/tmp`.** Use `TMPDIR=$HOME/.cache/davinci-bench/tmp`. Campaigns live in
  `~/.cache/davinci-bench/<campaign>` (mode 0700). Binaries go in `~/.cache/davinci-bench/bin/`.
- **Secrets.**
  - Never printed, never logged, never in an argv, never in a committed file: `~/optimism-privkey.txt`,
    `~/optimism-rpcs.txt`, `~/.config/davinci-l1blob/*.key` (seq-a and the new seq-b), and every
    campaign's `secret/seed`.
  - Keys travel only as files, read in-process. Private RPC URLs reach only the harness's environment,
    through `~/.config/davinci-l1blob/bench.sh` (Task 14).
  - `scan_secrets.py` (copied from `.superpowers/sdd/2026-10-13-op-core-stack/task-12-brief.md` to
    `~/.cache/op-core/scan_secrets.py` if missing) runs after every live step.
- **GPU.** Before every GPU step, `curl -s <prover>/health` must show `"queue_len":0` on both provers.
  Each prover runs one benchmark workload at a time. Every dry or live run uses a
  `davinci-sequencer` built from a tree that carries Ruling 20; a run on an older binary is void.
- **Memory.** Steps run in named scopes: `systemd-run --user --scope --unit davinci-bench-<step>`, with
  `-p MemoryMax=12G` for `dry`, `run` and `serve` and `16G` for `gen`. `gen` never runs on z6 while a
  GPU step runs.
- **Live spend.**
  - Caps: seq-a.l1 and seq-b.l1 at most 0.005 ETH each; seq-a.op and seq-b.op at most 0.0005 ETH
    each; the organizer (OP) at most 0.002 ETH; each post at most `0.00015eth` (`--l1-max-post-fee`).
  - Fee gate: base fee plus tip at most 0.5 gwei, and blob base fee at most 0.02 gwei.
  - Stop rule: spend at a cap stops the run (spec §13).
  - Each live step waits for its owner checkpoint (below).
- **Grace.** The OP registry's grace is `300/240/600/1800/60`. Every `dry` runs on two anvils with
  `DAVINCI_E2E_DA=l1-blobs DAVINCI_E2E_GRACE=300,240,600,1800,60`.
- **Reviews.** A strict Fable 5.1 audit per task, and one final audit before the report.
- **Reporting.** Besides the reports the binary writes, after every scenario (dry or live) the
  controller appends one line to `.superpowers/sdd/2026-10-13-op-core-stack/progress.md`: what ran,
  the audit verdicts (`no_vote_lost`, `clean`, `tally_ok`, `results_ok`, `counters_ok`), findings by
  kind, and the spend.

## Owner checkpoints

1. seq-b keys and funding amounts, and whether to plan and generate all three waves up front (Task 14).
2. Wave 1 go (Task 15).
3. Wave 2 go (Task 18).
4. Wave 3 go (Task 21).
5. TS live runs go (Task 23).

Each go shows `davinci-bench gate` (fees, balances, spend against caps, prover health), the expected
spend and the prover hours. Then wait for an explicit "go wave N".

## Decisions beyond the spec

- **A CLI binary, not gated tests.** One file is copied to z7 and runs under `systemd-run` without the
  test harness. The only gated test is the circom-backed `tests/bench_gen.rs`.
- **40902 is always retried.** The actor checks the vote id before the slot depth, so a resend whose
  first attempt got in answers 40901, never 40902.
- **Per-vote latency comes from the chain.** A vote's transition gives its seal, proof, L1 and OP
  times, so the run never polls per vote.
- **`rfc3339` moves** from `e2e/tests/throughput.rs` to `davinci_e2e::bench::metrics`; `throughput.rs`
  imports it.
- **The proxy forwards `/` to the upstream URL exactly.** Private RPC URLs carry a path, and
  `https://h/optimism/` may 404.
- **A refusal on a resend is uncertain** (except 40901, which means the first attempt got in): the
  first attempt may have admitted the vote and the retry met a later condition.
- **A stale `error` is not a lost vote.** A vote in exactly one transition is `Settled` whatever any
  node reports; nodes still reporting `error` are listed in `stale_error`, a finding under `clean`.
- **Ceiling cells share the nodes.** The cells of one nf run in ascending B with `batch_max 512` and
  no restart, so the proving estimate converges once per nf; each burst is sent at the previous one's
  seal, so the 60 s timer stays off the critical path (spec §8).

## Review focus

1. The audit's verdict order (spec §11): `Inconsistent` before `Settled`, `stale_error` under `Both`,
   `NeverAdmitted` against `Lost` for uncertain sends, a refusal on a resend as uncertain, and a
   `process closed (held: …)` reason never matching an allowed `process closed`. The audit waits for
   O's full sync and `pendingVotes == 0` before it classifies.
2. Only `127.0.0.1` URLs reach node environments, logs and reports. Upstreams stay in proxy memory.
3. The race preconditions: the chain root (read from OP), nothing pending, provers idle; the seal
   gap is recorded and a round above 6 s does not count.
4. The budget stop rule and the hard caps in `fund`.
5. Shard atomicity: the `.jsonl` is renamed into place before the meta is written, and the meta is the
   done marker.

---

## Shell setup (every task)

```bash
cd /home/p4u/davinci-sequencer
mkdir -p $HOME/.cache/davinci-bench/tmp $HOME/.cache/davinci-bench/bin
export TMPDIR=$HOME/.cache/davinci-bench/tmp PATH=$HOME/.foundry/bin:$PATH
```

For live tasks, also run `. ~/.config/davinci-l1blob/bench.sh` (Task 14 Step 1). It prints nothing.
Run verification pipelines through `rtk proxy bash -c '…'`; the `Expected:` lines below are raw
output. Every task ends with:

```bash
cargo fmt --all -- --check && cargo clippy -p davinci-e2e --all-targets -- -D warnings
```

## File Structure

davinci-sequencer, branch `feat/live-bench`:

- **Create** `e2e/src/bench/{mod,plan,ballots,gen,merge,rig,schedule,ledger,submit,chain_view,audit,metrics,report,money,scenarios,run,chaos,race}.rs`,
  `e2e/src/bin/davinci-bench.rs` and `e2e/tests/bench_gen.rs`.
- **Modify:**
  - `e2e/src/lib.rs` (`pub mod bench;`);
  - `e2e/Cargo.toml` (`clap`, `sha2`, `[[bin]]`);
  - `e2e/src/node.rs` (`min_mix`, `Node::start_on`);
  - `e2e/src/proxy.rs` (`target`, `set_delay`, `set_fail`);
  - `e2e/tests/throughput.rs` (imports `rfc3339`).
- **Create** `docs/superpowers/reports/<date>-live-bench.md` (Task 24).

davinci-sdk, branch `feat/op-bench`: create
`examples/bench/{package.json,tsconfig.json,src/{bench,config,prove-worker,schedule,ledger,stats}.ts,test/{schedule,ledger,stats}.test.ts}`.

Outside the repositories, never committed:

- `~/.config/davinci-l1blob/bench.sh` and the seq-b key files;
- `~/.cache/davinci-bench/`;
- on z7, `~/davinci-bench/{bin,artifacts,<campaign>}`.

---

### Task 1: Campaign seed, derivations and plan types — needs: nothing

**Files:**
- Create: `e2e/src/bench/mod.rs`, `e2e/src/bench/plan.rs`
- Modify: `e2e/src/lib.rs` (`pub mod bench;`), `e2e/Cargo.toml` (`clap.workspace = true`, `sha2.workspace = true`)

**Interfaces (produces):**
```rust
#[serde(rename_all = "lowercase")] pub enum NodeId { A, B }
pub struct Seed([u8; 32]);                 // Debug prints "Seed(..)"
impl Seed {
    pub fn create(path: &Path) -> Result<Seed>;   // 32 OsRng bytes as hex, create_new, 0600
    pub fn load(path: &Path) -> Result<Seed>;     // errors name the path, never the content
    pub fn voter_key(&self, tag: &str, v: u32) -> SigningKey;
    pub fn vote_k(&self, tag: &str, v: u32, round: u8) -> Fr;          // StdRng from the hash, then random_k
    pub fn revoter(&self, tag: &str, v: u32, permille: u16) -> bool;
    pub fn csp_key(&self, wave: u32) -> SigningKey;
    pub fn choices(&self, tag: &str, v: u32, round: u8, nf: u8) -> Vec<u64>; // 1..=6 units, each field ≤ 5
}
// Every derivation: sha256("davinci-bench/v1/" ‖ label ‖ 0 ‖ seed ‖ tag ‖ 0 ‖ be(v) ‖ round)
pub struct ElectionSpec { pub tag: String, pub scenario: String, pub nf: u8, pub origin: u8, pub voters: u32,
    pub rounds: u8, pub revote_permille: u16, pub key_node: NodeId, pub grace: u32 }
impl ElectionSpec { pub fn max_voters(&self) -> u32; } // the registry's maxVoters; create_wave passes exactly this
pub struct ProcSnap { pub pid: ProcessId, pub encryption_key: String /* compressed hex */,
    pub ballot_mode: BallotMode, pub census_origin: u8, pub census_root: String /* decimal */ }
impl ProcSnap { pub fn of(p: &OnchainProcess) -> ProcSnap; pub fn onchain(&self) -> Result<OnchainProcess>; }
pub struct Election { pub spec: ElectionSpec, pub census_uri: String, pub census_contract: Option<Address>, pub snap: ProcSnap }
impl Election { pub fn ballots(&self, seed: &Seed) -> Vec<(u8, u32)>; } // (round, voter): round 1 all, ≥2 revoters
pub struct Wave { pub n: u32, pub created: u64, pub elections: Vec<Election> }
pub struct Plan { pub campaign: String, pub chain_id: u64, pub registry: Address, pub start_block: u64, pub waves: Vec<Wave> }
impl Plan {
    pub fn load(dir: &Path) -> Result<Plan>;
    pub fn save(&self, dir: &Path) -> Result<()>;            // plan.json.tmp, then rename
    pub fn wave(&self, n: u32) -> Result<&Wave>;
    pub fn stand_in(seed: &Seed, n: u32, specs: &[ElectionSpec]) -> Result<Plan>; // local keygen, no chain
}
```

- [ ] **Step 1: Write the failing tests** in `plan.rs`:
  - `derivations_are_deterministic_and_separated`: the same inputs give the same key and `k`. Another
    tag, voter or round gives another one. 1000 voter addresses are distinct.
  - `choices_fit_every_ballot_mode`: for nf 2..=16, voters 0..200 and rounds 1..=3, each choice
    vector has length nf, every field is at most 5, the sum is in 1..=6, and
    `fx::ballot_mode_nf(nf)` with weight 10 accepts it. At nf 2, round 2 differs from round 1 for at
    least 80 % of voters.
  - `revoter_rate`: 10,000 voters at 150 ‰ give 1500 ± 100 revoters.
  - `seed_file_is_private`:
    - the file has mode `0o600`;
    - a second `create` fails;
    - `load` returns the same seed;
    - the error for a bad file and the `{:?}` output contain no hex of the seed.
  - `snapshot_round_trip`: an `OnchainProcess` with a random key and `ballot_mode_nf(8)` goes through
    `of`, then JSON, then `onchain`, and gets back the same `id`, `encryption_key`, `ballot_mode`,
    `census_origin` and `census_root`.
  - `plan_round_trip`: `stand_in` (2 elections), then `save`, then `load`, gives an equal plan.
- [ ] **Step 2: Run them.** Run `cargo test -p davinci-e2e --lib bench::plan`. Expected: they fail to
  compile.
- [ ] **Step 3: Implement** `plan.rs` against the interfaces. `Fr` is serialized as decimal and the
  point as compressed hex.
- [ ] **Step 4: Run the tests again.** Expected: `test result: ok. 6 passed`.
- [ ] **Step 5: Commit.**
  `git add e2e/Cargo.toml e2e/src/lib.rs e2e/src/bench/mod.rs e2e/src/bench/plan.rs && git commit -m "feat(e2e): bench campaign seed, derivations and plan"`

### Task 2: Ballot shard format and host assignment — needs: nothing

**Files:**
- Create: `e2e/src/bench/ballots.rs`

**Interfaces (produces):**
```rust
pub struct BallotRecord { pub tag: String, pub voter: u32, pub round: u8, pub choices: Vec<u64>, pub req: VoteRequest }
#[serde(rename_all = "camelCase")]
pub struct ShardMeta { pub wave: u32, pub tag: String, pub round: u8, pub from: u32, pub to: u32, pub count: u32,
    pub sha256: String, pub host: String, pub bin_sha256: String, pub wasm_sha256: String, pub zkey_sha256: String, pub secs: f64 }
pub struct ShardId { pub tag: String, pub round: u8, pub from: u32, pub to: u32 } // voters [from, to)
pub const SHARD: u32 = 500;
pub fn shards(wave: &Wave, seed: &Seed, size: u32) -> Vec<ShardId>;          // by (tag, round, from); non-empty only
pub fn assigned(all: &[ShardId], host: usize, hosts: usize) -> Vec<ShardId>; // index % hosts == host
pub fn shard_path(campaign: &Path, wave: u32, s: &ShardId) -> PathBuf;     // shards/w<N>/<tag>/r<round>-<from>-<to>.jsonl
pub fn write_shard(path: &Path, recs: &[BallotRecord], meta: ShardMeta) -> Result<()>; // .tmp, rename, then meta
pub fn read_shard(path: &Path) -> Result<(ShardMeta, Vec<BallotRecord>)>;  // sha256 vs the sidecar before parsing any line, then count
pub fn done(path: &Path) -> bool;
pub fn sha256_file(path: &Path) -> Result<String>;
#[cfg(test)] pub fn fake_req(vid: u64, address: [u8; 20]) -> VoteRequest;  // identity ballot, zero proof/signature
```

- [ ] **Step 1: Write the failing tests:**

```rust
#[test]
fn shard_round_trip_and_tamper() {
    let dir = tempfile::tempdir().unwrap();
    let recs: Vec<BallotRecord> = (0..3u32)
        .map(|v| BallotRecord { tag: "R1".into(), voter: v, round: 1, choices: vec![1, 2],
                                req: fake_req(1 << 63 | v as u64, [v as u8; 20]) })
        .collect();
    let p = dir.path().join("r1-0-3.jsonl");
    write_shard(&p, &recs, meta("R1", 1, 0, 3)).unwrap();
    let (m, back) = read_shard(&p).unwrap();
    assert_eq!((m.count, &back), (3, &recs));
    assert!(done(&p));
    let good = std::fs::read(&p).unwrap();
    let mut bad = good.clone();
    bad[20] ^= 1;
    std::fs::write(&p, &bad).unwrap();
    assert!(read_shard(&p).unwrap_err().to_string().contains("sha256"));
    assert!(!done(&p));
    // A dropped line with a matching sha256 still fails the count.
    let short: Vec<u8> = good.split(|b| *b == b'\n').take(2).flat_map(|l| [l, b"\n".as_slice()].concat()).collect();
    std::fs::write(&p, &short).unwrap();
    let mut m2 = m.clone();
    m2.sha256 = sha256_file(&p).unwrap();
    std::fs::write(p.with_extension("jsonl.meta.json"), serde_json::to_vec(&m2).unwrap()).unwrap();
    assert!(read_shard(&p).unwrap_err().to_string().contains("count"));
}

#[test]
fn hosts_partition_the_shards() {
    let seed = Seed::from_bytes([7; 32]);
    let plan = Plan::stand_in(&seed, 1, &[spec("A", 2, 1, 1200, 2, 150), spec("B", 16, 4, 300, 1, 0)]).unwrap();
    let all = shards(plan.wave(1).unwrap(), &seed, SHARD);
    for hosts in 1..=3 {
        let mut got: Vec<_> = (0..hosts).flat_map(|h| assigned(&all, h, hosts)).collect();
        got.sort_by(|a, b| (&a.tag, a.round, a.from).cmp(&(&b.tag, b.round, b.from)));
        assert_eq!(got, all);
    }
    // Every planned ballot falls in exactly one shard.
    for e in &plan.wave(1).unwrap().elections {
        for (r, v) in e.ballots(&seed) {
            let n = all.iter().filter(|s| s.tag == e.spec.tag && s.round == r && (s.from..s.to).contains(&v)).count();
            assert_eq!(n, 1, "{} r{r} v{v}", e.spec.tag);
        }
    }
}
```

  (`meta()` and `spec()` are small test helpers. `Seed::from_bytes` is `#[cfg(test)]`.)
- [ ] **Step 2: Run them.** Run `cargo test -p davinci-e2e --lib bench::ballots`. Expected: they fail
  to compile.
- [ ] **Step 3: Implement.** One `serde_json` line per record, then `\n`. Write the meta last.
  `read_shard` hashes the file and compares it to the sidecar *before* parsing a line, so a flipped
  byte fails with `sha256` even when it breaks the JSON.
- [ ] **Step 4: Run the tests again.** Expected: `test result: ok. 2 passed`.
- [ ] **Step 5: Commit.**
  `git add e2e/src/bench/ballots.rs e2e/src/bench/mod.rs && git commit -m "feat(e2e): bench ballot shard format and host assignment"`

### Task 3: `gen` and the CLI skeleton — needs: CPU (gated test uses the circom artifacts), after Track A

**Files:**
- Create: `e2e/src/bench/gen.rs`, `e2e/src/bin/davinci-bench.rs`, `e2e/tests/bench_gen.rs`
- Modify: `e2e/Cargo.toml` (`[[bin]] name = "davinci-bench"`, `path = "src/bin/davinci-bench.rs"`)

**Interfaces (produces):**
```rust
pub enum CensusSource { Merkle(LeanImt), Csp(SigningKey) }
pub fn census_source(e: &Election, seed: &Seed, wave: u32) -> Result<CensusSource>; // origins 1, 3: (addr, 10) in voter order; 4: CSP
pub struct GenOpts { pub host: usize, pub hosts: usize, pub threads: usize, pub host_name: String, pub artifacts: PathBuf }
pub struct GenStats { pub shards: usize, pub skipped: usize, pub ballots: usize, pub secs: f64 }
pub fn gen_wave(campaign: &Path, plan: &Plan, wave: u32, seed: &Seed, prover: &BallotProver, o: &GenOpts) -> Result<GenStats>;
```

CLI (clap derive). Every command takes `--campaign DIR`, default `$DAVINCI_BENCH_DIR`. This task adds:

- `stand-in --wave N --elections "tag:nf:origin:voters:rounds:permille,..."`: writes `secret/seed`
  (dir 0700) and a stand-in `plan.json`.
- `gen --wave N --host I --hosts H [--threads 8] [--artifacts DIR]`:
  - the artifacts default to `$CIRCOM_ARTIFACTS`;
  - it skips `done` shards and builds `fx::Job`s from `snap.onchain()`;
  - the census witness is `fx::merkle_witness`, or for CSP `csp_sign(csp, pid, addr, 10, voter)`;
  - it proves with `fx::prove_all_on` and writes each shard with its meta;
  - every 30 s it prints `gen host I/H: s/S shards, n ballots, r/s, eta m min`.

- [ ] **Step 1: Write the failing gated test** `tests/bench_gen.rs::gen_two_hosts_offline`. It skips
  unless `CIRCOM_ARTIFACTS` is set. It builds a stand-in plan with `G2:2:1:6:2:500` and
  `G16:16:4:4:1:0`, runs `gen_wave` as host 0 of 2 and as host 1 of 2 into one dir, and asserts the
  following:
  - every shard is `done`;
  - every ballot's proof verifies with `BallotVerifier::from_snarkjs_json(release::ballot_vk_json())`;
  - `vote_id_recover` gives the seed's voter address;
  - a second `gen_wave` returns `skipped == shards`.
- [ ] **Step 2: Run it.** Run
  `CIRCOM_ARTIFACTS=/home/p4u/davinci-circom/artifacts cargo test -p davinci-e2e --test bench_gen -- --nocapture`.
  Expected: it fails to compile.
- [ ] **Step 3: Implement** `gen.rs` and the two CLI commands.
- [ ] **Step 4: Run it again.** Expected: `test result: ok. 1 passed`, with about 15 ballots proved in
  under 10 s.
- [ ] **Step 5: Commit.**
  `git add e2e/Cargo.toml e2e/src/bench/gen.rs e2e/src/bench/mod.rs e2e/src/bin/davinci-bench.rs e2e/tests/bench_gen.rs && git commit -m "feat(e2e): davinci-bench gen proves ballot shards from a campaign seed"`

### Task 4: `merge` — needs: CPU, after Track A

**Files:**
- Create: `e2e/src/bench/merge.rs`
- Modify: `e2e/src/bin/davinci-bench.rs` (`merge`), `e2e/tests/bench_gen.rs`

**Interfaces (produces):**
```rust
pub enum Verify { Full, Skip }
pub struct ElectionIndex { pub ballots: usize, pub rounds: BTreeMap<u8, usize>, pub sha256: String }
pub fn check_record(e: &Election, seed: &Seed, r: &BallotRecord, v: Option<&BallotVerifier>) -> Result<()>;
    // pid == snap.pid, address == seed voter, choices == seed.choices, weight 10, signature recovers, proof (Full)
pub fn merge_wave(campaign: &Path, plan: &Plan, wave: u32, seed: &Seed, v: Verify) -> Result<BTreeMap<String, ElectionIndex>>;
    // every planned (tag, round, voter) exactly once, sidecar sha256, unique vote ids;
    // writes ballots/w<N>/<tag>.jsonl in (round, voter) order and ballots/w<N>/index.json; proofs on N threads
pub async fn check_snapshots(plan: &Plan, wave: u32, rpc: &str) -> Result<()>; // live: registry == snap
```

- [ ] **Step 1: Write the failing tests.** Unit tests in `merge.rs`, using `Verify::Skip` and fake
  records from `fake_req` written as shards:
  - `merge_reports_missing_and_duplicates`:
    - a missing shard fails, and the error names its tag, round and range;
    - one vote id in two shards fails with `duplicate vote id`;
    - a foreign pid fails with `pid`.
  - `merge_writes_round_voter_order`: the merged file is sorted by `(round, voter)`, and `index.json`
    counts each round.

  In `bench_gen.rs`, extend the gated test: `merge_wave(.., Verify::Full)` succeeds. Then one shard's
  `pi_a` is replaced by `pi_c`, with the sidecar sha256 recomputed, and the merge fails with `proof`.
- [ ] **Step 2: Run them.** Run `cargo test -p davinci-e2e --lib bench::merge`, then the gated command
  from Task 3. Expected: they fail.
- [ ] **Step 3: Implement** `merge.rs` and the CLI command.
- [ ] **Step 4: Run them again.** Expected: `ok. 2 passed`, and the gated test `ok. 1 passed`.
- [ ] **Step 5: Commit.**
  `git add e2e/src/bench/merge.rs e2e/src/bin/davinci-bench.rs e2e/tests/bench_gen.rs && git commit -m "feat(e2e): davinci-bench merge verifies and joins the shards of both hosts"`

### Task 5: Two-host generation dry run on a stand-in plan — needs: ssh to z7 and both CPUs (no chain, no GPU), after Track A

**Files:** none (a failure leads to a fix commit with a test).

- [ ] **Step 1: Build once and record the binary.**

```bash
cargo build --release -p davinci-e2e --bin davinci-bench 2>&1 | tail -1
B=$HOME/.cache/davinci-bench/bin/davinci-bench-$(git rev-parse --short HEAD); cp target/release/davinci-bench $B
sha256sum $B; ldd $B | grep -c "not found"
```
Expected: `Finished`, a sha256, and `0`.

- [ ] **Step 2: Create a stand-in campaign (about 2000 ballots).**

```bash
C=$HOME/.cache/davinci-bench/standin
$B stand-in --campaign $C --wave 1 --elections "G2:2:1:1000:2:200,G16:16:4:800:1:0"
```
Expected: `stand-in plan: 2 elections, <n> ballots, 6 shards` (n about 2000).

- [ ] **Step 3: Prepare z7.** The seed is copied as a file, never as an argument.

```bash
Z7=p4u@10.200.0.27
ssh $Z7 'mkdir -p ~/davinci-bench/bin ~/davinci-bench/artifacts ~/davinci-bench/standin/secret && chmod 700 ~/davinci-bench/standin ~/davinci-bench/standin/secret; nproc; free -g | head -2; systemctl --user is-system-running'
scp -q $B $Z7:davinci-bench/bin/
scp -q /home/p4u/davinci-circom/artifacts/ballot_proof.wasm /home/p4u/davinci-circom/artifacts/ballot_proof_pkey.zkey $Z7:davinci-bench/artifacts/
scp -q $C/plan.json $Z7:davinci-bench/standin/ && scp -qp $C/secret/seed $Z7:davinci-bench/standin/secret/
ssh $Z7 'chmod 600 davinci-bench/standin/secret/seed; stat -c %a davinci-bench/standin/secret/seed'
sha256sum /home/p4u/davinci-circom/artifacts/ballot_proof*.{wasm,zkey}
ssh $Z7 "sha256sum davinci-bench/bin/${B##*/} davinci-bench/artifacts/*; ldd davinci-bench/bin/${B##*/} | grep -c 'not found'"
```
Expected: `32`, the memory line, `running` (or `degraded`), `600`, equal sha256s on both hosts, and
`0`. If
`systemctl --user` is not running on z7, use `nohup setsid … &` in Step 4 and tell the owner (lingering
needs `loginctl enable-linger`).

- [ ] **Step 4: Generate on both hosts.**

```bash
systemd-run --user --unit davinci-bench-gen -p MemoryMax=16G -- $B gen --campaign $C --wave 1 --host 0 --hosts 2 --threads 8 --artifacts /home/p4u/davinci-circom/artifacts
ssh $Z7 "systemd-run --user --unit davinci-bench-gen -p MemoryMax=16G -- \$HOME/davinci-bench/bin/${B##*/} gen --campaign \$HOME/davinci-bench/standin --wave 1 --host 1 --hosts 2 --threads 8 --artifacts \$HOME/davinci-bench/artifacts"
journalctl --user -u davinci-bench-gen -f      # and: ssh $Z7 journalctl --user -u davinci-bench-gen -f
```
Expected: progress lines on both hosts at about 6 ballots/s, ending with `gen host 0/2 done` and
`gen host 1/2 done`.

- [ ] **Step 5: Merge on z6, scan, then delete the seed on z7.**

```bash
rsync -a $Z7:davinci-bench/standin/shards/ $C/shards/
$B merge --campaign $C --wave 1
scp -q ~/.cache/op-core/scan_secrets.py $Z7:davinci-bench/
ssh $Z7 'python3 -I davinci-bench/scan_secrets.py davinci-bench/standin/secret/seed /dev/null davinci-bench/standin/shards davinci-bench/standin/plan.json; shred -u davinci-bench/standin/secret/seed'
python3 -I ~/.cache/op-core/scan_secrets.py $C/secret/seed /dev/null $C/shards $C/ballots $C/plan.json
```
Expected: `merged 2 elections, <n> ballots, <n> proofs verify` and `<m> files, 0 with a secret` on
both hosts. Write down proofs/s per host for the report.

### Task 6: Fixed-port nodes, exact-path proxy, the rig — needs: nothing (live use in Task 12)

**Files:**
- Modify:
  - `e2e/src/node.rs`: `NodeConfig.min_mix: Option<u32>` becomes `DAVINCI_MIN_MIX`; add
    `pub async fn start_on(bin: &Path, cfg: &NodeConfig, port: u16) -> Result<Node>`, which retries
    `spawn` on the same port after `START_PAUSES`.
  - `e2e/src/proxy.rs`: `pub fn target(base: &str, path: &str) -> String`, the forwarding URL.
- Create: `e2e/src/bench/rig.rs`

**Interfaces (produces):**
```rust
pub enum Role { A, B, O }
pub struct Knobs { pub batch_max: usize, pub batch_time: String, pub min_mix: Option<u32>,
    pub confirmations: Option<u64>, pub l1_max_post_fee: Option<String> }
impl Knobs { pub fn production() -> Knobs; pub fn apply(&self, c: &mut NodeConfig); }
pub struct RigSpec { pub campaign: PathBuf, pub prover: [String; 2], pub ports: [u16; 3], pub knobs: [Knobs; 2] }
pub struct NodeProxies { pub prover: Option<Proxy>, pub op: Vec<Proxy>, pub l1: Vec<Proxy>, pub send: Vec<Proxy>, pub beacon: Vec<Proxy> }
pub struct ProveCall { pub at: f64, pub job: Option<String> }
pub struct Rig { /* nodes: [Option<Node>; 3], cfgs, proxies: [NodeProxies; 3], bin, calls */ }
impl Rig {
    pub async fn up(net: &Net, spec: RigSpec) -> Result<Rig>; // A = seq 0 / rpc 0, B = seq 1 / rpc 1, O = observer / rpc 2
    pub fn node(&self, r: Role) -> Result<&Node>;
    pub fn proxies(&self, r: Role) -> &NodeProxies;
    pub async fn kill(&mut self, r: Role) -> Result<()>;
    pub async fn start(&mut self, r: Role) -> Result<()>;
    pub async fn restart_with(&mut self, r: Role, k: Knobs) -> Result<()>;
    pub fn prove_calls(&self, r: Role) -> Vec<ProveCall>;
}
pub fn wrap_list(list: &str, urls: &[String]) -> String; // i-th entry of `list` replaced by urls[i]
```

Datadirs and logs live in `<campaign>/nodes/`, the census dir is `<campaign>/census`, and the ports
default to 9101-9103. Every URL in a node's OP, L1, L1-send and beacon lists gets its own `Proxy`; the
prover proxies are `Proxy::tapped`. `start_block` is `plan.start_block`, which is ignored on a used
datadir.

- [ ] **Step 1: Write the failing tests:**
  - `wrap_list_keeps_order` (rig): `"a, b,,c"` with `["p","q","r"]` gives `"p,q,r"`.
  - `target_keeps_a_path_upstream` (proxy):
    - `target("https://h/optimism", "/") == "https://h/optimism"`;
    - `target("https://h", "/eth/v1/beacon/blobs/1?x=y") == "https://h/eth/v1/beacon/blobs/1?x=y"`;
    - `target("http://h:8080/", "/prove") == "http://h:8080/prove"`.
  - `forwards_root_to_a_path_upstream` (proxy): the echo upstream at `<url>/optimism` sees `POST /optimism`.
  - `knobs_reach_the_config` (rig): `apply` sets `batch_max`, `batch_time`, `min_mix`,
    `confirmations` and `l1_max_post_fee`. `production()` is `512`, `"15m"`, and `None` for the rest.
- [ ] **Step 2: Run them.** Run `cargo test -p davinci-e2e --lib bench::rig proxy`. Expected: they fail.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run the tests again.** Expected: `ok`, with 4 new tests passing and the existing proxy
  tests still passing.
- [ ] **Step 5: Commit.**
  `git add e2e/src/node.rs e2e/src/proxy.rs e2e/src/bench/rig.rs e2e/src/bench/mod.rs && git commit -m "feat(e2e): bench rig with fixed-port nodes and a loopback proxy per upstream"`

### Task 7: Rate scheduler — needs: nothing

**Files:**
- Create: `e2e/src/bench/schedule.rs`

**Interfaces (produces):**
```rust
pub enum Phase { Rate { per_min: f64, secs: u64, poisson: bool }, Burst { n: usize }, Idle { secs: u64 } }
pub struct Schedule { pub phases: Vec<Phase> }
impl Schedule {
    pub fn capacity(&self) -> usize;                       // votes the phases hold (Rate: per_min * secs / 60)
    pub fn offsets(&self, n: usize, seed: u64) -> Vec<Duration>; // exactly n; past the end the last Rate continues
}
pub enum Gap { Overwrite { min: u64, max: u64 }, Revote { secs: u64 } }
/// Round 1 at `offsets[i]` for `first[i]`; round r ≥ 2 of voter v at its round r-1 time plus the gap.
pub fn order(first: &[u32], later: &[(u32, u8)], offsets: &[Duration], gap: &Gap, seed: u64) -> Vec<(Duration, u32, u8)>;
```

- [ ] **Step 1: Write the failing tests:**

```rust
fn rate(per_min: f64, secs: u64, poisson: bool) -> Phase { Phase::Rate { per_min, secs, poisson } }

#[test]
fn uniform_rate_burst_and_determinism() {
    let s = Schedule { phases: vec![rate(60.0, 600, false), Phase::Burst { n: 50 }, Phase::Idle { secs: 30 }, rate(120.0, 60, true)] };
    assert_eq!(s.capacity(), 600 + 50 + 120);
    let o = s.offsets(s.capacity(), 7);
    assert_eq!(o.len(), 770);
    assert!(o.windows(2).all(|w| w[0] <= w[1]));
    assert_eq!((o[0], o[599]), (Duration::ZERO, Duration::from_secs(599)));
    assert!(o[600..650].iter().all(|t| *t == Duration::from_secs(600)));
    assert!(o[650] >= Duration::from_secs(630));          // after the idle phase
    assert_eq!(s.offsets(100, 7), s.offsets(100, 7));
    assert_ne!(s.offsets(770, 7)[700..], s.offsets(770, 8)[700..]);
    assert_eq!(s.offsets(800, 7).len(), 800);             // past the end the last rate continues
}

#[test]
fn poisson_keeps_the_mean_rate() {
    let s = Schedule { phases: vec![rate(600.0, 3600, true)] };
    let span = s.offsets(6000, 1).last().unwrap().as_secs_f64();
    assert!((span - 600.0).abs() < 30.0, "{span}");        // 10/s; sd ≈ 7.7 s
}

#[test]
fn later_rounds_follow_their_voter() {
    let first: Vec<u32> = (0..100).collect();
    let offs = Schedule { phases: vec![rate(60.0, 100, false)] }.offsets(100, 1);
    let ord = order(&first, &[(3, 2), (3, 3), (50, 2)], &offs, &Gap::Overwrite { min: 5, max: 30 }, 9);
    let at = |v: u32, r: u8| ord.iter().find(|x| x.1 == v && x.2 == r).unwrap().0;
    assert_eq!(ord.len(), 103);
    assert!(ord.windows(2).all(|w| w[0].0 <= w[1].0));
    let d = at(3, 2) - at(3, 1);
    assert!(d >= Duration::from_secs(5) && d <= Duration::from_secs(30), "{d:?}");
    assert!(at(3, 3) > at(3, 2));
    let rv = order(&first, &[(50, 2)], &offs, &Gap::Revote { secs: 900 }, 9);
    assert_eq!(rv.last().unwrap(), &(Duration::from_secs(50 + 900), 50, 2));
}
```
- [ ] **Step 2: Run them.** Run `cargo test -p davinci-e2e --lib bench::schedule`. Expected: they fail
  to compile.
- [ ] **Step 3: Implement.** Uniform phases step by `60 / per_min`. Poisson phases draw exponential
  gaps from `StdRng::seed_from_u64(seed ^ phase index)`. A burst puts all n votes at the phase start.
  An idle phase adds its seconds. Overwrite gaps are uniform in `[min, max]`.
- [ ] **Step 4: Run the tests again.** Expected: `test result: ok. 3 passed`.
- [ ] **Step 5: Commit.**
  `git add e2e/src/bench/schedule.rs e2e/src/bench/mod.rs && git commit -m "feat(e2e): bench arrival scheduler with rates, bursts and revote gaps"`

### Task 8: Vote ledger and submitter — needs: nothing

**Files:**
- Create: `e2e/src/bench/ledger.rs`, `e2e/src/bench/submit.rs`

**Interfaces (produces):**
```rust
// ledger.rs
pub enum Outcome { Accepted, Refused(u32), Uncertain }
pub enum Attempt { Done(Outcome), Retry, Resend }
// A refusal on a resend is uncertain: the first attempt may have admitted the vote and the
// retry met a later condition (41201 after the end, 42903, 40002 on a replay). Only 40901 on
// a resend means the first attempt got in.
pub fn judge(r: &Result<(), davinci_client::Error>, resent: bool) -> Attempt;
pub struct Line { pub t: f64, pub pid: ProcessId, pub vid: String, pub tag: String, pub voter: u32, pub round: u8,
    pub node: NodeId, pub res: String, pub code: Option<u32>, pub tries: u32, pub ms: u64 }
pub struct Entry { pub pid: ProcessId, pub tag: String, pub voter: u32, pub round: u8, pub accepted: BTreeSet<NodeId>,
    pub uncertain: BTreeSet<NodeId>, pub refused: Vec<(NodeId, u32)>, pub first_accept: Option<f64> }
pub struct Ledger;   // append-only ledger.jsonl, flushed every 100 lines or 1 s, plus the in-memory map
impl Ledger {
    pub fn create(path: &Path) -> Result<Ledger>;
    pub fn append(&self, l: Line) -> Result<()>;
    pub fn entries(&self) -> BTreeMap<u64, Entry>;
    pub fn replay(path: &Path) -> Result<BTreeMap<u64, Entry>>; // ignores a torn last line
}
// submit.rs
pub enum Route { ByVoter, Only(NodeId), Both, Cross }
pub fn targets(route: &Route, tag: &str, voter: u32, round: u8) -> Vec<NodeId>;
pub struct Submitter { /* clients A and B, ledger, a Semaphore(64) per node, paused: Arc<AtomicBool> */ }
impl Submitter {
    pub fn new(a: &str, b: &str, ledger: Arc<Ledger>) -> Submitter;
    pub fn pause(&self, on: bool);
    pub async fn run(&self, plan: Vec<(Duration, BallotRecord)>, route: Route, t0: Instant) -> Result<SubmitStats>;
}
pub struct SubmitStats { pub sent: usize, pub accepted: usize, pub refused: BTreeMap<u32, usize>, pub uncertain: usize }
```

- [ ] **Step 1: Write the failing tests:**

```rust
#[test]
fn judge_maps_every_answer() {
    use davinci_client::Error as E;
    let api = |status, code| Err(E::Api { status, code: Some(code), message: String::new() });
    assert_eq!(judge(&Ok(()), false), Attempt::Done(Outcome::Accepted));
    assert_eq!(judge(&api(429, 42903), false), Attempt::Retry);
    assert_eq!(judge(&api(409, 40902), true), Attempt::Retry);         // never ours: vid is checked first
    assert_eq!(judge(&api(409, 40901), true), Attempt::Done(Outcome::Accepted));
    assert_eq!(judge(&api(409, 40901), false), Attempt::Done(Outcome::Refused(40901)));
    assert_eq!(judge(&api(412, 41201), false), Attempt::Done(Outcome::Refused(41201)));
    assert_eq!(judge(&api(400, 40002), false), Attempt::Done(Outcome::Refused(40002)));
    assert_eq!(judge(&api(408, 40801), false), Attempt::Resend);
    assert_eq!(judge(&Err(E::Http("reset".into())), false), Attempt::Resend);
    assert_eq!(judge(&api(503, 50001), false), Attempt::Resend);
    // A refusal on a resend is uncertain: the first attempt may have admitted the vote and the
    // retry met a later condition (41201 after the end, 42903, 40002 on a replay). Only 40901 on
    // a resend means the first attempt got in.
    assert_eq!(judge(&api(412, 41201), true), Attempt::Done(Outcome::Uncertain));
    assert_eq!(judge(&api(429, 42903), true), Attempt::Retry);
    assert_eq!(judge(&api(400, 40002), true), Attempt::Done(Outcome::Uncertain));
}
```

  Also:
  - `ledger_replays_and_ignores_a_torn_line`: four lines (A accepted; B refused 40901; A uncertain then
    accepted), then a truncated fifth. `replay` gives the same entries as `entries()`.
  - `routes_are_stable`:
    - `ByVoter` gives every round of a voter one node, and a split of 5000 ± 200 over 10,000 voters;
    - `Both` is `[A, B]`;
    - `Cross` round 1 differs from round 2;
    - `Only(B)` is `[B]`.
  - `submit_retries_then_records` (tokio): a scripted loopback HTTP server, written like
    `proxy::tests::echo`, answers `429`, `408`, then `409 40901`. Exactly one ledger line is written,
    with `res = "accepted"` and `tries = 3`.
- [ ] **Step 2: Run them.** Run `cargo test -p davinci-e2e --lib bench::ledger bench::submit`.
  Expected: they fail.
- [ ] **Step 3: Implement.** Back off from 50 ms up to 2 s. Give up after 10 minutes: `Uncertain` if
  any try was resent, otherwise the last refusal. With `Both`, send to both nodes concurrently and
  write one line per node.
- [ ] **Step 4: Run the tests again.** Expected: `ok. 4 passed`.
- [ ] **Step 5: Commit.**
  `git add e2e/src/bench/ledger.rs e2e/src/bench/submit.rs e2e/src/bench/mod.rs && git commit -m "feat(e2e): bench vote ledger and routed submitter"`

### Task 9: Chain view and audit — needs: nothing

**Files:**
- Create: `e2e/src/bench/chain_view.rs` (I/O), `e2e/src/bench/audit.rs` (pure)

**Interfaces (produces):**
```rust
// chain_view.rs
pub struct TransitionRow { pub pid: [u8; 31], pub index: u64, pub block: u64, pub ts: u64, pub sender: [u8; 20],
    pub new_root: [u8; 32], pub voters: u64, pub overwrites: u64, pub n_blobs: u64, pub vids: Vec<u64> }
pub fn decode_vids(blobs: &[Vec<u8>], nf: u8) -> Result<Vec<u64>>;          // decode_blobs(..).vote_ids
pub async fn chain_count(rpc: &str, registry: Address, from: u64, pid: &[u8; 31]) -> Result<u64>; // chain::transitions, this pid
pub struct Quiet { pub chain: u64, pub observer: u64, pub pending: [u64; 2], pub timed_out: bool }
pub async fn wait_quiet(obs: &Node, ab: [&Node; 2], rpc: &str, registry: Address, from: u64, pid: &[u8; 31], max: Duration)
    -> Result<Quiet>;   // until O's transition count == chain_count and A and B report pendingVotes == 0, or max
pub struct Transitions { pub rows: Vec<TransitionRow>, pub from_fallback: Vec<u64> /* indices O lacked */ }
pub async fn transitions(obs: &Node, fallback: [&Node; 2], op: &DynProvider, pid: &[u8; 31], nf: u8, chain: u64)
    -> Result<Transitions>;   // O's archive first; an index O lacks is read from A's or B's /transitions/{i}/blobs
pub async fn statuses(nodes: &[(NodeId, &Node)], e: &BTreeMap<u64, Entry>)
    -> BTreeMap<(u64, NodeId), NodeStatus>;   // every accepted or uncertain vote (settled ones too, for stale_error) on the nodes that took it, 64 at a time
pub struct JobRow { pub node: NodeId, pub job: String, pub secs: f64, pub started: f64, pub finished: f64, pub root_after: [u8; 32] }
pub async fn jobs(prover: &str, node: NodeId, calls: &[ProveCall]) -> Vec<JobRow>;
// audit.rs
pub enum NodeStatus { Settled, Open(String), Error(String), Unknown }
pub enum Verdict { Twice(Vec<u64>), Inconsistent(String), Settled { transition: u64, counted: bool, stale_error: Vec<NodeId> },
    Errored { reason: String, expected: bool }, NeverAdmitted, Lost { status: String } }   // spec §11 order
pub struct Classified { pub verdicts: BTreeMap<u64, Verdict>, pub phantoms: Vec<(u64, u64)> }
pub fn classify(e: &BTreeMap<u64, Entry>, rows: &[TransitionRow], st: &BTreeMap<(u64, NodeId), NodeStatus>, allowed: &[&str]) -> Classified;
pub struct Expect { pub tally: Vec<u64>, pub voters: u64, pub overwrites: u64 }
pub fn expect(rows: &[TransitionRow], who: &dyn Fn(u64) -> Option<(u32, Vec<u64>)>, nf: usize) -> Result<Expect>;
pub struct AuditResult { pub counts: BTreeMap<String, usize>, pub errors: BTreeMap<String, usize>, pub no_vote_lost: bool,
    pub clean: bool, pub tally_ok: bool, pub results_ok: bool, pub counters_ok: bool, pub observer_incomplete: bool,
    pub stale_errors: BTreeMap<NodeId, usize>, pub notes: Vec<String> }
impl Classified { pub fn summary(&self) -> AuditResult; }   // tally, results, counters and observer filled in by run.rs
```

- [ ] **Step 1: Write the failing tests** in `audit.rs`:

```rust
use NodeId::{A, B};
fn acc(n: NodeId, voter: u32) -> Entry { Entry { accepted: [n].into(), voter, ..Entry::test() } }
fn unc(n: NodeId, voter: u32) -> Entry { Entry { uncertain: [n].into(), voter, ..Entry::test() } }
fn refd(n: NodeId, voter: u32) -> Entry { Entry { refused: vec![(n, 40002)], voter, ..Entry::test() } }
fn row(i: u64, vids: &[u64]) -> TransitionRow { TransitionRow { index: i, vids: vids.to_vec(), ..TransitionRow::test() } }

#[test]
fn classify_every_case() {
    let e: BTreeMap<u64, Entry> = [(1, acc(A, 1)), (2, acc(B, 2)), (3, acc(A, 3)), (4, acc(A, 4)),
        (5, unc(A, 5)), (6, unc(B, 6)), (7, acc(A, 7)), (8, acc(B, 8)), (9, refd(A, 9))].into();
    let rows = vec![row(0, &[1, 7]), row(1, &[2, 7, 8]), row(2, &[99, 9])];
    let st: BTreeMap<_, _> = [
        ((3, A), NodeStatus::Error("process closed".into())),
        ((4, A), NodeStatus::Open("pending".into())),
        ((5, A), NodeStatus::Unknown),
        ((6, B), NodeStatus::Open("pending".into())),
        ((8, B), NodeStatus::Error("settlement reverted: X".into())),
    ].into();
    let c = classify(&e, &rows, &st, &["process closed"]);
    assert_eq!(c.verdicts[&1], Verdict::Settled { transition: 0, counted: true, stale_error: vec![] });
    assert_eq!(c.verdicts[&2], Verdict::Settled { transition: 1, counted: true, stale_error: vec![] });
    assert_eq!(c.verdicts[&3], Verdict::Errored { reason: "process closed".into(), expected: true });
    assert_eq!(c.verdicts[&4], Verdict::Lost { status: "pending".into() });
    assert_eq!(c.verdicts[&5], Verdict::NeverAdmitted);
    assert_eq!(c.verdicts[&6], Verdict::Lost { status: "pending".into() }); // uncertain, but the node holds it
    assert_eq!(c.verdicts[&7], Verdict::Twice(vec![0, 1]));
    assert_eq!(c.verdicts[&8], Verdict::Settled { transition: 1, counted: true, stale_error: vec![B] });
    assert!(matches!(c.verdicts[&9], Verdict::Inconsistent(_)));         // refused everywhere, yet on chain
    assert_eq!(c.phantoms, vec![(99, 2)]);
    assert!(c.summary().no_vote_lost == false);           // because of 4, 6, 7, 9 and the phantom, not 8
    let s8 = classify(&[(8, acc(B, 8))].into(), &[row(1, &[8])], &st, &[]).summary();
    assert!(s8.no_vote_lost && !s8.clean);                // stale error is a finding only

    // Ruling 20: an isolated vote errors with the prover's reason; a finding, not a loss.
    let only3: BTreeMap<u64, Entry> = [(3, acc(A, 3))].into();
    let st2: BTreeMap<_, _> = [((3, A), NodeStatus::Error("prover: job failed (isolated by bisection)".into()))].into();
    let s = classify(&only3, &[], &st2, &["process closed"]).summary();
    assert!(s.no_vote_lost && !s.clean);
    // A held batch closed at the end is never the allowed `process closed`.
    let held = "process closed (held: proved publics do not match)";
    let st3: BTreeMap<_, _> = [((3, A), NodeStatus::Error(held.into()))].into();
    let c3 = classify(&only3, &[], &st3, &["process closed"]);
    assert_eq!(c3.verdicts[&3], Verdict::Errored { reason: held.into(), expected: false });
}

#[test]
fn tally_and_counters_follow_chain_order() {
    // Voter 1's revote (vid 12) lands before its first ballot (vid 11): across nodes the later landing stands.
    let rows = vec![row(0, &[10, 12]), row(1, &[11])];
    let who = |vid: u64| match vid { 10 => Some((0, vec![1, 0])), 11 => Some((1, vec![0, 2])), 12 => Some((1, vec![3, 0])), _ => None };
    let x = expect(&rows, &who, 2).unwrap();
    assert_eq!((x.tally, x.voters, x.overwrites), (vec![1, 2], 2, 1));
    assert!(expect(&[row(0, &[77])], &who, 2).is_err());   // an unknown vote id is an error
}
```

  Also `decode_vids_reads_the_vote_ids` (chain_view): build `TransitionData` with vote ids
  `[1<<63 | 5, 1<<63 | 9]` and two identity-ballot updates at nf 4, then `cells`, `blobs_from_cells`,
  and `decode_vids`. The result is the two ids.
- [ ] **Step 2: Run them.** Run `cargo test -p davinci-e2e --lib bench::audit bench::chain_view`.
  Expected: they fail.
- [ ] **Step 3: Implement.** Use the verdict order of spec §11: `Twice`, `Inconsistent`, `Settled`,
  `Errored`, `NeverAdmitted`, `Lost`; the first that holds applies. `counted` means the vote is the
  latest settled ballot of its voter in transition order. `stale_error` lists the accepting nodes that
  report `Error` for a settled vote. `Lost.status` is the first non-error status a node reports, or
  `none`. Allowed reasons match by substring, except that a reason containing `(held:` never matches.
  `wait_quiet` polls every 10 s; `transitions` marks every index it had to read from A or B.
- [ ] **Step 4: Run the tests again.** Expected: `ok. 3 passed`.
- [ ] **Step 5: Commit.**
  `git add e2e/src/bench/chain_view.rs e2e/src/bench/audit.rs e2e/src/bench/mod.rs && git commit -m "feat(e2e): bench audit classifies every vote against the chain's blobs"`

### Task 10: Metrics and reports — needs: nothing

**Files:**
- Create: `e2e/src/bench/metrics.rs`, `e2e/src/bench/report.rs`
- Modify: `e2e/tests/throughput.rs` (remove `rfc3339` and import it from `davinci_e2e::bench::metrics`)

**Interfaces (produces):**
```rust
// metrics.rs
pub fn rfc3339(s: &str) -> Option<f64>;
pub struct LogEvent { pub at: f64, pub level: String, pub msg: String, pub fields: BTreeMap<String, String> }
pub fn parse_line(l: &str) -> Option<LogEvent>;                 // "<rfc3339>  LEVEL msg k=v ..."
pub struct LogTail { /* path, offset */ } impl LogTail { pub fn new(p: &Path) -> Self; pub fn poll(&mut self) -> Vec<LogEvent>; }
pub enum Kind { Sealed { pid: String, votes: u64, reason: String }, Settled { pid: String, votes: u64, root: String },
    Synced { pid: String, root: String }, LostEvent { pid: String }, LostRevert { pid: String }, Posted { nonce: u64, tx: String, blobs: u64 },
    PostRetry { pid: String, error: String }, Pause { pid: String, secs: u64 }, RpcFailover { from: String, to: String }, BeaconFailover,
    // Ruling 20 and the other ways a vote or a result can be lost:
    Bisect { pid: String, votes: u64, cap: u64 }, Isolated { pid: String, vid: u64 },
    Held { pid: String, error: String }, RefreshOverflow { pid: String, votes: u64 },
    FinalizeFailed { pid: String, error: String }, GuestRejected { pid: String, mask: String } }
pub fn kind(e: &LogEvent) -> Option<Kind>;
pub fn percentiles(v: &[f64], ps: &[f64]) -> Vec<f64>;           // nearest rank
pub fn steady_rate(landings: &[(f64, u64)]) -> Option<f64>;     // votes/min after the first landing
pub struct CellRate { pub mean_batch: f64, pub steady_per_min: Option<f64>, pub floor: bool }
pub fn cell_rate(landings: &[(f64, u64)], b: u64) -> CellRate;  // steady rate over batches ≥ 0.9·B; floor = median batch < 0.9·B
// report.rs
pub struct Progress { /* counters */ } impl Progress { pub fn line(&self, elapsed: Duration) -> String; }
/// Per node: the vote-loss metrics (log Kinds, cross-checked with /info `bisections`, `isolated_votes`, `held_batches`).
pub struct VoteLoss { pub bisections: u64, pub isolated: u64, pub held: u64, pub refresh_overflow: u64,
    pub guest_rejection: u64, pub finalize_failed: u64 }
#[serde(rename_all = "snake_case")]
pub enum FindingKind { UnexpectedError, StaleError, Bisections, Isolated, Held, RefreshOverflow, GuestRejection,
    ResultsFailed, ObserverIncomplete, PendingAfterWait }
pub struct Finding { pub scenario: String, pub tag: String, pub kind: FindingKind, pub count: u64, pub detail: String }
pub fn findings(scenario: &str, tag: &str, audit: &AuditResult, events: &[(NodeId, Kind)]) -> Vec<Finding>;
pub struct ScenarioReport { /* name, params, audit, latency, throughput, cells, batches, races, chaos, nodes, vote_loss, findings, spend */ }
pub fn write_run(dir: &Path, r: &ScenarioReport) -> Result<()>;               // report.json + report.md
pub fn write_campaign(campaign: &Path, all: &[ScenarioReport]) -> Result<()>; // roll-up with the ceiling table
```

- [ ] **Step 1: Write the failing tests:**
  - `parses_real_log_lines`. It uses these lines verbatim (from `~/.cache/davinci-e2e` runs on
    2026-10-13):

```
2026-10-13T17:24:24.614943Z  INFO batch sealed pid=f39f…0000 votes=225 overwrites=0 reason=at_max
2026-10-13T17:25:36.321979Z  INFO transition settled pid=f39f…0000 votes=225 overwrites=0 root=0x6900…5cfd
2026-10-13T17:24:29.845987Z  INFO blobs posted on L1 nonce=0 tx=0xf126…8800 block=98 blobs=2
2026-10-13T17:57:53.963997Z  WARN foreign transition beat the in-flight batch pid=f39f…0000
2026-10-13T17:55:29.977773Z  INFO synced transition from another sequencer pid=f39f…0001 votes=3 root=0xd2ef…
```

    (Use the full hex strings from the logs.) It also covers `lost the settlement race, waiting for
    sync`, `RPC failover from=127.0.0.1:1 to=127.0.0.1:2`, and an `L1 post failed; retrying` line with
    `error=…`. Assert each line's `Kind` and that `at` is within 1 µs of the timestamp.

    It also has one line per new `Kind`, with the exact texts:
    - `Bisect`, `Isolated`, `Held` and the results retry (`FinalizeFailed`): from the Ruling 20 commit
      (`batch proof failed; requeued for bisection` with `votes=` and `cap=`, the isolated vote's
      `prover: … (isolated by bisection)`, the held batch's line). If Task 10 runs before that commit
      lands, add these lines in a follow-up commit before Task 13;
    - `RefreshOverflow`: `ERROR exposed refresh set too large; cleared, its votes must recast pid=… slots=… votes=…`
      (`actor.rs`, `refresh_overflow`);
    - `GuestRejected`: `ERROR batch failed in the guest pid=… msg=guest rejected the batch: … (mask 0x…)`
      (`actor.rs`, `JobOutcome::Guest`);
    - `FinalizeFailed` also from `results_request failed` and `dkg_results_inputs failed`.

    A value with spaces (`msg=`, `error=`) runs to the next ` key=` or the end of the line.
  - `percentiles_and_steady_rate`:
    - `percentiles(1..=100, [50, 90, 99]) == [50, 90, 99]`;
    - landings `[(0, 100), (60, 100), (120, 100)]` give 100.0 votes/min;
    - a single landing gives `None`;
    - `cell_rate` of `[(0, 128), (60, 128), (90, 40), (120, 128)]` at B 128 gives mean 106.0, a steady
      128.0 votes/min (the 40-vote batch is left out) and `floor: false`; all batches at 100 give
      `floor: true`.
  - `progress_line_format`: the line matches spec §12's shape for fixed counters.
  - `ceiling_table_renders`: two reports give a Markdown table with nf rows, B columns and cells
    `votes/min (mean batch)`, with `floor` where the median batch is below 0.9·B.
  - `findings_from_events_and_audit`: one event of each new `Kind` on node A, plus an audit with a
    stale error on B, an unexpected `Errored` and `results_ok = false`, give one `Finding` per kind
    with its count (`refresh_overflow` counts the votes, `isolated` lists the vote id); `write_run`
    renders them in a "Findings" section and `VoteLoss` for A counts one of each.
  - The existing `throughput.rs` test `parses_prover_times` still passes.
- [ ] **Step 2: Run them.** Run `cargo test -p davinci-e2e --lib bench::metrics bench::report`.
  Expected: they fail.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run them again.** Run the same command, then
  `cargo test -p davinci-e2e --test throughput`. Expected: `ok. 5 passed`, and the throughput unit
  tests `ok`.
- [ ] **Step 5: Commit.**
  `git add e2e/src/bench/metrics.rs e2e/src/bench/report.rs e2e/src/bench/mod.rs e2e/tests/throughput.rs && git commit -m "feat(e2e): bench log metrics, progress lines and JSON and Markdown reports"`

### Task 11: Money: fee gate, budget, seq-b keygen, capped funding — needs: nothing

**Files:**
- Create: `e2e/src/bench/money.rs`
- Modify: `e2e/src/bin/davinci-bench.rs` (`keygen`, `fund`, `gate`)

**Interfaces (produces):**
```rust
pub struct Fees { pub base: u128, pub tip: u128, pub blob: u128 }       // wei
pub const MAX_EXEC: u128 = 500_000_000; pub const MAX_BLOB: u128 = 20_000_000;
pub fn gate(f: &Fees) -> Result<(), String>;                             // base + tip ≤ MAX_EXEC, blob ≤ MAX_BLOB
pub async fn fees(l1: &DynProvider) -> Result<Fees>;                     // latest base fee, eth_maxPriorityFeePerGas, eth_blobBaseFee or fee history
pub struct Caps { pub l1_per_key: U256, pub op_per_key: U256, pub organizer_op: U256 }
pub struct Balances { pub l1: [U256; 2], pub op: [U256; 2], pub organizer_op: U256 }
pub struct Budget { pub start: Balances, pub caps: Caps }                 // <campaign>/budget.json, written at the first go
impl Budget { pub fn check(&self, now: &Balances) -> Result<(), String>; pub fn spent(&self, now: &Balances) -> Balances; }
pub fn keygen(dir: &Path, name: &str) -> Result<(Address, Address)>;      // <name>.op.key, <name>.l1.key: create_new, 0600
pub const FUND_MAX_L1: &str = "0.01"; pub const FUND_MAX_OP: &str = "0.001";
pub fn check_fund(amount: U256, max: U256, to: Address, confirm: &[Address]) -> Result<()>;
pub async fn fund(owner: &PrivateKeySigner, op_rpc: &str, l1_rpc: &str, to_op: Option<(Address, U256)>, to_l1: Option<(Address, U256)>) -> Result<()>;
/// Lines of `pgrep -af davinci-sequencer` and `docker ps --format '{{.Names}} {{.Image}}'` that are not
/// the rig's own (`own` pids); prover containers (davinci-zkvm images) are not sequencers.
pub fn foreign_sequencers(pgrep: &str, docker: &str, own: &[u32]) -> Vec<String>;
pub fn nonce_gaps(pending: &[(String, u64)], latest: &[(String, u64)]) -> Vec<String>; // accounts with pending > latest
```

CLI:
- `keygen --dir DIR --name NAME` prints only `<name> OP <addr>` and `<name> Ethereum <addr>`.
- `fund --to-op ADDR --op-eth X --to-l1 ADDR --l1-eth Y --confirm ADDR,ADDR`. The owner key comes from
  `DAVINCI_E2E_ORGANIZER_KEY`. The fee gate runs before the Ethereum send. It prints the tx hashes.
- `gate [--campaign DIR]` prints fees, balances, spend against caps (`DAVINCI_BENCH_CAP_L1` default
  0.005, `_OP` 0.0005, `_ORG` 0.002) and both provers' `/health`. It exits 0 on a go and 3 on a no-go.
- `gate` also fails (exit 3) when any `davinci-sequencer` process or container other than the
  rig's is running (`pgrep -af davinci-sequencer`, `docker ps --format '{{.Names}} {{.Image}}'`
  on z6), or when `seq-a.l1`/`seq-a.op`/`seq-b.l1`/`seq-b.op` have a pending nonce above their
  latest nonce. It prints the sha256 of `DAVINCI_SEQUENCER_BIN` and of itself.
- It also fails when `DAVINCI_E2E_BEACON` lists fewer than two beacons (C6 takes O's first one down).
  Run standalone, every sequencer is foreign; when `run` calls the gate in-process, the rig's own pids
  are excluded.

- [ ] **Step 1: Write the failing tests:**
  - `gate_bounds`: `(base 400M, tip 100M, blob 20M)` passes; one wei more on either side fails.
  - `budget_stops_at_a_cap`: spend under the cap passes; at the cap, the error names the account.
  - `keygen_is_private`:
    - the files have mode 0600;
    - the returned addresses equal `read_key` of each file;
    - a second run fails, and its error holds no hex.
  - `fund_checks`: an amount above the max fails, an unconfirmed recipient fails, and the confirmed one
    within the max passes.
  - `gate_finds_foreign_sequencers`: a `pgrep` listing with an own pid and another
    `davinci-sequencer` reports only the other; a docker line `davinci-sequencer ghcr.io/…/davinci-sequencer:…`
    is reported and a `davinci-zkvm` prover container is not; `nonce_gaps` reports only the account
    whose pending nonce is above its latest.
- [ ] **Step 2: Run them.** Run `cargo test -p davinci-e2e --lib bench::money`. Expected: they fail.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run the tests again.** Expected: `ok. 5 passed`.
- [ ] **Step 5: Commit.**
  `git add e2e/src/bench/money.rs e2e/src/bin/davinci-bench.rs e2e/src/bench/mod.rs && git commit -m "feat(e2e): bench fee gate, spend budget, keygen and capped funding"`

### Task 12: `plan`, `serve`, `cancel` and wave 1 elections — needs: two anvils (no GPU), Track A and the Ruling 20 node binary

**Files:**
- Create: `e2e/src/bench/scenarios.rs` (wave 1 election specs)
- Modify: `e2e/src/bench/plan.rs` (`create_wave`, `write_pins`), `e2e/src/bin/davinci-bench.rs`
  (`plan`, `serve`, `cancel`, `dry --stop-after plan`)

**Interfaces (produces):**
```rust
pub const S_CELLS: [(usize /* B */, usize /* bursts per election */); 3] = [(128, 4), (256, 4), (512, 3)];
pub fn wave_specs(n: u32, scale: f64) -> Vec<ElectionSpec>;
    // wave 1: R1..R4; S groups nf {2, 8, 16}, 4 elections each of Σ bursts·⌈scale·B⌉ voters (3072 at scale 1); S-c (2 × 768)
pub async fn create_wave(net: &Net, rig: &Rig, campaign: &Path, seed: &Seed, n: u32, specs: &[ElectionSpec], onchain_census: bool) -> Result<()>;
    // per election: census file (origins 1) / OwnedCensus deploy + addMembers in chunks of 64 (origin 3) / CSP root (origin 4);
    // metadata; next_process_id; key from the key node (≤ 10 per minute per node, 42901 → wait 60 s);
    // create_process (start 0, duration 72 h, max_voters = spec.max_voters()); set_process_grace when ≠ registry default;
    // wait until A and B list it and serve its census; append to plan.json (an election already there is skipped)
pub fn write_pins(campaign: &Path) -> Result<()>;   // batchProgramVK, resultsProgramVK, rootCVadcopFinal, ziskVerifierCodeHash, ballotVKHash
```

`plan` creates `secret/seed` on the first wave. It records `start_block` (the head before the first
creation) and brings the rig up (`Knobs::production()`) only to get the keys. `plan` and `gen` take a
list of waves (`--wave 1,2,3`), so all waves can be planned and generated up front if the owner picks
that at checkpoint 1; a single wave stays additive. `serve` holds the rig until SIGTERM and prints the
three node URLs. `cancel --wave N` cancels that wave's READY and PAUSED processes from `plan.json`;
`cancel --all` cancels every READY or PAUSED process the organizer created since `start_block`, from
the registry logs like `cancel_leftovers`, so it also finds the TS runs' processes (Task 24 only).

- [ ] **Step 1: Write the failing tests:**
  - `wave1_is_well_formed`:
    - tags are unique;
    - every S election holds Σ `S_CELLS` bursts × ⌈scale·B⌉ voters (3072 at scale 1), and each S
      group has 4 elections, 2 per key node;
    - the wave holds 44,000 ± 2,000 ballots at scale 1;
    - at scale 0.02 every election still has at least 4 voters;
    - key nodes alternate.
  - `max_voters_equal_planned_voters`: for waves 1 to 3 at scales 1 and 0.02, every election of a
    `stand_in` plan has exactly `spec.max_voters()` distinct voters in `Election::ballots`. The actor
    errors queued new-slot votes at `maxVoters`, so a spec row whose planned voters exceed it would
    lose votes. Waves 2 and 3 are empty until Tasks 17 and 20 add them; the test then covers them.
  - `pins_json_matches_release`: the hex in `pins.json` equals `davinci_zkvm_sdk::release` and
    `chain::ballot_vk_hash()`.
- [ ] **Step 2: Run them.** Run `cargo test -p davinci-e2e --lib bench::scenarios bench::plan`.
  Expected: the new tests fail.
- [ ] **Step 3: Implement.** `create_wave` reuses `fx::write_census`, `fx::write_metadata`,
  `census::Census::deploy` and `add_members`, `org.create_process`, `org.set_process_grace` and
  `RegistryReader::process`, then `ProcSnap::of`. `create_process` gets `spec.max_voters()`, never a
  literal.
- [ ] **Step 4: Run the unit tests and a dry plan on two anvils.**

```bash
git merge-base --is-ancestor <ruling-20 commit> HEAD && echo "ruling 20 ok"
cargo test -p davinci-e2e --lib bench
cargo build --release -p davinci-e2e --bin davinci-bench -p davinci-sequencer --bin davinci-sequencer
DAVINCI_E2E_DA=l1-blobs DAVINCI_E2E_GRACE=300,240,600,1800,60 \
  systemd-run --user --scope --unit davinci-bench-dry -p MemoryMax=12G -- \
  target/release/davinci-bench dry --scenario R --scale 0.02 --stop-after plan 2>&1 | tail -20
```
Expected:
- `ruling 20 ok` (the commit recorded in `progress.md`); without it, stop: no dry runs on an older node;
- the unit tests are all `ok`;
- `chain 31337 registry 0x… verifier 0x… (pins ok)` and `L1-blob mode: …`;
- `node a at http://127.0.0.1:9101`, and the same for b and o;
- four `election R<i> 0x… nf <n> origin <o> key <a|b>` lines, R4 with `origin 3`;
- `plan.json: wave 1, 4 elections`;
- `dry stopped after plan`.
- [ ] **Step 5: Commit.**
  `git add e2e/src/bench/scenarios.rs e2e/src/bench/plan.rs e2e/src/bin/davinci-bench.rs && git commit -m "feat(e2e): davinci-bench plan, serve and cancel with the wave 1 elections"`

### Task 13: `run` with the realistic and ceiling scenarios — needs: two anvils, both GPU provers, the Ruling 20 node binary

**Files:**
- Create: `e2e/src/bench/run.rs`
- Modify: `e2e/src/bench/scenarios.rs` (`Scenario`, wave 1 schedules, routes and ends),
  `e2e/src/bin/davinci-bench.rs` (`run`, `audit`, full `dry`)

**Interfaces (produces):**
```rust
pub enum End { Shorten { after: Duration, notice: u64 }, Now { after: Duration }, AfterSettled }
pub struct Cell { pub b: usize, pub bursts: usize }         // per election, from S_CELLS
pub enum Kind { Plain, Ceiling(Vec<Cell>), Race(RaceSpec) /* Task 17 */ }
pub struct Scenario { pub name: String, pub wave: u32, pub tags: Vec<String>, pub route: Route, pub schedule: Schedule,
    pub gap: Gap, pub knobs: [Knobs; 2], pub end: End, pub allowed: BTreeMap<String /* tag */, Vec<&'static str>>,
    pub chaos: Vec<Chaos> /* Task 19 */, pub kind: Kind }
pub fn scenarios(n: u32, scale: f64) -> Vec<Scenario>;
    // wave 1: R (4 tags, staggered 5 min); S-2, S-8, S-16 (Ceiling, batch_max 512, batch_time 60s, no restart between
    // them); S-c (Ceiling with one 256 cell, `Both`, right after S-16 with the same knobs)
pub struct Ctx { pub net: Net, pub plan: Plan, pub campaign: PathBuf, pub seed: Seed, pub budget: Option<Budget> }
pub async fn run_scenario(ctx: &Ctx, rig: &mut Rig, sc: &Scenario) -> Result<ScenarioReport>;
pub async fn audit_run(ctx: &Ctx, rig: &Rig, run_dir: &Path) -> Result<AuditResult>;
```

`run_scenario` goes through these stages in order:

1. Fee gate and budget. On a no-go, wait up to 30 min, then skip the scenario.
2. Provers idle.
3. `restart_with` the scenario's knobs, only if they differ from the running ones (a restart re-seeds
   the node's proving estimate).
4. Wait until A and B list the elections and serve their census.
5. Load `ballots/w<N>/<tag>.jsonl` and build the order (`order`, `targets`).
6. Run `Submitter::run` (`Plain`). For `Kind::Ceiling`, send the cells in ascending B: for each
   election, bursts of ⌈scale·B⌉ votes, each sent once that election's previous burst has sealed (its
   `batch sealed pid=…` line; under `Both`, once both nodes have sealed it). One burst then waits in
   the queue during each flight and seals at the landing, so the 60 s timer stays off the critical path.
   Alongside it, every 30 s, print the progress line, re-check the fee gate (pause or resume sends) and
   the budget (stop rule), and tail the node logs.
7. End each election per its `End`.
8. Wait for `org.results` until `grace_end + 15 min`, and for every node to report it.
9. Audit:
   - wait until O's transition count for every election equals `chain::transitions` from the
     registry logs and A and B report `pendingVotes == 0`, up to 15 min; if O never catches up,
     take the missing transitions' blobs from A or B and mark the audit `observer incomplete`;
   - `chain_view::transitions` from O (or the fallback), `statuses` from A and B, `classify`;
   - `results_ok`: `ProcessResultsSet` landed and equals `expect`; a missing result after
     `grace_end + 15 min` is a finding with the key node's `finalize` log lines;
   - `expect` against the results, and the registry counters against the observer's per-transition
     `voters` and `overwrites` sums;
   - `syncedFromOthers` (O) against the transition count (skipped when `observer incomplete`), and
     `settledBySelf` (A + B) against it;
   - `report::findings` from the audit and the scenario's log `Kind`s (`bisections`, `isolated`,
     `held`, `refresh_overflow`, `guest_rejection`, `results_failed`); a vote still pending after the
     wait is a `pending_after_wait` finding.
10. Build the batch rows (jobs, `l1::posts`, `cost::fetch`), the `VoteLoss` counters per node, spend
    from balances, and for `Kind::Ceiling` one `cell_rate` per cell over that cell's landings.
11. `write_run`, then `write_campaign`.

`run --scenario all` runs the wave's scenarios in order. `dry` runs plan, an in-process `gen_wave`
(one host), `merge` and `run`, on anvil.

- [ ] **Step 1: Write the failing tests:**
  - `wave1_scenarios_match_specs`: every scenario's tags exist in `wave_specs(1, s)`. S-2, S-8 and
    S-16 are `Kind::Ceiling` with `S_CELLS` in ascending B, use `Only` routing with each tag on its key
    node, and have `batch_max = 512`, `batch_time = "60s"`. S-2 to S-c have identical knobs, so the
    rig never restarts between them, and S-c directly follows S-16 with `Both`. R allows
    `process closed` for R2 and R3 only.
  - `scenarios_are_consistent` (properties, not restated constants; for every wave at scales 1 and
    0.05):
    - every scenario's planned ballots are at most the sum of its elections' ballots;
    - every `Plain` schedule has `Schedule::capacity()` at least its round-1 ballots, except
      the scenarios listed as sending past the end on purpose (R2);
    - in every `Ceiling` scenario, Σ bursts × ⌈scale·B⌉ equals each election's voters;
    - every tag in `allowed` is one of the scenario's tags.
- [ ] **Step 2: Run them.** Run `cargo test -p davinci-e2e --lib bench::scenarios`. Expected: they fail.
- [ ] **Step 3: Implement** `run.rs` and the CLI commands.
- [ ] **Step 4: Run the unit tests, then a GPU dry run of R and two small S groups.** The dry uses
  the `davinci-sequencer` built in Task 12 Step 4 (it carries Ruling 20).

```bash
cargo test -p davinci-e2e --lib bench
curl -s http://127.0.0.1:8080/health; echo; curl -s http://10.200.0.27:8080/health; echo
DAVINCI_BENCH_PROVER_A=http://127.0.0.1:8080 DAVINCI_BENCH_PROVER_B=http://10.200.0.27:8080 \
DAVINCI_E2E_DA=l1-blobs DAVINCI_E2E_GRACE=300,240,600,1800,60 \
  systemd-run --user --scope --unit davinci-bench-dry -p MemoryMax=12G -- \
  target/release/davinci-bench dry --scenario R,S-2,S-16 --scale 0.05 2>&1 | tee ~/.cache/davinci-bench/dry-w1.log | grep -E "^\[bench|audit|results|findings|cell|report|FAILED"
```
Expected:
- `"queue_len":0` on both provers;
- progress lines every 30 s;
- for every election, `audit <tag>: no_vote_lost=true clean=true tally_ok=true results_ok=true counters_ok=true`;
- `results <tag> = tally`;
- `findings: none`. Any `bisections`, `isolated`, `held`, `refresh_overflow`, `guest_rejection`,
  `stale_error` or `results_failed` is investigated before going live: a prover crash that the
  bisection absorbed is recorded, a node or harness bug is fixed with a test and the dry rerun;
- one `cell nf <n> B <b>: <v> votes/min, mean batch <m>` line per S cell;
- `report written: …/report.md`;
- no `FAILED`.

It takes about 60-90 min. On a failure, fix with a test and rerun the dry.
- [ ] **Step 5: Commit.**
  `git add e2e/src/bench/run.rs e2e/src/bench/scenarios.rs e2e/src/bin/davinci-bench.rs e2e/src/bench/mod.rs && git commit -m "feat(e2e): davinci-bench run drives realistic and ceiling scenarios to an audited report"`

### Task 14: seq-b keys, live environment, funding — needs: network, spend (owner checkpoint 1), Track A and the Ruling 20 node binary

**Files:** none committed. Creates `~/.config/davinci-l1blob/bench.sh` and the seq-b key files.

- [ ] **Step 1: Live environment file** (mode 0600; secrets only through the variables `op-core.sh`
  exports). Fill in `<REGISTRY>` and `<BLOCK>` from `deployments/10.json` once Track A has redeployed
  (not known before), and the two binaries' short shas: the node built from a tree that carries
  Ruling 20, and the bench. Do not type sha256s into the report; `gate` prints both and the report
  copies them.

```bash
# ~/.config/davinci-l1blob/bench.sh
. ~/.config/davinci-l1blob/op-core.sh
export DAVINCI_E2E_LIVE=1 DAVINCI_E2E_NETWORK=optimism DAVINCI_E2E_POLL=2s \
  DAVINCI_E2E_REGISTRY=<REGISTRY> DAVINCI_E2E_FROM_BLOCK=<BLOCK> DAVINCI_E2E_RPC="$OP_RPC" \
  DAVINCI_E2E_NODE_RPCS="$OP_RPC,https://optimism-rpc.publicnode.com;$OP_RPC,https://mainnet.optimism.io;https://mainnet.optimism.io,https://optimism-rpc.publicnode.com" \
  DAVINCI_E2E_ORGANIZER_KEY=$HOME/optimism-privkey.txt \
  DAVINCI_E2E_SEQUENCER_KEYS=$KEYS/seq-a.op.key,$KEYS/seq-b.op.key \
  DAVINCI_E2E_L1_KEYS=$KEYS/seq-a.l1.key,$KEYS/seq-b.l1.key \
  DAVINCI_E2E_L1_SEND_RPC=$L1_SEND DAVINCI_E2E_BEACON=$ETH_BEACON DAVINCI_E2E_L1_MIN_BALANCE=0.002 \
  DAVINCI_BENCH_PROVER_A=http://127.0.0.1:8080 DAVINCI_BENCH_PROVER_B=http://10.200.0.27:8080 \
  DAVINCI_SEQUENCER_BIN=$HOME/.cache/davinci-l1blob/bin/davinci-sequencer-<sha> \
  BENCH=$HOME/.cache/davinci-bench/bin/davinci-bench-<sha> DAVINCI_BENCH_DIR=$HOME/.cache/davinci-bench/c1
```

- [ ] **Step 2: Keygen.** Run
  `$BENCH keygen --dir ~/.config/davinci-l1blob --name seq-b; ls -l ~/.config/davinci-l1blob/seq-b.*`.
  Expected: `seq-b OP 0x…`, `seq-b Ethereum 0x…`, and two `-rw-------` files.
- [ ] **Step 3: Gate (read-only).** Run `$BENCH gate --campaign $DAVINCI_BENCH_DIR; echo exit $?`.
  Expected: fees, balances (seq-b at 0), the sha256s of both binaries, the registry's verifiers,
  `no-go: seq-b below …`, and `exit 3`. `budget.json` is not written yet, because it is written only on
  a go. A `foreign sequencer` or `pending nonce` no-go means the production node (or something else)
  still signs with seq-a: stop and ask the owner, because Ruling 19 holds that node and two signers
  on one key race nonces. A `beacons` no-go means `DAVINCI_E2E_BEACON` needs a second beacon.
- [ ] **Step 4: Owner checkpoint 1.** Show the addresses, the amounts (seq-b.l1 0.006 ETH, seq-b.op
  0.0006 ETH, and a seq-a.l1 top-up to 0.006 only if it is below), the caps in the Global
  Constraints, and the fees. Also ask whether to plan and generate all three waves before wave 1
  (one `plan --wave 1,2,3` and one `gen` session, about 70 min, meeting "generate before starting"
  literally; the cost is that wave 2 and 3 ballots are spent if wave 1 findings change those
  scenarios) or wave by wave. Wait for "fund" and the answer.
- [ ] **Step 5: Fund and verify.**

```bash
$BENCH fund --to-op <seq-b.op> --op-eth 0.0006 --to-l1 <seq-b.l1> --l1-eth 0.006 --confirm <seq-b.op>,<seq-b.l1> 2>&1 | redact
$BENCH gate --campaign $DAVINCI_BENCH_DIR; echo exit $?
python3 -I ~/.cache/op-core/scan_secrets.py "$KEYS/seq-a.l1.key,$KEYS/seq-a.op.key,$KEYS/seq-b.l1.key,$KEYS/seq-b.op.key,$HOME/optimism-privkey.txt" "$HOME/optimism-rpcs.txt" ~/.cache/davinci-bench
```
Expected: two tx hashes; the gate shows the new balances with `go` and `exit 0`, and writes
`budget.json`; the scan reports `0 with a secret`.

### Task 15: Live wave 1, realistic and ceiling — needs: network, both GPU provers, spend (owner checkpoint 2), the Ruling 20 node binary

**Files:** none (a failure leads to a fix commit with its test, a dry rerun, then a new wave).

- [ ] **Step 1: Binaries and provers.**

```bash
. ~/.config/davinci-l1blob/bench.sh
git status --short | wc -l
cargo build --release -p davinci-e2e --bin davinci-bench && cp target/release/davinci-bench "$BENCH"
docker ps --format '{{.Names}} {{.Image}}' | grep -i zkvm; ssh p4u@10.200.0.27 "docker ps --format '{{.Names}} {{.Image}}' | grep -i zkvm"
docker exec <z6 prover> printenv ZISK_MINIMAL_MEMORY_FROM; ssh p4u@10.200.0.27 docker exec <z7 prover> printenv ZISK_MINIMAL_MEMORY_FROM
git merge-base --is-ancestor <ruling-20 commit> <commit of $DAVINCI_SEQUENCER_BIN> && echo "ruling 20 ok"
```
Expected:
- `0`, and the build;
- z6's container on Track A's 1.3.2 image and z7's on the CI-built davinci-zkvm 1.3.2 image (compare
  the digests Track A recorded);
- `ZISK_MINIMAL_MEMORY_FROM` = 256 on both hosts (2048 OOM-killed nf-16 batches);
- `ruling 20 ok`.

On a mismatch, stop and report. The one exception is z7's `ZISK_MINIMAL_MEMORY_FROM`: the owner
allowed changes to z7's compose file and `.env`, so set it to 256 there, recreate the container, and
re-check `/health`. z6's production prover is not changed by this plan.

- [ ] **Step 2: Owner checkpoint 2.** Show:
  - `$BENCH gate`;
  - the wave 1 estimate: 18 elections (under 0.0002 ETH of OP organizer gas), about 44k ballots,
    about 165 transitions, Ethereum at most about 5 mETH at the gate's fee ceilings;
  - prover occupancy: about 5-7 h for R, S and S-c on both provers (S is three groups without
    restarts now; re-estimate it from the Task 13 dry run).

  Wait for "go wave 1".
- [ ] **Step 3: Plan.**

```bash
systemd-run --user --scope --unit davinci-bench-plan -p MemoryMax=12G -- $BENCH plan --wave 1 --onchain-census 2>&1 | redact | tee $DAVINCI_BENCH_DIR/plan-w1.log | grep -E "pins ok|election|skipped|plan.json"
```
Expected: `pins ok` for chain 10 and the new registry with its verifiers, 18 election lines (or R4
`skipped: …` with the reason), and `plan.json: wave 1, <n> elections`. If the owner chose up-front
generation at checkpoint 1, run `plan --wave 1,2,3` here (and `gen`/`merge` with `--wave 1,2,3` in
Step 4); Tasks 18 and 21 then skip their plan and generation steps.

- [ ] **Step 4: Generate on both hosts, then merge.** Follow Task 5 Steps 3-5 with
  `C=$DAVINCI_BENCH_DIR`, `--wave 1` and the campaign paths. On z7 use `~/davinci-bench/c1`, and copy
  only `plan.json` and `secret/seed`. Merge with the live registry check:
  `$BENCH merge --wave 1` (it calls `check_snapshots`). Expected: `merged <n> elections, ~44000 ballots,
  all proofs verify, snapshots match the registry`; scans clean on both hosts; the seed is deleted on
  z7.
- [ ] **Step 5: Run R, then S and S-c.**

```bash
$BENCH gate; echo exit $?
setsid -f systemd-run --user --scope --unit davinci-bench-run-w1 -p MemoryMax=12G -- \
  bash -c "$BENCH run --wave 1 --scenario all > $DAVINCI_BENCH_DIR/runs/w1.log 2>&1"
tail -f $DAVINCI_BENCH_DIR/runs/w1.log | grep -E "^\[bench|audit|results|stopped|FAILED"
```
Expected:
- progress lines;
- per scenario, `audit … no_vote_lost=true results_ok=true` for every election, and `clean=true`
  (R2's and R3's expected `process closed` are listed with their counts);
- `results … = tally`;
- `findings: none`, or each finding listed by kind; any `isolated`, `held`, `refresh_overflow`,
  `guest_rejection` or `results_failed` is reported to the owner at once;
- one `cell` line per S cell with its mean batch, `floor` where the floor bounded it;
- the roll-up `report.md` updated after each scenario, and one line per scenario appended to
  `progress.md` (Global Constraints, Reporting).

Stop rule: `stopped: budget` or `stopped on fees` ends the run with its audit; report it. Stop it
manually with `systemctl --user stop davinci-bench-run-w1.scope`, then `$BENCH cancel --wave 1`.
- [ ] **Step 6: Scan and record.** Run Task 14 Step 5's scanner over `~/.cache/davinci-bench`, then
  `$BENCH gate`. Write down the ceiling table and the spend against `budget.json` for the report.

### Task 16: Proxy delay and fail modes — needs: nothing

**Files:**
- Modify: `e2e/src/proxy.rs`

**Interfaces (produces):**
```rust
impl Proxy {
    pub fn set_delay(&self, path_prefix: &str, d: Duration);  // hold matching requests d before forwarding; Duration::ZERO clears;
                                                              // a request whose client closes during the hold is dropped
    pub fn dropped(&self) -> u64;                             // held requests dropped on a client close
    pub fn set_fail(&self, status: Option<u16>);              // answer `status` with an empty body, no forwarding
    pub fn delayed(&self) -> u64; pub fn failed(&self) -> u64;
}
```

- [ ] **Step 1: Write the failing tests:**
  - `delays_matching_paths`: with `set_delay("/prove", 300ms)`, `POST /prove` takes at least 300 ms
    and `GET /health` under 100 ms; `delayed() == 1`.
  - `fails_with_a_status`: `set_fail(Some(503))` gives 503 with the upstream untouched (the echo
    counter does not move); `set_fail(None)` forwards again.
  - `delay_drops_a_closed_client`: with `set_delay("/prove", 300ms)`, a client that sends
    `POST /prove` and closes its socket after 50 ms leaves the echo counter unchanged after 500 ms, and
    `dropped() == 1`. (X2's loser gives up on the winner's event; forwarding its held request anyway
    would run a wasted prover job every round and stall the next round's "provers idle" check by
    150 s plus the job.)
- [ ] **Step 2: Run them.** Run `cargo test -p davinci-e2e --lib proxy`. Expected: they fail.
- [ ] **Step 3: Implement.** Use a `Mutex<(String, Duration)>` and an `AtomicU32` status checked
  before forwarding. During a hold, `select!` on the sleep and a read of the client socket: a read of
  0 bytes (the client closed) drops the request.
- [ ] **Step 4: Run the tests again.** Expected: `ok`, with 3 new tests and the old ones passing.
- [ ] **Step 5: Commit.**
  `git add e2e/src/proxy.rs && git commit -m "feat(e2e): proxy delay and fail modes for slow provers and failing RPCs"`

### Task 17: Race scenarios X1-X5 — needs: two anvils, both GPU provers, the Ruling 20 node binary

**Files:**
- Create: `e2e/src/bench/race.rs`
- Modify: `e2e/src/bench/scenarios.rs` (wave 2 specs and scenarios), `e2e/src/bench/run.rs`
  (`Kind::Race`)

**Interfaces (produces):**
```rust
pub struct RaceSpec { pub k: usize, pub rounds: usize, pub skew: Option<(NodeId /* loser */, Duration)>, pub lag_confirmations: Option<u64> }
pub enum LossPath { Event, Revert, None }
pub struct RaceRow { pub round: usize, pub winner: Option<NodeId>, pub path: LossPath, pub seal_gap_s: f64,
    pub detect_s: Option<f64>, pub recover_s: Option<f64>, pub wasted_job_s: Option<f64>, pub wasted_posts: u64, pub wasted_wei: u128 }
pub fn race_row(round: usize, landing: &TransitionRow, a: &[LogEvent], b: &[LogEvent], jobs: &[JobRow]) -> RaceRow; // pure
pub async fn race_round(ctx: &Ctx, rig: &mut Rig, sc: &Scenario, tag: &str, round: usize, votes: &[BallotRecord]) -> Result<RaceRow>;
```

`race_round` goes through these stages:

1. **Preconditions.** O, A and B are at the chain root with `pendingVotes == 0`, and both provers are
   idle. The chain root is the registry's `stateRoot` for the process, read from OP; a lagging node
   (X3's loser at `confirmations 150`) counts once its `/processes` root equals it. With X3's lag each
   round takes at least 5 min.
2. **Skew.** `set_delay("/prove", D)` on the loser's prover proxy, and, for X3,
   `restart_with(loser, confirmations = 150)`.
3. **Burst.** Send K votes `Both` at once.
4. **Seal check.** Record the gap between the two `batch sealed reason=at_max` lines. A round
   with a gap above 6 s, or with one node never sealing, is reported `path: None` and does not
   count toward `rounds`; the run does not abort on it.
5. **Wait.** For the landing and the loser's sync.
6. **Undo the skew.**
7. **Row.** Build the `RaceRow`.

The run keeps adding rounds until it has `rounds` races, at most 2 × `rounds`. X4 and X5 are `Plain`
scenarios: `Both` with `End::Now`, and `Cross`.

- [ ] **Step 1: Write the failing tests:**
  - `race_row_event_and_revert`: synthetic events. The winner lands at t = 100 (OP ts). The loser logs
    `foreign transition beat` at 101.5 and `synced … root=<landing root>` at 104. The result is
    `path Event`, `detect 1.5`, `recover 4.0`. The same with `lost the settlement race` gives
    `Revert`. A round where the other node never sealed gives `None`.
  - `wave2_specs`: X1 has K 64, 10 rounds, no skew. X2 has two scenarios with a 150 s skew, one per
    loser. X3 has a skew and `lag_confirmations 150`. X4 and X5 have `Both` and `Cross` routes, and X4
    allows `process closed` (expected, counted: a race lost at the AGM end at nf 16 can leave a
    re-attempt that no longer fits the grace).
  - Properties, added to `scenarios_are_consistent`: every race's `K ≤ batch_max` of both nodes'
    knobs, and `K` times the rounds (at most 2 × `rounds`) fits the election's voters.
    `max_voters_equal_planned_voters` now covers wave 2.
- [ ] **Step 2: Run them.** Run `cargo test -p davinci-e2e --lib bench::race bench::scenarios`.
  Expected: they fail.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run the unit tests, then a GPU dry run.** Run `dry --scenario X1,X2-a,X3,X5 --scale 0.3`
  under the Task 13 command. Expected:
  - X1 reports `races <r>/<rounds>`, with the winners and paths shown;
  - X2 has every round won by the unskewed node, through the `Event` path;
  - X3 shows the `Revert` path in at least 1 round;
  - X2's loser proxy reports its held requests `dropped` (the loser gave up on the winner's event);
  - X5 reports `tally_ok=true` with the cross-node order;
  - every audit has `no_vote_lost=true clean=true results_ok=true` and `findings: none` (a stale error
    under `Both` would show here).
- [ ] **Step 5: Commit.**
  `git add e2e/src/bench/race.rs e2e/src/bench/scenarios.rs e2e/src/bench/run.rs e2e/src/bench/mod.rs && git commit -m "feat(e2e): bench race scenarios with forced seals, skew and loss-path metrics"`

### Task 18: Live wave 2, races — needs: network, both GPU provers, spend (owner checkpoint 3), the Ruling 20 node binary

- [ ] **Step 1: Owner checkpoint 3.** Show `$BENCH gate`, about 2.5k ballots, about 40 transitions plus
  about 25 lost posts (≤ 1.5 mETH worst case on Ethereum), and about 3 h on both provers. Of that, X3
  alone takes about 45 min: the loser at `confirmations 150` sees events 5 min late, so its own
  landings commit only at the confirmed event and its sync wait clears only when the sync arrives;
  each round takes at least 5 min, five rounds plus retries about 45 min. Wait for "go wave 2".
- [ ] **Step 2: Plan, generate on both hosts and merge.** Same as Task 15 Steps 3-4, with `--wave 2`
  (skipped if all waves were generated up front).
- [ ] **Step 3: Run.** Same as Task 15 Step 5, with `--wave 2` and the unit `davinci-bench-run-w2`.
  Expected:
  - a races table with winner, path, detect, recover and wasted posts per round;
  - X2 is deterministic;
  - X3 has `Revert` rows;
  - every audit has `no_vote_lost=true results_ok=true`; X4's `process closed` count, if any, is
    listed;
  - one line per scenario appended to `progress.md`.
- [ ] **Step 4: Scan and gate** as in Task 15 Step 6.

### Task 19: Chaos controller — needs: nothing

**Files:**
- Create: `e2e/src/bench/chaos.rs`

**Interfaces (produces):**
```rust
pub enum Rpc { Op, L1, Send, Beacon }
pub enum Trigger { At(Duration), L1Pending(NodeId), Sealed(NodeId), AfterEnd(Duration) }
pub enum Action { Kill { node: Role, down: Duration }, ProverDown { node: NodeId, for_: Duration },
    ProverDelay { node: NodeId, delay: Duration, for_: Duration }, RpcDown { node: Role, rpc: Rpc, index: usize, for_: Duration },
    RpcFail { node: Role, rpc: Rpc, index: usize, status: u16, for_: Duration }, Knobs { node: NodeId, knobs: Knobs, for_: Option<Duration> } }
pub struct Chaos { pub name: String, pub trigger: Trigger, pub action: Action }
pub struct Observed { pub elapsed: Duration, pub ended_for: Option<Duration>, pub pending_nonce_gap: [bool; 2], pub new_seals: [u64; 2] }
pub fn due(t: &Trigger, o: &Observed) -> bool;  // pure
pub struct ChaosRow { pub name: String, pub due_s: f64, pub applied_s: f64, pub undone_s: Option<f64>, pub effects: Vec<String>, pub recovery_s: Option<f64> }
pub async fn apply(rig: &mut Rig, net: &Net, a: &Action) -> Result<Undo>;   // Undo carries what to restore and when
/// Whether the action's node and proxy index exist, given each role's URL counts [OP, L1, Send, Beacon].
pub fn references_exist(a: &Action, urls: &[[usize; 4]; 3]) -> bool;
```

- [ ] **Step 1: Write the failing tests:**
  - `triggers_fire_once_their_condition_holds`: one case per trigger variant, with an `Observed` just
    before and just after its condition.
  - `undo_restores_the_knobs`: `Knobs { l1_max_post_fee: Some("0.000000001eth") }` for 15 min yields
    an `Undo` that holds the previous knobs and the deadline.
  - `references_are_checked`: an `RpcDown` on O's beacon index 1 passes with two beacons and fails
    with one; a `ProverDown` on a node passes (every signing node has a prover proxy).
- [ ] **Step 2: Run them.** Run `cargo test -p davinci-e2e --lib bench::chaos`. Expected: they fail.
- [ ] **Step 3: Implement.** `L1Pending` reads `get_transaction_count(addr).pending()` against
  `latest` every 250 ms. On anvil, `dry` calls `L1Anvil::pause_mining` at the trigger's `Sealed`
  precursor and `resume_mining` after the kill, as `l1_restart.rs` does. Effects come from the node
  log `Kind`s between the apply and the undo.
- [ ] **Step 4: Run the tests again.** Expected: `ok. 3 passed`.
- [ ] **Step 5: Commit.**
  `git add e2e/src/bench/chaos.rs e2e/src/bench/mod.rs && git commit -m "feat(e2e): bench chaos triggers and actions on nodes and proxies"`

### Task 20: Chaos scenarios C1-C8 — needs: two anvils, both GPU provers, the Ruling 20 node binary

**Files:**
- Modify: `e2e/src/bench/scenarios.rs` (wave 3 specs and scenarios), `e2e/src/bench/run.rs` (the
  chaos loop beside the submitter, `ChaosRow`s in the report)

- [ ] **Step 1: Write the failing test** `wave3_scenarios`: C1-C8 match spec §8. Every scenario has the
  background load (Poisson 30/min, nf 8, `ByVoter`) and only C8 allows `process closed`. C3's kill
  targets its election's key node. C7 holds two `Knobs` actions (B for 15 min; both at the end, undone
  120 s into the grace window). Properties, added to `scenarios_are_consistent`: every chaos action
  passes `references_exist` for the live URL layout (each node's OP, L1, send and beacon lists from a
  fixture env with two beacons), and `max_voters_equal_planned_voters` now covers wave 3.
- [ ] **Step 2: Run it.** Run `cargo test -p davinci-e2e --lib bench::scenarios`. Expected: it fails.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run a GPU dry run** of `C1,C3,C4,C6,C7` at scale 0.2 (the Task 13 command). Expected:
  - C1 logs `L1 post of an earlier run on record` and a replacement at its nonce;
  - C3 publishes its results after the restart (`results_ok=true`);
  - C4 posts the batch's blobs on each of its three attempts, logs the 10 min sealing pause
    (`pause_secs=600 failed_cycles=3`), and settles the votes within 2 min of the pause's end;
    none of them error; the chaos row shows 3 wasted posts;
  - C6 logs `RPC failover` and `beacon failover`;
  - in C7, B logs `L1 post failed; retrying … FeeCap` until the job deadline, requeues
    (`transient settlement failure`), re-proves every cycle (count the prover jobs), posts nothing
    while capped, then settles after the restore;
  - every audit has `no_vote_lost=true clean=true results_ok=true`, and no `bisections` finding (a
    down prover is a transient failure, not a failed job).
- [ ] **Step 5: Commit.**
  `git add e2e/src/bench/scenarios.rs e2e/src/bench/run.rs && git commit -m "feat(e2e): bench chaos scenarios: kills, prover and RPC faults, fee caps, early ends"`

### Task 21: Live wave 3, chaos — needs: network, both GPU provers, spend (owner checkpoint 4), the Ruling 20 node binary

- [ ] **Step 1: Owner checkpoint 4.** Show `$BENCH gate`, about 6k ballots, about 40 transitions
  (≤ 2 mETH worst case), about 4 h on both provers, and what C4, C5 and C7 cost: C4 pauses A for
  10 min after three attempts that each post their blobs (3 wasted posts, recovery about 11-12 min);
  C5 occupies prover B; C7 posts nothing while capped but re-proves every cycle (about 3-4 wasted GPU
  jobs in 15 min). Wait for "go wave 3".
- [ ] **Step 2: Plan, generate on both hosts, merge, run.** Same as Task 15 Steps 3-5, with `--wave 3`
  (plan and generation skipped if all waves were generated up front). Expected: a chaos table
  (applied, effects, recovery, wasted posts and prover jobs); every audit has `no_vote_lost=true
  results_ok=true`; C8's `process closed` count is reported if any; one line per scenario appended
  to `progress.md`.
- [ ] **Step 3: Scan and gate** as in Task 15 Step 6.

### Task 22: TypeScript SDK bench script — needs: the npm registry for `yarn install` (no chain)

**Repository:** `/home/p4u/davinci-sdk`, on branch `feat/op-bench`.

**Files:**
- Create:
  - `examples/bench/package.json` (like `examples/script`, with scripts `bench: tsx src/bench.ts`,
    `test: node --import tsx --test test/*.test.ts`, `typecheck: tsc --noEmit`);
  - `examples/bench/tsconfig.json` (a copy of `examples/script`'s, `include: ["src/**/*", "test/**/*", …]`);
  - `examples/bench/src/config.ts`, `schedule.ts`, `stats.ts`, `ledger.ts`, `prove-worker.ts`,
    `bench.ts`;
  - `examples/bench/test/schedule.test.ts`, `ledger.test.ts`, `stats.test.ts`, `config.test.ts`.

**Interfaces (produces):**
```ts
// config.ts: env only; never prints OP_RPC or the key
export interface Config { opRpc: string; nodes: string[]; keyNode: number; organizerKeyFile: string; registry: string;
  startBlock: number; pins: DeploymentPins; artifactsDir: string; voters: number; nf: number; revotePct: number;
  workers: number; route: 'a' | 'b' | 'both' | 'pick'; ratePerMin: number; grace: number; notice: number; out: string }
export function loadConfig(env: NodeJS.ProcessEnv): Config;
export function redact(s: string): string;              // scheme://host only, as in test/e2e/env.ts
// schedule.ts
export function offsets(n: number, ratePerMin: number, seed: number): number[];  // ms; 0 rate = all at 0
// stats.ts
export function percentile(xs: number[], p: number): number; export function perMinute(tsMs: number[]): number;
// ledger.ts
export type Status = { status: 'pending' | 'aggregated' | 'processed' | 'settled' | 'error'; error?: string };
export function classify(accepted: Map<string, string[]>, status: Map<string, Status[]>, allowed: string[]):
  { settled: string[]; errored: Map<string, number>; unexpected: string[]; lost: string[]; staleError: string[] };
  // one Status per node that took the vote; settled on any node wins (an `error` on the other is a stale error);
  // allowed reasons match by substring, never one containing `(held:`
export function tally(last: Map<string, number[]>, nf: number): number[];
```

`bench.ts` covers these stages:

1. **T1.** `DavinciSDK` with
   `network: { chainId: 10, processRegistry, startBlock, rpcUrls: [OP_RPC] }`,
   `verifyDeployment: { pins }` read from `PINS_FILE` (`pins.json` of the campaign),
   `keySequencerUrl: nodes[keyNode]`, and a CSP census (`CspSigner` with a key held in memory).
   It creates the process with sequencer keys and `metadataUri: 'https://bench.invalid/<run>.json'`,
   `metadataHash: sha256(JSON of a fixed title/questions document)`; no `uploader`, nothing is
   downloaded. On any failure after creation it ends the process (`closeProcessIn(NOTICE)`) or
   cancels it, so `cancel` has nothing to find. It then calls `setProcessGrace(GRACE)` and waits
   until both nodes list it.
2. **T2.** `WORKERS` child processes (`fork` with `--import tsx`), each with its own
   `BallotProver({ artifacts: { dir } })`, `verifyProof: false`, run `buildBallot` → `prove` →
   `signVoteId`, and stream `{i, round, req, ms}` back.
3. **T3.** Submit at `RATE` (0 means as soon as proved) by `ROUTE`. Retries follow Task 8's table.
4. **T4.** Poll every accepted vote every 10 s (16 concurrent) until each is settled or errored,
   recording first-seen times. (Every 3 s at 32 concurrent is about 500 requests/s against one node
   for 1500 votes.)
5. **T5.** Close with `NOTICE`, then `waitForResults`; the results must equal the tally of each
   voter's last ballot. Every voter's ballots go to one node, or identically to both.
6. **Report.** JSON and Markdown under `OUT`, with progress every 15 s and redacted errors.

- [ ] **Step 1: Write the failing tests:**
  - `schedule.test.ts`: `offsets(600, 60, 1)` is non-decreasing, its last value is about 600,000 ms
    (within 10 %), it is deterministic per seed, and rate 0 gives all zeros.
  - `stats.test.ts`: `percentile([1..100], 90) === 90`; `perMinute` of 120 settlements over 60 s
    is 120.
  - `ledger.test.ts`:
    - accepted and settled is `settled`;
    - `error 'process closed'` with it allowed is `errored`, not `unexpected`;
    - `error 'process closed (held: x)'` with `process closed` allowed is `unexpected`;
    - accepted by both nodes, `settled` on one and `error` on the other, is `settled` and listed in
      `staleError`;
    - pending at the end is `lost`;
    - `tally` sums the last ballots.
  - `config.test.ts`: `redact('x https://u:p@h.example/v2/KEY?y=1 z') === 'x https://h.example z'`, so
    the private RPC's path never reaches a log.
- [ ] **Step 2: Run them.**
  `cd /home/p4u/davinci-sdk && git checkout -b feat/op-bench && cd examples/bench && yarn install && yarn test`.
  Expected: they fail.
- [ ] **Step 3: Implement** the modules and `bench.ts`.
- [ ] **Step 4: Run them again.** Run `yarn test && yarn typecheck`. Expected: `# fail 0` and no type
  errors.
- [ ] **Step 5: Commit** in davinci-sdk.
  `git add examples/bench/package.json examples/bench/tsconfig.json examples/bench/yarn.lock examples/bench/src examples/bench/test && git commit -m "feat(bench): op mainnet e2e benchmark script with per-stage timings"`

### Task 23: TS live runs — needs: network, both GPU provers, spend (owner checkpoint 5), the Ruling 20 node binary

- [ ] **Step 1: Owner checkpoint 5.** Show `$BENCH gate`, three runs (3, 500 and 1500 voters), about
  15 transitions (≤ 1 mETH worst case), and about 1.5 h on both provers. Wait for "go ts".
- [ ] **Step 2: Serve the rig.**

```bash
. ~/.config/davinci-l1blob/bench.sh
setsid -f systemd-run --user --scope --unit davinci-bench-serve -p MemoryMax=12G -- bash -c "$BENCH serve > $DAVINCI_BENCH_DIR/serve.log 2>&1"
until curl -sf http://127.0.0.1:9102/ping; do sleep 2; done
```

- [ ] **Step 3: Run the smoke, then the two runs.**

```bash
cd /home/p4u/davinci-sdk/examples/bench
export NODES=http://127.0.0.1:9101,http://127.0.0.1:9102 ORGANIZER_KEY_FILE=$HOME/optimism-privkey.txt \
  REGISTRY=$DAVINCI_E2E_REGISTRY START_BLOCK=$DAVINCI_E2E_FROM_BLOCK PINS_FILE=$DAVINCI_BENCH_DIR/pins.json \
  ARTIFACTS_DIR=/home/p4u/davinci-circom/artifacts OP_RPC
VOTERS=3 NF=2 WORKERS=1 ROUTE=a OUT=$DAVINCI_BENCH_DIR/ts/smoke yarn bench 2>&1 | redact | tail -5
VOTERS=500 NF=8 WORKERS=8 ROUTE=pick REVOTE_PCT=10 RATE=0 OUT=$DAVINCI_BENCH_DIR/ts/pick-500 yarn bench 2>&1 | redact | tail -8
VOTERS=1500 NF=16 WORKERS=8 ROUTE=both RATE=300 KEY_NODE=1 OUT=$DAVINCI_BENCH_DIR/ts/both-1500 yarn bench 2>&1 | redact | tail -8
```
Expected: each run ends with `audit: settled <n> errored 0 lost 0 stale 0`,
`results = tally`, the stage percentiles (prove, submit, submit→settled), votes/min, and
`report: …/report.md`. A failed run leaves no open process (the script ends or cancels it); append
one line per run to `progress.md`.

- [ ] **Step 4: Stop the rig, scan, gate.**
  `systemctl --user stop davinci-bench-serve.scope`, then the Task 14 Step 5 scanner over
  `~/.cache/davinci-bench` and `$BENCH gate`.

### Task 24: Campaign report and cleanup — needs: network (reads), organizer key for `cancel`, Track A and the Ruling 20 node binary (for `gate`)

**Files:**
- Create: `docs/superpowers/reports/<date>-live-bench.md`. It is built from
  `$DAVINCI_BENCH_DIR/report.md` and the TS reports, and contains no URL other than the node and prover
  hosts.

- [ ] **Step 1: Leftovers.** Run
  `systemctl --user list-units 'davinci-bench-*' --no-legend | wc -l; $BENCH cancel --all 2>&1 | redact`.
  `--all` scans every process the organizer created since `start_block` (registry logs, as
  `cancel_leftovers` does), so the TS runs' processes are covered, not only `plan.json`'s.
  Expected: `0`, and `canceled 0 of <n>` (or the list of leftovers it canceled).
- [ ] **Step 2: Write the report.** Include:
  - the setup (commits, binary sha256s copied from `gate`'s output, image digests, the registry and
    verifier addresses, `ZISK_MINIMAL_MEMORY_FROM` on both hosts);
  - the ceiling table, with each cell's mean batch and the cells the 240 s floor bounded (and that,
    at nf 16, OP's floor bounds open-window batches to what the converged estimate allows);
  - latency and stage tables;
  - races;
  - chaos;
  - audits (every election's verdicts, including `results_ok` and `observer incomplete`);
  - findings by kind: unexpected errors, stale errors, `bisections` and isolated votes, `held`
    batches, `refresh_overflow`, `guest_rejection`, failed or missing results, votes pending after
    the wait;
  - spend against caps;
  - TS stage timings;
  - open issues.
- [ ] **Step 3: Scan the report.** Run the scanner over the report and
  `~/.cache/davinci-bench`. Expected: `0 with a secret`.
- [ ] **Step 4: Final audit and commit.** Dispatch the final Fable audit (the report's numbers against
  the run JSONs, the audit verdicts, and the secrets). Then:
  `git add docs/superpowers/reports/<date>-live-bench.md && git commit -m "docs(bench): live op mainnet benchmark campaign report"`
