# L1-blob DA, Plan 2: the node, the client and their tests

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the DAVINCI node and client run a registry in L1-blob mode (settlement on an OP Stack chain, blobs in Ethereum blob transactions) next to today's blob mode: configuration and the `optimism` preset, mode detection at boot, a dedicated L1 blob poster, the actor's prove-while-posting and settle loop with the L1 proof, `ProcessStateTransitionedL1` sync from Ethereum blob sources, and unit, fake-chain and two-anvil tests.

**Architecture:** The registry's `l1BlobVerifier()` decides the mode; the node refuses to boot when `--l1-rpc-url` disagrees. A new module `sequencer/src/l1da/` holds the policy the actor calls (the window-1 `Poster`, `with_post` that posts while the batch is proved and records OP's origin once, and `settle`, which waits for the blob block and the origin, picks the anchor, builds and checks the L1 proof, simulates and sends); it talks to Ethereum through the `L1Chain` trait (`web3/l1.rs`, real `L1Rpc`, test `FakeL1`) and to OP through new `Chain` methods (`web3/l1op.rs`). Blob mode keeps its code path; the actor branches on `Deps.l1`.

**Tech Stack:** Rust 1.95 (edition 2024), alloy 2.5 (already with `trie`, `rlp`, `kzg`, `signer-local`), tokio, redb, axum 0.7 stubs in tests, anvil 1.8.3 (`~/.foundry/bin`), davinci-contracts artifacts (`forge build` on branch `feat/l1-blob-da`).

**Spec:** `docs/superpowers/specs/2026-10-12-l1-blob-da-op-design.md` (sections 5 and 6 are this plan's scope).

**Addendum that overrides the spec where they differ:** `.superpowers/plan2-notes.md`, in particular its section "After the Fable gate" (measured gas, the anchor policy with `record()`, re-prove on restart plus a persisted L1 post record, estimate + 20 % gas instead of a fixed limit, event pairing rules).

**Precondition (from Plan 1's fix round):** davinci-contracts `IL1BlobVerifier` has `record()`, `recorded(uint256)`, `MAX_AGE()` (7200), `error TxTooOld(uint256)`, `event OriginRecorded(uint256 indexed number, bytes32 hash)`, and `verify` returns `(bytes32[] vhs, uint256[] blobBlocks)` with one block per versioned hash; `sequencer/src/web3/l1proof.rs` has `pub fn verify_local(p: &L1BlobProof, anchor: B256) -> Result<(Vec<B256>, Vec<u64>), L1ProofError>` whose second vector is per blob. Task 3 Step 1 checks this.

## Global Constraints

- No change to the zkVM program, any program vk, or the contracts; this plan consumes the ABI fixed by Plan 1 and its fix round.
- Blob mode behaviour is unchanged: same calls, same order, same errors. Existing tests pass with only mechanical edits (`l1: None` in `StateTransitioned` literals, `l1: None` in `Deps`, `da` in the test `Contracts`).
- The mode is the registry's `l1BlobVerifier()` (zero, or a registry without the function, is blob mode). A node whose `--l1-rpc-url` presence disagrees refuses to boot.
- Header parsing, anchoring and the trie proof stay in `web3/l1proof.rs` (Plan 1); the node only calls `build` and `verify_local`.
- Secrets never logged or persisted: the L1 key is read like the OP key (`--l1-privkey-file` or `DAVINCI_L1_PRIVKEY`, never a command-line value, `SecretString`, zeroed). The re-encryption seed and refresh selection stay in memory only, so a restart re-prepares and re-proves. The L1 post record holds no secret (pid, roots, nonce, hash, fees, versioned hashes). Never `{:?}` a `ProveRequest` or a signed blob envelope; log URLs by host only.
- No new dependencies.
- CPU-heavy work off the async workers: sidecars in `spawn_blocking`; state work in the actor through `actor.rs::cpu()`.
- Blobs stay on the heap (`Box`, `Bytes`); never pass a 128 KiB blob by value through serde or deep call chains.
- No panics on external input; `thiserror` in libraries; short comments that say why.
- Storage: `SCHEMA_VERSION` 8 → 9 (new table `l1_posts`). A version-8 file upgrades in place (the change only adds a table); any other version is refused as today.
- L1 sender: a dedicated L1 account; one blob transaction in flight; `to` = own address, empty data, 21,000 gas; tip `max(10^6 wei, eth_maxPriorityFeePerGas)`; `max_fee = 2·base + tip`; `max_fee_per_blob_gas = max(2·blob base fee, 1)`; replacement after 3 L1 blocks with all three fees doubled; worst-case cost `21000·max_fee + blobs·131072·blob_fee` at most `--l1-max-post-fee` (default `0.001eth`), else wait; nonce = the `latest` transaction count, never a gap; EIP-7594 sidecars from today's `build_sidecar`; at most 6 blobs (the L1 chain's cap).
- Anchor policy: L1 head ≥ n + 1 (n = blob block) and OP origin ≥ n; current-origin proofs carry headers from `min(head, origin + 2)` down to n while that is at most 16 headers; else headers from a recorded origin m down to n; else, without a record, up to 64 headers from the current origin; past 64 headers or 7,200 blocks behind the origin, post the blobs again. `record()` is sent at most once per batch, only when the origin reaches n before the proof is ready.
- OP settlement through the existing `Contracts::send(tx, None)` (gas estimate + 20 %), never a fixed gas limit. `L1OriginMismatch` rebuilds the proof after re-reading the L1 head and the origin, at most 5 times.
- Commit titles `type(scope): lowercase title`, no AI attribution or `Co-Authored-By` lines. Commit on branch `feat/l1-blob-da`; never push.
- Build output stays in the repository's `target/` or under `~/.cache`, never `/tmp`.
- Before every commit: `cargo fmt --all -- --check` and `cargo clippy --workspace --all-targets -- -D warnings` pass.

## Review Focus

- **OP's origin stalls or lags far behind the blob block** (sequencer drift of minutes): the batch must wait, anchor at its recorded origin past 16 headers, re-post past 64 without one, and never error the votes. Tests: Task 8 `a_recorded_origin_anchors_a_deep_proof`, `a_proof_too_deep_without_a_record_reposts`; Task 10 `l1_origin_lag_records_once_and_anchors_at_the_record`.
- **An L1 fee spike:** no post whose worst case exceeds `--l1-max-post-fee`; a replacement above it keeps the current price and waits; posting resumes when fees fall. Tests: Task 7 `the_fee_cap_waits_for_cheaper_fees`, `a_replacement_above_the_cap_waits_at_the_current_price`.
- **A restart with a blob transaction still pending, or an orphan the database lost:** the next post replaces it at its nonce with every fee doubled, never queues behind it, never leaves a gap. Tests: Task 7 `a_restart_replaces_the_recorded_post`, `an_unrecorded_orphan_is_outbid`; Task 10 `l1_restart_replaces_the_pending_post`.
- **A public L1 RPC that rejects 1.5 MB bodies (HTTP 413) or blob methods:** blob sends go only to `--l1-send-rpc-url`; a 413 is a refusal, not "maybe sent". Tests: Task 5 `blob_sends_go_to_the_send_endpoints`, `send_errors_are_classified`.
- **A follower whose first beacon API does not keep full blobs** (404 after PeerDAS): blobs come from the next beacon, and blobs that do not match their versioned hashes are refused. Tests: Task 9 `l1_blobs_fail_over_to_a_full_blob_beacon`, `l1_blobs_must_match_their_hashes`.

---

## File Structure

davinci-sequencer (`/home/p4u/davinci-sequencer`):

- Modify `client/src/networks.rs`: `NetworkDa`, `OP_L1_BLOCK`, the `optimism` preset.
- Modify `client/src/organizer.rs`: `RegistryDa`, `RegistryInfo.da`, `verify_registry` reads the mode.
- Modify `client/tests/organizer.rs`: an L1-mode registry in `registry_pins`.
- Modify `sequencer/src/config.rs`: `--l1-rpc-url`, `--l1-send-rpc-url`, `--l1-privkey-file`/`DAVINCI_L1_PRIVKEY`, `--l1-max-post-fee`, zero-registry refusal, `check_l1`, `parse_wei`.
- Create `sequencer/abi/L1BlobVerifier.json`: `jq .abi` of the forge artifact.
- Create `sequencer/src/web3/l1op.rs`: OP-side L1 calls on `Contracts` (mode, origin, `record()`, `submitStateTransitionL1`).
- Create `sequencer/src/web3/l1.rs`: the `L1Chain` trait, its types and the real `L1Rpc`.
- Modify `sequencer/src/web3/mod.rs`: modules, `L1Origin`, `L1Blobs`, `RegistryDa`, `l1v` bindings, `StateTransitioned.l1`.
- Modify `sequencer/src/web3/contracts.rs`: `da` cell, `open(.., blobs)`, `blob_support`, verifier error names, L1 event pairing, `Contracts::at` for unit tests.
- Modify `sequencer/src/web3/tx.rs`: visibility of helpers the L1 sender reuses; the test handle.
- Modify `sequencer/src/web3/blobs.rs`: `BlobSource::blobs_for_l1`, Anvil and beacon implementations, `blob_source` reads L1 in L1 mode.
- Modify `sequencer/src/storage.rs`: `L1PostRecord`, table `l1_posts`, schema 9.
- Create `sequencer/src/l1da/mod.rs`, `sequencer/src/l1da/post.rs`, `sequencer/src/l1da/settle.rs`: the L1-blob policy.
- Modify `sequencer/src/lib.rs`: `pub mod l1da;`.
- Modify `sequencer/src/actor.rs`: `Chain` L1 methods, `Deps.l1`, the job split, the flight's cancel guard, commit at the event in L1 mode, sync with `L1Blobs`.
- Modify `sequencer/src/api/mod.rs`: boot mode check, L1 connect, blob cap from L1.
- Create `sequencer/tests/common/l1.rs`: `FakeL1`.
- Modify `sequencer/tests/common/mod.rs`: FakeChain L1 support, `FakeBlobs::blobs_for_l1`, `start_l1_node`.
- Create `sequencer/tests/l1da.rs`, `sequencer/tests/actor_l1.rs`.
- Modify `sequencer/tests/storage.rs`, `sequencer/tests/actor.rs` (one literal), `sequencer/tests/web3_anvil.rs` (one literal plus the two-anvil tests).
- Modify `docs/lifecycle.md`, `docs/configuration.md`, `docs/deployments.md`, `docs/testing.md`, `README.md`.

---

### Task 1: Client: DA modes, the `optimism` preset and `verify_registry`

**Files:**
- Modify: `client/src/networks.rs` (struct `Network` L8-21, `GNOSIS` L35-48, `NETWORKS` L51, tests L62-91)
- Modify: `client/src/organizer.rs` (`sol!` blocks L38-96, `RegistryInfo` L982-995, `verify_registry` L1008-1107)
- Test: `client/tests/organizer.rs` (`deploy_registry`/`deploy_registry_with` L71-104, `registry_pins` L106-171)

**Interfaces:**
- Produces:
  - `pub enum NetworkDa { Blobs, L1Blobs { l1_chain_id: u64, l1_block: Address, l1_rpc_urls: &'static [&'static str] } }` (`Clone, Copy, Debug, PartialEq, Eq`)
  - `pub const OP_L1_BLOCK: Address` = `0x4200000000000000000000000000000000000015`
  - `Network.da: NetworkDa`; `pub const OPTIMISM: Network` (chain 10, registry `Address::ZERO`, start block 0); `NETWORKS = &[GNOSIS, OPTIMISM]`
  - `pub enum RegistryDa { Blobs, L1Blobs { verifier: Address, l1_block: Address } }` (`Clone, Copy, Debug, PartialEq, Eq`) in `davinci_client::organizer`
  - `RegistryInfo.da: RegistryDa`

- [ ] **Step 1: Write the failing preset test**

In `client/src/networks.rs`, append to `mod tests`:

```rust
    #[test]
    fn optimism_posts_its_blobs_on_ethereum() {
        assert_eq!(by_name(" Optimism "), Some(&OPTIMISM));
        assert_eq!(OPTIMISM.chain_id, 10);
        assert_eq!(OPTIMISM.confirmations, 1);
        let NetworkDa::L1Blobs {
            l1_chain_id,
            l1_block,
            l1_rpc_urls,
        } = OPTIMISM.da
        else {
            panic!("optimism must be an L1-blob preset");
        };
        assert_eq!((l1_chain_id, l1_block), (1, OP_L1_BLOCK));
        assert!(!l1_rpc_urls.is_empty());
        // Followers read the blobs from Ethereum beacon APIs.
        assert!(!OPTIMISM.beacon_urls().is_empty());
        assert_eq!(GNOSIS.da, NetworkDa::Blobs);
    }
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cd /home/p4u/davinci-sequencer && cargo test -p davinci-client --lib networks 2>&1 | tail -5`
Expected: FAIL to compile (`OPTIMISM`, `NetworkDa`, `OP_L1_BLOCK` not found).

- [ ] **Step 3: Implement the preset**

In `client/src/networks.rs`, add the field at the end of `struct Network` (after `confirmations`):

```rust
    /// Where the deployment publishes transition data.
    pub da: NetworkDa,
}

/// How a deployment publishes transition data.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NetworkDa {
    /// Blob transactions on the registry's own chain.
    Blobs,
    /// Ethereum blob transactions, proven on an OP Stack chain through the
    /// registry's `L1BlobVerifier`. `blob_source` then names Ethereum beacon APIs.
    L1Blobs {
        /// Ethereum's chain id.
        l1_chain_id: u64,
        /// The `L1Block` predeploy the verifier reads OP's origin from.
        l1_block: Address,
        /// Ethereum execution JSON-RPCs, in order of preference.
        l1_rpc_urls: &'static [&'static str],
    },
}

/// OP Stack's `L1Block` predeploy.
pub const OP_L1_BLOCK: Address = address!("4200000000000000000000000000000000000015");
```

(The closing brace shown first replaces the existing closing brace of `struct Network`.) Add `da: NetworkDa::Blobs,` as the last field of `GNOSIS`, then add after `GNOSIS`:

```rust
/// OP Mainnet in L1-blob mode: transitions settle on OP, their blobs live on
/// Ethereum. Not deployed yet: the zero registry makes the node refuse this
/// preset (`--registry` overrides it) until a release fills in the registry
/// and its start block. The endpoint lists are provisional until then.
pub const OPTIMISM: Network = Network {
    name: "optimism",
    chain_id: 10,
    registry: Address::ZERO,
    start_block: 0,
    rpc_urls: &[
        "https://optimism-rpc.publicnode.com",
        "https://mainnet.optimism.io",
    ],
    // Ethereum beacon APIs: they must keep full blobs (supernodes, archives).
    blob_source: "beacon:https://ethereum-beacon-api.publicnode.com",
    confirmations: 1,
    da: NetworkDa::L1Blobs {
        l1_chain_id: 1,
        l1_block: OP_L1_BLOCK,
        l1_rpc_urls: &[
            "https://ethereum-rpc.publicnode.com",
            "https://eth.drpc.org",
        ],
    },
};
```

and change `NETWORKS`:

```rust
pub const NETWORKS: &[Network] = &[GNOSIS, OPTIMISM];
```

- [ ] **Step 4: Run the networks tests**

Run: `cargo test -p davinci-client --lib networks 2>&1 | tail -5`
Expected: PASS (`lookup`, `gnosis_preset_is_deployed`, `names_are_unique_and_lowercase`, `optimism_posts_its_blobs_on_ethereum`).

- [ ] **Step 5: Write the failing `verify_registry` test**

In `client/tests/organizer.rs`, replace `deploy_registry` and `deploy_registry_with` (L71-104) with:

```rust
/// A registry pinned to the release, except for `chain_id`, `verifier` and
/// `batch_vk`, with no DKG manager, in blob mode.
async fn deploy_registry(
    p: &impl Provider,
    chain_id: u32,
    verifier: Address,
    batch_vk: [u8; 32],
) -> anyhow::Result<Address> {
    deploy_registry_with(p, chain_id, verifier, batch_vk, Address::ZERO, Address::ZERO).await
}

/// [`deploy_registry`] with a Council manager and an L1 blob verifier (zero:
/// blob mode).
async fn deploy_registry_with(
    p: &impl Provider,
    chain_id: u32,
    verifier: Address,
    batch_vk: [u8; 32],
    council_manager: Address,
    l1_blob_verifier: Address,
) -> anyhow::Result<Address> {
    let vk_hash = BallotVerifier::from_snarkjs_json(release::ballot_vk_json())?.vk_hash();
    let args = (
        chain_id,
        verifier,
        B256::from(batch_vk),
        B256::from(release::RESULTS_PROGRAM_VK),
        B256::from(release::ROOT_C_VADCOP_FINAL),
        B256::from(vk_hash),
        Address::ZERO, // _dkgManager
        council_manager,
    )
        .abi_encode_params();
    let mut code = registry_bytecode();
    code.extend_from_slice(&args);
    code.extend_from_slice(&GRACE_ARGS.abi_encode_params());
    code.extend_from_slice(&l1_blob_verifier.abi_encode());
    deploy(p, code).await
}
```

In `registry_pins`, change the Council call `deploy_registry_with(&p, 31337, verifier, vk, manager)` to `deploy_registry_with(&p, 31337, verifier, vk, manager, Address::ZERO)`, add `assert_eq!(info.da, RegistryDa::Blobs);` right after the first `verify_registry(&url, good)` assertion, and append before `Ok(())`:

```rust
    // L1-blob mode: the verifier and the L1Block it reads come back.
    let l1_block = deploy(&p, bytecode("MockL1Block.sol/MockL1Block.json")).await?;
    let mut code = bytecode("L1BlobVerifier.sol/L1BlobVerifier.json");
    code.extend_from_slice(&l1_block.abi_encode());
    let l1v = deploy(&p, code).await?;
    let r = deploy_registry_with(&p, 31337, verifier, vk, Address::ZERO, l1v).await?;
    let info = verify_registry(&url, r).await?;
    assert_eq!(
        info.da,
        RegistryDa::L1Blobs {
            verifier: l1v,
            l1_block
        }
    );
```

Add `RegistryDa` to the file's `use davinci_client::organizer::{...}` list.

- [ ] **Step 6: Run it to verify it fails**

Run: `cargo test -p davinci-client --test organizer --no-run 2>&1 | tail -5`
Expected: FAIL to compile (`RegistryDa` not found, no field `da`).

- [ ] **Step 7: Implement the mode read**

In `client/src/organizer.rs`, after the `IZiskVerifier` `sol!` block add:

```rust
sol! {
    /// The one `L1BlobVerifier` view the client reads.
    #[sol(rpc)]
    interface L1Verifier {
        function l1Block() external view returns (address);
    }
}
```

(Named `L1Verifier`: the registry bindings already declare an `IL1BlobVerifier` library for its structs.) Add after `RegistryInfo`:

```rust
/// How a registry publishes transition data (`l1BlobVerifier()`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RegistryDa {
    /// Blob transactions on the registry's own chain: a zero verifier, or a
    /// registry from before the mode.
    Blobs,
    /// Ethereum blob transactions proven through `verifier`, which reads OP's
    /// L1 origin from `l1_block`.
    L1Blobs { verifier: Address, l1_block: Address },
}
```

Add the field to `RegistryInfo` (after `council_adapter`):

```rust
    /// The DA mode: where settlements put their blobs.
    pub da: RegistryDa,
```

In `verify_registry`, extend the doc comment's last sentence with "It also reports the DA mode (a registry without `l1BlobVerifier()` is in blob mode).", and insert before `Ok(RegistryInfo {`:

```rust
    let da = match r.l1BlobVerifier().call().await {
        Ok(a) if a == Address::ZERO => RegistryDa::Blobs,
        Ok(a) => {
            let l1_block = L1Verifier::new(a, &provider)
                .l1Block()
                .call()
                .await
                .map_err(chain_err)?;
            RegistryDa::L1Blobs {
                verifier: a,
                l1_block,
            }
        }
        Err(e) if missing_function(&e) => RegistryDa::Blobs,
        Err(e) => return Err(chain_err(e)),
    };
```

and add `da,` to the returned struct literal.

- [ ] **Step 8: Run the client tests**

Run: `cargo test -p davinci-client 2>&1 | tail -5`
Expected: PASS. Then `cd /home/p4u/davinci-contracts && ~/.foundry/bin/forge build 2>&1 | tail -2 && cd /home/p4u/davinci-sequencer && ANVIL=1 cargo test -p davinci-client --test organizer registry_pins 2>&1 | tail -5`
Expected: PASS (the line `test registry_pins ... ok`, not a skip line).

- [ ] **Step 9: Commit**

```bash
cargo fmt --all && cargo clippy -p davinci-client --all-targets -- -D warnings
git add client/src/networks.rs client/src/organizer.rs client/tests/organizer.rs
git commit -m "feat(client): optimism preset and the registry's da mode"
```

---

### Task 2: Node configuration for L1-blob mode

**Files:**
- Modify: `sequencer/src/config.rs` (imports L8-13, parsers L168-217, `Config` L223-373, `Debug` L375-413, `load` L418-427, `resolve_network` L464-496, `check_chain_id` L505-529, `resolve_privkey` L533-555, tests L570-844)

**Interfaces:**
- Consumes: Task 1 `NetworkDa`, `OPTIMISM`.
- Produces (fields and methods of `Config`):
  - `pub l1_rpc_url: Vec<Url>` (resolved: flag, else the preset's `l1_rpc_urls`, else empty)
  - `pub l1_send_rpc_url: Vec<Url>`, `pub l1_privkey: Option<SecretString>`, `pub l1_privkey_file: Option<PathBuf>`, `pub l1_max_post_fee: u128` (wei)
  - `pub fn l1_mode(&self) -> bool`
  - `pub fn resolve_l1_privkey(&mut self, env: Option<SecretString>) -> Result<(), ConfigError>`
  - `pub fn check_l1(&self, l1_chain_id: u64, l1_block: Address) -> Result<(), ConfigError>`
  - `pub fn parse_wei(s: &str) -> Result<u128, ConfigError>`
  - Env vars: `DAVINCI_L1_RPC_URL`, `DAVINCI_L1_SEND_RPC_URL`, `DAVINCI_L1_PRIVKEY_FILE`, `DAVINCI_L1_PRIVKEY`, `DAVINCI_L1_MAX_POST_FEE`

- [ ] **Step 1: Write the failing tests**

Append to `mod tests` in `sequencer/src/config.rs`:

```rust
    #[test]
    fn l1_options() {
        let c = Config::parse_args(with_rpc()).unwrap();
        assert!(!c.l1_mode());
        assert!(c.l1_send_rpc_url.is_empty() && c.l1_privkey.is_none());
        assert_eq!(c.l1_max_post_fee, 1_000_000_000_000_000);
        let mut args = with_rpc();
        args.extend([
            "--l1-rpc-url",
            "http://l1a:8545,https://l1b/l1secretpath",
            "--l1-send-rpc-url",
            "https://send.example/sendsecret",
            "--l1-max-post-fee",
            "5gwei",
        ]);
        let c = Config::parse_args(args).unwrap();
        assert!(c.l1_mode());
        assert_eq!(c.l1_rpc_url.len(), 2);
        assert_eq!(c.l1_send_rpc_url.len(), 1);
        assert_eq!(c.l1_max_post_fee, 5_000_000_000);
        let d = format!("{c:?}");
        for secret in ["l1secretpath", "sendsecret"] {
            assert!(!d.contains(secret), "{secret} in {d}");
        }
        assert!(d.contains("l1a:8545") && d.contains("send.example"), "{d}");
        // A send endpoint or an L1 key without the L1 RPCs is a mistake.
        for extra in [
            ["--l1-send-rpc-url", "http://s:8545"],
            ["--l1-privkey-file", "/nonexistent"],
        ] {
            let mut args = with_rpc();
            args.extend(extra);
            assert!(Config::parse_args(args).is_err(), "{extra:?}");
        }
    }

    #[test]
    fn l1_key_only_from_env_or_file() {
        let mut args = with_rpc();
        args.extend(["--l1-privkey", "beef"]);
        assert!(Config::try_parse_from(args).is_err());
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("l1key");
        std::fs::write(&f, "  beef\n").unwrap();
        let fs = f.display().to_string();
        let mut args = with_rpc();
        args.extend(["--l1-rpc-url", "http://l1:8545", "--l1-privkey-file", &fs]);
        let mut c = Config::parse_args(args).unwrap();
        c.resolve_l1_privkey(None).unwrap();
        assert_eq!(c.l1_privkey.as_ref().map(|s| s.expose()), Some("beef"));
        assert!(!format!("{c:?}").contains("beef"));
        assert!(c.resolve_l1_privkey(Some(SecretString::new("x"))).is_err());
        // The OP key and the L1 key are read independently.
        assert!(c.privkey.is_none());
    }

    #[test]
    fn wei_amounts() {
        assert_eq!(parse_wei("123").unwrap(), 123);
        assert_eq!(parse_wei("5gwei").unwrap(), 5_000_000_000);
        assert_eq!(parse_wei("0.5 gwei").unwrap(), 500_000_000);
        assert_eq!(parse_wei(".5gwei").unwrap(), 500_000_000);
        assert_eq!(parse_wei("0.01eth").unwrap(), 10_000_000_000_000_000);
        assert_eq!(parse_wei("1ETH").unwrap(), 10u128.pow(18));
        for bad in [
            "",
            "eth",
            "1.5",
            "0.0000000001gwei",
            "-1",
            "1e9",
            "1.2.3gwei",
        ] {
            assert!(parse_wei(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn optimism_preset_is_l1_mode_and_needs_its_registry() {
        let o = &networks::OPTIMISM;
        let plain = Config::parse_args(["davinci-sequencer", "--network", "optimism"]);
        if o.registry == Address::ZERO {
            let e = plain.unwrap_err().to_string();
            assert!(e.contains("--registry") && e.contains("optimism"), "{e}");
        } else {
            assert_eq!(plain.unwrap().start_block, Some(o.start_block));
        }
        let c = Config::parse_args(["davinci-sequencer", "--network", "optimism", "--registry", REG])
            .unwrap();
        assert!(c.l1_mode());
        let NetworkDa::L1Blobs { l1_rpc_urls, .. } = o.da else {
            panic!("optimism is an L1-blob preset");
        };
        assert_eq!(c.l1_rpc_url.len(), l1_rpc_urls.len());
        assert_eq!(c.confirmations, 1);
        assert_eq!(c.blob_source, o.blob_source.parse().unwrap());
        assert_eq!(c.start_block, None);
        // Gnosis stays in blob mode.
        assert!(!Config::parse_args(["davinci-sequencer"]).unwrap().l1_mode());
        // The zero address is refused as an explicit registry too.
        let mut args = base();
        args[4] = "0x0000000000000000000000000000000000000000";
        args.extend(["--rpc-url", "http://a:8545"]);
        assert!(Config::parse_args(args).is_err());
    }

    #[test]
    fn l1_side_must_match_the_preset() {
        let mut c = Config::parse_args([
            "davinci-sequencer",
            "--network",
            "optimism",
            "--registry",
            REG,
        ])
        .unwrap();
        // An explicit registry: mismatches only warn.
        assert!(c.check_l1(11_155_111, Address::repeat_byte(1)).is_ok());
        // The preset's own registry: they are fatal.
        c.registry_arg = None;
        assert!(c.check_l1(1, networks::OP_L1_BLOCK).is_ok());
        let e = c.check_l1(11_155_111, networks::OP_L1_BLOCK).unwrap_err();
        assert!(e.to_string().contains("11155111"), "{e}");
        assert!(c.check_l1(1, Address::repeat_byte(1)).is_err());
        // Custom deployments check nothing.
        let c = Config::parse_args(with_rpc()).unwrap();
        assert!(c.check_l1(5, Address::ZERO).is_ok());
    }
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -p davinci-sequencer --lib config 2>&1 | tail -5`
Expected: FAIL to compile (`l1_mode`, `parse_wei`, `check_l1`, the `l1_*` fields not found).

- [ ] **Step 3: Implement**

Change the client import line to `use davinci_client::networks::{self, Network, NetworkDa};`. Add after `parse_blob_cap`:

```rust
/// A wei amount: `<n>` (wei), `<n>gwei` or `<n>eth`, decimals allowed with a
/// unit (`0.01eth`).
pub fn parse_wei(s: &str) -> Result<u128, ConfigError> {
    let bad = || ConfigError::Invalid("wei amount", s.to_string());
    let t = s.trim().to_ascii_lowercase();
    let (num, decimals) = if let Some(n) = t.strip_suffix("gwei") {
        (n.trim(), 9u32)
    } else if let Some(n) = t.strip_suffix("eth") {
        (n.trim(), 18)
    } else {
        (t.as_str(), 0)
    };
    let (int, frac) = num.split_once('.').unwrap_or((num, ""));
    let digits = |x: &str| x.chars().all(|c| c.is_ascii_digit());
    if (int.is_empty() && frac.is_empty())
        || frac.len() > decimals as usize
        || !digits(int)
        || !digits(frac)
    {
        return Err(bad());
    }
    let int: u128 = if int.is_empty() {
        0
    } else {
        int.parse().map_err(|_| bad())?
    };
    let frac: u128 = if frac.is_empty() {
        0
    } else {
        format!("{frac:0<width$}", width = decimals as usize)
            .parse()
            .map_err(|_| bad())?
    };
    int.checked_mul(10u128.pow(decimals))
        .and_then(|v| v.checked_add(frac))
        .ok_or_else(bad)
}
```

Add to `struct Config`, after the `blob_source` fields:

```rust
    /// L1-blob mode: Ethereum execution JSON-RPCs, comma-separated, in order
    /// of preference. They post the blob transactions and serve the L1 proofs.
    /// Set exactly when the registry has an `L1BlobVerifier`. Default: the
    /// network's (none in blob mode).
    #[arg(long = "l1-rpc-url", env = "DAVINCI_L1_RPC_URL", value_name = "URL",
        value_parser = parse_http_url, value_delimiter = ',')]
    l1_rpc_url_arg: Vec<Url>,
    /// Resolved L1 RPC endpoints; empty in blob mode.
    #[arg(skip)]
    pub l1_rpc_url: Vec<Url>,
    /// Endpoints that only receive the blob transactions (about 1.5 MB per
    /// 6-blob send, which many public RPCs refuse). Default: `--l1-rpc-url`.
    #[arg(long = "l1-send-rpc-url", env = "DAVINCI_L1_SEND_RPC_URL", value_name = "URL",
        value_parser = parse_http_url, value_delimiter = ',')]
    pub l1_send_rpc_url: Vec<Url>,
    /// The dedicated Ethereum key that posts blobs, from `DAVINCI_L1_PRIVKEY`
    /// or `--l1-privkey-file`. Nothing else may send from it: the blob pool
    /// forbids nonce gaps.
    #[arg(skip)]
    pub l1_privkey: Option<SecretString>,
    /// File holding the hex Ethereum blob key.
    #[arg(long, env = "DAVINCI_L1_PRIVKEY_FILE")]
    pub l1_privkey_file: Option<PathBuf>,
    /// Most one blob post may cost at the fees it offers (`<wei>`, `<n>gwei`
    /// or `<n>eth`); above it the node waits for cheaper fees.
    #[arg(long, env = "DAVINCI_L1_MAX_POST_FEE", default_value = "0.001eth", value_parser = parse_wei)]
    pub l1_max_post_fee: u128,
```

In `impl fmt::Debug for Config`, after `.field("blob_source", &self.blob_source)`:

```rust
            .field("l1_rpc_url", &Hosts(&self.l1_rpc_url))
            .field("l1_send_rpc_url", &Hosts(&self.l1_send_rpc_url))
            .field("l1_privkey", &self.l1_privkey)
            .field("l1_privkey_file", &self.l1_privkey_file)
            .field("l1_max_post_fee", &self.l1_max_post_fee)
```

In `Config::load`, after the `resolve_privkey` line:

```rust
        c.l1_privkey_file = c.l1_privkey_file.as_deref().map(expand_home);
        c.resolve_l1_privkey(std::env::var("DAVINCI_L1_PRIVKEY").ok().map(SecretString::new))?;
```

In `resolve_network`, after the `self.registry = match ... ;` statement:

```rust
        if self.registry == Address::ZERO {
            let why = match (self.registry_arg, preset) {
                (None, Some(n)) => format!(
                    "network {} has no registry yet (deployment pending); pass --registry",
                    n.name
                ),
                _ => "the zero address is not a registry".into(),
            };
            return Err(ConfigError::Invalid("registry", why));
        }
```

and after the `self.rpc_url = match ... ;` statement:

```rust
        let preset_l1 = preset.and_then(|n| match n.da {
            NetworkDa::L1Blobs { l1_rpc_urls, .. } => Some(l1_rpc_urls),
            NetworkDa::Blobs => None,
        });
        self.l1_rpc_url = match (self.l1_rpc_url_arg.is_empty(), preset_l1) {
            (false, _) => self.l1_rpc_url_arg.clone(),
            (true, Some(urls)) => urls
                .iter()
                .map(|u| parse_http_url(u))
                .collect::<Result<_, _>>()?,
            (true, None) => Vec::new(),
        };
        if self.l1_rpc_url.is_empty()
            && (!self.l1_send_rpc_url.is_empty() || self.l1_privkey_file.is_some())
        {
            return Err(ConfigError::Invalid(
                "l1",
                "--l1-send-rpc-url and --l1-privkey-file need --l1-rpc-url".into(),
            ));
        }
```

Replace `resolve_privkey` with a shared reader and two callers:

```rust
    /// Sets `privkey` from the environment value or `privkey_file`; both
    /// set is an error.
    pub fn resolve_privkey(&mut self, env: Option<SecretString>) -> Result<(), ConfigError> {
        self.privkey = read_key(env, self.privkey_file.as_deref(), "privkey", "DAVINCI_PRIVKEY or --privkey-file")?;
        Ok(())
    }

    /// Sets `l1_privkey` from the environment value or `l1_privkey_file`;
    /// both set is an error.
    pub fn resolve_l1_privkey(&mut self, env: Option<SecretString>) -> Result<(), ConfigError> {
        self.l1_privkey = read_key(
            env,
            self.l1_privkey_file.as_deref(),
            "l1 privkey",
            "DAVINCI_L1_PRIVKEY or --l1-privkey-file",
        )?;
        Ok(())
    }

    /// Whether the node runs in L1-blob mode (an L1 RPC is configured).
    pub fn l1_mode(&self) -> bool {
        !self.l1_rpc_url.is_empty()
    }

    /// L1-blob mode: checks the Ethereum chain id and the verifier's
    /// `L1Block` against the network's. A mismatch is fatal unless the
    /// registry was given explicitly, then a warning.
    pub fn check_l1(&self, l1_chain_id: u64, l1_block: Address) -> Result<(), ConfigError> {
        let Some((n, want_id, want_block)) = self.network.preset().and_then(|n| match n.da {
            NetworkDa::L1Blobs {
                l1_chain_id,
                l1_block,
                ..
            } => Some((n, l1_chain_id, l1_block)),
            NetworkDa::Blobs => None,
        }) else {
            return Ok(());
        };
        let problems: Vec<String> = [
            (l1_chain_id != want_id).then(|| {
                format!(
                    "the L1 RPC is on chain {l1_chain_id}, network {} posts to chain {want_id}",
                    n.name
                )
            }),
            (l1_block != want_block).then(|| {
                format!(
                    "the registry's verifier reads L1Block at {l1_block}, network {} uses {want_block}",
                    n.name
                )
            }),
        ]
        .into_iter()
        .flatten()
        .collect();
        if problems.is_empty() {
            return Ok(());
        }
        if self.registry_explicit() {
            for p in &problems {
                tracing::warn!(network = n.name, "{p}; following the explicit --registry");
            }
            return Ok(());
        }
        Err(ConfigError::Invalid("l1", problems.join("; ")))
    }
```

and add after `expand_home`:

```rust
// A key from the environment or from `file`, never both; blank is none.
fn read_key(
    env: Option<SecretString>,
    file: Option<&std::path::Path>,
    what: &'static str,
    sources: &str,
) -> Result<Option<SecretString>, ConfigError> {
    let from_file = match file {
        Some(p) => {
            let mut raw = std::fs::read_to_string(p)
                .map_err(|e| ConfigError::Invalid(what, format!("{}: {e}", p.display())))?;
            let key = SecretString::new(raw.trim());
            raw.zeroize();
            Some(key)
        }
        None => None,
    };
    match (env, from_file) {
        (Some(_), Some(_)) => Err(ConfigError::Invalid(
            what,
            format!("set {sources}, not both"),
        )),
        (k, None) | (None, k) => Ok(k.filter(|k| !k.expose().trim().is_empty())),
    }
}
```

The existing test `key_only_from_env_or_file` asserts `resolve_privkey`'s error message only through `is_err()`, so the shared message (`set DAVINCI_PRIVKEY or --privkey-file, not both`) keeps it green.

- [ ] **Step 4: Run the config tests**

Run: `cargo test -p davinci-sequencer --lib config 2>&1 | tail -5`
Expected: PASS (all existing config tests plus the five new ones).

- [ ] **Step 5: Commit**

```bash
cargo fmt --all && cargo clippy -p davinci-sequencer --all-targets -- -D warnings
git add sequencer/src/config.rs
git commit -m "feat(config): l1 rpc, send endpoint, key and post fee cap for l1-blob mode"
```

---
### Task 3: Registry side on OP: the mode, the origin, `record()` and `submitStateTransitionL1`

**Files:**
- Create: `sequencer/abi/L1BlobVerifier.json`
- Create: `sequencer/src/web3/l1op.rs`
- Modify: `sequencer/src/web3/mod.rs` (module list L4-9, re-exports L18-26, `sol!` blocks L73-105)
- Modify: `sequencer/src/web3/contracts.rs` (ABI consts L35-41, `Contracts` L85-105, `error_names` L129-146, `known_chain_cell_proofs` L208-213, `connect`/`new` L484-565, tests)
- Modify: `sequencer/src/web3/tx.rs` (helper visibility L59, L76, L206, L214, L223, L728; test handle L941-959)
- Modify: `sequencer/src/actor.rs` (imports L35-38, `Chain` trait L61-105, `impl Chain for Contracts` L191-250)

**Interfaces:**
- Consumes: Task 1 `davinci_client::organizer::RegistryDa`; Task 2 `Config::l1_mode`; Plan 1 `web3::l1proof::{L1BlobProof, L1TxProof}`.
- Produces:
  - `pub struct L1Origin { pub number: u64, pub hash: B256 }` (`Clone, Copy, Debug, PartialEq, Eq`) in `crate::web3`
  - `pub use davinci_client::organizer::RegistryDa;` in `crate::web3`
  - `pub(crate) mod l1v` with the `L1BlobVerifier` bindings
  - `pub fn revert_message(data: &[u8]) -> Option<String>` in `crate::web3`
  - `Contracts::open(rpcs: &[Url], registry: Address, privkey: Option<&SecretString>, blobs: bool) -> Result<Contracts>`
  - `Contracts::{da_mode() -> Result<RegistryDa>, l1_origin() -> Result<L1Origin>, record_l1_origin() -> Result<L1Origin>, simulate_transition_l1(pid, snark, blobs, proof) -> Result<(), RevertReason>, submit_transition_l1(pid, snark, blobs, proof) -> Result<TxReceipt>}` with `pid: &[u8; 31], snark: &PlonkSnark, blobs: &TransitionBlobs, proof: &L1BlobProof`
  - `pub(super) async fn blob_support(rpcs: &[Url], provider: &DynProvider, chain_id: u64) -> Result<(Option<usize>, bool)>` in `web3::contracts`
  - `Chain` trait gains `l1_origin`, `record_l1_origin`, `simulate_transition_l1`, `submit_transition_l1` (same signatures, `Result<_, Web3Error>`), each with a default that fails with `Web3Error::Config` (or `RevertReason::Rpc`), so other `Chain` implementations compile unchanged.
  - `#[cfg(test)] Contracts::at(url: Url) -> Contracts` (read-only handle without boot checks).

- [ ] **Step 1: Check the precondition and vendor the verifier ABI**

```bash
cd /home/p4u/davinci-contracts && git log --oneline -1 && ~/.foundry/bin/forge build 2>&1 | tail -2
jq -c '[.abi[] | select(.type=="function" or .type=="event") | .name]' out/L1BlobVerifier.sol/L1BlobVerifier.json
jq -c '.abi[] | select(.name=="verify") | [.outputs[].name]' out/L1BlobVerifier.sol/L1BlobVerifier.json
grep -n "pub fn verify_local" /home/p4u/davinci-sequencer/sequencer/src/web3/l1proof.rs
```
Expected: the names include `MAX_AGE`, `l1Block`, `record`, `recorded`, `verify`, `OriginRecorded`; `verify`'s outputs are `["vhs","blobBlocks"]`; `verify_local(p: &L1BlobProof, anchor: B256)`. If any differs, stop: Plan 1's fix round has not landed.

```bash
cd /home/p4u/davinci-sequencer && jq .abi ../davinci-contracts/out/L1BlobVerifier.sol/L1BlobVerifier.json > sequencer/abi/L1BlobVerifier.json
```

- [ ] **Step 2: Write the failing unit tests**

Create `sequencer/src/web3/l1op.rs` with the module doc and the tests only:

```rust
//! L1-blob mode on the registry's chain (OP): the mode read, OP's L1 origin,
//! `record()` on the verifier, and `submitStateTransitionL1` with its
//! pre-flight. The registry bubbles the verifier's reverts up unchanged.

#[cfg(test)]
mod tests {
    use alloy::primitives::{Bytes, U256, keccak256};
    use alloy::sol_types::{SolCall, SolError};
    use davinci_zkvm_sdk::blob::TransitionBlobs;
    use davinci_zkvm_sdk::client::PlonkSnark;
    use serde_json::{Value, json};

    use super::*;
    use crate::web3::l1proof::{L1BlobProof, L1TxProof};

    // An RPC whose `eth_call` to the registry (0x01..01) answers `registry`
    // and to anything else returns 0x22..22 (the verifier's `l1Block()`).
    async fn stub(registry: Value) -> url::Url {
        let app = axum::Router::new().route(
            "/",
            axum::routing::post(move |axum::Json(req): axum::Json<Value>| {
                let mut answer = registry.clone();
                async move {
                    let id = req["id"].clone();
                    let to = req["params"][0]["to"]
                        .as_str()
                        .unwrap_or_default()
                        .to_ascii_lowercase();
                    let reg = format!("{:#x}", Address::repeat_byte(1));
                    let body = match req["method"].as_str() {
                        Some("eth_call") if to == reg => {
                            answer["jsonrpc"] = "2.0".into();
                            answer["id"] = id;
                            answer
                        }
                        Some("eth_call") => json!({"jsonrpc": "2.0", "id": id,
                            "result": format!("0x{}{}", "00".repeat(12), "22".repeat(20))}),
                        m => panic!("unexpected {m:?}"),
                    };
                    axum::Json(body)
                }
            }),
        );
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        format!("http://{addr}").parse().unwrap()
    }

    #[tokio::test]
    async fn the_mode_is_read_from_the_registry() {
        let verifier = format!("0x{}{}", "00".repeat(12), "11".repeat(20));
        let c = Contracts::at(stub(json!({ "result": verifier })).await);
        assert_eq!(
            c.da_mode().await.unwrap(),
            RegistryDa::L1Blobs {
                verifier: Address::repeat_byte(0x11),
                l1_block: Address::repeat_byte(0x22),
            }
        );
        let zero = format!("0x{}", "00".repeat(32));
        let c = Contracts::at(stub(json!({ "result": zero })).await);
        assert_eq!(c.da_mode().await.unwrap(), RegistryDa::Blobs);
        // A registry from before the mode: the call reverts without data.
        for err in [
            json!({"code": 3, "message": "execution reverted"}),
            json!({"code": 3, "message": "execution reverted", "data": "0x"}),
        ] {
            let c = Contracts::at(stub(json!({ "error": err })).await);
            assert_eq!(c.da_mode().await.unwrap(), RegistryDa::Blobs);
        }
        // Anything else is an error, never "blob mode".
        let err = json!({"code": -32000, "message": "header not found"});
        let c = Contracts::at(stub(json!({ "error": err })).await);
        assert!(c.da_mode().await.is_err());
    }

    #[test]
    fn the_l1_settlement_call_carries_the_proof() {
        let proof = L1BlobProof {
            headers: vec![Bytes::from(vec![0xc0])],
            txs: vec![L1TxProof {
                header_index: 0,
                key: Bytes::from(vec![0x80]),
                proof: vec![Bytes::from(vec![1, 2])],
            }],
        };
        let snark = PlonkSnark {
            program_vk: [0; 32],
            root_c_vadcop_final: [0; 32],
            public_values: vec![1; 512],
            proof_bytes: vec![2; 768],
        };
        let blobs = TransitionBlobs {
            blobs: vec![],
            commitments: vec![[3; 48]],
            zs: vec![[0; 32]],
            ys: vec![[4; 32]],
            proofs: vec![[5; 48]],
            versioned_hashes: vec![[1; 32]],
            digest: [0; 32],
        };
        let data = transition_call_l1(&[7; 31], &snark, &blobs, &proof);
        let call = PR::submitStateTransitionL1Call::abi_decode(&data).unwrap();
        assert_eq!(call.processId.0, [7; 31]);
        assert_eq!(call.ys[0].0, [4; 32]);
        assert_eq!(call.l1.headers, proof.headers);
        assert_eq!(call.l1.txs[0].headerIndex, U256::ZERO);
        assert_eq!(call.l1.txs[0].key, proof.txs[0].key);
        assert_eq!(call.l1.txs[0].proof, proof.txs[0].proof);
    }

    #[test]
    fn l1_verifier_errors_are_named() {
        let names = super::super::contracts::error_names().unwrap();
        let sel = |sig: &str| <[u8; 4]>::try_from(&keccak256(sig)[..4]).unwrap();
        for sig in [
            "L1OriginMismatch()",
            "TxTooOld(uint256)",
            "BrokenHeaderChain(uint256)",
            "BadHeader(uint256)",
            "TxAboveOrigin(uint256)",
            "NotBlobTx(uint256)",
            "BadVersionedHash(uint256)",
            "WrongDAMode()",
        ] {
            let name = sig.split('(').next().unwrap();
            assert_eq!(names.get(&sel(sig)).map(String::as_str), Some(name), "{sig}");
        }
        let msg = alloy::sol_types::Revert::from("MerkleTrie: invalid root hash").abi_encode();
        assert_eq!(
            revert_message(&msg).as_deref(),
            Some("MerkleTrie: invalid root hash")
        );
        assert_eq!(revert_message(&[1, 2, 3]), None);
    }
}
```

Register the module in `sequencer/src/web3/mod.rs` (next to the others): `mod l1op;` and `pub use l1op::revert_message;`.

- [ ] **Step 3: Run them to verify they fail**

Run: `cargo test -p davinci-sequencer --lib l1op 2>&1 | tail -5`
Expected: FAIL to compile (`Contracts::at`, `da_mode`, `transition_call_l1`, `revert_message`, `RegistryDa` not found).

- [ ] **Step 4: Types and bindings in `web3/mod.rs`**

Add to the re-exports:

```rust
pub use davinci_client::organizer::RegistryDa;
```

After the `council` module add:

```rust
/// Own module: the verifier ABI declares its own `IL1BlobVerifier` structs.
pub(crate) mod l1v {
    alloy::sol!(
        #[allow(missing_docs)]
        #[derive(Debug)]
        L1BlobVerifier,
        "abi/L1BlobVerifier.json"
    );
}

/// OP's L1 origin: `L1Block.number()` and `hash()`, read at one OP block.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct L1Origin {
    pub number: u64,
    pub hash: B256,
}
```

- [ ] **Step 5: `Contracts`: the mode cell, the probe switch and the error names**

In `sequencer/src/web3/contracts.rs`:

Add after `COUNCIL_ABI_JSON`:

```rust
/// And `L1BlobVerifier`'s (`L1OriginMismatch`, `TxTooOld`, the RLP errors).
const L1_VERIFIER_ABI_JSON: &str = include_str!("../../abi/L1BlobVerifier.json");
```

Add `("l1 blob verifier", L1_VERIFIER_ABI_JSON),` to the list in `error_names` (after `("verifier", VERIFIER_ABI_JSON),`) and change the doc comment to "Custom error selectors of the registry, the ZisK verifier, the L1 blob verifier, the DKG and Council adapters, and the DKG and Council contracts."

Add the field at the end of `struct Contracts`:

```rust
    /// The registry's DA mode, an immutable: read once (`da_mode`).
    pub(super) da: Arc<tokio::sync::OnceCell<super::RegistryDa>>,
```

In `known_chain_cell_proofs`, replace the match body with:

```rust
    match chain_id {
        100 | 10200 => Some(true), // Gnosis, Chiado: Osaka
        1 | 11_155_111 => Some(true), // Ethereum, Sepolia: Osaka
        _ => None,
    }
```

Move the blob-support block of `Contracts::new` (from `// Endpoints lacking eth_config are common` through the `if !cell_proofs { ... }` info log) into a function after `eth_config_blob_cap`:

```rust
/// The chain's blob limit per transaction (`eth_config`, if an endpoint
/// serves it) and whether its blob transactions carry EIP-7594 cell proofs:
/// `eth_config`, else the P256VERIFY probe, else the known-chain table.
pub(super) async fn blob_support(
    rpcs: &[Url],
    provider: &DynProvider,
    chain_id: u64,
) -> Result<(Option<usize>, bool)> {
    // Endpoints lacking eth_config are common: ask each before the probe.
    let mut eth_config = Err(TransportErrorKind::custom_str("no RPC endpoint"));
    for u in rpcs {
        eth_config = rpc_provider(std::slice::from_ref(u))
            .raw_request("eth_config".into(), ())
            .await;
        if eth_config.is_ok() {
            break;
        }
    }
    let blob_cap = eth_config_blob_cap(&eth_config);
    let cell_proofs = match &eth_config {
        Ok(v) => eth_config_osaka(v),
        Err(e) => {
            tracing::warn!(error = %e, "eth_config unavailable: probing P256VERIFY for Osaka");
            match (probe_osaka(provider).await, known_chain_cell_proofs(chain_id)) {
                (Ok(v), _) => v,
                (Err(e), Some(v)) => {
                    tracing::warn!(error = %e, chain_id, cell_proofs = v,
                        "P256VERIFY probe failed: sidecar version from the known-chain table");
                    v
                }
                (Err(e), None) => {
                    return Err(Web3Error::Config(format!(
                        "blob sidecar version unknown: no eth_config and the P256VERIFY probe failed ({e})"
                    )));
                }
            }
        }
    };
    if !cell_proofs {
        tracing::info!("pre-Osaka chain: blob transactions carry v0 sidecars");
    }
    Ok((blob_cap, cell_proofs))
}
```

Replace `connect` and the head of `new` with:

```rust
    /// Connects with the node configuration. In L1-blob mode this chain
    /// never carries blobs, so its blob support is not probed.
    pub async fn connect(cfg: &Config) -> Result<Self> {
        Self::open(&cfg.rpc_url, cfg.registry, cfg.privkey.as_ref(), !cfg.l1_mode()).await
    }

    /// Connects to `rpcs` (in order of preference, with sticky failover).
    /// Without `privkey` the handle is read-only. Fails if an endpoint is on
    /// another chain, none answers, or the registry has no code. The sidecar
    /// version is fixed here.
    pub async fn new(
        rpcs: &[Url],
        registry: Address,
        privkey: Option<&SecretString>,
    ) -> Result<Self> {
        Self::open(rpcs, registry, privkey, true).await
    }

    /// [`Contracts::new`]; `blobs` false skips the blob-support probe (the
    /// sidecar version and the blob cap stay unknown).
    pub async fn open(
        rpcs: &[Url],
        registry: Address,
        privkey: Option<&SecretString>,
        blobs: bool,
    ) -> Result<Self> {
```

and in the body of `open` (the former body of `new`) replace the moved block with:

```rust
        let (blob_cap, cell_proofs) = if blobs {
            blob_support(rpcs, &provider, chain_id).await?
        } else {
            (None, false)
        };
```

and add `da: Arc::default(),` to the returned struct literal.

Add after the `impl Contracts` block (before `#[cfg(test)] mod tests`):

```rust
#[cfg(test)]
impl Contracts {
    /// A read-only handle on `url` without the boot checks (unit tests).
    pub(super) fn at(url: Url) -> Contracts {
        let (provider, epoch) = super::failover_provider(&[url]);
        Contracts {
            provider,
            epoch,
            registry: Address::repeat_byte(1),
            signer: None,
            wallet: None,
            chain_id: 1,
            cell_proofs: Default::default(),
            blob_cap: None,
            errors: Arc::new(error_names().unwrap()),
            send_lock: Default::default(),
            receipt_timeout: Duration::from_secs(1),
            dkg_adapter: Default::default(),
            council_adapter: Default::default(),
            da: Default::default(),
        }
    }
}
```

In `sequencer/src/web3/tx.rs` `mod tests`, replace the body of `contracts_at` with `Contracts::at(url)` (keep the function and its comment). Make these items `pub(super)`: `fn check_shape`, `fn build_sidecar`, `fn gapped`, `fn maybe_sent`, `fn underpriced`, `async fn blob_base_fee`; `sidecar_version_error` is already `pub(crate)`.

- [ ] **Step 6: The OP-side calls**

Insert above the test module of `sequencer/src/web3/l1op.rs`:

```rust
use alloy::network::TransactionBuilder;
use alloy::primitives::{Address, Bytes, FixedBytes, U256};
use alloy::providers::Provider;
use alloy::rpc::types::TransactionRequest;
use alloy::sol_types::{SolCall, SolEvent};
use alloy::transports::{RpcError, TransportErrorKind};
use davinci_zkvm_sdk::blob::TransitionBlobs;
use davinci_zkvm_sdk::client::PlonkSnark;

use super::contracts::{Contracts, sat_u64};
use super::l1proof::L1BlobProof;
use super::l1v::L1BlobVerifier as LV;
use super::tx::check_shape;
use super::{
    IL1BlobVerifier as IL1, L1Origin, ProcessRegistry as PR, RegistryDa, Result, RevertReason,
    TxReceipt, Web3Error, rpc_err,
};

alloy::sol! {
    /// OP Stack's `L1Block` predeploy (a mock on test chains).
    interface IL1Block {
        function number() external view returns (uint64);
        function hash() external view returns (bytes32);
    }
}

/// The message of an `Error(string)` revert: the vendored `MerkleTrie`
/// reverts with strings.
pub fn revert_message(data: &[u8]) -> Option<String> {
    use alloy::sol_types::SolError;
    alloy::sol_types::Revert::abi_decode(data)
        .ok()
        .map(|r| r.reason)
}

fn proof_to_abi(p: &L1BlobProof) -> IL1::L1BlobProof {
    IL1::L1BlobProof {
        headers: p.headers.clone(),
        txs: p
            .txs
            .iter()
            .map(|t| IL1::L1TxProof {
                headerIndex: U256::from(t.header_index),
                key: t.key.clone(),
                proof: t.proof.clone(),
            })
            .collect(),
    }
}

/// `submitStateTransitionL1` calldata.
pub(super) fn transition_call_l1(
    pid: &[u8; 31],
    snark: &PlonkSnark,
    blobs: &TransitionBlobs,
    proof: &L1BlobProof,
) -> Bytes {
    PR::submitStateTransitionL1Call {
        processId: FixedBytes(*pid),
        publicValues: snark.public_values.clone().into(),
        proofBytes: snark.proof_bytes.clone().into(),
        commitments: blobs
            .commitments
            .iter()
            .map(|c| Bytes::copy_from_slice(c))
            .collect(),
        ys: blobs.ys.iter().map(|y| FixedBytes(*y)).collect(),
        kzgProofs: blobs
            .proofs
            .iter()
            .map(|p| Bytes::copy_from_slice(p))
            .collect(),
        l1: proof_to_abi(proof),
    }
    .abi_encode()
    .into()
}

// What a registry from before the mode answers `l1BlobVerifier()` with: a
// revert without data. A transport failure or a revert with data is not that.
fn missing_function(e: &RpcError<TransportErrorKind>) -> bool {
    e.as_error_resp().is_some_and(|p| {
        p.message.to_ascii_lowercase().contains("revert")
            && p.as_revert_data().is_none_or(|d| d.is_empty())
    })
}

impl Contracts {
    /// The registry's DA mode: `l1BlobVerifier()` and, when set, the
    /// verifier's `l1Block()`. A registry without the function settles with
    /// blobs. Read once; [`Contracts::events`] pairs L1 events strictly from
    /// then on.
    pub async fn da_mode(&self) -> Result<RegistryDa> {
        self.da
            .get_or_try_init(|| async {
                let tx = TransactionRequest::default()
                    .with_to(self.registry)
                    .with_input(PR::l1BlobVerifierCall {}.abi_encode());
                let verifier = match self
                    .provider
                    .call(tx)
                    .block(alloy::eips::BlockId::latest())
                    .await
                {
                    Ok(out) => PR::l1BlobVerifierCall::abi_decode_returns(&out)
                        .map_err(|e| Web3Error::Data(format!("l1BlobVerifier(): {e}")))?,
                    Err(e) if missing_function(&e) => Address::ZERO,
                    Err(e) => return Err(rpc_err(format!("l1BlobVerifier(): {e}"))),
                };
                if verifier == Address::ZERO {
                    return Ok(RegistryDa::Blobs);
                }
                let l1_block = self.view(verifier, LV::l1BlockCall {}).await?;
                Ok::<_, Web3Error>(RegistryDa::L1Blobs { verifier, l1_block })
            })
            .await
            .copied()
    }

    async fn l1_parts(&self) -> Result<(Address, Address)> {
        match self.da_mode().await? {
            RegistryDa::L1Blobs { verifier, l1_block } => Ok((verifier, l1_block)),
            RegistryDa::Blobs => Err(Web3Error::Config(
                "the registry settles with blobs, not in L1-blob mode".into(),
            )),
        }
    }

    /// OP's L1 origin, `number()` and `hash()` read at the same OP block so
    /// they belong together.
    pub async fn l1_origin(&self) -> Result<L1Origin> {
        let (_, l1_block) = self.l1_parts().await?;
        let at = alloy::eips::BlockId::number(self.provider.get_block_number().await.map_err(rpc_err)?);
        let read = async |data: Vec<u8>| {
            self.provider
                .call(TransactionRequest::default().with_to(l1_block).with_input(data))
                .block(at)
                .await
                .map_err(rpc_err)
        };
        let n = read(IL1Block::numberCall {}.abi_encode()).await?;
        let h = read(IL1Block::hashCall {}.abi_encode()).await?;
        let bad = |e: alloy::sol_types::Error| Web3Error::Data(format!("L1Block: {e}"));
        Ok(L1Origin {
            number: IL1Block::numberCall::abi_decode_returns(&n).map_err(bad)?,
            hash: IL1Block::hashCall::abi_decode_returns(&h).map_err(bad)?,
        })
    }

    /// `record()` on the verifier: stores OP's current origin so later
    /// proofs may anchor at it. Returns the origin its event names.
    pub async fn record_l1_origin(&self) -> Result<L1Origin> {
        let (verifier, _) = self.l1_parts().await?;
        let tx = TransactionRequest::default()
            .with_to(verifier)
            .with_input(LV::recordCall {}.abi_encode());
        let (_, logs) = self.send(tx, None).await?;
        logs.iter()
            .filter(|l| l.address() == verifier)
            .find_map(|l| LV::OriginRecorded::decode_log(&l.inner).ok())
            .map(|e| L1Origin {
                number: sat_u64(e.number),
                hash: e.hash,
            })
            .ok_or_else(|| Web3Error::Data("no OriginRecorded log".into()))
    }

    /// `eth_call` of `submitStateTransitionL1`; the revert name on failure.
    pub async fn simulate_transition_l1(
        &self,
        pid: &[u8; 31],
        snark: &PlonkSnark,
        blobs: &TransitionBlobs,
        proof: &L1BlobProof,
    ) -> std::result::Result<(), RevertReason> {
        check_shape(blobs).map_err(|e| RevertReason::Rpc(e.to_string()))?;
        let mut tx = TransactionRequest::default()
            .with_to(self.registry)
            .with_input(transition_call_l1(pid, snark, blobs, proof));
        if let Some(from) = self.signer {
            tx = tx.with_from(from);
        }
        self.provider
            .call(tx)
            .block(alloy::eips::BlockId::latest())
            .await
            .map(|_| ())
            .map_err(|e| self.reason(&e))
    }

    /// `submitStateTransitionL1`: a plain transaction (no blobs on this
    /// chain), gas estimated plus the usual margin.
    pub async fn submit_transition_l1(
        &self,
        pid: &[u8; 31],
        snark: &PlonkSnark,
        blobs: &TransitionBlobs,
        proof: &L1BlobProof,
    ) -> Result<TxReceipt> {
        if self.signer.is_none() {
            return Err(Web3Error::NoSigner);
        }
        check_shape(blobs)?;
        let tx = TransactionRequest::default()
            .with_to(self.registry)
            .with_input(transition_call_l1(pid, snark, blobs, proof));
        Ok(self.send(tx, None).await?.0)
    }
}
```

`Contracts`'s fields (`provider`, `registry`, `signer`, `da`) and `view`, `send`, `reason` are `pub(super)`, so a sibling module of `web3` reaches them. If the compiler reports `use alloy::sol_types::SolError` as unused at the top of the test module, drop it there (the encode in the test needs it; the import inside `revert_message` is local).

- [ ] **Step 7: The `Chain` trait and its `Contracts` forwarders**

In `sequencer/src/actor.rs`, extend the `crate::web3` import with `L1Origin` and add `use crate::web3::l1proof::L1BlobProof;`. Add after `fn gate_backoff`:

```rust
fn not_l1() -> Web3Error {
    Web3Error::Config("the registry is not in L1-blob mode".into())
}
```

Append to `trait Chain` (after `finalize_results_from_dkg`):

```rust
    /// L1-blob mode: OP's L1 origin (`L1Block` read at one block).
    async fn l1_origin(&self) -> Result<L1Origin, Web3Error> {
        Err(not_l1())
    }
    /// L1-blob mode: `record()` on the verifier; the origin it stored.
    async fn record_l1_origin(&self) -> Result<L1Origin, Web3Error> {
        Err(not_l1())
    }
    /// L1-blob mode: the pre-flight `eth_call` of `submitStateTransitionL1`.
    async fn simulate_transition_l1(
        &self,
        _pid: &[u8; 31],
        _snark: &PlonkSnark,
        _blobs: &TransitionBlobs,
        _proof: &L1BlobProof,
    ) -> Result<(), RevertReason> {
        Err(RevertReason::Rpc(not_l1().to_string()))
    }
    /// L1-blob mode: `submitStateTransitionL1`.
    async fn submit_transition_l1(
        &self,
        _pid: &[u8; 31],
        _snark: &PlonkSnark,
        _blobs: &TransitionBlobs,
        _proof: &L1BlobProof,
    ) -> Result<TxReceipt, Web3Error> {
        Err(not_l1())
    }
```

Append to `impl Chain for crate::web3::Contracts`:

```rust
    async fn l1_origin(&self) -> Result<L1Origin, Web3Error> {
        crate::web3::Contracts::l1_origin(self).await
    }
    async fn record_l1_origin(&self) -> Result<L1Origin, Web3Error> {
        crate::web3::Contracts::record_l1_origin(self).await
    }
    async fn simulate_transition_l1(
        &self,
        pid: &[u8; 31],
        snark: &PlonkSnark,
        blobs: &TransitionBlobs,
        proof: &L1BlobProof,
    ) -> Result<(), RevertReason> {
        crate::web3::Contracts::simulate_transition_l1(self, pid, snark, blobs, proof).await
    }
    async fn submit_transition_l1(
        &self,
        pid: &[u8; 31],
        snark: &PlonkSnark,
        blobs: &TransitionBlobs,
        proof: &L1BlobProof,
    ) -> Result<TxReceipt, Web3Error> {
        crate::web3::Contracts::submit_transition_l1(self, pid, snark, blobs, proof).await
    }
```

- [ ] **Step 8: Run the tests**

Run: `cargo test -p davinci-sequencer --lib web3 2>&1 | tail -5`
Expected: PASS (the three new `l1op` tests and every existing `web3` unit test, including `contracts::tests::verifier_errors_are_named` and the `tx` tests that now use `Contracts::at`).
Run: `cargo build -p davinci-sequencer --tests 2>&1 | tail -3`
Expected: builds (the `Chain` defaults keep `FakeChain` compiling).

- [ ] **Step 9: Commit**

```bash
cargo fmt --all && cargo clippy -p davinci-sequencer --all-targets -- -D warnings
git add sequencer/abi/L1BlobVerifier.json sequencer/src/web3 sequencer/src/actor.rs
git commit -m "feat(web3): l1-blob mode reads, record and submitStateTransitionL1 on op"
```

---

### Task 4: `ProcessStateTransitionedL1`: decoding and pairing

**Files:**
- Modify: `sequencer/src/web3/mod.rs` (`EventKind::StateTransitioned` L296-304)
- Modify: `sequencer/src/web3/contracts.rs` (`event_from_log` L373-481, `events` L639-693, tests)
- Modify: `sequencer/tests/common/mod.rs` (two `StateTransitioned` literals: `settle_foreign` ~L834, `submit_transition` ~L1146)
- Modify: `sequencer/tests/actor.rs` (literal ~L1921), `sequencer/tests/web3_anvil.rs` (literal ~L484)

**Interfaces:**
- Consumes: Task 3 `Contracts.da`, `RegistryDa`.
- Produces:
  - `pub struct L1Blobs { pub blocks: Vec<u64>, pub vhs: Vec<B256> }` (`Clone, Debug, PartialEq, Eq`), one block per versioned hash
  - `EventKind::StateTransitioned { .., n_blobs: u64, l1: Option<L1Blobs> }`
  - `Contracts::events` returns L1-mode transitions with `l1: Some(..)`; in L1 mode a transition without its L1 event fails the page.

- [ ] **Step 1: Write the failing pairing test**

Append to `mod tests` in `sequencer/src/web3/contracts.rs`:

```rust
    fn transition(block: u64, tx: u8, log_index: u64, n_blobs: u64) -> RegistryEvent {
        RegistryEvent {
            block,
            tx_hash: B256::repeat_byte(tx),
            log_index,
            kind: EventKind::StateTransitioned {
                pid: PID,
                sender: Address::ZERO,
                old_root: [1; 32],
                new_root: [2; 32],
                voters: 1,
                overwrites: 0,
                n_blobs,
                l1: None,
            },
        }
    }

    fn l1_event(block: u64, tx: u8, log_index: u64, pid: [u8; 31], n: usize) -> L1Event {
        L1Event {
            block,
            tx_hash: B256::repeat_byte(tx),
            log_index,
            pid,
            blobs: super::super::L1Blobs {
                blocks: vec![77; n],
                vhs: (0..n).map(|i| B256::repeat_byte(i as u8 + 1)).collect(),
            },
        }
    }

    #[test]
    fn l1_events_pair_with_their_transition() {
        let mut out = vec![transition(5, 1, 0, 2)];
        pair_l1(&mut out, vec![l1_event(5, 1, 1, PID, 2)], true).unwrap();
        assert!(
            matches!(&out[0].kind, EventKind::StateTransitioned { l1: Some(b), .. }
                if b.blocks == [77, 77] && b.vhs.len() == 2)
        );
        // Another tx, another process, not right after it, another count: refused.
        for bad in [
            l1_event(5, 2, 1, PID, 2),
            l1_event(5, 1, 1, [9; 31], 2),
            l1_event(5, 1, 2, PID, 2),
            l1_event(5, 1, 1, PID, 3),
        ] {
            assert!(pair_l1(&mut vec![transition(5, 1, 0, 2)], vec![bad], false).is_err());
        }
        // Blocks and hashes of different lengths: refused.
        let mut odd = l1_event(5, 1, 1, PID, 2);
        odd.blobs.blocks.pop();
        assert!(pair_l1(&mut vec![transition(5, 1, 0, 2)], vec![odd], false).is_err());
        // L1-blob mode: a transition without its L1 event fails the page;
        // blob mode passes it through.
        assert!(pair_l1(&mut vec![transition(5, 1, 0, 2)], vec![], true).is_err());
        assert!(pair_l1(&mut vec![transition(5, 1, 0, 2)], vec![], false).is_ok());
    }
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test -p davinci-sequencer --lib l1_events_pair 2>&1 | tail -5`
Expected: FAIL to compile (`L1Event`, `pair_l1`, `L1Blobs`, field `l1` not found).

- [ ] **Step 3: Implement**

In `sequencer/src/web3/mod.rs`, add after `L1Origin`:

```rust
/// Where a transition's blobs are on Ethereum (`ProcessStateTransitionedL1`):
/// the block of each versioned hash, in blob order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct L1Blobs {
    pub blocks: Vec<u64>,
    pub vhs: Vec<B256>,
}
```

and extend the variant:

```rust
    /// `voters` and `overwrites` are the process totals after the transition.
    /// `l1` is set in L1-blob mode, from the paired `ProcessStateTransitionedL1`.
    StateTransitioned {
        pid: [u8; 31],
        sender: Address,
        old_root: [u8; 32],
        new_root: [u8; 32],
        voters: u64,
        overwrites: u64,
        n_blobs: u64,
        l1: Option<L1Blobs>,
    },
```

In `sequencer/src/web3/contracts.rs`, import `L1Blobs` and `RegistryDa` from `super` and `alloy::primitives::B256`; in `event_from_log`, add `l1: None,` to the `StateTransitioned` literal. Add after `event_from_log`:

```rust
/// A `ProcessStateTransitionedL1` log, merged into its transition by [`pair_l1`].
struct L1Event {
    block: u64,
    tx_hash: B256,
    log_index: u64,
    pid: [u8; 31],
    blobs: L1Blobs,
}

fn l1_event_from_log(log: &Log) -> Result<Option<L1Event>> {
    if log.removed {
        return Ok(None);
    }
    let (Some(block), Some(tx_hash), Some(log_index)) =
        (log.block_number, log.transaction_hash, log.log_index)
    else {
        return Err(Web3Error::Data("log without block, tx or index".into()));
    };
    let e = PR::ProcessStateTransitionedL1::decode_log(&log.inner)
        .map_err(|e| Web3Error::Data(format!("event decode: {e}")))?;
    let blocks = e
        .l1BlockNumbers
        .iter()
        .map(|b| exact_u64("l1BlockNumber", *b))
        .collect::<Result<Vec<_>>>()?;
    Ok(Some(L1Event {
        block,
        tx_hash,
        log_index,
        pid: e.processId.0,
        blobs: L1Blobs {
            blocks,
            vhs: e.versionedHashes.clone(),
        },
    }))
}

/// Merges each L1 event into the `ProcessStateTransitioned` logged right
/// before it, in the same transaction, for the same process and blob count.
/// With `strict` (L1-blob mode) every transition must have one.
fn pair_l1(out: &mut [RegistryEvent], l1s: Vec<L1Event>, strict: bool) -> Result<()> {
    for e in l1s {
        let prev = out.iter_mut().find(|r| {
            r.block == e.block && r.tx_hash == e.tx_hash && r.log_index + 1 == e.log_index
        });
        match prev {
            Some(RegistryEvent {
                kind:
                    EventKind::StateTransitioned {
                        pid, n_blobs, l1, ..
                    },
                ..
            }) if *pid == e.pid
                && l1.is_none()
                && *n_blobs == e.blobs.vhs.len() as u64
                && e.blobs.blocks.len() == e.blobs.vhs.len() =>
            {
                *l1 = Some(e.blobs);
            }
            _ => {
                return Err(Web3Error::Data(format!(
                    "ProcessStateTransitionedL1 in tx {} has no matching transition",
                    e.tx_hash
                )));
            }
        }
    }
    if strict
        && let Some(r) = out
            .iter()
            .find(|r| matches!(r.kind, EventKind::StateTransitioned { l1: None, .. }))
    {
        return Err(Web3Error::Data(format!(
            "transition in tx {} without its ProcessStateTransitionedL1",
            r.tx_hash
        )));
    }
    Ok(())
}
```

In `events`, add `PR::ProcessStateTransitionedL1::SIGNATURE_HASH,` to `sigs` (after `ProcessStateTransitioned`), declare `let mut l1s = Vec::new();` next to `let mut out = Vec::new();`, and replace the log loop body:

```rust
            for log in self.provider.get_logs(&filter).await.map_err(rpc_err)? {
                if log.address() != self.registry {
                    continue;
                }
                if log.topic0() == Some(&PR::ProcessStateTransitionedL1::SIGNATURE_HASH) {
                    l1s.extend(l1_event_from_log(&log)?);
                } else if let Some(ev) = event_from_log(&log)? {
                    out.push(ev);
                }
            }
```

and replace the final `out.sort_by_key(|e| (e.block, e.log_index)); Ok(out)` with:

```rust
        out.sort_by_key(|e| (e.block, e.log_index));
        let strict = matches!(self.da.get(), Some(RegistryDa::L1Blobs { .. }));
        pair_l1(&mut out, l1s, strict)?;
        Ok(out)
```

Add `l1: None,` to the four `StateTransitioned` literals in `sequencer/tests/common/mod.rs` (two), `sequencer/tests/actor.rs` (one) and `sequencer/tests/web3_anvil.rs` (one).

- [ ] **Step 4: Run the tests**

Run: `cargo test -p davinci-sequencer --lib contracts 2>&1 | tail -5`
Expected: PASS (`l1_events_pair_with_their_transition` and the existing contracts tests).
Run: `cargo test -p davinci-sequencer --test actor 2>&1 | tail -3`
Expected: PASS, the same count as before this task (blob mode unchanged).

- [ ] **Step 5: Commit**

```bash
cargo fmt --all && cargo clippy -p davinci-sequencer --all-targets -- -D warnings
git add sequencer/src/web3 sequencer/tests/common/mod.rs sequencer/tests/actor.rs sequencer/tests/web3_anvil.rs
git commit -m "feat(web3): pair l1 transition events with their settlement"
```

---
### Task 5: The Ethereum side: `L1Chain` and `L1Rpc`

**Files:**
- Create: `sequencer/src/web3/l1.rs`
- Modify: `sequencer/src/web3/mod.rs` (module list and re-exports)

**Interfaces:**
- Consumes: Task 3 `contracts::blob_support`, the `pub(super)` helpers of `tx.rs` (`build_sidecar`, `check_shape`, `maybe_sent`, `underpriced`, `gapped`, `blob_base_fee`), `sidecar_version_error`, `failover::known_tx`; Plan 1 `l1proof::build`.
- Produces (all re-exported from `crate::web3`):
  - `pub const BLOB_TX_GAS: u64 = 21_000; pub const GAS_PER_BLOB: u128 = 131_072; pub const MIN_TIP: u128 = 1_000_000;`
  - `pub struct L1Market { pub base_fee: u128, pub tip: u128, pub blob_base_fee: u128 }` (`Clone, Copy, Debug, PartialEq, Eq`)
  - `pub struct L1Fees { pub max_fee: u128, pub tip: u128, pub blob_fee: u128 }` (`Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize`) with `offer(&L1Market) -> L1Fees`, `doubled(&self) -> L1Fees`, `max_each(self, L1Fees) -> L1Fees`, `cost(&self, n_blobs: usize) -> u128`
  - `pub struct L1Receipt { pub block: u64, pub index: u64 }`
  - `pub struct SignedBlobTx { pub hash: B256, pub nonce: u64, pub fees: L1Fees, pub envelope: Option<Arc<TxEnvelope>> }` (redacted `Debug`)
  - `pub enum L1SendError { Underpriced(String), NonceTaken(String), Gap(String), Sidecar(String), Unanswered(String), Refused(String) }`
  - `#[async_trait] pub trait L1Chain: Send + Sync { fn sender(&self) -> Option<Address>; async fn head(&self) -> Result<u64>; async fn nonce(&self) -> Result<u64>; async fn fees(&self) -> Result<L1Market>; async fn sign_blob_tx(&self, nonce: u64, fees: &L1Fees, blobs: &TransitionBlobs) -> Result<SignedBlobTx>; async fn send_blob_tx(&self, tx: &SignedBlobTx) -> std::result::Result<(), L1SendError>; async fn receipt(&self, tx: B256) -> Result<Option<L1Receipt>>; async fn build_proof(&self, txs: &[B256], top: u64) -> Result<L1BlobProof>; }`
  - `pub struct L1Rpc` with `connect(rpcs: &[Url], send: &[Url], privkey: Option<&SecretString>) -> Result<L1Rpc>`, `chain_id()`, `cell_proofs()`, `chain_blob_cap()`, and `impl L1Chain`
  - `pub(crate) fn send_error(e: &RpcError<TransportErrorKind>) -> Option<L1SendError>`

- [ ] **Step 1: Write the failing tests**

Create `sequencer/src/web3/l1.rs` with the module doc and the tests:

```rust
//! Ethereum for the L1-blob mode: the [`L1Chain`] operations the poster and
//! the settle loop use, and [`L1Rpc`], which runs them on execution RPCs:
//! reads and proofs on `--l1-rpc-url` (sticky failover), blob sends on
//! `--l1-send-rpc-url` when set (many public RPCs refuse 1.5 MB bodies).

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;

    use alloy::rpc::json_rpc::ErrorPayload;
    use alloy::transports::HttpError;
    use davinci_zkvm_sdk::ballot::Ballot;
    use davinci_zkvm_sdk::blob::{TransitionData, build_blobs};
    use davinci_zkvm_sdk::crypto::field::Fr;
    use davinci_zkvm_sdk::limits::VOTE_ID_MIN;
    use serde_json::{Value, json};

    use super::*;

    #[test]
    fn send_errors_are_classified() {
        let resp = |m: &str| {
            RpcError::<TransportErrorKind>::ErrorResp(ErrorPayload {
                code: -32000,
                message: m.to_string().into(),
                data: None,
            })
        };
        let class = |m: &str| send_error(&resp(m)).map(|e| e.to_string());
        assert!(send_error(&resp("already known")).is_none());
        for (msg, want) in [
            ("replacement transaction underpriced", "underpriced"),
            ("max fee per blob gas less than block blob gas fee", "underpriced"),
            ("nonce too low: next nonce 5, tx nonce 4", "nonce taken"),
            ("nonce too high", "nonce gap"),
            ("unexpected blob sidecar version", "sidecar version refused"),
            ("insufficient funds for gas * price + value", "refused"),
        ] {
            let got = class(msg).unwrap();
            assert!(got.starts_with(want), "{msg}: {got}");
        }
        let http = |status| {
            RpcError::<TransportErrorKind>::Transport(TransportErrorKind::HttpError(HttpError {
                status,
                body: String::new(),
            }))
        };
        assert!(matches!(send_error(&http(503)), Some(L1SendError::Unanswered(_))));
        // A body too large for the endpoint is refused at the door, never "maybe sent".
        assert!(matches!(send_error(&http(413)), Some(L1SendError::Refused(_))));
    }

    #[test]
    fn fee_offers_and_replacements() {
        let m = L1Market {
            base_fee: 10,
            tip: 0,
            blob_base_fee: 3,
        };
        let f = L1Fees::offer(&m);
        assert_eq!(
            f,
            L1Fees {
                max_fee: 20 + MIN_TIP,
                tip: MIN_TIP,
                blob_fee: 6
            }
        );
        let m = L1Market {
            base_fee: 0,
            tip: 5_000_000,
            blob_base_fee: 0,
        };
        assert_eq!(
            L1Fees::offer(&m),
            L1Fees {
                max_fee: 5_000_000,
                tip: 5_000_000,
                blob_fee: 1
            }
        );
        let d = f.doubled();
        assert_eq!(
            d,
            L1Fees {
                max_fee: 2 * f.max_fee,
                tip: 2 * MIN_TIP,
                blob_fee: 12
            }
        );
        assert_eq!(f.max_each(L1Fees { max_fee: 1, tip: 9 * MIN_TIP, blob_fee: 1 }).tip, 9 * MIN_TIP);
        assert_eq!(f.cost(2), 21_000 * f.max_fee + 2 * 131_072 * 6);
        let huge = L1Fees {
            max_fee: u128::MAX,
            tip: 1,
            blob_fee: u128::MAX,
        };
        assert_eq!(huge.cost(6), u128::MAX);
        assert_eq!(huge.doubled().max_fee, u128::MAX);
    }

    // An Ethereum RPC stub on `chain`: Osaka `eth_config`, six blobs per
    // transaction; counts `eth_sendRawTransaction`.
    async fn stub(chain: u64) -> (Url, Arc<AtomicUsize>) {
        let sends = Arc::new(AtomicUsize::new(0));
        let n = sends.clone();
        let app = axum::Router::new().route(
            "/",
            axum::routing::post(move |axum::Json(req): axum::Json<Value>| {
                let n = n.clone();
                async move {
                    let id = req["id"].clone();
                    let result = match req["method"].as_str().unwrap_or_default() {
                        "eth_chainId" => json!(format!("{chain:#x}")),
                        "eth_config" => json!({"current": {"blobSchedule": {"max": 6},
                            "precompiles": {"P256VERIFY": "0x0000000000000000000000000000000000000100"}}}),
                        "eth_sendRawTransaction" => {
                            n.fetch_add(1, Ordering::SeqCst);
                            json!(B256::repeat_byte(5))
                        }
                        m => panic!("unexpected {m}"),
                    };
                    axum::Json(json!({"jsonrpc": "2.0", "id": id, "result": result}))
                }
            }),
        );
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        (format!("http://{addr}").parse().unwrap(), sends)
    }

    fn one_blob() -> TransitionBlobs {
        let t = TransitionData {
            vote_ids: vec![VOTE_ID_MIN + 1],
            updates: vec![],
            accumulator: Ballot::identity(),
            num_fields: 1,
        };
        build_blobs(&t, &Fr::from(7u64), &[2u8; 32]).unwrap()
    }

    #[tokio::test]
    async fn blob_sends_go_to_the_send_endpoints() {
        let (reads, read_sends) = stub(1).await;
        let (send, sends) = stub(1).await;
        let key = SecretString::new(hex::encode([0x11u8; 32]));
        let l1 = L1Rpc::connect(std::slice::from_ref(&reads), &[send], Some(&key))
            .await
            .unwrap();
        assert_eq!(
            (l1.chain_id(), l1.cell_proofs(), l1.chain_blob_cap()),
            (1, true, Some(6))
        );
        assert!(l1.sender().is_some());
        let fees = L1Fees {
            max_fee: 10,
            tip: 1,
            blob_fee: 2,
        };
        let tx = l1.sign_blob_tx(0, &fees, &one_blob()).await.unwrap();
        assert_eq!((tx.nonce, tx.fees), (0, fees));
        assert!(!format!("{tx:?}").contains("envelope"), "no sidecar in Debug");
        l1.send_blob_tx(&tx).await.unwrap();
        assert_eq!(
            (read_sends.load(Ordering::SeqCst), sends.load(Ordering::SeqCst)),
            (0, 1)
        );
        // A replacement re-signs over the cached sidecar: another hash.
        let again = l1.sign_blob_tx(0, &fees.doubled(), &one_blob()).await.unwrap();
        assert_ne!(again.hash, tx.hash);
        // Send endpoints on another chain are refused.
        let (other, _) = stub(5).await;
        assert!(L1Rpc::connect(&[reads], &[other], Some(&key)).await.is_err());
    }
}
```

Register it in `sequencer/src/web3/mod.rs`: `mod l1;` and

```rust
pub use l1::{
    BLOB_TX_GAS, GAS_PER_BLOB, L1Chain, L1Fees, L1Market, L1Receipt, L1Rpc, L1SendError, MIN_TIP,
    SignedBlobTx,
};
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -p davinci-sequencer --lib web3::l1:: 2>&1 | tail -5`
Expected: FAIL to compile (`L1Rpc`, `L1Fees`, `send_error`, ... not found).

- [ ] **Step 3: Implement**

Insert above the test module of `sequencer/src/web3/l1.rs`:

```rust
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use alloy::consensus::TxEnvelope;
use alloy::eips::BlockNumberOrTag;
use alloy::eips::eip7594::BlobTransactionSidecarVariant;
use alloy::network::{
    EthereumWallet, NetworkTransactionBuilder, TransactionBuilder, TransactionBuilder4844,
};
use alloy::primitives::{Address, B256, U256};
use alloy::providers::{DynProvider, Provider};
use alloy::rpc::types::TransactionRequest;
use alloy::signers::local::PrivateKeySigner;
use alloy::transports::{RpcError, TransportErrorKind};
use async_trait::async_trait;
use davinci_zkvm_sdk::blob::TransitionBlobs;
use serde::{Deserialize, Serialize};
use url::Url;

use super::failover::known_tx;
use super::l1proof::{L1BlobProof, L1ProofError};
use super::tx::{
    blob_base_fee, build_sidecar, check_shape, gapped, maybe_sent, sidecar_version_error,
    underpriced,
};
use super::{Result, Web3Error, rpc_err};
use crate::config::SecretString;

/// Gas of a blob transaction to an EOA without data.
pub const BLOB_TX_GAS: u64 = 21_000;
/// Blob gas of one blob.
pub const GAS_PER_BLOB: u128 = 131_072;
/// Floor of the priority fee: 0.001 gwei.
pub const MIN_TIP: u128 = 1_000_000;

/// What the L1 market asks now, in wei.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct L1Market {
    pub base_fee: u128,
    pub tip: u128,
    pub blob_base_fee: u128,
}

/// The three fees a blob transaction offers, in wei.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct L1Fees {
    pub max_fee: u128,
    pub tip: u128,
    pub blob_fee: u128,
}

impl L1Fees {
    /// A fresh offer: a tip of at least [`MIN_TIP`], twice the base fee plus
    /// the tip, twice the blob base fee.
    pub fn offer(m: &L1Market) -> Self {
        let tip = m.tip.max(MIN_TIP);
        L1Fees {
            max_fee: m.base_fee.saturating_mul(2).saturating_add(tip),
            tip,
            blob_fee: m.blob_base_fee.saturating_mul(2).max(1),
        }
    }

    /// The blob pool's replacement price: every fee doubled.
    pub fn doubled(&self) -> Self {
        let d = |v: u128| v.saturating_mul(2).max(1);
        L1Fees {
            max_fee: d(self.max_fee),
            tip: d(self.tip),
            blob_fee: d(self.blob_fee),
        }
    }

    /// Each fee at least `other`'s.
    pub fn max_each(self, other: L1Fees) -> Self {
        L1Fees {
            max_fee: self.max_fee.max(other.max_fee),
            tip: self.tip.max(other.tip),
            blob_fee: self.blob_fee.max(other.blob_fee),
        }
    }

    /// Worst-case cost in wei of a post of `n` blobs at these fees.
    pub fn cost(&self, n: usize) -> u128 {
        u128::from(BLOB_TX_GAS)
            .saturating_mul(self.max_fee)
            .saturating_add((n as u128).saturating_mul(GAS_PER_BLOB).saturating_mul(self.blob_fee))
    }
}

/// Where a transaction is mined.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct L1Receipt {
    pub block: u64,
    pub index: u64,
}

/// A signed blob transaction, ready to send.
#[derive(Clone)]
pub struct SignedBlobTx {
    pub hash: B256,
    pub nonce: u64,
    pub fees: L1Fees,
    /// The network form with its sidecar (about 130 KB per blob); test
    /// doubles leave it out.
    pub envelope: Option<Arc<TxEnvelope>>,
}

// The envelope carries every blob: never printed.
impl std::fmt::Debug for SignedBlobTx {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SignedBlobTx")
            .field("hash", &self.hash)
            .field("nonce", &self.nonce)
            .field("fees", &self.fees)
            .finish_non_exhaustive()
    }
}

/// Why a blob send did not go through.
#[derive(Debug, thiserror::Error)]
pub enum L1SendError {
    /// A transaction of ours holds this nonce and this one does not outbid
    /// it by 100 % on every fee.
    #[error("underpriced: {0}")]
    Underpriced(String),
    /// The nonce is mined.
    #[error("nonce taken: {0}")]
    NonceTaken(String),
    /// A lower nonce of the account is missing.
    #[error("nonce gap: {0}")]
    Gap(String),
    /// The node wants the other sidecar version; the next signature uses it.
    #[error("sidecar version refused: {0}")]
    Sidecar(String),
    /// No endpoint answered; the transaction may be in.
    #[error("unanswered: {0}")]
    Unanswered(String),
    /// Refused for good (funds, size, malformed).
    #[error("refused: {0}")]
    Refused(String),
}

/// The Ethereum operations of the L1-blob mode: [`L1Rpc`] in production, a
/// scripted chain in tests.
#[async_trait]
pub trait L1Chain: Send + Sync {
    /// The account that posts blobs; `None` without an Ethereum key.
    fn sender(&self) -> Option<Address>;
    async fn head(&self) -> Result<u64>;
    /// The sender's mined transaction count (`latest`).
    async fn nonce(&self) -> Result<u64>;
    async fn fees(&self) -> Result<L1Market>;
    /// Signs a blob transaction from the sender to itself carrying `blobs`.
    async fn sign_blob_tx(
        &self,
        nonce: u64,
        fees: &L1Fees,
        blobs: &TransitionBlobs,
    ) -> Result<SignedBlobTx>;
    /// Sends it; an endpoint that already holds it counts as accepted.
    async fn send_blob_tx(&self, tx: &SignedBlobTx) -> std::result::Result<(), L1SendError>;
    /// Where `tx` is mined; `None` while pending or unknown.
    async fn receipt(&self, tx: B256) -> Result<Option<L1Receipt>>;
    /// The L1 proof of `txs` with headers from `top` down to the oldest of
    /// their blocks.
    async fn build_proof(&self, txs: &[B256], top: u64) -> Result<L1BlobProof>;
}

/// Classifies a refused blob send; `None` when the endpoint already holds it.
pub(crate) fn send_error(e: &RpcError<TransportErrorKind>) -> Option<L1SendError> {
    let msg = e.to_string();
    if known_tx(&msg) {
        return None;
    }
    if maybe_sent(e) {
        return Some(L1SendError::Unanswered(msg));
    }
    let m = msg.to_ascii_lowercase();
    Some(if sidecar_version_error(&msg) {
        L1SendError::Sidecar(msg)
    } else if underpriced(&msg) || m.contains("blob gas fee") {
        L1SendError::Underpriced(msg)
    } else if m.contains("nonce too low") {
        L1SendError::NonceTaken(msg)
    } else if gapped(&msg) {
        L1SendError::Gap(msg)
    } else {
        L1SendError::Refused(msg)
    })
}

/// Ethereum over execution RPCs.
pub struct L1Rpc {
    provider: DynProvider,
    send: DynProvider,
    chain_id: u64,
    signer: Option<Address>,
    wallet: Option<EthereumWallet>,
    cell_proofs: AtomicBool,
    /// The sidecar version was switched once after a refusal.
    switched: AtomicBool,
    blob_cap: Option<usize>,
    /// The last sidecar built (one post at a time): its replacements reuse it.
    sidecar: tokio::sync::Mutex<Option<(B256, bool, Arc<BlobTransactionSidecarVariant>)>>,
}

impl std::fmt::Debug for L1Rpc {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("L1Rpc")
            .field("chain_id", &self.chain_id)
            .field("sender", &self.signer)
            .field("cell_proofs", &self.cell_proofs())
            .field("blob_cap", &self.blob_cap)
            .finish_non_exhaustive()
    }
}

impl L1Rpc {
    /// Connects to `rpcs` and, for blob sends, `send` (empty: `rpcs`). Fails
    /// if an endpoint set is on another chain or none answers; fixes the
    /// sidecar version and reads the blob cap of this chain.
    pub async fn connect(
        rpcs: &[Url],
        send: &[Url],
        privkey: Option<&SecretString>,
    ) -> Result<Self> {
        let (wallet, signer) = match privkey {
            Some(k) => {
                // The parse error is not echoed: it could quote the key.
                let s = PrivateKeySigner::from_str(k.expose().trim()).map_err(|_| {
                    Web3Error::Config("the L1 private key is not 32-byte hex".into())
                })?;
                let a = s.address();
                (Some(EthereumWallet::from(s)), Some(a))
            }
            None => (None, None),
        };
        let chain_id = super::rpc_chain_id(rpcs).await?;
        if !send.is_empty() {
            let id = super::rpc_chain_id(send).await?;
            if id != chain_id {
                return Err(Web3Error::Config(format!(
                    "the L1 send endpoints are on chain {id}, the L1 RPCs on chain {chain_id}"
                )));
            }
        }
        let provider = super::rpc_provider(rpcs);
        let send = if send.is_empty() {
            provider.clone()
        } else {
            super::rpc_provider(send)
        };
        let (blob_cap, cell_proofs) =
            super::contracts::blob_support(rpcs, &provider, chain_id).await?;
        Ok(L1Rpc {
            provider,
            send,
            chain_id,
            signer,
            wallet,
            cell_proofs: AtomicBool::new(cell_proofs),
            switched: AtomicBool::new(false),
            blob_cap,
            sidecar: Default::default(),
        })
    }

    pub fn chain_id(&self) -> u64 {
        self.chain_id
    }

    /// Whether blob transactions carry EIP-7594 cell proofs (v1 sidecars).
    pub fn cell_proofs(&self) -> bool {
        self.cell_proofs.load(Ordering::Relaxed)
    }

    /// Blobs per transaction from `eth_config`, capped at six; `None` if no
    /// endpoint serves it.
    pub fn chain_blob_cap(&self) -> Option<usize> {
        self.blob_cap
    }

    async fn sidecar_for(&self, blobs: &TransitionBlobs) -> Result<Arc<BlobTransactionSidecarVariant>> {
        let v1 = self.cell_proofs();
        let key = blobs
            .versioned_hashes
            .first()
            .map(|h| B256::from(*h))
            .unwrap_or_default();
        let mut cache = self.sidecar.lock().await;
        if let Some((k, v, sc)) = cache.as_ref()
            && *k == key
            && *v == v1
        {
            return Ok(sc.clone());
        }
        // KZG cell proofs take a while: off the async workers.
        let owned = blobs.clone();
        let sc = tokio::task::spawn_blocking(move || build_sidecar(v1, &owned))
            .await
            .map_err(|e| Web3Error::Blob(format!("sidecar task: {e}")))??;
        let sc = Arc::new(sc);
        *cache = Some((key, v1, sc.clone()));
        Ok(sc)
    }
}

#[async_trait]
impl L1Chain for L1Rpc {
    fn sender(&self) -> Option<Address> {
        self.signer
    }

    async fn head(&self) -> Result<u64> {
        self.provider.get_block_number().await.map_err(rpc_err)
    }

    async fn nonce(&self) -> Result<u64> {
        let from = self.signer.ok_or(Web3Error::NoSigner)?;
        self.provider
            .get_transaction_count(from)
            .latest()
            .await
            .map_err(rpc_err)
    }

    async fn fees(&self) -> Result<L1Market> {
        let b = self
            .provider
            .get_block_by_number(BlockNumberOrTag::Latest)
            .await
            .map_err(rpc_err)?
            .ok_or_else(|| Web3Error::Data("no latest L1 block".into()))?;
        // Not every endpoint serves it; the floor covers that.
        let tip = self.provider.get_max_priority_fee_per_gas().await.unwrap_or(0);
        Ok(L1Market {
            base_fee: u128::from(b.header.base_fee_per_gas.unwrap_or(0)),
            tip,
            blob_base_fee: blob_base_fee(&self.provider).await?,
        })
    }

    async fn sign_blob_tx(
        &self,
        nonce: u64,
        fees: &L1Fees,
        blobs: &TransitionBlobs,
    ) -> Result<SignedBlobTx> {
        let (Some(from), Some(wallet)) = (self.signer, self.wallet.as_ref()) else {
            return Err(Web3Error::NoSigner);
        };
        check_shape(blobs)?;
        let sidecar = self.sidecar_for(blobs).await?;
        let mut tx = TransactionRequest::default()
            .with_from(from)
            .with_to(from)
            .with_chain_id(self.chain_id)
            .with_nonce(nonce)
            .with_gas_limit(BLOB_TX_GAS)
            .with_value(U256::ZERO)
            .with_max_fee_per_gas(fees.max_fee)
            .with_max_priority_fee_per_gas(fees.tip)
            .with_max_fee_per_blob_gas(fees.blob_fee);
        tx.blob_versioned_hashes = Some(
            blobs
                .versioned_hashes
                .iter()
                .map(|h| B256::from(*h))
                .collect(),
        );
        tx.set_blob_sidecar((*sidecar).clone());
        let env = tx
            .build(wallet)
            .await
            .map_err(|e| Web3Error::Rpc(format!("signing: {e}")))?;
        Ok(SignedBlobTx {
            hash: *env.tx_hash(),
            nonce,
            fees: *fees,
            envelope: Some(Arc::new(env)),
        })
    }

    async fn send_blob_tx(&self, tx: &SignedBlobTx) -> std::result::Result<(), L1SendError> {
        let env = tx
            .envelope
            .as_deref()
            .cloned()
            .ok_or_else(|| L1SendError::Refused("not signed by this client".into()))?;
        let Err(e) = self.send.send_tx_envelope(env).await else {
            return Ok(());
        };
        match send_error(&e) {
            None => Ok(()),
            Some(L1SendError::Sidecar(m)) => {
                // Once: the next signature uses the other version.
                if !self.switched.swap(true, Ordering::Relaxed) {
                    self.cell_proofs.fetch_xor(true, Ordering::Relaxed);
                }
                Err(L1SendError::Sidecar(m))
            }
            Some(e) => Err(e),
        }
    }

    async fn receipt(&self, tx: B256) -> Result<Option<L1Receipt>> {
        let r = self
            .provider
            .get_transaction_receipt(tx)
            .await
            .map_err(rpc_err)?;
        Ok(r.and_then(|r| {
            Some(L1Receipt {
                block: r.block_number?,
                index: r.transaction_index?,
            })
        }))
    }

    async fn build_proof(&self, txs: &[B256], top: u64) -> Result<L1BlobProof> {
        super::l1proof::build(&self.provider, txs, top)
            .await
            .map_err(|e| match e {
                L1ProofError::Rpc(m) => Web3Error::Rpc(m),
                e => Web3Error::Data(e.to_string()),
            })
    }
}
```

- [ ] **Step 4: Run the tests**

Run: `cargo test -p davinci-sequencer --lib web3::l1:: 2>&1 | tail -5`
Expected: PASS (`send_errors_are_classified`, `fee_offers_and_replacements`, `blob_sends_go_to_the_send_endpoints`).

- [ ] **Step 5: Commit**

```bash
cargo fmt --all && cargo clippy -p davinci-sequencer --all-targets -- -D warnings
git add sequencer/src/web3/l1.rs sequencer/src/web3/mod.rs
git commit -m "feat(web3): ethereum blob sender over l1 rpcs with a send-only endpoint"
```

---

### Task 6: Storage: the L1 post record (schema 9)

**Files:**
- Modify: `sequencer/src/storage.rs` (`SCHEMA_VERSION` L25, tables L32-55, records after L177, `init` L308-348, new methods after `clear_exposed` L477-483)
- Test: `sequencer/tests/storage.rs`

**Interfaces:**
- Consumes: Task 5 `L1Fees`.
- Produces:
  - `pub struct L1PostRecord { pub pid: [u8; 31], pub root_before: [u8; 32], pub nonce: u64, pub tx_hash: B256, pub fees: L1Fees, pub vhs: Vec<B256>, pub posted_at: u64 }` (`Clone, Debug, PartialEq, Eq, Serialize, Deserialize`; `pid` and `root_before` as hex)
  - `Db::put_l1_post(&self, r: &L1PostRecord) -> Result<()>` (key: nonce, so a replacement overwrites)
  - `Db::l1_posts(&self) -> Result<Vec<L1PostRecord>>` (by nonce)
  - `Db::remove_l1_posts_upto(&self, nonce: u64) -> Result<()>`
  - `SCHEMA_VERSION = 9`; a version-8 file is upgraded in place.

- [ ] **Step 1: Write the failing tests**

In `sequencer/tests/storage.rs`, add `L1PostRecord` to the `davinci_sequencer::storage` import and `use davinci_sequencer::web3::L1Fees;`, then append:

```rust
fn post(nonce: u64, fee: u128) -> L1PostRecord {
    L1PostRecord {
        pid: [3; 31],
        root_before: [4; 32],
        nonce,
        tx_hash: alloy::primitives::B256::repeat_byte(nonce as u8),
        fees: L1Fees {
            max_fee: fee,
            tip: 1_000_000,
            blob_fee: 2,
        },
        vhs: vec![alloy::primitives::B256::repeat_byte(1)],
        posted_at: 1_700_000_000,
    }
}

#[test]
fn l1_posts_round_trip_by_nonce() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    assert!(db.l1_posts().unwrap().is_empty());
    db.put_l1_post(&post(7, 10)).unwrap();
    db.put_l1_post(&post(5, 10)).unwrap();
    // A replacement at the same nonce overwrites its record.
    db.put_l1_post(&post(7, 20)).unwrap();
    let got = db.l1_posts().unwrap();
    assert_eq!(got, vec![post(5, 10), post(7, 20)]);
    // Fees above u64 survive the JSON encoding.
    db.put_l1_post(&post(9, u128::from(u64::MAX) * 4)).unwrap();
    drop(db);
    let db = open(&dir);
    assert_eq!(db.l1_posts().unwrap().len(), 3);
    db.remove_l1_posts_upto(7).unwrap();
    assert_eq!(db.l1_posts().unwrap(), vec![post(9, u128::from(u64::MAX) * 4)]);
}

#[test]
fn a_schema_8_file_upgrades_in_place() {
    let dir = tempfile::tempdir().unwrap();
    {
        let db = open(&dir);
        db.put_process(&process(3)).unwrap();
        db.set_meta_u64(SCHEMA_VERSION_KEY, 8).unwrap();
    }
    let db = open(&dir);
    assert_eq!(db.meta_u64(SCHEMA_VERSION_KEY).unwrap(), Some(9));
    assert_eq!(db.process(&pid(3)).unwrap(), Some(process(3)));
    db.put_l1_post(&post(1, 1)).unwrap();
    drop(db);
    // Older versions are still refused.
    {
        let db = open(&dir);
        db.set_meta_u64(SCHEMA_VERSION_KEY, 7).unwrap();
    }
    let err = Db::open(&dir.path().join("sequencer.redb")).err().unwrap();
    assert!(matches!(err, StorageError::Schema { found: 7, .. }), "{err}");
}
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -p davinci-sequencer --test storage l1_posts 2>&1 | tail -5`
Expected: FAIL to compile (`L1PostRecord`, `put_l1_post` not found).

- [ ] **Step 3: Implement**

In `sequencer/src/storage.rs`: set `pub const SCHEMA_VERSION: u64 = 9;` and add after the `EXPOSED` table:

```rust
// nonce (BE8) -> `L1PostRecord`: blob transactions of the node's Ethereum
// account not yet seen mined (L1-blob mode).
const L1_POSTS: Bytes = TableDefinition::new("l1_posts");
/// The last schema this build upgrades in place: 9 only added `l1_posts`.
const UPGRADES_FROM: u64 = 8;
```

Add after `ExposedRecord`:

```rust
/// A blob transaction the node signed on Ethereum (L1-blob mode), written
/// before it is sent and dropped once mined, so a restart replaces it at its
/// nonce instead of queueing behind it. Holds nothing secret.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct L1PostRecord {
    #[serde(with = "hex::serde")]
    pub pid: [u8; 31],
    #[serde(with = "hex::serde")]
    pub root_before: [u8; 32],
    pub nonce: u64,
    pub tx_hash: alloy::primitives::B256,
    pub fees: crate::web3::L1Fees,
    pub vhs: Vec<alloy::primitives::B256>,
    /// Unix seconds.
    pub posted_at: u64,
}
```

In `init`, add an arm before the refusing one and the table to the list:

```rust
                Some(v) if v == SCHEMA_VERSION => {}
                // Additive: the new table is opened below.
                Some(UPGRADES_FROM) => {
                    meta.insert(SCHEMA_VERSION_KEY, SCHEMA_VERSION)
                        .map_err(db_err)?;
                }
```

(`L1_POSTS,` goes after `EXPOSED,` in the `for t in [...]` list.) Add after `clear_exposed`:

```rust
    /// Records a blob transaction before it is sent; a replacement at the
    /// same nonce overwrites the record.
    pub fn put_l1_post(&self, r: &L1PostRecord) -> Result<()> {
        self.put(L1_POSTS, &r.nonce.to_be_bytes(), &encode(r)?)
    }

    /// Every recorded blob transaction, by nonce.
    pub fn l1_posts(&self) -> Result<Vec<L1PostRecord>> {
        self.scan(L1_POSTS, &[], 8)?
            .iter()
            .map(|(_, v)| decode("l1_posts", v))
            .collect()
    }

    /// Drops the records at or below `nonce`: mined.
    pub fn remove_l1_posts_upto(&self, nonce: u64) -> Result<()> {
        self.write(|tx| {
            let mut t = tx.open_table(L1_POSTS).map_err(db_err)?;
            let (lo, hi) = ([0u8; 8], nonce.to_be_bytes());
            let keys: Vec<Vec<u8>> = t
                .range(lo.as_slice()..=hi.as_slice())
                .map_err(db_err)?
                .map(|r| r.map(|(k, _)| k.value().to_vec()))
                .collect::<std::result::Result<_, _>>()
                .map_err(db_err)?;
            for k in keys {
                t.remove(k.as_slice()).map_err(db_err)?;
            }
            Ok(())
        })
    }
```

- [ ] **Step 4: Run the storage tests**

Run: `cargo test -p davinci-sequencer --test storage 2>&1 | tail -5`
Expected: PASS (both new tests and every existing one, `schema_mismatch_is_refused` included).

- [ ] **Step 5: Commit**

```bash
cargo fmt --all && cargo clippy -p davinci-sequencer --all-targets -- -D warnings
git add sequencer/src/storage.rs sequencer/tests/storage.rs
git commit -m "feat(storage): record l1 blob posts before sending them (schema 9)"
```

---
### Task 7: The blob poster and the scripted Ethereum

**Files:**
- Create: `sequencer/src/l1da/mod.rs`, `sequencer/src/l1da/post.rs`
- Modify: `sequencer/src/lib.rs` (add `pub mod l1da;`)
- Create: `sequencer/tests/common/l1.rs`
- Modify: `sequencer/tests/common/mod.rs` (module list L8, re-exports L19-30)
- Create: `sequencer/tests/l1da.rs`

**Interfaces:**
- Consumes: Task 5 `L1Chain`, `L1Fees`, `L1Market`, `L1SendError`, `SignedBlobTx`, `L1Receipt`; Task 6 `Db::{put_l1_post, l1_posts, remove_l1_posts_upto}`, `L1PostRecord`; Task 3 `L1Origin`; Plan 1 `l1proof::{header_rlp, tx_key, tx_trie_proof, L1BlobProof, L1TxProof}`.
- Produces:
  - `crate::l1da::{L1Da, Poster, Posted, PostError, REPLACE_AFTER}`
  - `pub struct L1Da { pub l1: Arc<dyn L1Chain>, pub poster: Poster, pub poll: Duration }`, `L1Da::new(l1: Arc<dyn L1Chain>, db: Db, fee_cap: u128, poll: Duration) -> Result<L1Da, StorageError>`
  - `Poster::new(l1: Arc<dyn L1Chain>, db: Db, fee_cap: u128, poll: Duration) -> Result<Poster, StorageError>`
  - `Poster::post(&self, pid: &[u8; 31], root_before: &[u8; 32], blobs: &TransitionBlobs, deadline: tokio::time::Instant) -> Result<Posted, PostError>`
  - `pub struct Posted { pub tx: B256, pub nonce: u64, pub block: u64 }` (`Clone, Copy, Debug, PartialEq, Eq`)
  - `pub enum PostError { NoSigner, FeeCap { want: u128, cap: u128 }, Deadline, L1(String), Storage(String) }`
  - Test double `FakeL1` (in `tests/common/l1.rs`, re-exported from `common`): `new()`, `with_sender(u8)`, `observer()`, `origin()`, `mine(n)`, `head_number()`, `set_lag(n)`, `pin_origin(Option<u64>)`, `hold(bool)`, `set_auto_mine(bool)`, `drop_next(n)`, `market()`, `set_market(L1Market)`, `reorg(depth)`, `sends() -> Vec<L1Send>`, `mined_posts() -> usize`, `block_of(&B256) -> Option<u64>`, `blob(&B256) -> Option<Blob>`, `inject_orphan(nonce, L1Fees) -> B256`, `post_foreign(&TransitionBlobs) -> u64`; `pub struct L1Send { pub from: Address, pub nonce: u64, pub fees: L1Fees, pub hash: B256, pub head: u64 }`. Its default sender is `Address::repeat_byte(0x1a)`.

- [ ] **Step 1: Write the scripted Ethereum**

Create `sequencer/tests/common/l1.rs`:

```rust
//! A scripted Ethereum for the L1-blob mode: real RLP headers chained by
//! parent hash and real type-3 envelopes in real transactions tries, so the
//! node's proofs pass `verify_local`. `head()` mines one block per call while
//! auto-mining is on (time passing); tests drive everything else.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

use alloy::consensus::{Header, SignableTransaction, TxEip4844, TxEip4844Variant, TxEnvelope};
use alloy::eips::eip2718::Encodable2718;
use alloy::primitives::{Address, B256, Bytes, Signature, U256};
use async_trait::async_trait;
use davinci_sequencer::web3::l1proof::{
    L1BlobProof, L1TxProof, header_rlp, tx_key, tx_trie_proof,
};
use davinci_sequencer::web3::{
    L1Chain, L1Fees, L1Market, L1Origin, L1Receipt, L1SendError, SignedBlobTx, Web3Error,
};
use davinci_zkvm_sdk::blob::{Blob, TransitionBlobs};

/// One send the chain saw, accepted or not.
#[derive(Clone, Debug)]
pub struct L1Send {
    pub from: Address,
    pub nonce: u64,
    pub fees: L1Fees,
    pub hash: B256,
    /// The head when it was sent.
    pub head: u64,
}

#[derive(Clone)]
struct FakeTx {
    hash: B256,
    from: Address,
    nonce: u64,
    fees: L1Fees,
    envelope: Bytes,
}

struct Block {
    header: Header,
    hash: B256,
    txs: Vec<FakeTx>,
}

struct L1Inner {
    blocks: Vec<Block>,
    signed: HashMap<B256, FakeTx>,
    pool: BTreeMap<(Address, u64), FakeTx>,
    blobs: HashMap<B256, Blob>,
    market: L1Market,
    /// OP's origin trails the head by this much, unless pinned.
    lag: u64,
    pinned: Option<u64>,
    /// Pool sends without mining them.
    hold: bool,
    /// `head()` mines a block per call.
    auto_mine: bool,
    /// The next N accepted sends are lost.
    drop_next: u32,
    sends: Vec<L1Send>,
    /// Sets the blocks of a reorg apart from the ones they replace.
    epoch: u8,
}

/// The scripted Ethereum; every clone shares it, each handle posts as its
/// own sender.
#[derive(Clone)]
pub struct FakeL1 {
    inner: Arc<Mutex<L1Inner>>,
    sender: Option<Address>,
}

impl Default for FakeL1 {
    fn default() -> Self {
        Self::new()
    }
}

// A type-3 envelope from `from` to itself; the signature is a placeholder,
// nothing here recovers it.
fn envelope(from: Address, nonce: u64, fees: &L1Fees, vhs: &[B256]) -> (B256, Bytes) {
    let tx = TxEip4844 {
        chain_id: 1,
        nonce,
        gas_limit: 21_000,
        max_fee_per_gas: fees.max_fee,
        max_priority_fee_per_gas: fees.tip,
        to: from,
        value: U256::ZERO,
        access_list: Default::default(),
        blob_versioned_hashes: vhs.to_vec(),
        max_fee_per_blob_gas: fees.blob_fee,
        input: Default::default(),
    };
    let env = TxEnvelope::Eip4844(
        TxEip4844Variant::TxEip4844(tx).into_signed(Signature::test_signature()),
    );
    (*env.tx_hash(), env.encoded_2718().into())
}

fn mine(i: &mut L1Inner) {
    let number = i.blocks.len() as u64;
    let parent_hash = i.blocks.last().map_or(B256::ZERO, |b| b.hash);
    let txs: Vec<FakeTx> = if i.hold {
        Vec::new()
    } else {
        std::mem::take(&mut i.pool).into_values().collect()
    };
    let envs: Vec<Bytes> = txs.iter().map(|t| t.envelope.clone()).collect();
    let header = Header {
        parent_hash,
        number,
        timestamp: 1_700_000_000 + 12 * number,
        transactions_root: alloy::trie::root::ordered_trie_root_with_encoder(&envs, |e, buf| {
            buf.extend_from_slice(e)
        }),
        extra_data: Bytes::from(vec![i.epoch]),
        ..Default::default()
    };
    let hash = header.hash_slow();
    i.blocks.push(Block { header, hash, txs });
}

fn head_of(i: &L1Inner) -> u64 {
    i.blocks.len() as u64 - 1
}

fn mined_count(i: &L1Inner, from: Address) -> u64 {
    i.blocks
        .iter()
        .flat_map(|b| &b.txs)
        .filter(|t| t.from == from)
        .count() as u64
}

fn origin_of(i: &L1Inner) -> L1Origin {
    let head = head_of(i);
    let n = i.pinned.unwrap_or(head.saturating_sub(i.lag)).min(head);
    L1Origin {
        number: n,
        hash: i.blocks[n as usize].hash,
    }
}

fn locate(i: &L1Inner, h: &B256) -> Option<(u64, usize)> {
    i.blocks.iter().enumerate().find_map(|(n, b)| {
        b.txs
            .iter()
            .position(|t| t.hash == *h)
            .map(|idx| (n as u64, idx))
    })
}

impl FakeL1 {
    /// Ten empty blocks, a 1 gwei base fee and a 1 wei blob base fee; OP's
    /// origin at the head.
    pub fn new() -> Self {
        let mut i = L1Inner {
            blocks: Vec::new(),
            signed: HashMap::new(),
            pool: BTreeMap::new(),
            blobs: HashMap::new(),
            market: L1Market {
                base_fee: 1_000_000_000,
                tip: 0,
                blob_base_fee: 1,
            },
            lag: 0,
            pinned: None,
            hold: false,
            auto_mine: true,
            drop_next: 0,
            sends: Vec::new(),
            epoch: 0,
        };
        for _ in 0..10 {
            mine(&mut i);
        }
        FakeL1 {
            inner: Arc::new(Mutex::new(i)),
            sender: Some(Address::repeat_byte(0x1a)),
        }
    }

    /// Another sender on the same chain.
    pub fn with_sender(&self, byte: u8) -> FakeL1 {
        FakeL1 {
            inner: self.inner.clone(),
            sender: Some(Address::repeat_byte(byte)),
        }
    }

    /// The same chain for a node without an Ethereum key.
    pub fn observer(&self) -> FakeL1 {
        FakeL1 {
            inner: self.inner.clone(),
            sender: None,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, L1Inner> {
        self.inner.lock().unwrap()
    }

    /// OP's view of Ethereum: the head minus the lag, or the pinned block.
    pub fn origin(&self) -> L1Origin {
        origin_of(&self.lock())
    }

    pub fn mine(&self, n: u64) {
        let mut i = self.lock();
        for _ in 0..n {
            mine(&mut i);
        }
    }

    pub fn head_number(&self) -> u64 {
        head_of(&self.lock())
    }

    pub fn set_lag(&self, lag: u64) {
        self.lock().lag = lag;
    }

    pub fn pin_origin(&self, n: Option<u64>) {
        self.lock().pinned = n;
    }

    pub fn hold(&self, on: bool) {
        self.lock().hold = on;
    }

    pub fn set_auto_mine(&self, on: bool) {
        self.lock().auto_mine = on;
    }

    pub fn drop_next(&self, n: u32) {
        self.lock().drop_next = n;
    }

    pub fn market(&self) -> L1Market {
        self.lock().market
    }

    pub fn set_market(&self, m: L1Market) {
        self.lock().market = m;
    }

    /// Drops the newest `depth` blocks with their transactions (they do not
    /// return to the pool) and mines as many empty ones.
    pub fn reorg(&self, depth: u64) {
        let mut i = self.lock();
        let keep = i.blocks.len().saturating_sub(depth as usize).max(1);
        i.blocks.truncate(keep);
        i.epoch = i.epoch.wrapping_add(1);
        let hold = std::mem::replace(&mut i.hold, true);
        for _ in 0..depth {
            mine(&mut i);
        }
        i.hold = hold;
    }

    pub fn sends(&self) -> Vec<L1Send> {
        self.lock().sends.clone()
    }

    /// Blob transactions mined, any sender.
    pub fn mined_posts(&self) -> usize {
        self.lock().blocks.iter().map(|b| b.txs.len()).sum()
    }

    pub fn block_of(&self, tx: &B256) -> Option<u64> {
        locate(&self.lock(), tx).map(|(n, _)| n)
    }

    pub fn blob(&self, vh: &B256) -> Option<Blob> {
        self.lock().blobs.get(vh).cloned()
    }

    /// A pending transaction of this sender at `nonce` that no record knows
    /// (a lost database).
    pub fn inject_orphan(&self, nonce: u64, fees: L1Fees) -> B256 {
        let from = self.sender.expect("a sender");
        let (hash, envelope) = envelope(from, nonce, &fees, &[B256::repeat_byte(1)]);
        self.lock().pool.insert(
            (from, nonce),
            FakeTx {
                hash,
                from,
                nonce,
                fees,
                envelope,
            },
        );
        hash
    }

    /// Another sequencer posts `blobs`, mined at once; returns the block.
    pub fn post_foreign(&self, blobs: &TransitionBlobs) -> u64 {
        let from = Address::repeat_byte(0xee);
        let vhs: Vec<B256> = blobs.versioned_hashes.iter().map(|h| B256::from(*h)).collect();
        let mut i = self.lock();
        let nonce = mined_count(&i, from);
        let fees = L1Fees {
            max_fee: 1,
            tip: 1,
            blob_fee: 1,
        };
        let (hash, envelope) = envelope(from, nonce, &fees, &vhs);
        for (vh, b) in vhs.iter().zip(&blobs.blobs) {
            i.blobs.insert(*vh, b.clone());
        }
        i.pool.insert(
            (from, nonce),
            FakeTx {
                hash,
                from,
                nonce,
                fees,
                envelope,
            },
        );
        let hold = std::mem::replace(&mut i.hold, false);
        mine(&mut i);
        i.hold = hold;
        head_of(&i)
    }
}

#[async_trait]
impl L1Chain for FakeL1 {
    fn sender(&self) -> Option<Address> {
        self.sender
    }

    async fn head(&self) -> Result<u64, Web3Error> {
        let mut i = self.lock();
        if i.auto_mine {
            mine(&mut i);
        }
        Ok(head_of(&i))
    }

    async fn nonce(&self) -> Result<u64, Web3Error> {
        let from = self.sender.ok_or(Web3Error::NoSigner)?;
        Ok(mined_count(&self.lock(), from))
    }

    async fn fees(&self) -> Result<L1Market, Web3Error> {
        Ok(self.lock().market)
    }

    async fn sign_blob_tx(
        &self,
        nonce: u64,
        fees: &L1Fees,
        blobs: &TransitionBlobs,
    ) -> Result<SignedBlobTx, Web3Error> {
        let from = self.sender.ok_or(Web3Error::NoSigner)?;
        let vhs: Vec<B256> = blobs.versioned_hashes.iter().map(|h| B256::from(*h)).collect();
        let (hash, envelope) = envelope(from, nonce, fees, &vhs);
        let mut i = self.lock();
        for (vh, b) in vhs.iter().zip(&blobs.blobs) {
            i.blobs.insert(*vh, b.clone());
        }
        i.signed.insert(
            hash,
            FakeTx {
                hash,
                from,
                nonce,
                fees: *fees,
                envelope,
            },
        );
        Ok(SignedBlobTx {
            hash,
            nonce,
            fees: *fees,
            envelope: None,
        })
    }

    // The blob pool's rules: the next nonce only, a replacement must double
    // every fee.
    async fn send_blob_tx(&self, tx: &SignedBlobTx) -> Result<(), L1SendError> {
        let mut i = self.lock();
        let t = i
            .signed
            .get(&tx.hash)
            .cloned()
            .ok_or_else(|| L1SendError::Refused("unknown transaction".into()))?;
        let head = head_of(&i);
        i.sends.push(L1Send {
            from: t.from,
            nonce: t.nonce,
            fees: t.fees,
            hash: t.hash,
            head,
        });
        let count = mined_count(&i, t.from);
        if t.nonce < count {
            return Err(L1SendError::NonceTaken(format!(
                "nonce too low: next nonce {count}, tx nonce {}",
                t.nonce
            )));
        }
        if t.nonce > count {
            return Err(L1SendError::Gap("nonce too high".into()));
        }
        if let Some(old) = i.pool.get(&(t.from, t.nonce)) {
            if old.hash == t.hash {
                return Ok(());
            }
            let (o, n) = (old.fees, t.fees);
            if n.max_fee < 2 * o.max_fee || n.tip < 2 * o.tip || n.blob_fee < 2 * o.blob_fee {
                return Err(L1SendError::Underpriced(
                    "replacement transaction underpriced".into(),
                ));
            }
        }
        if i.drop_next > 0 {
            i.drop_next -= 1;
            return Ok(());
        }
        i.pool.insert((t.from, t.nonce), t);
        Ok(())
    }

    async fn receipt(&self, tx: B256) -> Result<Option<L1Receipt>, Web3Error> {
        Ok(locate(&self.lock(), &tx).map(|(block, index)| L1Receipt {
            block,
            index: index as u64,
        }))
    }

    // `l1proof::build` on this chain's blocks.
    async fn build_proof(&self, txs: &[B256], top: u64) -> Result<L1BlobProof, Web3Error> {
        let i = self.lock();
        let located = txs
            .iter()
            .map(|h| locate(&i, h).ok_or_else(|| Web3Error::Rpc(format!("no receipt for {h}"))))
            .collect::<Result<Vec<_>, _>>()?;
        let oldest = located
            .iter()
            .map(|(n, _)| *n)
            .min()
            .ok_or_else(|| Web3Error::Data("no transactions".into()))?;
        if top < oldest || top > head_of(&i) {
            return Err(Web3Error::Rpc(format!("top {top} out of range")));
        }
        let headers = (oldest..=top)
            .rev()
            .map(|n| {
                let b = &i.blocks[n as usize];
                header_rlp(&b.header, b.hash).map_err(|e| Web3Error::Data(e.to_string()))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let txs = located
            .iter()
            .map(|(n, idx)| {
                let envs: Vec<Bytes> = i.blocks[*n as usize]
                    .txs
                    .iter()
                    .map(|t| t.envelope.clone())
                    .collect();
                let (_, proof) = tx_trie_proof(&envs, *idx);
                L1TxProof {
                    header_index: (top - n) as usize,
                    key: tx_key(*idx),
                    proof,
                }
            })
            .collect();
        Ok(L1BlobProof { headers, txs })
    }
}
```

In `sequencer/tests/common/mod.rs`: add `pub mod l1;` after `pub mod anvil;`, and `pub use l1::{FakeL1, L1Send};`. Extend the `davinci_sequencer::web3::{...}` re-export with `L1Blobs, L1Fees, L1Market, L1Origin`.

- [ ] **Step 2: Write the failing poster tests**

Create `sequencer/tests/l1da.rs`:

```rust
//! The L1-blob poster and settle loop on the scripted Ethereum (`FakeL1`)
//! and registry (`FakeChain`).

mod common;

use common::*;
use davinci_sequencer::l1da::{PostError, Poster};
use davinci_zkvm_sdk::ballot::Ballot;
use davinci_zkvm_sdk::blob::{TransitionData, build_blobs};
use davinci_zkvm_sdk::limits::VOTE_ID_MIN;

const PID: [u8; 31] = [7; 31];
const ROOT: [u8; 32] = [1; 32];

fn blobs(seed: u8) -> TransitionBlobs {
    let t = TransitionData {
        vote_ids: vec![VOTE_ID_MIN + u64::from(seed)],
        updates: vec![],
        accumulator: Ballot::identity(),
        num_fields: 1,
    };
    build_blobs(&t, &Fr::from(u64::from(seed)), &[seed; 32]).unwrap()
}

fn poster(l1: &FakeL1, db: &Db, cap: u128) -> Poster {
    Poster::new(Arc::new(l1.clone()), db.clone(), cap, Duration::from_millis(5)).unwrap()
}

fn soon() -> tokio::time::Instant {
    tokio::time::Instant::now() + Duration::from_secs(20)
}

fn fresh_db() -> (TempDir, Db) {
    let dir = TempDir::new().unwrap();
    let db = Db::open_in(dir.path()).unwrap();
    (dir, db)
}

#[tokio::test]
async fn posts_are_serial_with_consecutive_nonces() {
    let l1 = FakeL1::new();
    let (_dir, db) = fresh_db();
    let p = poster(&l1, &db, u128::MAX);
    let (b1, b2) = (blobs(1), blobs(2));
    let (a, b) = tokio::join!(
        p.post(&PID, &ROOT, &b1, soon()),
        p.post(&PID, &ROOT, &b2, soon())
    );
    let (a, b) = (a.unwrap(), b.unwrap());
    let mut nonces = [a.nonce, b.nonce];
    nonces.sort();
    assert_eq!(nonces, [0, 1]);
    let s = l1.sends();
    assert_eq!(s.len(), 2);
    // The second went out only once the first was mined.
    let first = if a.nonce == 0 { a } else { b };
    assert!(s[1].head >= first.block, "{s:?}");
    assert_eq!(l1.mined_posts(), 2);
    assert!(db.l1_posts().unwrap().is_empty());
}

#[tokio::test]
async fn the_fee_cap_waits_for_cheaper_fees() {
    let l1 = FakeL1::new();
    let (_dir, db) = fresh_db();
    // 1,000 gwei base fee; the cap allows about 50 gwei.
    l1.set_market(L1Market {
        base_fee: 1_000_000_000_000,
        tip: 0,
        blob_base_fee: 1,
    });
    let p = poster(&l1, &db, 21_000 * 100_000_000_000);
    let e = p.post(&PID, &ROOT, &blobs(1), soon()).await.unwrap_err();
    assert!(matches!(e, PostError::FeeCap { .. }), "{e}");
    assert!(l1.sends().is_empty(), "nothing sent above the cap");
    l1.set_market(L1Market {
        base_fee: 1_000_000_000,
        tip: 0,
        blob_base_fee: 1,
    });
    p.post(&PID, &ROOT, &blobs(1), soon()).await.unwrap();
}

#[tokio::test]
async fn a_replacement_above_the_cap_waits_at_the_current_price() {
    let l1 = FakeL1::new();
    let (_dir, db) = fresh_db();
    l1.hold(true);
    let first = L1Fees::offer(&l1.market());
    // One doubling fits, a second does not.
    let p = poster(&l1, &db, first.cost(1) * 3);
    let deadline = tokio::time::Instant::now() + Duration::from_millis(500);
    let e = p.post(&PID, &ROOT, &blobs(1), deadline).await.unwrap_err();
    assert!(matches!(e, PostError::Deadline), "{e}");
    let s = l1.sends();
    assert_eq!(s.len(), 2, "the post and one doubled replacement: {s:?}");
    assert_eq!(s[0].nonce, s[1].nonce);
    assert_eq!(s[1].fees, first.doubled());
    // The record keeps the price on the pool, for the next post.
    let rec = db.l1_posts().unwrap();
    assert_eq!(rec.len(), 1);
    assert_eq!(rec[0].fees, s[1].fees);
}

#[tokio::test]
async fn a_dropped_post_is_replaced_at_its_nonce() {
    let l1 = FakeL1::new();
    let (_dir, db) = fresh_db();
    l1.drop_next(1);
    let p = poster(&l1, &db, u128::MAX);
    let posted = p.post(&PID, &ROOT, &blobs(1), soon()).await.unwrap();
    let s = l1.sends();
    assert_eq!(s.len(), 2);
    assert_eq!((s[0].nonce, s[1].nonce, posted.nonce), (0, 0, 0));
    assert_eq!(s[1].fees, s[0].fees.doubled());
    assert!(s[1].head >= s[0].head + davinci_sequencer::l1da::REPLACE_AFTER);
    assert_eq!(posted.tx, s[1].hash);
    assert_eq!(l1.mined_posts(), 1);
}

#[tokio::test]
async fn an_unrecorded_orphan_is_outbid() {
    let l1 = FakeL1::new();
    let (_dir, db) = fresh_db();
    let orphan_fees = L1Fees::offer(&l1.market()).doubled().doubled();
    let orphan = l1.inject_orphan(0, orphan_fees);
    let p = poster(&l1, &db, u128::MAX);
    let posted = p.post(&PID, &ROOT, &blobs(1), soon()).await.unwrap();
    assert_eq!(posted.nonce, 0);
    let s = l1.sends();
    assert_eq!(s.len(), 4, "refused until every fee doubles the orphan's: {s:?}");
    let won = s.last().unwrap();
    assert!(won.fees.max_fee >= 2 * orphan_fees.max_fee);
    assert!(won.fees.tip >= 2 * orphan_fees.tip);
    assert!(won.fees.blob_fee >= 2 * orphan_fees.blob_fee);
    assert_eq!(l1.block_of(&orphan), None);
    assert_eq!(l1.mined_posts(), 1);
}

#[tokio::test]
async fn a_restart_replaces_the_recorded_post() {
    let l1 = FakeL1::new();
    let (_dir, db) = fresh_db();
    // No blocks: the post stays pending.
    l1.set_auto_mine(false);
    let p1 = poster(&l1, &db, u128::MAX);
    let deadline = tokio::time::Instant::now() + Duration::from_millis(200);
    let e = p1.post(&PID, &ROOT, &blobs(1), deadline).await.unwrap_err();
    assert!(matches!(e, PostError::Deadline), "{e}");
    let rec = db.l1_posts().unwrap().pop().unwrap();
    drop(p1);
    // A new poster on the same database: the first post replaces it.
    l1.set_auto_mine(true);
    let p2 = poster(&l1, &db, u128::MAX);
    let posted = p2.post(&PID, &ROOT, &blobs(2), soon()).await.unwrap();
    assert_eq!(posted.nonce, rec.nonce);
    let last = l1.sends().pop().unwrap();
    assert_eq!(last.nonce, rec.nonce);
    assert!(last.fees.max_fee >= 2 * rec.fees.max_fee);
    assert!(last.fees.tip >= 2 * rec.fees.tip);
    assert!(last.fees.blob_fee >= 2 * rec.fees.blob_fee);
    assert_eq!(l1.mined_posts(), 1);
    assert!(db.l1_posts().unwrap().is_empty());
}

#[tokio::test]
async fn no_key_no_post() {
    let l1 = FakeL1::new();
    let (_dir, db) = fresh_db();
    let p = poster(&l1.observer(), &db, u128::MAX);
    let e = p.post(&PID, &ROOT, &blobs(1), soon()).await.unwrap_err();
    assert!(matches!(e, PostError::NoSigner), "{e}");
}
```

- [ ] **Step 3: Run them to verify they fail**

Run: `cargo test -p davinci-sequencer --test l1da 2>&1 | tail -5`
Expected: FAIL to compile (`davinci_sequencer::l1da` not found).

- [ ] **Step 4: Implement the poster**

Add `pub mod l1da;` to `sequencer/src/lib.rs` (after `pub mod keys;`). Create `sequencer/src/l1da/mod.rs`:

```rust
//! L1-blob mode (OP Stack registries): a batch's blobs are posted on
//! Ethereum while it is proved, and the settlement on OP carries a proof that
//! the blob transaction is in Ethereum's canonical chain. The actor calls
//! into this module only when `Deps.l1` is set; blob mode never does.

mod post;

use std::sync::Arc;
use std::time::Duration;

use crate::storage::{Db, StorageError};
use crate::web3::L1Chain;

pub use post::{PostError, Posted, Poster, REPLACE_AFTER};

/// The Ethereum side of a node in L1-blob mode.
pub struct L1Da {
    pub l1: Arc<dyn L1Chain>,
    pub poster: Poster,
    /// Pace of the settle loop and the poster (the chain poll interval).
    pub poll: Duration,
}

impl L1Da {
    /// `fee_cap`: the most one post may cost at the fees it offers, in wei.
    pub fn new(
        l1: Arc<dyn L1Chain>,
        db: Db,
        fee_cap: u128,
        poll: Duration,
    ) -> Result<Self, StorageError> {
        Ok(L1Da {
            poster: Poster::new(l1.clone(), db, fee_cap, poll)?,
            l1,
            poll,
        })
    }
}
```

Create `sequencer/src/l1da/post.rs`:

```rust
//! The L1 blob poster: one type-3 transaction at a time from the node's
//! dedicated Ethereum account (the blob pool forbids nonce gaps and wants
//! every fee doubled to replace), each recorded before it is sent so a
//! restart replaces it instead of queueing behind it.

use std::sync::Arc;
use std::time::Duration;

use alloy::primitives::B256;
use davinci_zkvm_sdk::blob::TransitionBlobs;
use tokio::sync::Mutex;
use tokio::time::Instant;
use tracing::{debug, info, warn};

use crate::storage::{Db, L1PostRecord, StorageError};
use crate::web3::{L1Chain, L1Fees, L1SendError, Web3Error};

/// L1 blocks a post waits before it is replaced at its nonce.
pub const REPLACE_AFTER: u64 = 3;
/// Refused sends (underpriced, nonce trouble) one post tolerates.
const MAX_REFUSALS: u32 = 6;

/// A mined blob transaction of the node's account.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Posted {
    pub tx: B256,
    pub nonce: u64,
    pub block: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum PostError {
    #[error("no Ethereum key: set --l1-privkey-file or DAVINCI_L1_PRIVKEY")]
    NoSigner,
    #[error("an L1 post would cost up to {want} wei, above the cap of {cap} wei")]
    FeeCap { want: u128, cap: u128 },
    #[error("the L1 post was not mined before the batch deadline")]
    Deadline,
    #[error("L1: {0}")]
    L1(String),
    #[error("storage: {0}")]
    Storage(String),
}

impl From<Web3Error> for PostError {
    fn from(e: Web3Error) -> Self {
        match e {
            Web3Error::NoSigner => PostError::NoSigner,
            e => PostError::L1(e.to_string()),
        }
    }
}

impl From<StorageError> for PostError {
    fn from(e: StorageError) -> Self {
        PostError::Storage(e.to_string())
    }
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Posts blobs from one Ethereum account, one transaction at a time.
pub struct Poster {
    l1: Arc<dyn L1Chain>,
    db: Db,
    fee_cap: u128,
    poll: Duration,
    /// The account's newest post: it prices a replacement at its nonce. Held
    /// for a whole post, so a second post waits until the first is mined.
    // ponytail: a window of one post per account (about 5 posts a minute).
    // More in flight needs a record per nonce and gap repair on drops.
    last: Mutex<Option<L1PostRecord>>,
}

impl Poster {
    /// Picks up the newest record an earlier run left: if that transaction
    /// is still pending, the first post replaces it.
    pub fn new(
        l1: Arc<dyn L1Chain>,
        db: Db,
        fee_cap: u128,
        poll: Duration,
    ) -> Result<Self, StorageError> {
        let last = db.l1_posts()?.into_iter().max_by_key(|r| r.nonce);
        if let Some(r) = &last {
            info!(nonce = r.nonce, tx = %r.tx_hash,
                "L1 post of an earlier run on record; the next post replaces it if still pending");
        }
        Ok(Poster {
            l1,
            db,
            fee_cap,
            poll,
            last: Mutex::new(last),
        })
    }

    fn within_cap(&self, fees: &L1Fees, n: usize) -> Result<(), PostError> {
        let want = fees.cost(n);
        if want > self.fee_cap {
            return Err(PostError::FeeCap {
                want,
                cap: self.fee_cap,
            });
        }
        Ok(())
    }

    /// Posts `blobs` in one blob transaction at the account's next nonce and
    /// waits until it is mined. A transaction of ours already at that nonce
    /// (a replaced post, an earlier run's) is outbid with every fee doubled;
    /// one not mined after [`REPLACE_AFTER`] blocks is replaced the same way
    /// while that stays within the fee cap. `deadline` bounds the wait for
    /// the account too.
    pub async fn post(
        &self,
        pid: &[u8; 31],
        root_before: &[u8; 32],
        blobs: &TransitionBlobs,
        deadline: Instant,
    ) -> Result<Posted, PostError> {
        let mut last = tokio::time::timeout_at(deadline, self.last.lock())
            .await
            .map_err(|_| PostError::Deadline)?;
        if self.l1.sender().is_none() {
            return Err(PostError::NoSigner);
        }
        let vhs: Vec<B256> = blobs
            .versioned_hashes
            .iter()
            .map(|h| B256::from(*h))
            .collect();
        let n = vhs.len();
        let mut refusals = 0u32;
        let mut sidecar_retried = false;
        'nonce: loop {
            let nonce = self.l1.nonce().await?;
            if nonce > 0 && last.as_ref().is_some_and(|r| r.nonce < nonce) {
                // Mined (or replaced by a tx of ours): nothing left to replace.
                self.db.remove_l1_posts_upto(nonce - 1)?;
            }
            let mut fees = L1Fees::offer(&self.l1.fees().await?);
            if let Some(r) = last.as_ref().filter(|r| r.nonce == nonce) {
                // A tx of ours may hold this nonce: outbid it.
                fees = fees.max_each(r.fees.doubled());
            }
            self.within_cap(&fees, n)?;
            let mut hashes: Vec<B256> = Vec::new();
            'send: loop {
                if Instant::now() >= deadline {
                    return Err(PostError::Deadline);
                }
                let tx = self.l1.sign_blob_tx(nonce, &fees, blobs).await?;
                // Recorded before the send: a crash right after it still knows the tx.
                let rec = L1PostRecord {
                    pid: *pid,
                    root_before: *root_before,
                    nonce,
                    tx_hash: tx.hash,
                    fees,
                    vhs: vhs.clone(),
                    posted_at: unix_now(),
                };
                self.db.put_l1_post(&rec)?;
                *last = Some(rec);
                match self.l1.send_blob_tx(&tx).await {
                    Ok(()) => hashes.push(tx.hash),
                    Err(L1SendError::Unanswered(e)) => {
                        warn!(nonce, error = %e, "L1 send unanswered; waiting for its receipt");
                        hashes.push(tx.hash);
                    }
                    Err(L1SendError::Sidecar(e)) if !sidecar_retried => {
                        sidecar_retried = true;
                        warn!(error = %e, "the L1 node refused the sidecar version; re-signing with the other");
                        continue 'send;
                    }
                    Err(L1SendError::Underpriced(e)) => {
                        refusals += 1;
                        if refusals > MAX_REFUSALS {
                            return Err(PostError::L1(e));
                        }
                        // Something of ours holds the nonce: double until it is outbid.
                        fees = fees.doubled();
                        self.within_cap(&fees, n)?;
                        continue 'send;
                    }
                    Err(L1SendError::NonceTaken(e) | L1SendError::Gap(e)) => {
                        refusals += 1;
                        if refusals > MAX_REFUSALS {
                            return Err(PostError::L1(e));
                        }
                        if hashes.is_empty() {
                            continue 'nonce;
                        }
                        // A send of ours at this nonce may be the one mined: wait for it.
                    }
                    Err(e @ (L1SendError::Sidecar(_) | L1SendError::Refused(_))) => {
                        return Err(PostError::L1(e.to_string()));
                    }
                }
                let mut since: Option<u64> = None;
                loop {
                    for h in &hashes {
                        match self.l1.receipt(*h).await {
                            Ok(Some(r)) => {
                                self.db.remove_l1_posts_upto(nonce)?;
                                if let Some(rec) = last.as_mut() {
                                    rec.tx_hash = *h;
                                }
                                info!(nonce, tx = %h, block = r.block, blobs = n, "blobs posted on L1");
                                return Ok(Posted {
                                    tx: *h,
                                    nonce,
                                    block: r.block,
                                });
                            }
                            Ok(None) => {}
                            // A failed read says nothing about a tx already out.
                            Err(e) => debug!(error = %e, "L1 receipt read failed; still waiting"),
                        }
                    }
                    if Instant::now() >= deadline {
                        return Err(PostError::Deadline);
                    }
                    if let Ok(head) = self.l1.head().await {
                        let start = *since.get_or_insert(head);
                        if head >= start + REPLACE_AFTER
                            && let (Ok(count), Ok(market)) =
                                (self.l1.nonce().await, self.l1.fees().await)
                        {
                            if count > nonce {
                                // Mined, by none of the hashes this post sent.
                                refusals += 1;
                                if refusals > MAX_REFUSALS {
                                    return Err(PostError::L1(format!(
                                        "nonce {nonce} taken by an unknown transaction"
                                    )));
                                }
                                warn!(nonce, "L1 nonce taken by a transaction this post did not send");
                                continue 'nonce;
                            }
                            let up = fees.doubled().max_each(L1Fees::offer(&market));
                            if up.cost(n) <= self.fee_cap {
                                warn!(nonce, blocks = REPLACE_AFTER,
                                    "L1 post not mined; replacing it with doubled fees");
                                fees = up;
                                continue 'send;
                            }
                            warn!(nonce, cap = self.fee_cap,
                                "an L1 replacement would pass the fee cap; waiting at the current price");
                            since = Some(head);
                        }
                    }
                    tokio::time::sleep(self.poll).await;
                }
            }
        }
    }
}
```

- [ ] **Step 5: Run the poster tests**

Run: `cargo test -p davinci-sequencer --test l1da 2>&1 | tail -5`
Expected: PASS (7 tests).

- [ ] **Step 6: Commit**

```bash
cargo fmt --all && cargo clippy -p davinci-sequencer --all-targets -- -D warnings
git add sequencer/src/lib.rs sequencer/src/l1da sequencer/tests/common sequencer/tests/l1da.rs
git commit -m "feat(l1da): window-1 blob poster with doubled replacements and a fee cap"
```

---
### Task 8: The settle loop, the anchor policy and the fake registry's L1 mode

**Files:**
- Create: `sequencer/src/l1da/settle.rs`
- Modify: `sequencer/src/l1da/mod.rs`
- Modify: `sequencer/tests/common/mod.rs` (`Inner` L262-334, `FakeChain::new` L342-427, accessors, `settle_foreign` L819-848, `impl Chain for FakeChain` L1006-1355)
- Test: `sequencer/tests/l1da.rs`

**Interfaces:**
- Consumes: Task 3 `Chain::{l1_origin, record_l1_origin, simulate_transition_l1, submit_transition_l1}`, `revert_message`; Task 4 `L1Blobs`; Task 7 `L1Da`, `Poster`, `Posted`, `PostError`, `FakeL1`; Plan 1 `verify_local`.
- Produces:
  - `pub struct L1Job<'a> { pub pid: [u8; 31], pub root_before: [u8; 32], pub blobs: &'a TransitionBlobs, pub deadline: tokio::time::Instant, pub cancel: CancellationToken }`
  - `pub struct Posting { pub post: Result<Posted, PostError>, pub recorded: Option<L1Origin> }`
  - `pub enum Settled { Landed(TxReceipt), Reverted(String), Lost(String), Transient(String), Failed(String) }`
  - `pub async fn with_post<T>(da: &L1Da, chain: &dyn Chain, job: &L1Job<'_>, prove: impl Future<Output = T>) -> (T, Posting)`
  - `pub async fn settle(da: &L1Da, chain: &dyn Chain, job: &L1Job<'_>, snark: &PlonkSnark, posting: Posting) -> Settled`
  - `pub enum Anchor { At { top: u64, hash: B256 }, Wait, Repost }`, `pub fn choose_anchor(n: u64, head: u64, origin: L1Origin, recorded: Option<L1Origin>) -> Anchor`
  - Constants `REORG_MARGIN = 1`, `TOP_ABOVE_ORIGIN = 2`, `CURRENT_MAX_HEADERS = 16`, `MAX_L1_PROOF_HEADERS = 64`, `L1_MAX_AGE = 7200`, `MAX_PROOF_RETRIES = 5`
  - `FakeChain::{attach_l1(&FakeL1), l1_records(), l1_simulates(), l1_last_headers(), l1_last() -> Option<L1Blobs>, vanish_next_l1_submit()}`; FakeChain implements the four L1 `Chain` methods like the registry and `L1BlobVerifier` (anchor at the current origin or a recorded one, `TxTooOld`, `L1OriginMismatch`, count and hash checks), and a foreign settlement (`settle_externally`, `race_next_submit`) posts its blobs on the attached `FakeL1`.

- [ ] **Step 1: Write the failing anchor test**

Create `sequencer/src/l1da/settle.rs` with the module doc and the test only:

```rust
//! Settling a proved batch whose blobs are on Ethereum: wait for the blob
//! transaction's block and for OP's origin, anchor the L1 proof where OP
//! accepts it, check it locally, simulate and send. The blobs are posted
//! again when their transaction is gone or the proof would be too deep.

#[cfg(test)]
mod tests {
    use super::*;

    fn o(number: u64) -> L1Origin {
        L1Origin {
            number,
            hash: B256::with_last_byte(number as u8),
        }
    }

    #[test]
    fn the_anchor_follows_the_origin_then_the_record() {
        let n = 100;
        // At the origin: two headers above it, capped by our head.
        assert_eq!(choose_anchor(n, 101, o(100), None), Anchor::At { top: 101, hash: o(100).hash });
        assert_eq!(choose_anchor(n, 120, o(110), None), Anchor::At { top: 112, hash: o(110).hash });
        // 16 headers from the current origin is the limit (origin + 2 - n + 1)...
        assert_eq!(
            choose_anchor(n, 200, o(113), Some(o(101))),
            Anchor::At { top: 115, hash: o(113).hash }
        );
        // ...past it, the recorded origin anchors: headers 101 down to 100.
        assert_eq!(
            choose_anchor(n, 200, o(114), Some(o(101))),
            Anchor::At { top: 101, hash: o(101).hash }
        );
        // Without a record, up to 64 headers from the current origin...
        assert_eq!(choose_anchor(n, 200, o(161), None), Anchor::At { top: 163, hash: o(161).hash });
        // ...then the blobs go out again.
        assert_eq!(choose_anchor(n, 200, o(162), None), Anchor::Repost);
        // A record from before the blob block anchors nothing for it.
        assert_eq!(choose_anchor(n, 200, o(162), Some(o(99))), Anchor::Repost);
        // Our L1 RPC behind OP's origin: wait for it.
        assert_eq!(choose_anchor(n, 105, o(110), None), Anchor::Wait);
        // As old as the verifier's MAX_AGE: post again, record or not.
        assert_eq!(
            choose_anchor(n, n + 8000, o(n + L1_MAX_AGE), Some(o(101))),
            Anchor::Repost
        );
    }
}
```

Register it in `sequencer/src/l1da/mod.rs`: `mod settle;` and `pub use settle::{Anchor, CURRENT_MAX_HEADERS, L1_MAX_AGE, MAX_L1_PROOF_HEADERS, MAX_PROOF_RETRIES, REORG_MARGIN, TOP_ABOVE_ORIGIN, choose_anchor, settle};`.

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test -p davinci-sequencer --lib l1da 2>&1 | tail -5`
Expected: FAIL to compile (`choose_anchor`, `Anchor`, `L1_MAX_AGE` not found).

- [ ] **Step 3: Implement the job types and `with_post` in `l1da/mod.rs`**

Add to the imports of `sequencer/src/l1da/mod.rs`:

```rust
use davinci_zkvm_sdk::blob::TransitionBlobs;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::actor::Chain;
use crate::web3::{L1Origin, TxReceipt};
```

and append:

```rust
impl L1Da {
    /// Waits one poll; false once `cancel` fires.
    pub(crate) async fn pause(&self, cancel: &CancellationToken) -> bool {
        tokio::select! {
            _ = cancel.cancelled() => false,
            _ = tokio::time::sleep(self.poll) => true,
        }
    }
}

/// One sealed batch on its way to OP.
pub struct L1Job<'a> {
    pub pid: [u8; 31],
    pub root_before: [u8; 32],
    pub blobs: &'a TransitionBlobs,
    /// No settlement after this (the grace end known at seal time).
    pub deadline: tokio::time::Instant,
    /// Fired when the actor drops the flight (rollback, shutdown).
    pub cancel: CancellationToken,
}

/// What [`with_post`] leaves for [`settle`].
#[derive(Debug)]
pub struct Posting {
    pub post: Result<Posted, PostError>,
    /// OP's origin recorded with `record()` while the proof was not ready.
    pub recorded: Option<L1Origin>,
}

/// How an L1-mode settlement ended; the actor maps it onto its job outcome.
#[derive(Debug)]
pub enum Settled {
    Landed(TxReceipt),
    /// The registry's error name (`Error(<message>)` for a string revert).
    Reverted(String),
    Lost(String),
    Transient(String),
    Failed(String),
}

/// Runs `prove` while posting the batch's blobs. Once the post is mined, if
/// OP's origin reaches its block before the proof is ready, records that
/// origin on OP once, so a late proof still anchors close to the blobs.
pub async fn with_post<T>(
    da: &L1Da,
    chain: &dyn Chain,
    job: &L1Job<'_>,
    prove: impl Future<Output = T>,
) -> (T, Posting) {
    let post = da
        .poster
        .post(&job.pid, &job.root_before, job.blobs, job.deadline);
    tokio::pin!(prove);
    tokio::pin!(post);
    let mut posted: Option<Result<Posted, PostError>> = None;
    let mut recorded = None;
    let mut record_tried = false;
    let proved = loop {
        let block = match &posted {
            Some(Ok(p)) if !record_tried => Some(p.block),
            _ => None,
        };
        tokio::select! {
            out = &mut prove => break out,
            p = &mut post, if posted.is_none() => posted = Some(p),
            _ = tokio::time::sleep(da.poll), if block.is_some() => {
                if let Some(n) = block
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
    let post = match posted {
        Some(p) => p,
        None => post.await,
    };
    (proved, Posting { post, recorded })
}

/// Why the registry's DA mode and the configuration disagree, if they do.
pub fn mode_error(da: &crate::web3::RegistryDa, l1_configured: bool) -> Option<String> {
    use crate::web3::RegistryDa;
    match (da, l1_configured) {
        (RegistryDa::Blobs, true) => Some(
            "the registry settles with blobs on its own chain (no L1BlobVerifier); \
             remove --l1-rpc-url (DAVINCI_L1_RPC_URL)"
                .into(),
        ),
        (RegistryDa::L1Blobs { verifier, .. }, false) => Some(format!(
            "the registry settles with Ethereum blobs through L1BlobVerifier {verifier}; \
             set --l1-rpc-url (DAVINCI_L1_RPC_URL)"
        )),
        _ => None,
    }
}
```

Add a test module at the end of `mod.rs`:

```rust
#[cfg(test)]
mod tests {
    use alloy::primitives::Address;

    use super::*;
    use crate::web3::RegistryDa;

    #[test]
    fn the_registry_mode_must_match_the_configuration() {
        let l1 = RegistryDa::L1Blobs {
            verifier: Address::repeat_byte(1),
            l1_block: Address::repeat_byte(2),
        };
        assert!(mode_error(&RegistryDa::Blobs, false).is_none());
        assert!(mode_error(&l1, true).is_none());
        assert!(mode_error(&RegistryDa::Blobs, true).unwrap().contains("--l1-rpc-url"));
        assert!(mode_error(&l1, false).unwrap().contains("--l1-rpc-url"));
    }
}
```

- [ ] **Step 4: Implement the settle loop**

Insert above the test module of `sequencer/src/l1da/settle.rs`:

```rust
use alloy::primitives::B256;
use davinci_zkvm_sdk::client::PlonkSnark;
use tokio::time::Instant;
use tracing::{info, warn};

use super::{L1Da, L1Job, PostError, Posting, Settled};
use crate::actor::Chain;
use crate::web3::l1proof::verify_local;
use crate::web3::{L1Origin, RevertReason, Web3Error, revert_message};

/// L1 blocks on top of the blob block before a proof is built (reorg margin).
pub const REORG_MARGIN: u64 = 1;
/// Headers above OP's origin a proof carries, so it survives the origin
/// advancing while the settlement is in flight.
pub const TOP_ABOVE_ORIGIN: u64 = 2;
/// Most headers anchored at the current origin before a recorded origin is
/// preferred (about 60-75k gas each on OP).
pub const CURRENT_MAX_HEADERS: u64 = 16;
/// Most headers in any proof (about 5M gas and 40 KB of calldata on OP).
pub const MAX_L1_PROOF_HEADERS: u64 = 64;
/// `L1BlobVerifier.MAX_AGE`: how far behind the origin a proven
/// transaction's block may be.
pub const L1_MAX_AGE: u64 = 7200;
/// Proof rebuilds (after `L1OriginMismatch` or a local anchor miss) before
/// the batch goes back to the queue.
pub const MAX_PROOF_RETRIES: u32 = 5;

/// Where a proof is anchored.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Anchor {
    /// Headers from `top` down to the blob block; one of them hashes to `hash`.
    At { top: u64, hash: B256 },
    /// Our L1 RPC is behind OP's origin.
    Wait,
    /// No anchor is close enough: post the blobs again.
    Repost,
}

/// The anchor of a proof for blob block `n` (with `origin.number >= n`):
/// the current origin while that takes at most [`CURRENT_MAX_HEADERS`]
/// headers (up to [`TOP_ABOVE_ORIGIN`] above it, within our L1 head), else a
/// recorded origin at or above `n`, else the current origin up to
/// [`MAX_L1_PROOF_HEADERS`]; deeper, or [`L1_MAX_AGE`] behind, a re-post.
pub fn choose_anchor(n: u64, head: u64, origin: L1Origin, recorded: Option<L1Origin>) -> Anchor {
    let behind = origin.number.saturating_sub(n);
    if behind >= L1_MAX_AGE {
        return Anchor::Repost;
    }
    let current = behind + 1 + TOP_ABOVE_ORIGIN;
    if current > CURRENT_MAX_HEADERS
        && let Some(r) = recorded
        && r.number >= n
        && r.number - n < MAX_L1_PROOF_HEADERS
    {
        return Anchor::At {
            top: r.number,
            hash: r.hash,
        };
    }
    if current > MAX_L1_PROOF_HEADERS {
        return Anchor::Repost;
    }
    if head < origin.number {
        return Anchor::Wait;
    }
    Anchor::At {
        top: head.min(origin.number + TOP_ABOVE_ORIGIN),
        hash: origin.hash,
    }
}

// A revert's name, with the message of a string one.
fn named(name: String, data: &[u8]) -> String {
    match revert_message(data) {
        Some(m) if name == "Error" => format!("Error({m})"),
        _ => name,
    }
}

/// Settles `job`'s proved batch on OP. Waits (within the deadline) for the
/// blob transaction to have [`REORG_MARGIN`] blocks on top and for OP's
/// origin to reach it, then sends the transition with an L1 proof anchored by
/// [`choose_anchor`]. A vanished blob transaction, a too-deep proof or
/// `TxTooOld` post the blobs again; `L1OriginMismatch` rebuilds the proof
/// from fresh reads, up to [`MAX_PROOF_RETRIES`] times. Other reverts end the
/// job with their name, as in blob mode.
pub async fn settle(
    da: &L1Da,
    chain: &dyn Chain,
    job: &L1Job<'_>,
    snark: &PlonkSnark,
    posting: Posting,
) -> Settled {
    let pid = hex::encode(job.pid);
    let want: Vec<B256> = job
        .blobs
        .versioned_hashes
        .iter()
        .map(|h| B256::from(*h))
        .collect();
    let mut post = match posting.post {
        Ok(p) => Some(p),
        Err(e) => {
            warn!(%pid, error = %e, "L1 post during proving failed; posting again");
            None
        }
    };
    let mut recorded = posting.recorded;
    let mut retries = 0u32;
    let aborted = || Settled::Transient("flight aborted".into());
    loop {
        if job.cancel.is_cancelled() {
            return aborted();
        }
        if Instant::now() >= job.deadline {
            return Settled::Transient("L1 settlement past the batch deadline".into());
        }
        let p = match post {
            Some(p) => p,
            None => match da
                .poster
                .post(&job.pid, &job.root_before, job.blobs, job.deadline)
                .await
            {
                Ok(p) => {
                    post = Some(p);
                    recorded = None;
                    p
                }
                Err(e @ (PostError::Deadline | PostError::NoSigner | PostError::Storage(_))) => {
                    return Settled::Transient(e.to_string());
                }
                // Fee cap, L1 trouble: wait and post again within the deadline.
                Err(e) => {
                    warn!(%pid, error = %e, "L1 post failed; retrying");
                    if !da.pause(&job.cancel).await {
                        return aborted();
                    }
                    continue;
                }
            },
        };
        let rc = match da.l1.receipt(p.tx).await {
            Ok(Some(rc)) => rc,
            Ok(None) => {
                warn!(%pid, tx = %p.tx, "L1 blob transaction gone (dropped or reorged out); posting again");
                post = None;
                continue;
            }
            Err(e) => {
                warn!(%pid, error = %e, "L1 receipt read failed");
                if !da.pause(&job.cancel).await {
                    return aborted();
                }
                continue;
            }
        };
        let n = rc.block;
        let (head, origin) = match (da.l1.head().await, chain.l1_origin().await) {
            (Ok(h), Ok(o)) => (h, o),
            (Err(e), _) | (_, Err(e)) => {
                warn!(%pid, error = %e, "L1 head or OP origin read failed");
                if !da.pause(&job.cancel).await {
                    return aborted();
                }
                continue;
            }
        };
        if head < n + REORG_MARGIN || origin.number < n {
            if !da.pause(&job.cancel).await {
                return aborted();
            }
            continue;
        }
        let (top, anchor) = match choose_anchor(n, head, origin, recorded) {
            Anchor::At { top, hash } => (top, hash),
            Anchor::Wait => {
                if !da.pause(&job.cancel).await {
                    return aborted();
                }
                continue;
            }
            Anchor::Repost => {
                warn!(%pid, n, origin = origin.number, "the L1 proof would be too deep; posting the blobs again");
                post = None;
                continue;
            }
        };
        let proof = match da.l1.build_proof(&[p.tx], top).await {
            Ok(proof) => proof,
            Err(e) => {
                warn!(%pid, error = %e, "L1 proof build failed");
                if !da.pause(&job.cancel).await {
                    return aborted();
                }
                continue;
            }
        };
        match verify_local(&proof, anchor) {
            Ok((vhs, _)) if vhs == want => {}
            Ok(_) => return Settled::Failed("the L1 blob transaction carries other blobs".into()),
            Err(e) => {
                retries += 1;
                if retries > MAX_PROOF_RETRIES {
                    return Settled::Transient(format!("L1 proof does not anchor: {e}"));
                }
                warn!(%pid, error = %e, "L1 proof does not anchor; rebuilding");
                if !da.pause(&job.cancel).await {
                    return aborted();
                }
                continue;
            }
        }
        let refused = match chain
            .simulate_transition_l1(&job.pid, snark, job.blobs, &proof)
            .await
        {
            Ok(()) => match chain
                .submit_transition_l1(&job.pid, snark, job.blobs, &proof)
                .await
            {
                Ok(rc) => {
                    info!(%pid, l1_block = n, headers = proof.headers.len(), tx = %rc.tx_hash,
                        "L1-blob transition sent");
                    return Settled::Landed(rc);
                }
                Err(Web3Error::Revert(RevertReason::Revert { name, data })) => named(name, &data),
                Err(e @ Web3Error::Lost { .. }) => return Settled::Lost(e.to_string()),
                Err(e) => return Settled::Transient(e.to_string()),
            },
            Err(RevertReason::Revert { name, data }) => named(name, &data),
            Err(RevertReason::Rpc(e)) => return Settled::Transient(e),
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
                if !da.pause(&job.cancel).await {
                    return aborted();
                }
            }
            "TxTooOld" => {
                warn!(%pid, n, "the blob transaction is too old for the verifier; posting again");
                post = None;
            }
            _ => return Settled::Reverted(refused),
        }
    }
}
```

- [ ] **Step 5: Run the unit tests**

Run: `cargo test -p davinci-sequencer --lib l1da 2>&1 | tail -5`
Expected: PASS (`the_anchor_follows_the_origin_then_the_record`, `the_registry_mode_must_match_the_configuration`).

- [ ] **Step 6: The fake registry in L1 mode**

In `sequencer/tests/common/mod.rs`:

Add to the imports: `use davinci_sequencer::l1da::L1_MAX_AGE;` and `use davinci_sequencer::web3::l1proof::{L1BlobProof, verify_local};`.

Add these fields at the end of `pub struct Inner`:

```rust
    /// L1-blob mode: the Ethereum the verifier reads its origin from.
    l1: Option<FakeL1>,
    /// Origins stored with `record()`: number -> hash.
    l1_recorded: HashMap<u64, B256>,
    l1_records: u64,
    l1_simulates: u64,
    /// The last L1 settlement: its proof's headers and its blobs' place.
    l1_last_headers: usize,
    l1_last: Option<L1Blobs>,
    /// The next L1 settlement "lands" with neither an event nor a state
    /// change: an OP reorg took it.
    vanish_l1_submit: bool,
```

and initialise them at the end of the `Inner { ... }` literal in `FakeChain::new`:

```rust
            l1: None,
            l1_recorded: HashMap::new(),
            l1_records: 0,
            l1_simulates: 0,
            l1_last_headers: 0,
            l1_last: None,
            vanish_l1_submit: false,
```

Add to `impl FakeChain`:

```rust
    /// Puts the registry in L1-blob mode over `l1`.
    pub fn attach_l1(&self, l1: &FakeL1) {
        self.inner.lock().unwrap().l1 = Some(l1.clone());
    }

    pub fn l1_records(&self) -> u64 {
        self.inner.lock().unwrap().l1_records
    }

    pub fn l1_simulates(&self) -> u64 {
        self.inner.lock().unwrap().l1_simulates
    }

    pub fn l1_last_headers(&self) -> usize {
        self.inner.lock().unwrap().l1_last_headers
    }

    pub fn l1_last(&self) -> Option<L1Blobs> {
        self.inner.lock().unwrap().l1_last.clone()
    }

    /// The next L1 settlement returns a receipt but never lands.
    pub fn vanish_next_l1_submit(&self) {
        self.inner.lock().unwrap().vanish_l1_submit = true;
    }
```

Add after `fn grace_end`:

```rust
// A transition of ours lands: state, counters and its event. Returns the tx.
fn land(
    i: &mut Inner,
    pid: &[u8; 31],
    sender: Address,
    p: &BatchPublics,
    n_blobs: u64,
    l1: Option<L1Blobs>,
) -> B256 {
    i.last_census_root = Some(p.census_root);
    i.proc.state_root = p.root_after;
    i.proc.last_vote_at = i.time;
    i.proc.voters_count += p.voters as u64;
    i.proc.overwritten_count += p.overwrites as u64;
    i.proc.batch_number += 1;
    i.block += 1;
    i.submits += 1;
    let mut h = [0u8; 32];
    h[..8].copy_from_slice(&(0x7788_0000 + i.submits).to_be_bytes());
    let tx = B256::from(h);
    let ev = RegistryEvent {
        block: i.block,
        tx_hash: tx,
        log_index: 0,
        kind: EventKind::StateTransitioned {
            pid: *pid,
            sender,
            old_root: p.root_before,
            new_root: p.root_after,
            voters: i.proc.voters_count,
            overwrites: i.proc.overwritten_count,
            n_blobs,
            l1,
        },
    };
    i.events.push(ev);
    tx
}

/// `L1BlobVerifier.verify` and the registry's count and opening checks: the
/// proof anchors at the current origin or a recorded one; returns the block
/// of each blob.
fn check_l1_proof(
    i: &Inner,
    blobs: &TransitionBlobs,
    proof: &L1BlobProof,
) -> Result<Vec<u64>, RevertReason> {
    let l1 = i.l1.as_ref().ok_or_else(|| revert("WrongDAMode"))?;
    let origin = l1.origin();
    let (vhs, blocks) = std::iter::once(origin.hash)
        .chain(i.l1_recorded.values().copied())
        .find_map(|anchor| verify_local(proof, anchor).ok())
        .ok_or_else(|| revert("L1OriginMismatch"))?;
    if blocks.iter().any(|b| origin.number.saturating_sub(*b) > L1_MAX_AGE) {
        return Err(revert("TxTooOld"));
    }
    let want: Vec<B256> = blobs.versioned_hashes.iter().map(|h| B256::from(*h)).collect();
    if vhs.len() != want.len() {
        return Err(revert("BlobCountMismatch"));
    }
    if vhs != want {
        return Err(revert("InvalidBlobOpening"));
    }
    Ok(blocks)
}
```

Replace the landing part of `submit_transition` (from `check_transition(&i, &p).map_err(Web3Error::Revert)?;` through the end of the function) with:

```rust
        check_transition(&i, &p).map_err(Web3Error::Revert)?;
        let tx = land(&mut i, pid, sender, &p, blobs.blobs.len() as u64, None);
        i.blobs.insert(tx, blobs.blobs.clone());
        if i.timeout_submits > 0 {
            // The tx landed (root moved, event queued) but the receipt
            // never came back to the sender.
            i.timeout_submits -= 1;
            return Err(Web3Error::Stuck {
                nonce: 0,
                attempts: 1,
            });
        }
        Ok(TxReceipt {
            tx_hash: tx,
            block: i.block,
            gas_used: 0,
            replacements: 0,
        })
    }
```

In `settle_foreign`, replace everything after `i.blobs.insert(tx, b.blobs.blobs.clone());` with the following, so the event's `l1` follows the mode:

```rust
    // L1-blob mode: the winner posted its blobs on Ethereum.
    let l1 = i.l1.clone().map(|l1| {
        let vhs: Vec<B256> = b.blobs.versioned_hashes.iter().map(|h| B256::from(*h)).collect();
        L1Blobs {
            blocks: vec![l1.post_foreign(&b.blobs); vhs.len()],
            vhs,
        }
    });
    let (block, voters, overwrites) = (i.block, i.proc.voters_count, i.proc.overwritten_count);
    i.events.push(RegistryEvent {
        block,
        tx_hash: tx,
        log_index: 0,
        kind: EventKind::StateTransitioned {
            pid: pid31(),
            sender: Address::repeat_byte(0xEE),
            old_root: b.old_root,
            new_root: b.new_root,
            voters,
            overwrites,
            n_blobs: b.blobs.blobs.len() as u64,
            l1,
        },
    });
    block
```

Append to `impl Chain for FakeChain`:

```rust
    async fn l1_origin(&self) -> Result<L1Origin, Web3Error> {
        let l1 = self.inner.lock().unwrap().l1.clone();
        l1.map(|l| l.origin())
            .ok_or_else(|| Web3Error::Config("not an L1-blob registry".into()))
    }

    async fn record_l1_origin(&self) -> Result<L1Origin, Web3Error> {
        let mut i = self.inner.lock().unwrap();
        let o = i
            .l1
            .as_ref()
            .map(|l| l.origin())
            .ok_or_else(|| Web3Error::Config("not an L1-blob registry".into()))?;
        i.l1_recorded.insert(o.number, o.hash);
        i.l1_records += 1;
        i.block += 1;
        Ok(o)
    }

    async fn simulate_transition_l1(
        &self,
        _pid: &[u8; 31],
        snark: &PlonkSnark,
        blobs: &TransitionBlobs,
        proof: &L1BlobProof,
    ) -> Result<(), RevertReason> {
        let p = BatchPublics::from_public_values(&snark.public_values)
            .map_err(|e| RevertReason::Rpc(e.to_string()))?;
        let mut i = self.inner.lock().unwrap();
        i.l1_simulates += 1;
        if let Some((name, n)) = &mut i.revert_simulate
            && *n > 0
        {
            *n -= 1;
            return Err(revert(name));
        }
        check_transition(&i, &p)?;
        check_l1_proof(&i, blobs, proof).map(|_| ())
    }

    async fn submit_transition_l1(
        &self,
        pid: &[u8; 31],
        snark: &PlonkSnark,
        blobs: &TransitionBlobs,
        proof: &L1BlobProof,
    ) -> Result<TxReceipt, Web3Error> {
        let Some(sender) = self.signer else {
            return Err(Web3Error::NoSigner);
        };
        let p = BatchPublics::from_public_values(&snark.public_values)
            .map_err(|e| Web3Error::Data(e.to_string()))?;
        let mut i = self.inner.lock().unwrap();
        if i.fail_submits > 0 {
            i.fail_submits -= 1;
            return Err(Web3Error::Rpc("scripted submit failure".into()));
        }
        if let Some(b) = i.race_submit.take() {
            let block = settle_foreign(&mut i, &b);
            if check_transition(&i, &p).is_err() {
                return Err(Web3Error::Lost {
                    tx_hash: B256::repeat_byte(0x10),
                    block,
                });
            }
        }
        check_transition(&i, &p).map_err(Web3Error::Revert)?;
        let blocks = check_l1_proof(&i, blobs, proof).map_err(Web3Error::Revert)?;
        if std::mem::take(&mut i.vanish_l1_submit) {
            return Ok(TxReceipt {
                tx_hash: B256::repeat_byte(0x77),
                block: i.block,
                gas_used: 0,
                replacements: 0,
            });
        }
        let l1 = L1Blobs {
            blocks,
            vhs: blobs.versioned_hashes.iter().map(|h| B256::from(*h)).collect(),
        };
        i.l1_last_headers = proof.headers.len();
        i.l1_last = Some(l1.clone());
        let tx = land(&mut i, pid, sender, &p, blobs.blobs.len() as u64, Some(l1));
        Ok(TxReceipt {
            tx_hash: tx,
            block: i.block,
            gas_used: 0,
            replacements: 0,
        })
    }
```

- [ ] **Step 7: Write the settle tests**

Append to `sequencer/tests/l1da.rs`:

```rust
use davinci_sequencer::l1da::{
    L1Da, L1Job, MAX_PROOF_RETRIES, Posting, Settled, settle,
};

/// FakeChain in L1 mode over a FakeL1, a poster on a fresh database, and a
/// one-vote batch on the chain's root with the snark FakeProver returns.
struct Rig {
    s: TestSetup,
    l1: FakeL1,
    da: L1Da,
    snark: PlonkSnark,
    blobs: TransitionBlobs,
    _dir: TempDir,
}

fn rig() -> Rig {
    let s = setup(2, 8, None);
    let l1 = FakeL1::new();
    s.chain.attach_l1(&l1);
    let (dir, db) = fresh_db();
    let da = L1Da::new(Arc::new(l1.clone()), db, u128::MAX, Duration::from_millis(5)).unwrap();
    let blobs = blobs(9);
    let p = BatchPublics {
        ok: true,
        fail_mask: 0,
        root_before: s.chain.root(),
        root_after: [0x42; 32],
        voters: 1,
        overwrites: 0,
        census_root: [0; 32],
        blobs_digest: blobs.digest,
        n_blobs: blobs.blobs.len() as u32,
        occupied_before: 0,
        nproofs: 1,
        n_public: 3,
        log_n: 0,
    };
    let snark = PlonkSnark {
        program_vk: release::BATCH_PROGRAM_VK,
        root_c_vadcop_final: release::ROOT_C_VADCOP_FINAL,
        public_values: encode_batch(&p),
        proof_bytes: vec![0; 768],
    };
    Rig {
        s,
        l1,
        da,
        snark,
        blobs,
        _dir: dir,
    }
}

impl Rig {
    fn job(&self) -> L1Job<'_> {
        L1Job {
            pid: pid31(),
            root_before: self.s.chain.root(),
            blobs: &self.blobs,
            deadline: soon(),
            cancel: CancellationToken::new(),
        }
    }

    async fn posting(&self, job: &L1Job<'_>) -> Posting {
        Posting {
            post: self
                .da
                .poster
                .post(&job.pid, &job.root_before, job.blobs, job.deadline)
                .await,
            recorded: None,
        }
    }
}

#[tokio::test]
async fn settle_lands_with_the_l1_blocks() {
    let r = rig();
    let job = r.job();
    let posting = r.posting(&job).await;
    let posted = *posting.post.as_ref().unwrap();
    let out = settle(&r.da, &r.s.chain, &job, &r.snark, posting).await;
    assert!(matches!(out, Settled::Landed(_)), "{out:?}");
    assert_eq!(r.s.chain.root(), [0x42; 32]);
    assert_eq!(
        r.s.chain.l1_last().unwrap().blocks,
        vec![posted.block; r.blobs.blobs.len()]
    );
    assert!(r.s.chain.l1_last_headers() <= 3);
    assert_eq!(r.s.chain.l1_records(), 0);
}

#[tokio::test]
async fn settle_gives_up_after_bounded_origin_mismatches() {
    let r = rig();
    r.s.chain.revert_simulates("L1OriginMismatch", 100);
    let job = r.job();
    let posting = r.posting(&job).await;
    let out = settle(&r.da, &r.s.chain, &job, &r.snark, posting).await;
    assert!(
        matches!(&out, Settled::Transient(e) if e.contains("L1OriginMismatch")),
        "{out:?}"
    );
    assert_eq!(r.s.chain.l1_simulates(), u64::from(MAX_PROOF_RETRIES) + 1);
    assert_eq!(r.l1.mined_posts(), 1, "a mismatch never re-posts");
}

#[tokio::test]
async fn a_recorded_origin_anchors_a_deep_proof() {
    let r = rig();
    let job = r.job();
    let posting = r.posting(&job).await;
    let n = posting.post.as_ref().unwrap().block;
    r.l1.pin_origin(Some(n));
    let rec = r.s.chain.record_l1_origin().await.unwrap();
    assert_eq!(rec.number, n);
    r.l1.mine(30);
    r.l1.pin_origin(Some(n + 30));
    let posting = Posting {
        recorded: Some(rec),
        ..posting
    };
    let out = settle(&r.da, &r.s.chain, &job, &r.snark, posting).await;
    assert!(matches!(out, Settled::Landed(_)), "{out:?}");
    assert_eq!(r.s.chain.l1_last_headers(), 1, "headers n..=n, not 33");
    assert_eq!(r.l1.mined_posts(), 1);
}

#[tokio::test]
async fn a_proof_too_deep_without_a_record_reposts() {
    let r = rig();
    let job = r.job();
    let posting = r.posting(&job).await;
    let n = posting.post.as_ref().unwrap().block;
    r.l1.mine(80);
    r.l1.pin_origin(Some(n + 70));
    let driver = async {
        wait_until("re-posted", async || r.l1.mined_posts() == 2).await;
        r.l1.pin_origin(None);
    };
    let (out, ()) = tokio::join!(settle(&r.da, &r.s.chain, &job, &r.snark, posting), driver);
    assert!(matches!(out, Settled::Landed(_)), "{out:?}");
    let second = r.l1.sends().last().unwrap().hash;
    let m = r.l1.block_of(&second).unwrap();
    assert!(m > n + 70);
    assert_eq!(r.s.chain.l1_last().unwrap().blocks[0], m);
}

#[tokio::test]
async fn tx_too_old_reposts() {
    let r = rig();
    r.s.chain.revert_simulates("TxTooOld", 1);
    let job = r.job();
    let posting = r.posting(&job).await;
    let out = settle(&r.da, &r.s.chain, &job, &r.snark, posting).await;
    assert!(matches!(out, Settled::Landed(_)), "{out:?}");
    assert_eq!(r.l1.mined_posts(), 2);
}
```

- [ ] **Step 8: Run the tests**

Run: `cargo test -p davinci-sequencer --test l1da 2>&1 | tail -5`
Expected: PASS (12 tests).
Run: `cargo test -p davinci-sequencer --test actor 2>&1 | tail -3`
Expected: PASS, the same count as before (the `land` refactor keeps blob mode's fake identical).

- [ ] **Step 9: Commit**

```bash
cargo fmt --all && cargo clippy -p davinci-sequencer --all-targets -- -D warnings
git add sequencer/src/l1da sequencer/tests/common/mod.rs sequencer/tests/l1da.rs
git commit -m "feat(l1da): settle loop with origin wait, anchor policy and re-posts"
```

---
### Task 9: Followers read Ethereum blobs

**Files:**
- Modify: `sequencer/src/web3/blobs.rs` (imports L7-23, trait L28-34, `blob_source` L36-42, `AnvilBlobs` impl L131-150, `BeaconBlobs` impl L362-377, tests)
- Modify: `sequencer/tests/common/mod.rs` (`impl BlobSource for FakeBlobs` L1359-1385)

**Interfaces:**
- Consumes: Task 2 `Config::l1_mode`, `Config::l1_rpc_url`; Task 7 `FakeL1::blob`.
- Produces:
  - `BlobSource::blobs_for_l1(&self, blocks: &[u64], vhs: &[B256]) -> Result<Vec<Blob>>` (default: a config error), implemented by `AnvilBlobs` (scans each block's type-3 transactions, then `anvil_getBlobsByTransactionHash`), `BeaconBlobs` (block timestamp from the execution node, then `blobs_at`) and the test `FakeBlobs`.
  - `blob_source(cfg)` reads transactions and blocks from `cfg.l1_rpc_url` in L1-blob mode, `cfg.rpc_url` otherwise.

- [ ] **Step 1: Write the failing tests**

In `sequencer/src/web3/blobs.rs` `mod tests`, extend the axum import to `use axum::{Json, Router, routing::{get, post}};` and append:

```rust
    // An execution node serving headers with timestamp 1000 + 12·number.
    async fn el_stub() -> Url {
        let app = Router::new().route(
            "/",
            post(|Json(req): Json<serde_json::Value>| async move {
                let id = req["id"].clone();
                let hex_n = req["params"][0].as_str().unwrap().trim_start_matches("0x");
                let n = u64::from_str_radix(hex_n, 16).unwrap();
                let header = alloy::rpc::types::Header::new(alloy::consensus::Header {
                    number: n,
                    timestamp: 1000 + 12 * n,
                    ..Default::default()
                });
                let block: alloy::rpc::types::Block = alloy::rpc::types::Block::empty(header);
                Json(json!({"jsonrpc": "2.0", "id": id, "result": block}))
            }),
        );
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        Url::parse(&format!("http://{addr}/")).unwrap()
    }

    #[tokio::test]
    async fn l1_blobs_come_by_block_from_the_beacon() {
        let (blobs, hashes, other) = fixture();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let beacon = serve(Beacon {
            pool: vec![other, blobs[1].clone(), blobs[0].clone()],
            blobs_endpoint: true,
            claims: vec![],
            seen: seen.clone(),
        })
        .await;
        let src = BeaconBlobs::new(vec![beacon], &[el_stub().await]);
        assert_eq!(src.blobs_for_l1(&[42, 42], &hashes).await.unwrap(), blobs);
        let s = seen.lock().unwrap().clone();
        assert_eq!(s.len(), 1, "one block, one request: {s:?}");
        assert!(s[0].starts_with("blobs/42?"), "{}", s[0]);
        // Blobs over two blocks come back in hash order.
        assert_eq!(src.blobs_for_l1(&[43, 42], &hashes).await.unwrap(), blobs);
    }

    #[tokio::test]
    async fn l1_blobs_fail_over_to_a_full_blob_beacon() {
        let (blobs, hashes, _) = fixture();
        // The first beacon keeps no blobs for the slot (not a supernode).
        let (pruned, _) = broken(StatusCode::NOT_FOUND).await;
        let full = serve(Beacon {
            pool: blobs.clone(),
            blobs_endpoint: true,
            claims: vec![],
            seen: Arc::new(Mutex::new(Vec::new())),
        })
        .await;
        let src = BeaconBlobs::new(vec![pruned, full], &[el_stub().await]);
        assert_eq!(src.blobs_for_l1(&[42, 42], &hashes).await.unwrap(), blobs);
    }

    #[tokio::test]
    async fn l1_blobs_must_match_their_hashes() {
        let (blobs, hashes, other) = fixture();
        let beacon = serve(Beacon {
            pool: vec![blobs[0].clone(), other],
            blobs_endpoint: true,
            claims: vec![],
            seen: Arc::new(Mutex::new(Vec::new())),
        })
        .await;
        let src = BeaconBlobs::new(vec![beacon], &[el_stub().await]);
        assert!(src.blobs_for_l1(&[42, 42], &hashes).await.is_err());
        // Malformed events: no blob, or blocks and hashes of other lengths.
        assert!(src.blobs_for_l1(&[], &[]).await.is_err());
        assert!(src.blobs_for_l1(&[42], &hashes).await.is_err());
        let anvil = AnvilBlobs::new(&[Url::parse("http://127.0.0.1:1/").unwrap()]);
        assert!(anvil.blobs_for_l1(&[42], &hashes).await.is_err());
    }
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -p davinci-sequencer --lib l1_blobs 2>&1 | tail -5`
Expected: FAIL to compile (no method `blobs_for_l1`).

- [ ] **Step 3: Implement**

In `sequencer/src/web3/blobs.rs`, add `use std::collections::HashMap;` and `use alloy::network::TransactionResponse as _;`. Extend the module doc with: "In L1-blob mode the event names Ethereum blocks and versioned hashes instead, and [`BlobSource::blobs_for_l1`] reads them from Ethereum." Add to the trait:

```rust
    /// L1-blob mode: the blobs with versioned hashes `vhs`, in that order,
    /// from the Ethereum blocks `blocks` (one per hash, from
    /// `ProcessStateTransitionedL1`), each checked against its hash.
    async fn blobs_for_l1(&self, blocks: &[u64], vhs: &[B256]) -> Result<Vec<Blob>> {
        let _ = (blocks, vhs);
        Err(Web3Error::Config(
            "this blob source cannot read Ethereum blobs".into(),
        ))
    }
```

Replace `blob_source`:

```rust
/// The source the configuration names. It reads transactions and blocks from
/// `cfg.rpc_url`, or from `cfg.l1_rpc_url` in L1-blob mode, where the blobs
/// live on Ethereum.
pub fn blob_source(cfg: &Config) -> Arc<dyn BlobSource> {
    let rpcs = if cfg.l1_mode() {
        &cfg.l1_rpc_url
    } else {
        &cfg.rpc_url
    };
    match &cfg.blob_source {
        BlobSourceKind::Anvil => Arc::new(AnvilBlobs::new(rpcs)),
        BlobSourceKind::Beacon(urls) => Arc::new(BeaconBlobs::new(urls.clone(), rpcs)),
    }
}

// The distinct blocks of an L1 event, in first-seen order; a malformed event
// is an error.
fn l1_blocks(blocks: &[u64], vhs: &[B256]) -> Result<Vec<u64>> {
    if vhs.is_empty() || blocks.len() != vhs.len() {
        return Err(Web3Error::Blob(format!(
            "{} L1 blocks for {} blobs",
            blocks.len(),
            vhs.len()
        )));
    }
    let mut out: Vec<u64> = Vec::new();
    for b in blocks {
        if !out.contains(b) {
            out.push(*b);
        }
    }
    Ok(out)
}
```

Append to `impl BlobSource for AnvilBlobs`:

```rust
    async fn blobs_for_l1(&self, blocks: &[u64], vhs: &[B256]) -> Result<Vec<Blob>> {
        let mut candidates = Vec::new();
        for b in l1_blocks(blocks, vhs)? {
            let blk = self
                .provider
                .get_block_by_number(BlockNumberOrTag::Number(b))
                .full()
                .await
                .map_err(rpc_err)?
                .ok_or_else(|| Web3Error::Blob(format!("L1 block {b} not found")))?;
            for t in blk.transactions.txns() {
                let carries = t
                    .inner
                    .blob_versioned_hashes()
                    .is_some_and(|hs| hs.iter().any(|h| vhs.contains(h)));
                if !carries {
                    continue;
                }
                // Heap `Bytes`, as in `blobs_for_tx`.
                let raw: Option<Vec<alloy::primitives::Bytes>> = self
                    .provider
                    .raw_request("anvil_getBlobsByTransactionHash".into(), (t.tx_hash(),))
                    .await
                    .map_err(rpc_err)?;
                for r in raw.unwrap_or_default() {
                    candidates.push(to_blob(r.as_ref())?);
                }
            }
        }
        match_blobs_blocking(vhs.to_vec(), candidates).await
    }
```

Append to `impl BlobSource for BeaconBlobs`:

```rust
    async fn blobs_for_l1(&self, blocks: &[u64], vhs: &[B256]) -> Result<Vec<Blob>> {
        let distinct = l1_blocks(blocks, vhs)?;
        let p = self
            .provider
            .as_ref()
            .ok_or_else(|| Web3Error::Config("beacon source without an execution node".into()))?;
        let mut found: HashMap<B256, Blob> = HashMap::new();
        for b in distinct {
            let want: Vec<B256> = vhs
                .iter()
                .zip(blocks)
                .filter(|(_, x)| **x == b)
                .map(|(h, _)| *h)
                .collect();
            let blk = p
                .get_block_by_number(BlockNumberOrTag::Number(b))
                .await
                .map_err(rpc_err)?
                .ok_or_else(|| Web3Error::Blob(format!("L1 block {b} not found")))?;
            let got = self.blobs_at(blk.header.timestamp, &want).await?;
            found.extend(want.into_iter().zip(got));
        }
        vhs.iter()
            .map(|h| {
                found
                    .get(h)
                    .cloned()
                    .ok_or_else(|| Web3Error::Blob(format!("no blob for {h}")))
            })
            .collect()
    }
```

In `sequencer/tests/common/mod.rs`, append to `impl BlobSource for FakeBlobs`:

```rust
    async fn blobs_for_l1(&self, blocks: &[u64], vhs: &[B256]) -> Result<Vec<Blob>, Web3Error> {
        let l1 = {
            let mut i = self.0.lock().unwrap();
            if i.fail_blobs > 0 {
                i.fail_blobs -= 1;
                return Err(Web3Error::Blob("scripted blob failure".into()));
            }
            i.l1.clone()
        };
        let l1 = l1.ok_or_else(|| Web3Error::Blob("no L1 attached".into()))?;
        if vhs.is_empty() || blocks.len() != vhs.len() {
            return Err(Web3Error::Blob("malformed L1 blob list".into()));
        }
        vhs.iter()
            .map(|h| l1.blob(h).ok_or_else(|| Web3Error::Blob(format!("no L1 blob {h}"))))
            .collect()
    }
```

- [ ] **Step 4: Run the tests**

Run: `cargo test -p davinci-sequencer --lib blobs 2>&1 | tail -5`
Expected: PASS (three new tests and the existing blob-source tests).
Run: `cargo test -p davinci-sequencer --test stack 2>&1 | tail -3`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
cargo fmt --all && cargo clippy -p davinci-sequencer --all-targets -- -D warnings
git add sequencer/src/web3/blobs.rs sequencer/tests/common/mod.rs
git commit -m "feat(web3): fetch l1-blob mode blobs from ethereum beacons or anvil"
```

---
### Task 10: The actor in L1-blob mode

**Files:**
- Modify: `sequencer/src/actor.rs` (imports L35-38, `Deps` L177-188, constants near `LANDED_EVENT_WAIT` L688, `Flight` L702-711, `maybe_seal` job creation L1771-1795, `on_job_done` L2116-2123, `on_event` L2408-2417, `on_transition` L2447-2500, `apply_sync` L2522-2541, `replay_gap` L2603-2631, `JobInput`/`run_job`/`job_inner` L2788-2872)
- Modify: `sequencer/src/api/mod.rs` (`Deps` literal L206-214: `l1: None` for now)
- Modify: `sequencer/tests/common/mod.rs` (`start_node_tracked` L1716-1734, new `start_l1_node`)
- Create: `sequencer/tests/actor_l1.rs`

**Interfaces:**
- Consumes: Task 8 `L1Job`, `with_post`, `settle`, `Settled`, FakeChain L1 mode; Task 9 `blobs_for_l1`; Task 4 `L1Blobs`; Task 7 `L1Da`, `FakeL1`.
- Produces:
  - `Deps.l1: Option<Arc<crate::l1da::L1Da>>` (`None` = blob mode)
  - In L1 mode a landed settlement commits at its confirmed `StateTransitioned` event; one that never shows within `LANDED_EVENT_WAIT` (90 s) is rolled back and requeued.
  - Test helper `start_l1_node(db, datadir, &TestSetup, FakeChain, FakeL1, Arc<FakeProver>, batch_max, CancellationToken) -> Node`.

- [ ] **Step 1: Write the failing actor tests**

Add to `sequencer/tests/common/mod.rs`, after `start_node_tracked`:

```rust
/// [`start_node`] in L1-blob mode: `chain` reads OP's origin from `l1`, which
/// also takes the node's blob posts (no fee cap).
#[allow(clippy::too_many_arguments)]
pub async fn start_l1_node(
    db: Db,
    datadir: &Path,
    s: &TestSetup,
    chain: FakeChain,
    l1: FakeL1,
    prover: Arc<FakeProver>,
    batch_max: usize,
    shutdown: CancellationToken,
) -> Node {
    chain.attach_l1(&l1);
    let cfg = test_config(datadir, &s.census_dir, batch_max, "0s");
    let da = davinci_sequencer::l1da::L1Da::new(
        Arc::new(l1),
        db.clone(),
        u128::MAX,
        cfg.poll_interval,
    )
    .unwrap();
    let deps = Deps {
        db,
        contracts: Arc::new(chain.clone()),
        prover,
        blobs: Arc::new(FakeBlobs(chain.inner.clone())),
        clock: Arc::new(FakeClock(chain.inner.clone())),
        grace: chain.grace_params(),
        tasks: tokio_util::task::TaskTracker::new(),
        l1: Some(Arc::new(da)),
    };
    spawn_node(cfg, deps, shutdown).await.unwrap()
}
```

and add `l1: None,` to the `Deps` literal in `start_node_tracked`.

Create `sequencer/tests/actor_l1.rs`:

```rust
//! The actor in L1-blob mode on the scripted fakes: blobs posted on a
//! `FakeL1` while proving, settlement through `FakeChain`'s L1 mode, sync
//! from Ethereum blobs.

mod common;

use common::*;

async fn submit(
    h: &ActorHandle,
    s: &TestSetup,
    voters: std::ops::Range<usize>,
    kseed: u64,
) -> Vec<u64> {
    let mut vids = Vec::new();
    for i in voters {
        let v = fake_vote(&s.env, i, &[1, 2], kseed + i as u64);
        vids.push(v.pkg.vote_id);
        h.submit(v).await.unwrap();
    }
    vids
}

// `wait_until` with a longer (virtual) horizon.
async fn wait_long(what: &str, mut f: impl AsyncFnMut() -> bool) {
    for _ in 0..600 {
        if f().await {
            return;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    panic!("timeout waiting for {what}");
}

async fn l1_node(
    s: &TestSetup,
    l1: &FakeL1,
    prover: Arc<FakeProver>,
    dir: &TempDir,
    shutdown: &CancellationToken,
) -> (Node, ActorHandle) {
    let node = start_l1_node(
        Db::open_in(dir.path()).unwrap(),
        dir.path(),
        s,
        s.chain.clone(),
        l1.clone(),
        prover,
        3,
        shutdown.clone(),
    )
    .await;
    let h = handle(&node).await;
    (node, h)
}

#[tokio::test]
async fn l1_happy_path_posts_proves_and_settles() {
    let s = setup(2, 8, None);
    let l1 = FakeL1::new();
    let dir = TempDir::new().unwrap();
    let shutdown = CancellationToken::new();
    let (node, h) = l1_node(&s, &l1, FakeProver::open(), &dir, &shutdown).await;
    let vids = submit(&h, &s, 0..3, 100).await;
    wait_until("settled", async || all_settled(&h, &vids).await).await;
    assert_eq!(s.chain.voters(), 3);
    assert_eq!(l1.mined_posts(), 1);
    assert_eq!(s.chain.l1_records(), 0, "the proof was ready first");
    assert!(s.chain.l1_last_headers() <= 3);
    assert_eq!(node.metrics.settled_by_self.load(Ordering::SeqCst), 1);
    let ts = node.db.transitions(&pid_fr()).unwrap();
    assert_eq!(ts.len(), 1);
    assert!(ts[0].by_self);
    assert_eq!(
        node.db.blobs(&pid_fr(), 0).unwrap().len() as u64,
        ts[0].n_blobs
    );
    assert!(node.db.l1_posts().unwrap().is_empty());
    shutdown.cancel();
}

#[tokio::test]
async fn l1_origin_lag_records_once_and_anchors_at_the_record() {
    let s = setup(2, 8, None);
    let l1 = FakeL1::new();
    l1.pin_origin(Some(0));
    let dir = TempDir::new().unwrap();
    let shutdown = CancellationToken::new();
    let prover = FakeProver::gated();
    let (_node, h) = l1_node(&s, &l1, prover.clone(), &dir, &shutdown).await;
    let vids = submit(&h, &s, 0..3, 200).await;
    wait_until("blobs posted", async || l1.mined_posts() == 1).await;
    let n = l1.block_of(&l1.sends()[0].hash).unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(s.chain.l1_records(), 0, "OP's origin is not at the blobs yet");
    l1.pin_origin(Some(n));
    wait_until("origin recorded", async || s.chain.l1_records() == 1).await;
    // The origin runs 30 blocks ahead before the proof is ready.
    l1.mine(30);
    l1.pin_origin(Some(n + 30));
    prover.release(10);
    wait_until("settled", async || all_settled(&h, &vids).await).await;
    assert_eq!(s.chain.l1_records(), 1, "recorded once");
    assert_eq!(s.chain.l1_last_headers(), 1, "anchored at the record, not 33 headers");
    shutdown.cancel();
}

#[tokio::test]
async fn l1_origin_mismatch_is_rebuilt_not_reposted() {
    let s = setup(2, 8, None);
    let l1 = FakeL1::new();
    s.chain.revert_simulates("L1OriginMismatch", 2);
    let dir = TempDir::new().unwrap();
    let shutdown = CancellationToken::new();
    let (_node, h) = l1_node(&s, &l1, FakeProver::open(), &dir, &shutdown).await;
    let vids = submit(&h, &s, 0..3, 300).await;
    wait_until("settled", async || all_settled(&h, &vids).await).await;
    assert!(s.chain.l1_simulates() >= 3);
    assert_eq!(l1.mined_posts(), 1);
    shutdown.cancel();
}

#[tokio::test]
async fn l1_tx_too_old_posts_again() {
    let s = setup(2, 8, None);
    let l1 = FakeL1::new();
    s.chain.revert_simulates("TxTooOld", 1);
    let dir = TempDir::new().unwrap();
    let shutdown = CancellationToken::new();
    let (_node, h) = l1_node(&s, &l1, FakeProver::open(), &dir, &shutdown).await;
    let vids = submit(&h, &s, 0..3, 400).await;
    wait_until("settled", async || all_settled(&h, &vids).await).await;
    assert_eq!(l1.mined_posts(), 2);
    let second = l1.sends().last().unwrap().hash;
    assert_eq!(
        s.chain.l1_last().unwrap().blocks[0],
        l1.block_of(&second).unwrap()
    );
    shutdown.cancel();
}

#[tokio::test]
async fn l1_dropped_post_is_replaced_after_three_blocks() {
    let s = setup(2, 8, None);
    let l1 = FakeL1::new();
    l1.drop_next(1);
    let dir = TempDir::new().unwrap();
    let shutdown = CancellationToken::new();
    let (_node, h) = l1_node(&s, &l1, FakeProver::open(), &dir, &shutdown).await;
    let vids = submit(&h, &s, 0..3, 500).await;
    wait_until("settled", async || all_settled(&h, &vids).await).await;
    let sends = l1.sends();
    assert_eq!(sends.len(), 2);
    assert_eq!(sends[0].nonce, sends[1].nonce);
    assert_eq!(sends[1].fees, sends[0].fees.doubled());
    assert_eq!(l1.mined_posts(), 1);
    shutdown.cancel();
}

#[tokio::test]
async fn l1_reorged_post_is_posted_again() {
    let s = setup(2, 8, None);
    let l1 = FakeL1::new();
    let dir = TempDir::new().unwrap();
    let shutdown = CancellationToken::new();
    let prover = FakeProver::gated();
    let (_node, h) = l1_node(&s, &l1, prover.clone(), &dir, &shutdown).await;
    let vids = submit(&h, &s, 0..3, 600).await;
    wait_until("blobs posted", async || l1.mined_posts() == 1).await;
    let first = l1.sends()[0].clone();
    // An Ethereum reorg drops the block with the blob transaction.
    l1.reorg(1);
    assert_eq!(l1.mined_posts(), 0);
    prover.release(10);
    wait_until("settled", async || all_settled(&h, &vids).await).await;
    let sends = l1.sends();
    assert_eq!(sends.len(), 2);
    assert_eq!(sends[1].nonce, first.nonce, "the freed nonce");
    assert_eq!(sends[1].fees, first.fees.doubled(), "priced over the reorged tx");
    assert_eq!(l1.mined_posts(), 1);
    shutdown.cancel();
}

#[tokio::test]
async fn l1_lost_race_syncs_from_ethereum_and_settles_next() {
    let s = setup(2, 8, None);
    let l1 = FakeL1::new();
    let (dir_a, dir_b) = (TempDir::new().unwrap(), TempDir::new().unwrap());
    let shutdown = CancellationToken::new();
    let (prover_a, prover_b) = (FakeProver::gated(), FakeProver::gated());
    let node_a = start_l1_node(
        Db::open_in(dir_a.path()).unwrap(),
        dir_a.path(),
        &s,
        s.chain.with_signer(0xa1),
        l1.clone(),
        prover_a.clone(),
        2,
        shutdown.clone(),
    )
    .await;
    let node_b = start_l1_node(
        Db::open_in(dir_b.path()).unwrap(),
        dir_b.path(),
        &s,
        s.chain.with_signer(0xb2),
        l1.with_sender(0x2b),
        prover_b.clone(),
        2,
        shutdown.clone(),
    )
    .await;
    let (ha, hb) = (handle(&node_a).await, handle(&node_b).await);
    let vids_a = submit(&ha, &s, 0..2, 10).await;
    let vids_b = submit(&hb, &s, 2..4, 20).await;
    wait_until("both proving", async || {
        prover_a.calls() >= 1 && prover_b.calls() >= 1
    })
    .await;
    prover_a.release(100);
    wait_until("A settled", async || s.chain.voters() == 2).await;
    prover_b.release(100);
    wait_until("B settled", async || {
        all_settled(&hb, &vids_b).await && s.chain.voters() == 4
    })
    .await;
    assert_eq!(node_b.metrics.lost_races.load(Ordering::SeqCst), 1);
    assert_eq!(node_b.metrics.synced_from_others.load(Ordering::SeqCst), 1);
    let b_posts = l1
        .sends()
        .iter()
        .filter(|x| x.from == Address::repeat_byte(0x2b))
        .count();
    assert!(b_posts >= 2, "the losing post is wasted: {b_posts}");
    wait_until("A synced B's transition from Ethereum", async || {
        ha.snapshot().await.unwrap().root == s.chain.root()
    })
    .await;
    assert!(all_settled(&ha, &vids_a).await);
    shutdown.cancel();
}

#[tokio::test]
async fn l1_restart_replaces_the_pending_post() {
    let s = setup(2, 8, None);
    let l1 = FakeL1::new();
    // No blocks: the first run's post stays pending.
    l1.set_auto_mine(false);
    let dir = TempDir::new().unwrap();
    let shutdown = CancellationToken::new();
    let (node, h) = l1_node(&s, &l1, FakeProver::open(), &dir, &shutdown).await;
    let vids = submit(&h, &s, 0..3, 700).await;
    wait_until("post recorded", async || {
        node.db.l1_posts().unwrap().len() == 1
    })
    .await;
    let rec = node.db.l1_posts().unwrap().remove(0);
    assert_eq!(l1.mined_posts(), 0);
    shutdown.cancel();
    drop(h);
    drop(node);

    let db = reopen_db(dir.path()).await;
    l1.set_auto_mine(true);
    let shutdown2 = CancellationToken::new();
    let node2 = start_l1_node(
        db,
        dir.path(),
        &s,
        s.chain.clone(),
        l1.clone(),
        FakeProver::open(),
        3,
        shutdown2.clone(),
    )
    .await;
    let h2 = handle(&node2).await;
    // Re-proved (the batch secrets died with the first run) and re-posted
    // over the pending transaction, at its nonce.
    wait_until("recovered votes settled", async || {
        all_settled(&h2, &vids).await
    })
    .await;
    let last = l1.sends().pop().unwrap();
    assert_eq!(last.nonce, rec.nonce);
    assert!(last.fees.max_fee >= 2 * rec.fees.max_fee);
    assert!(last.fees.tip >= 2 * rec.fees.tip);
    assert!(last.fees.blob_fee >= 2 * rec.fees.blob_fee);
    assert_eq!(l1.mined_posts(), 1);
    assert!(node2.db.l1_posts().unwrap().is_empty());
    shutdown2.cancel();
}

#[tokio::test(start_paused = true)]
async fn l1_settlement_that_never_shows_is_rolled_back() {
    let s = setup(2, 8, None);
    let l1 = FakeL1::new();
    s.chain.vanish_next_l1_submit();
    let dir = TempDir::new().unwrap();
    let shutdown = CancellationToken::new();
    let (node, h) = l1_node(&s, &l1, FakeProver::open(), &dir, &shutdown).await;
    let vids = submit(&h, &s, 0..3, 800).await;
    // The receipt came back but no event ever does: after the landing wait
    // the votes go back to the queue, are proved again and settle.
    wait_long("settled after the rollback", async || {
        all_settled(&h, &vids).await
    })
    .await;
    assert_eq!(l1.mined_posts(), 2);
    assert_eq!(s.chain.voters(), 3);
    assert_eq!(node.metrics.settled_by_self.load(Ordering::SeqCst), 1);
    shutdown.cancel();
}
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -p davinci-sequencer --test actor_l1 2>&1 | tail -5`
Expected: FAIL to compile (`Deps` has no field `l1`).

- [ ] **Step 3: Wire the actor**

In `sequencer/src/actor.rs`:

Add `L1Blobs` to the `crate::web3::{...}` import. Add the field at the end of `struct Deps`:

```rust
    /// L1-blob mode: the Ethereum side (poster, proofs); `None` in blob mode.
    pub l1: Option<Arc<crate::l1da::L1Da>>,
```

Add after `LANDED_EVENT_WAIT`:

```rust
/// Longest an L1-blob job waits for Ethereum and OP's origin when the grace
/// end is far or unbounded.
const L1_JOB_CAP: std::time::Duration = std::time::Duration::from_secs(30 * 24 * 3600);
```

Add the field at the end of `struct Flight`:

```rust
    /// Cancels the job when the flight is dropped (rollback, requeue,
    /// shutdown), so an L1-blob job stops waiting on Ethereum.
    _abort: tokio_util::sync::DropGuard,
```

In `maybe_seal`, replace

```rust
        let job = JobInput {
            generation: self.generation,
            pid31: self.pid31,
            request: prepared.request.clone(),
            expected: prepared.expected,
            blobs: prepared.blobs.clone(),
            prover: self.deps.prover.clone(),
            chain: self.deps.contracts.clone(),
            tx: self.tx.clone(),
            clock: self.deps.clock.clone(),
            metrics: self.metrics.clone(),
            nf,
            n: vids.len(),
            prove_base: self.prove_base,
        };
        self.in_flight = Some(Flight {
            generation: self.generation,
            prepared,
            votes: selected,
            votes_at,
            landed_deadline: None,
        });
```

with

```rust
        let cancel = CancellationToken::new();
        // The grace end known now; it only moves later.
        let left = std::time::Duration::from_secs(self.grace_end().saturating_sub(now));
        let job = JobInput {
            generation: self.generation,
            pid31: self.pid31,
            request: prepared.request.clone(),
            expected: prepared.expected,
            blobs: prepared.blobs.clone(),
            prover: self.deps.prover.clone(),
            chain: self.deps.contracts.clone(),
            tx: self.tx.clone(),
            clock: self.deps.clock.clone(),
            metrics: self.metrics.clone(),
            nf,
            n: vids.len(),
            prove_base: self.prove_base,
            l1: self.deps.l1.clone(),
            deadline: tokio::time::Instant::now() + left.min(L1_JOB_CAP),
            cancel: cancel.clone(),
        };
        self.in_flight = Some(Flight {
            generation: self.generation,
            prepared,
            votes: selected,
            votes_at,
            landed_deadline: None,
            _abort: cancel.drop_guard(),
        });
```

In `on_job_done`, replace `            JobOutcome::Landed(rc) => self.commit_flight(f, rc.tx_hash.0, rc.block),` with

```rust
            JobOutcome::Landed(rc) if self.deps.l1.is_some() => {
                // L1-blob mode: OP's unsafe blocks reorg with Ethereum, so the
                // settlement commits at its confirmed event; one that never
                // shows is rolled back by tick() and requeued.
                info!(pid = %hex::encode(self.pid31), tx = %rc.tx_hash, block = rc.block,
                    "settlement mined; committing at its event");
                f.landed_deadline = Some(tokio::time::Instant::now() + LANDED_EVENT_WAIT);
                self.in_flight = Some(f);
            }
            JobOutcome::Landed(rc) => self.commit_flight(f, rc.tx_hash.0, rc.block),
```

In `on_event`, replace the `StateTransitioned` arm with

```rust
            EventKind::StateTransitioned {
                sender,
                old_root,
                new_root,
                n_blobs,
                l1,
                ..
            } => {
                self.on_transition(ev.block, ev.tx_hash.0, sender, old_root, new_root, n_blobs, l1)
                    .await;
            }
```

Give `on_transition` and `apply_sync` the event's `l1` as their last parameter, each with `#[allow(clippy::too_many_arguments)] // the event's fields, as routed`:

```rust
    #[allow(clippy::too_many_arguments)] // the event's fields, as routed
    async fn on_transition(
        &mut self,
        block: u64,
        tx_hash: [u8; 32],
        sender: Address,
        old_root: [u8; 32],
        new_root: [u8; 32],
        n_blobs: u64,
        l1: Option<L1Blobs>,
    ) {
```

with its sync call

```rust
        if old_root == committed {
            self.apply_sync(block, tx_hash, sender, old_root, new_root, n_blobs, l1)
                .await;
            return;
        }
```

and in `apply_sync` replace the signature and the blob fetch (down to the `let blobs = match ... {` header) with

```rust
    #[allow(clippy::too_many_arguments)] // the event's fields, as routed
    async fn apply_sync(
        &mut self,
        block: u64,
        tx_hash: [u8; 32],
        sender: Address,
        old_root: [u8; 32],
        new_root: [u8; 32],
        n_blobs: u64,
        l1: Option<L1Blobs>,
    ) -> bool {
        // Only the first n_blobs blobs: the count the registry checked. An
        // appended junk blob would otherwise fail every decode forever. In
        // L1-blob mode the event names the Ethereum blobs themselves.
        let fetched = match &l1 {
            None => {
                self.deps
                    .blobs
                    .blobs_for_tx(alloy::primitives::B256::from(tx_hash), block, n_blobs)
                    .await
            }
            Some(l) => self.deps.blobs.blobs_for_l1(&l.blocks, &l.vhs).await,
        };
        let blobs = match fetched {
```

(the `Ok(b) => b, Err(e) => { ... "blob fetch failed" ... }` arms stay). In `replay_gap`, add `l1,` to the `StateTransitioned { .. }` pattern and pass it: `.apply_sync(ev.block, ev.tx_hash.0, sender, old_root, new_root, n_blobs, l1)`.

Replace `struct JobInput` and `job_inner` with:

```rust
struct JobInput {
    generation: u64,
    pid31: [u8; 31],
    request: ProveRequest,
    expected: BatchPublics,
    blobs: TransitionBlobs,
    prover: Arc<dyn Prover>,
    chain: Arc<dyn Chain>,
    tx: mpsc::Sender<Msg>,
    clock: Arc<dyn Clock>,
    metrics: Arc<Metrics>,
    nf: u8,
    n: usize,
    prove_base: f64,
    /// L1-blob mode: the Ethereum side; `None` in blob mode.
    l1: Option<Arc<crate::l1da::L1Da>>,
    /// L1-blob mode: no settlement after this.
    deadline: tokio::time::Instant,
    /// Fired when the actor drops the flight.
    cancel: CancellationToken,
}
```

```rust
async fn job_inner(job: JobInput) -> JobOutcome {
    let Some(l1) = job.l1.clone() else {
        let snark = match proved(&job).await {
            Ok(s) => s,
            Err(o) => return o,
        };
        return settle_blobs(&job, &snark).await;
    };
    // L1-blob mode: the blobs go to Ethereum while the batch is proved; the
    // flight's end (rollback, shutdown) cancels the job.
    let lj = crate::l1da::L1Job {
        pid: job.pid31,
        root_before: job.expected.root_before,
        blobs: &job.blobs,
        deadline: job.deadline,
        cancel: job.cancel.clone(),
    };
    let work = async {
        let (proved, posting) =
            crate::l1da::with_post(&l1, &*job.chain, &lj, proved(&job)).await;
        let snark = match proved {
            Ok(s) => s,
            Err(o) => return o,
        };
        crate::l1da::settle(&l1, &*job.chain, &lj, &snark, posting)
            .await
            .into()
    };
    tokio::select! {
        _ = job.cancel.cancelled() => JobOutcome::Transient("flight aborted".into()),
        o = work => o,
    }
}

/// Proves the batch and checks the proof against the prepared batch and the
/// release pins.
async fn proved(job: &JobInput) -> Result<PlonkSnark, JobOutcome> {
    let t0 = job.clock.now();
    let (got, snark) = match job.prover.prove_batch(&job.request, &job.expected).await {
        Ok((got, snark, proving)) => {
            // Wall time counts queue wait: only when the prover reports none.
            let secs = proving.unwrap_or_else(|| job.clock.now().saturating_sub(t0) as f64);
            job.metrics
                .prove
                .observe(job.nf, job.n, secs, job.prove_base);
            (got, snark)
        }
        // Transport/queue trouble is retried; a failed job never is.
        Err(e) if !e.permanent => return Err(JobOutcome::Transient(e.to_string())),
        Err(e) => return Err(JobOutcome::Failed(e.to_string())),
    };
    if !got.ok || got.fail_mask != 0 {
        return Err(JobOutcome::Guest(got.fail_mask));
    }
    if got != job.expected {
        return Err(JobOutcome::Failed(
            "proved publics do not match the prepared batch".into(),
        ));
    }
    match BatchPublics::from_public_values(&snark.public_values) {
        Ok(pv) if pv == job.expected => {}
        _ => {
            return Err(JobOutcome::Failed(
                "snark public values do not match the batch".into(),
            ));
        }
    }
    if snark.program_vk != release::BATCH_PROGRAM_VK {
        return Err(JobOutcome::Failed(
            "program vk is not the pinned batch circuit vk".into(),
        ));
    }
    if snark.root_c_vadcop_final != release::ROOT_C_VADCOP_FINAL {
        return Err(JobOutcome::Failed(
            "root_c_vadcop_final is not the pinned setup".into(),
        ));
    }
    let _ = job
        .tx
        .send(Msg::JobProved {
            generation: job.generation,
        })
        .await;
    Ok(snark)
}

/// Blob mode: the pre-flight `eth_call`, then the blob transaction.
async fn settle_blobs(job: &JobInput, snark: &PlonkSnark) -> JobOutcome {
    if let Err(r) = job
        .chain
        .simulate_transition(&job.pid31, snark, &job.blobs)
        .await
    {
        return match r {
            RevertReason::Revert { name, .. } => JobOutcome::Reverted(name),
            RevertReason::Rpc(e) => JobOutcome::Transient(e),
        };
    }
    match job
        .chain
        .submit_transition(&job.pid31, snark, &job.blobs)
        .await
    {
        Ok(rc) => JobOutcome::Landed(rc),
        Err(Web3Error::Revert(r)) => JobOutcome::Reverted(r.name().unwrap_or("revert").to_string()),
        Err(e @ Web3Error::Lost { .. }) => JobOutcome::Lost(e.to_string()),
        Err(e) => JobOutcome::Transient(e.to_string()),
    }
}

impl From<crate::l1da::Settled> for JobOutcome {
    fn from(s: crate::l1da::Settled) -> Self {
        use crate::l1da::Settled as S;
        match s {
            S::Landed(rc) => JobOutcome::Landed(rc),
            S::Reverted(name) => JobOutcome::Reverted(name),
            S::Lost(e) => JobOutcome::Lost(e),
            S::Transient(e) => JobOutcome::Transient(e),
            S::Failed(e) => JobOutcome::Failed(e),
        }
    }
}
```

`proved` and `settle_blobs` keep blob mode's exact sequence (prove, the four checks, `JobProved`, simulate, submit) and its outcome mapping. In `sequencer/src/api/mod.rs`, add `l1: None,` to the `Deps` literal (Task 11 sets it).

- [ ] **Step 4: Run the tests**

Run: `cargo test -p davinci-sequencer --test actor_l1 2>&1 | tail -5`
Expected: PASS (9 tests).
Run: `cargo test -p davinci-sequencer 2>&1 | grep -E "^test result|FAILED|panicked" | head -20`
Expected: every `test result: ok`, the same counts as before this plan for `actor`, `api`, `recovery`, `storage`, `stack` (blob mode unchanged).

- [ ] **Step 5: Commit**

```bash
cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings
git add sequencer/src/actor.rs sequencer/src/api/mod.rs sequencer/tests/common/mod.rs sequencer/tests/actor_l1.rs
git commit -m "feat(actor): post blobs on ethereum while proving and settle with l1 proofs"
```

---

### Task 11: Boot in L1-blob mode

**Files:**
- Modify: `sequencer/src/api/mod.rs` (`start` L168-219)

**Interfaces:**
- Consumes: Task 3 `Contracts::da_mode`; Task 2 `Config::{l1_mode, check_l1, l1_rpc_url, l1_send_rpc_url, l1_privkey, l1_max_post_fee}`; Task 5 `L1Rpc`; Task 7 `L1Da`; Task 8 `l1da::mode_error`; Task 9 `blob_source`.
- Produces: a node that reads the registry's mode at boot, refuses a configuration that disagrees, refuses a signing node without an Ethereum key, checks the L1 chain id and the verifier's `L1Block` against the preset, sizes batches with the L1 chain's blob cap, and passes `Deps.l1`.

- [ ] **Step 1: Write the failing boot test**

Append to `mod tests` in `sequencer/src/api/mod.rs`:

```rust
    // The registry's mode and `--l1-rpc-url` must agree, both ways.
    #[test]
    fn a_registry_mode_mismatch_stops_the_boot() {
        use crate::web3::RegistryDa;
        let l1 = RegistryDa::L1Blobs {
            verifier: alloy::primitives::Address::repeat_byte(1),
            l1_block: alloy::primitives::Address::repeat_byte(2),
        };
        let blob_cfg = Config::parse_args([
            "davinci-sequencer", "--network", "custom", "--rpc-url", "http://a:8545",
            "--registry", "0x0000000000000000000000000000000000000001", "--blob-source", "anvil",
        ])
        .unwrap();
        let l1_cfg = Config::parse_args([
            "davinci-sequencer", "--network", "custom", "--rpc-url", "http://a:8545",
            "--registry", "0x0000000000000000000000000000000000000001", "--blob-source", "anvil",
            "--l1-rpc-url", "http://l1:8545",
        ])
        .unwrap();
        assert!(check_da(&RegistryDa::Blobs, &blob_cfg).is_ok());
        assert!(check_da(&l1, &l1_cfg).is_ok());
        let e = check_da(&l1, &blob_cfg).unwrap_err().to_string();
        assert!(e.contains("--l1-rpc-url"), "{e}");
        assert!(check_da(&RegistryDa::Blobs, &l1_cfg).is_err());
    }
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test -p davinci-sequencer --lib a_registry_mode_mismatch 2>&1 | tail -5`
Expected: FAIL to compile (`check_da` not found).

- [ ] **Step 3: Implement**

In `sequencer/src/api/mod.rs`, extend the imports:

```rust
use crate::l1da::L1Da;
use crate::web3::{Contracts, L1Chain, L1Rpc, RegistryDa, blob_source};
```

Add above `start`:

```rust
/// Refuses a configuration whose `--l1-rpc-url` disagrees with the
/// registry's DA mode.
fn check_da(da: &RegistryDa, cfg: &Config) -> anyhow::Result<()> {
    match crate::l1da::mode_error(da, cfg.l1_mode()) {
        Some(e) => anyhow::bail!(e),
        None => Ok(()),
    }
}

/// L1-blob mode: the Ethereum side, checked against the network's.
async fn connect_l1(
    cfg: &Config,
    db: &crate::storage::Db,
    l1_block: alloy::primitives::Address,
    signing: bool,
) -> anyhow::Result<(Arc<L1Rpc>, Arc<L1Da>)> {
    let rpc = L1Rpc::connect(&cfg.l1_rpc_url, &cfg.l1_send_rpc_url, cfg.l1_privkey.as_ref()).await?;
    cfg.check_l1(rpc.chain_id(), l1_block)?;
    if signing && rpc.sender().is_none() {
        anyhow::bail!(
            "L1-blob mode: a signing node needs its dedicated Ethereum key \
             (--l1-privkey-file or DAVINCI_L1_PRIVKEY)"
        );
    }
    tracing::info!(
        l1_chain_id = rpc.chain_id(),
        sender = ?rpc.sender(),
        %l1_block,
        cell_proofs = rpc.cell_proofs(),
        max_post_fee = cfg.l1_max_post_fee,
        "L1-blob mode: blobs go to Ethereum"
    );
    let rpc = Arc::new(rpc);
    let da = L1Da::new(rpc.clone(), db.clone(), cfg.l1_max_post_fee, cfg.poll_interval)?;
    Ok((rpc, Arc::new(da)))
}
```

In `start`, after `contracts.check_release(&vk_hash).await?;` add:

```rust
    let da = contracts.da_mode().await?;
    check_da(&da, &cfg)?;
```

and replace the block from `let mut cfg = cfg;` through `tracing::info!(cap, "blob cap per transaction");` with:

```rust
    let mut cfg = cfg;
    let l1 = match da {
        RegistryDa::L1Blobs { l1_block, .. } => {
            Some(connect_l1(&cfg, &db, l1_block, contracts.signer().is_some()).await?)
        }
        RegistryDa::Blobs => None,
    };
    // Batches are sized for the chain that carries the blobs.
    let (advertised, blob_chain) = match &l1 {
        Some((rpc, _)) => (rpc.chain_blob_cap(), rpc.chain_id()),
        None => (contracts.chain_blob_cap(), contracts.chain_id()),
    };
    let cap = crate::web3::resolve_blob_cap(cfg.max_blobs_per_tx, advertised, blob_chain);
    cfg.max_blobs_per_tx = Some(cap);
    tracing::info!(cap, "blob cap per transaction");
```

and in the `Deps` literal replace `l1: None,` with `l1: l1.map(|(_, da)| da),`.

- [ ] **Step 4: Run the tests**

Run: `cargo test -p davinci-sequencer --lib api 2>&1 | tail -5`
Expected: PASS (`a_registry_mode_mismatch_stops_the_boot`, `shutdown_during_startup`, `timeout_maps_to_json_408`).
Run: `cargo build -p davinci-sequencer --release 2>&1 | tail -2`
Expected: builds.

- [ ] **Step 5: Commit**

```bash
cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings
git add sequencer/src/api/mod.rs
git commit -m "feat(api): read the registry's da mode at boot and connect to ethereum"
```

---
### Task 12: Two anvils: a real L1-blob settlement end to end

**Files:**
- Modify: `sequencer/tests/web3_anvil.rs` (`ctor_tail` L38-46; new module at the end of the file)

**Interfaces:**
- Consumes: everything above; davinci-contracts artifacts `MockZiskVerifier`, `MockL1Block`, `L1BlobVerifier`, `ProcessRegistry` (`forge build` on `feat/l1-blob-da`).
- Produces: `ctor_tail_l1(council_manager, l1_blob_verifier) -> Vec<u8>` and `l1_blob_mode::relay_origin` (an origin relayer Plan 3's harness can copy).

- [ ] **Step 1: Write the tests**

In `sequencer/tests/web3_anvil.rs`, replace `ctor_tail` with:

```rust
/// Registry constructor args after `_dkgManager`: `_councilManager`, the grace
/// window, then `_l1BlobVerifier` (zero: blob mode).
fn ctor_tail(council_manager: Address) -> Vec<u8> {
    ctor_tail_l1(council_manager, Address::ZERO)
}

/// [`ctor_tail`] with an L1 blob verifier: the registry in L1-blob mode.
fn ctor_tail_l1(council_manager: Address, l1_blob_verifier: Address) -> Vec<u8> {
    let mut v = council_manager.abi_encode();
    v.extend(GRACE_ARGS.abi_encode_params());
    v.extend(l1_blob_verifier.abi_encode());
    v
}
```

Append to the end of the file:

```rust
/// L1-blob mode on two anvils: Ethereum (blobs, Osaka) and an OP-like chain
/// whose registry proves blob transactions through `L1BlobVerifier`, reading
/// its origin from a `MockL1Block` the tests point at Ethereum blocks. The
/// mock cannot reproduce OP's derivation timing, real blob-pool replacement,
/// RPC body limits or PeerDAS beacons: the live phase covers those.
mod l1_blob_mode {
    use std::sync::Arc;

    use alloy::consensus::Transaction as _;
    use alloy::node_bindings::AnvilInstance;
    use alloy::providers::DynProvider;
    use davinci_sequencer::l1da::{Anchor, L1Da, L1Job, Posting, Settled, choose_anchor, settle};
    use davinci_sequencer::storage::Db;
    use davinci_sequencer::web3::l1proof::verify_local;
    use davinci_sequencer::web3::{L1Blobs, L1Chain, L1Rpc, OnchainProcess, RegistryDa};
    use davinci_zkvm_sdk::crypto::babyjubjub::Point;
    use tokio_util::sync::CancellationToken;

    use super::*;

    alloy::sol! {
        #[sol(rpc)]
        interface MockL1Block {
            function set(uint64 n, bytes32 h) external;
        }
    }

    const CENSUS_ROOT: [u8; 32] = {
        let mut r = [0u8; 32];
        r[0] = 0x01;
        r[31] = 0x2a;
        r
    };

    struct TwoChains {
        _l1: AnvilInstance,
        _op: AnvilInstance,
        l1_url: url::Url,
        verifier: Address,
        l1_block: Address,
        me: Address,
        c: Contracts,
        l1c: Arc<L1Rpc>,
        /// OP key 2: moves the mock origin without racing `c`'s nonces.
        relay: DynProvider,
        l1_read: DynProvider,
        db: Db,
        _dir: tempfile::TempDir,
    }

    async fn two_chains() -> TwoChains {
        let l1 = Anvil::at(anvil_bin())
            .args(["--hardfork", "osaka"])
            .try_spawn()
            .expect("spawn the Ethereum anvil");
        let op = Anvil::at(anvil_bin())
            .args(["--hardfork", "osaka"])
            .try_spawn()
            .expect("spawn the OP anvil");
        let (l1_url, op_url) = (l1.endpoint_url(), op.endpoint_url());
        let key: PrivateKeySigner = op.keys()[0].clone().into();
        let me = key.address();
        let secret = SecretString::new(hex::encode(key.to_bytes()));
        let deployer = ProviderBuilder::new()
            .wallet(EthereumWallet::from(key))
            .connect_http(op_url.clone());
        let relay_key: PrivateKeySigner = op.keys()[2].clone().into();
        let relay = ProviderBuilder::new()
            .wallet(EthereumWallet::from(relay_key))
            .connect_http(op_url.clone())
            .erased();
        // The dedicated Ethereum key: L1 anvil key 1.
        let l1_key: PrivateKeySigner = l1.keys()[1].clone().into();
        let l1_secret = SecretString::new(hex::encode(l1_key.to_bytes()));
        let l1_read = ProviderBuilder::new()
            .connect_http(l1_url.clone())
            .erased();

        let zisk = deploy(
            &deployer,
            bytecode("MockZiskVerifier.sol/MockZiskVerifier.json"),
        )
        .await;
        let l1_block = deploy(&deployer, bytecode("MockL1Block.sol/MockL1Block.json")).await;
        let mut code = bytecode("L1BlobVerifier.sol/L1BlobVerifier.json");
        code.extend(l1_block.abi_encode());
        let verifier = deploy(&deployer, code).await;
        let mut code = bytecode("ProcessRegistry.sol/ProcessRegistry.json");
        code.extend(
            (
                op.chain_id() as u32,
                zisk,
                B256::from(BATCH_VK),
                B256::from(RESULTS_VK),
                B256::from(ROOT_C),
                B256::from(BALLOT_VK_HASH),
                Address::ZERO,
            )
                .abi_encode_params(),
        );
        code.extend(ctor_tail_l1(Address::ZERO, verifier));
        let registry = deploy(&deployer, code).await;

        let c = Contracts::new(std::slice::from_ref(&op_url), registry, Some(&secret))
            .await
            .unwrap();
        let l1c = Arc::new(
            L1Rpc::connect(std::slice::from_ref(&l1_url), &[], Some(&l1_secret))
                .await
                .unwrap(),
        );
        let dir = tempfile::TempDir::new().unwrap();
        let db = Db::open_in(dir.path()).unwrap();
        TwoChains {
            _l1: l1,
            _op: op,
            l1_url,
            verifier,
            l1_block,
            me,
            c,
            l1c,
            relay,
            l1_read,
            db,
            _dir: dir,
        }
    }

    impl TwoChains {
        fn da(&self) -> L1Da {
            L1Da::new(
                self.l1c.clone(),
                self.db.clone(),
                u128::MAX,
                Duration::from_millis(100),
            )
            .unwrap()
        }

        async fn l1_head(&self) -> u64 {
            self.l1_read.get_block_number().await.unwrap()
        }

        async fn mine_l1(&self, n: u64) {
            let _: serde_json::Value = self
                .l1_read
                .raw_request("anvil_mine".into(), (alloy::primitives::U256::from(n),))
                .await
                .unwrap();
        }

        /// Points OP's origin at Ethereum block `n`.
        async fn set_origin(&self, n: u64) {
            let h = self
                .l1_read
                .get_block_by_number(n.into())
                .await
                .unwrap()
                .unwrap()
                .header
                .hash;
            self.set_origin_raw(n, h).await;
        }

        async fn set_origin_raw(&self, n: u64, h: B256) {
            let r = MockL1Block::new(self.l1_block, &self.relay)
                .set(n, h)
                .send()
                .await
                .unwrap()
                .get_receipt()
                .await
                .unwrap();
            assert!(r.status());
        }

        /// A READY process with a fresh election key.
        async fn process(&self) -> ([u8; 31], OnchainProcess, Point) {
            let (_sk, pk) = keygen(&mut rand::rngs::OsRng);
            let np = NewProcess {
                status: ProcessStatus::Ready,
                start_time: 0,
                duration: 3600,
                max_voters: 1000,
                ballot_mode: BallotMode {
                    num_fields: NF,
                    group_size: 1,
                    unique_values: false,
                    cost_exponent: 1,
                    max_value: 5,
                    min_value: 0,
                    max_value_sum: 20,
                    min_value_sum: 0,
                },
                census: OnchainCensus {
                    origin: 1,
                    root: CENSUS_ROOT,
                    uri: "file:///tmp/census.json".into(),
                    contract_address: [0u8; 20],
                },
                metadata: "ipfs://meta".into(),
                metadata_hash: metadata_hash(META_DOC),
                enc_key: pk,
            };
            let (pid, _) = self.c.create_process(&np).await.unwrap();
            (pid, self.c.process(&pid).await.unwrap(), pk)
        }
    }

    /// A two-vote transition from `before`, its blobs and its snark.
    fn transition(
        pid: &[u8; 31],
        before: [u8; 32],
        after: [u8; 32],
        occupied_before: u32,
        pk: &Point,
        seed: u64,
    ) -> (TransitionBlobs, PlonkSnark) {
        let t = TransitionData {
            vote_ids: vec![VOTE_ID_MIN + seed, VOTE_ID_MIN + seed + 1],
            updates: vec![
                (0x100 + seed, some_ballot(pk, seed)),
                (0x101 + seed, some_ballot(pk, seed + 1)),
            ],
            accumulator: some_ballot(pk, seed + 2),
            num_fields: NF,
        };
        let blobs = build_blobs(&t, &pid_fr(pid), &before).unwrap();
        let snark = batch_snark(
            &Batch {
                before,
                after,
                voters: 2,
                overwrites: 0,
                occupied_before,
            },
            &CENSUS_ROOT,
            &blobs,
        );
        (blobs, snark)
    }

    /// Ethereum produces a block every 400 ms and OP's origin follows its
    /// head, until `stop`.
    async fn relay_origin(
        l1: DynProvider,
        op: DynProvider,
        l1_block: Address,
        stop: CancellationToken,
    ) {
        let mock = MockL1Block::new(l1_block, &op);
        loop {
            tokio::select! {
                _ = stop.cancelled() => return,
                _ = tokio::time::sleep(Duration::from_millis(400)) => {}
            }
            let _: serde_json::Value = l1.raw_request("evm_mine".into(), ()).await.unwrap();
            let b = l1
                .get_block_by_number(alloy::eips::BlockNumberOrTag::Latest)
                .await
                .unwrap()
                .unwrap();
            mock.set(b.header.number, b.header.hash)
                .send()
                .await
                .unwrap()
                .get_receipt()
                .await
                .unwrap();
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn l1_settlement_on_two_anvils() {
        if !enabled() {
            eprintln!("skipped: set ANVIL=1");
            return;
        }
        let t = two_chains().await;
        assert_eq!(
            t.c.da_mode().await.unwrap(),
            RegistryDa::L1Blobs {
                verifier: t.verifier,
                l1_block: t.l1_block
            }
        );
        assert!(t.l1c.cell_proofs(), "anvil --hardfork osaka: v1 sidecars");
        let da = t.da();
        let (pid, p, pk) = t.process().await;
        let (blobs1, snark1) = transition(&pid, p.state_root, [0x91; 32], 0, &pk, 1);
        // Blob mode's entry point is refused on this registry.
        let r = t.c.simulate_transition(&pid, &snark1, &blobs1).await.unwrap_err();
        assert_eq!(r.name(), Some("WrongDAMode"), "{r}");

        // The blobs go to Ethereum, then one more block on top.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
        let posted = da
            .poster
            .post(&pid, &p.state_root, &blobs1, deadline)
            .await
            .unwrap();
        assert!(t.db.l1_posts().unwrap().is_empty(), "a mined post leaves no record");
        t.mine_l1(1).await;
        let head = t.l1_head().await;
        let proof = t.l1c.build_proof(&[posted.tx], head).await.unwrap();
        // An origin that is no header of the proof is named.
        t.set_origin_raw(head, B256::repeat_byte(7)).await;
        let r = t
            .c
            .simulate_transition_l1(&pid, &snark1, &blobs1, &proof)
            .await
            .unwrap_err();
        assert_eq!(r.name(), Some("L1OriginMismatch"), "{r}");
        // OP's origin at the head: the proof anchors there.
        t.set_origin(head).await;
        let origin = t.c.l1_origin().await.unwrap();
        assert_eq!(origin.number, head);
        let want: Vec<B256> = blobs1
            .versioned_hashes
            .iter()
            .map(|h| B256::from(*h))
            .collect();
        let (vhs, blocks) = verify_local(&proof, origin.hash).unwrap();
        assert_eq!(vhs, want);
        assert_eq!(blocks, vec![posted.block; want.len()]);
        t.c.simulate_transition_l1(&pid, &snark1, &blobs1, &proof)
            .await
            .unwrap();
        let r1 = t
            .c
            .submit_transition_l1(&pid, &snark1, &blobs1, &proof)
            .await
            .unwrap();
        // The transition and its L1 event come back as one event.
        let evs = t.c.events(r1.block, r1.block).await.unwrap();
        assert_eq!(evs.len(), 1, "{evs:?}");
        assert_eq!(
            evs[0].kind,
            EventKind::StateTransitioned {
                pid,
                sender: t.me,
                old_root: p.state_root,
                new_root: [0x91; 32],
                voters: 2,
                overwrites: 0,
                n_blobs: want.len() as u64,
                l1: Some(L1Blobs {
                    blocks: vec![posted.block; want.len()],
                    vhs: want.clone(),
                }),
            }
        );
        // A follower reads the blobs from the Ethereum anvil.
        let got = AnvilBlobs::new(std::slice::from_ref(&t.l1_url))
            .blobs_for_l1(&blocks, &want)
            .await
            .unwrap();
        assert_eq!(got, blobs1.blobs);

        // Second transition: OP's origin runs ahead; the proof anchors at the
        // origin recorded when it reached the blobs.
        let (blobs2, snark2) = transition(&pid, [0x91; 32], [0x92; 32], 2, &pk, 10);
        let posted2 = da
            .poster
            .post(&pid, &[0x91; 32], &blobs2, deadline)
            .await
            .unwrap();
        assert_eq!(posted2.nonce, posted.nonce + 1);
        t.set_origin(posted2.block).await;
        let rec = t.c.record_l1_origin().await.unwrap();
        assert_eq!(rec.number, posted2.block);
        t.mine_l1(20).await;
        let head = t.l1_head().await;
        t.set_origin(head).await;
        let origin = t.c.l1_origin().await.unwrap();
        let Anchor::At { top, hash } = choose_anchor(posted2.block, head, origin, Some(rec)) else {
            panic!("a deep proof anchors at the record");
        };
        assert_eq!((top, hash), (rec.number, rec.hash));
        let proof2 = t.l1c.build_proof(&[posted2.tx], top).await.unwrap();
        assert_eq!(proof2.headers.len(), 1);
        assert!(verify_local(&proof2, origin.hash).is_err(), "not at the current origin");
        t.c.simulate_transition_l1(&pid, &snark2, &blobs2, &proof2)
            .await
            .unwrap();
        let r2 = t
            .c
            .submit_transition_l1(&pid, &snark2, &blobs2, &proof2)
            .await
            .unwrap();
        let p = t.c.process(&pid).await.unwrap();
        assert_eq!((p.state_root, p.voters_count), ([0x92; 32], 4));
        let evs = t.c.events(r2.block, r2.block).await.unwrap();
        assert!(
            matches!(&evs[0].kind, EventKind::StateTransitioned { l1: Some(b), .. }
                if b.blocks[0] == posted2.block),
            "{evs:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn l1_settle_loop_posts_again_after_an_l1_reorg() {
        if !enabled() {
            eprintln!("skipped: set ANVIL=1");
            return;
        }
        let t = two_chains().await;
        t.c.da_mode().await.unwrap();
        let da = t.da();
        let (pid, p, pk) = t.process().await;
        let (blobs, snark) = transition(&pid, p.state_root, [0x91; 32], 0, &pk, 1);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
        let posted = da
            .poster
            .post(&pid, &p.state_root, &blobs, deadline)
            .await
            .unwrap();
        // A reorg of depth one drops the block holding the blob transaction.
        assert_eq!(t.l1_head().await, posted.block);
        let _: serde_json::Value = t
            .l1_read
            .raw_request("anvil_reorg".into(), (1u64, Vec::<serde_json::Value>::new()))
            .await
            .unwrap();
        assert!(t.l1c.receipt(posted.tx).await.unwrap().is_none());
        // From now on Ethereum produces blocks and OP's origin follows its head.
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
            deadline,
            cancel: CancellationToken::new(),
        };
        let posting = Posting {
            post: Ok(posted),
            recorded: None,
        };
        let out = settle(&da, &t.c, &job, &snark, posting).await;
        stop.cancel();
        relay.await.unwrap();
        let Settled::Landed(rc) = out else {
            panic!("{out:?}");
        };
        let evs = t.c.events(rc.block, rc.block).await.unwrap();
        let EventKind::StateTransitioned { l1: Some(b), .. } = &evs[0].kind else {
            panic!("{evs:?}");
        };
        assert!(b.blocks[0] > posted.block, "the blobs of the new chain");
        assert!(t.db.l1_posts().unwrap().is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn l1_post_replaces_a_dropped_transaction() {
        if !enabled() {
            eprintln!("skipped: set ANVIL=1");
            return;
        }
        let t = two_chains().await;
        let da = Arc::new(t.da());
        let (pid, p, pk) = t.process().await;
        let (blobs, _) = transition(&pid, p.state_root, [0x91; 32], 0, &pk, 1);
        let _: serde_json::Value = t
            .l1_read
            .raw_request("evm_setAutomine".into(), (false,))
            .await
            .unwrap();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
        let task = {
            let (da, blobs, root) = (da.clone(), blobs.clone(), p.state_root);
            tokio::spawn(async move { da.poster.post(&pid, &root, &blobs, deadline).await })
        };
        common::wait_until("first send recorded", async || {
            t.db.l1_posts().unwrap().len() == 1
        })
        .await;
        let first = t.db.l1_posts().unwrap().remove(0);
        let _: serde_json::Value = t
            .l1_read
            .raw_request("anvil_dropTransaction".into(), (first.tx_hash,))
            .await
            .unwrap();
        for _ in 0..3 {
            t.mine_l1(1).await;
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
        common::wait_until("replacement sent", async || {
            t.db
                .l1_posts()
                .unwrap()
                .first()
                .is_some_and(|r| r.tx_hash != first.tx_hash)
        })
        .await;
        t.mine_l1(1).await;
        let posted = task.await.unwrap().unwrap();
        assert_ne!(posted.tx, first.tx_hash);
        assert_eq!(posted.nonce, first.nonce);
        let tx = t
            .l1_read
            .get_transaction_by_hash(posted.tx)
            .await
            .unwrap()
            .unwrap();
        assert!(tx.inner.max_fee_per_blob_gas().unwrap() >= 2 * first.fees.blob_fee);
        assert!(
            t.l1_read
                .get_transaction_by_hash(first.tx_hash)
                .await
                .unwrap()
                .is_none()
        );
    }
}
```

- [ ] **Step 2: Build the contracts and run the anvil tests**

```bash
cd /home/p4u/davinci-contracts && git branch --show-current && ~/.foundry/bin/forge build 2>&1 | tail -2
cd /home/p4u/davinci-sequencer && ANVIL=1 cargo test -p davinci-sequencer --test web3_anvil l1_blob_mode 2>&1 | tail -8
```
Expected: `feat/l1-blob-da`; then three tests PASS (`l1_settlement_on_two_anvils`, `l1_settle_loop_posts_again_after_an_l1_reorg`, `l1_post_replaces_a_dropped_transaction`), not skip lines. If `anvil_reorg` rejects the parameters, check `~/.foundry/bin/anvil --version` (1.8.3 takes positional `[depth, txBlockPairs]`).

- [ ] **Step 3: Run the whole anvil file and the client anvil test**

Run: `ANVIL=1 cargo test -p davinci-sequencer --test web3_anvil 2>&1 | tail -3 && ANVIL=1 cargo test -p davinci-client --test organizer 2>&1 | tail -3`
Expected: every test PASS (blob-mode anvil tests unchanged).

- [ ] **Step 4: Commit**

```bash
cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings
git add sequencer/tests/web3_anvil.rs
git commit -m "test(web3): l1-blob settlement, recorded anchor, reorg and dropped post on two anvils"
```

---

### Task 13: Documentation

**Files:**
- Modify: `docs/lifecycle.md` (new section after "Races and sync" L86-97; "Restarts" L246-256)
- Modify: `docs/configuration.md` (Networks table L18-20, Options table L50-84, new section after "Signing key and observer mode")
- Modify: `docs/deployments.md` (new section before "## Gnosis Chain")
- Modify: `docs/testing.md` (gated tests table L28-36)
- Modify: `README.md` (configuration table L107-118)

**Interfaces:**
- Consumes: the behaviour of Tasks 1-12. No code. `docs/api.md` is unchanged: no route, wire type or error code changes (the transitions API serves L1-mode blobs like any others).

- [ ] **Step 1: `docs/lifecycle.md`**

Insert after the "Races and sync" section:

```markdown
## L1-blob mode (OP Stack)

A registry deployed with an `L1BlobVerifier` settles on an OP Stack chain while its blobs live in
Ethereum blob transactions. The node reads `l1BlobVerifier()` at boot and refuses to start when
its configuration disagrees: `--l1-rpc-url` set for a blob-mode registry, or missing for an
L1-mode one. A signing node also needs its dedicated Ethereum key.

Each batch:

1. Seals and builds its blobs as in blob mode.
2. Posts the blobs on Ethereum while it is proved: one type-3 transaction from the node's
   Ethereum account to itself, EIP-7594 sidecar, at most six blobs. The account sends one
   transaction at a time; a later post waits until the earlier one is mined. The tip is at least
   0.001 gwei, the max fee twice the base fee plus the tip, the blob fee twice the blob base fee.
   A transaction not mined after 3 Ethereum blocks is replaced at its nonce with every fee
   doubled. A post whose worst-case cost would pass `--l1-max-post-fee` waits for cheaper fees,
   up to the grace end.
3. If OP's L1 origin (`L1Block.number()`) reaches the blob block before the proof is ready, the
   node calls `record()` on the verifier once, which stores that origin on OP.
4. With the proof ready, waits until the blob transaction has one Ethereum block on top and OP's
   origin reached it, then builds the L1 proof: Ethereum headers from at most two blocks above the
   origin down to the blob block while that is at most 16 headers; else from the recorded origin
   down; else up to 64 headers from the current origin. Deeper than that, or 7,200 blocks behind
   the origin, it posts the blobs again.
5. Checks the proof locally, simulates `submitStateTransitionL1` on OP and sends it (gas estimate
   plus 20 %). `L1OriginMismatch` (the origin moved past the proof, or the node's Ethereum RPC is
   behind OP's origin) rebuilds the proof from fresh reads, at most 5 times, then the votes go back
   to the queue.
6. A blob transaction that disappears (dropped, or reorged out of Ethereum) is posted again.
7. Commits when its `ProcessStateTransitioned` event reaches `--confirmations`, not at the receipt:
   OP's unsafe blocks reorg with Ethereum. A settlement whose event never shows within 90 s is
   rolled back and its votes requeued, like a lost race.

The registry emits `ProcessStateTransitionedL1(processId, l1BlockNumbers, versionedHashes)` right
after `ProcessStateTransitioned`; the node merges the two and fetches the blobs from Ethereum:
beacon APIs by slot (`--blob-source beacon:`, Ethereum beacons in this mode), or the L1 anvil.
After PeerDAS only beacon nodes that keep full blobs (supernodes, archives) serve them, so list
several. Each node stores and serves the blobs under `/processes/{pid}/transitions/{index}/blobs`
as in blob mode. A lost race wastes the loser's blob transaction (a few cents at most).
```

In "Restarts", append:

```markdown
In L1-blob mode the node records each Ethereum blob transaction before sending it and drops the
record once it is mined. After a restart the batch is proved again (its secrets died with the
process) and its first post replaces a still-pending transaction at its nonce with doubled fees
instead of queueing behind it. A transaction mined before the crash is wasted.
```

- [ ] **Step 2: `docs/configuration.md`**

Add a row to the Networks table:

```markdown
| `optimism` | 10 | `https://optimism-rpc.publicnode.com`, `https://mainnet.optimism.io` | `beacon:https://ethereum-beacon-api.publicnode.com` (Ethereum) | 1 |
```

and below the table: "The `optimism` preset is in L1-blob mode and is not deployed yet: its registry is the zero address, so `--network optimism` refuses to start without `--registry` until a release fills it in."

Add to the Options table, after `--blob-source`:

```markdown
| `--l1-rpc-url` | `DAVINCI_L1_RPC_URL` | network | L1-blob mode: Ethereum execution JSON-RPCs, comma-separated for failover. Set exactly when the registry has an `L1BlobVerifier`. |
| `--l1-send-rpc-url` | `DAVINCI_L1_SEND_RPC_URL` | `--l1-rpc-url` | Endpoints that only receive the blob transactions. A 6-blob send is about 1.5 MB; many public RPCs refuse such bodies. |
| `--l1-privkey-file` | `DAVINCI_L1_PRIVKEY_FILE` | unset | File with the hex key of the dedicated Ethereum account that posts blobs. A signing node in L1-blob mode needs it. |
| (env only) | `DAVINCI_L1_PRIVKEY` | unset | The same key, from the environment. |
| `--l1-max-post-fee` | `DAVINCI_L1_MAX_POST_FEE` | `0.001eth` | Most one blob post may cost at the fees it offers (`<wei>`, `<n>gwei`, `<n>eth`); above it the node waits for cheaper fees. |
```

Add a section after "Signing key and observer mode":

```markdown
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
  post costs about 21,000 gas plus 131,072 blob gas per blob.
- `--l1-send-rpc-url` when the read RPCs refuse large bodies.

At boot the node checks the Ethereum chain id and the verifier's `L1Block` address against the
network (the `optimism` preset expects chain 1 and `0x4200000000000000000000000000000000000015`),
and sizes batches with Ethereum's blob limit. Observers need no Ethereum key.
```

- [ ] **Step 3: `docs/deployments.md`**

Insert before `## Gnosis Chain`:

```markdown
## OP Mainnet (pending)

Chain id 10, preset `optimism`, L1-blob mode: transitions settle on OP Mainnet and their blobs
live in Ethereum blob transactions. Not deployed yet: the preset's registry is the zero address,
so `--network optimism` refuses to start without `--registry`. The deployment fills this section
and `client/src/networks.rs` together.

| Contract | Address | Block |
|---|---|---|
| ZiskVerifier | `0x6781FFF50D615CB3F331CCD3Bdad3416d4ce0d5c` (codehash equal to the pinned one) | |
| L1BlobVerifier | not deployed | |
| ProcessRegistry | not deployed | |

`L1BlobVerifier` reads OP's L1 origin from the `L1Block` predeploy
`0x4200000000000000000000000000000000000015`. Consumers of OP transitions (SDKs, explorers) read
`ProcessStateTransitionedL1` for the Ethereum blocks and versioned hashes; there is no `BLOBHASH`
on OP.
```

- [ ] **Step 4: `docs/testing.md` and `README.md`**

Add a row to the gated tests table in `docs/testing.md`:

```markdown
| `ANVIL=1 cargo test -p davinci-sequencer --test web3_anvil l1_blob_mode` | anvil, davinci-contracts build with `L1BlobVerifier` and `MockL1Block` | L1-blob mode on two anvils: blob post, settlement, recorded anchor, origin mismatch, reorg re-post, dropped-post replacement. |
```

and after the table: "`--test l1da` (poster and settle loop) and `--test actor_l1` (the actor in L1-blob mode) run on scripted fakes with no gate."

Add to the `README.md` configuration table, after `DAVINCI_BLOB_SOURCE`:

```markdown
| `DAVINCI_L1_RPC_URL` | `--l1-rpc-url` | network | L1-blob mode (OP Stack registries): Ethereum JSON-RPC endpoints. |
| `DAVINCI_L1_PRIVKEY_FILE` | `--l1-privkey-file` | unset | L1-blob mode: the dedicated Ethereum key that posts blobs. |
```

- [ ] **Step 5: Final checks and commit**

Run: `cargo fmt --all -- --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace 2>&1 | grep -E "^test result|FAILED|panicked" | sort | uniq -c`
Expected: no warnings; every line `test result: ok` (gated tests print their skip lines and pass).

```bash
git add docs/lifecycle.md docs/configuration.md docs/deployments.md docs/testing.md README.md
git commit -m "docs(da): l1-blob mode lifecycle, options and the pending op deployment"
```

---
## Handoff to Plan 3

What the e2e harness, the OP Mainnet deployment and the live phase can rely on:

- **Node environment** (the node binary, unchanged CLI otherwise):
  - `DAVINCI_NETWORK=custom` (or `optimism` once its registry is filled), `DAVINCI_RPC_URL` (OP), `DAVINCI_REGISTRY`, `DAVINCI_CONFIRMATIONS` (`1` on OP).
  - `DAVINCI_L1_RPC_URL` (Ethereum RPCs; on the e2e harness the L1 anvil), `DAVINCI_L1_SEND_RPC_URL` (optional send-only endpoints), `DAVINCI_L1_PRIVKEY_FILE` or `DAVINCI_L1_PRIVKEY` (one dedicated L1 key per sequencer; anvil keys of the L1 anvil in e2e, never shared between nodes), `DAVINCI_L1_MAX_POST_FEE` (default `0.001eth`).
  - `DAVINCI_BLOB_SOURCE`: in L1-blob mode it names Ethereum sources: `anvil` reads the L1 anvil at `DAVINCI_L1_RPC_URL`; `beacon:<urls>` names Ethereum beacon APIs that keep full blobs.
  - Observers need no L1 key; signing nodes refuse to boot without one; any node refuses to boot when `DAVINCI_L1_RPC_URL` presence disagrees with the registry's `l1BlobVerifier()`.
- **Harness building blocks** (`sequencer/tests/web3_anvil.rs`, module `l1_blob_mode`): the OP-side deploy order (`MockZiskVerifier`, `MockL1Block`, `L1BlobVerifier(mockL1Block)`, `ProcessRegistry` with `ctor_tail_l1(councilManager, verifier)`), and `relay_origin`, which mines an L1 block and copies its `(number, hash)` into `MockL1Block` every 400 ms. Plan 3's relayer adds a configurable lag on the same pattern.
- **Library API** for tools and benchmarks: `davinci_sequencer::l1da::{L1Da, Poster, Posted, PostError, L1Job, Posting, Settled, with_post, settle, choose_anchor, Anchor}` and its constants; `davinci_sequencer::web3::{L1Rpc, L1Chain, L1Fees, L1Market, L1Origin, L1Blobs, RegistryDa, revert_message}`; `Contracts::{open, da_mode, l1_origin, record_l1_origin, simulate_transition_l1, submit_transition_l1}`; `davinci_client::organizer::{RegistryDa, RegistryInfo::da}`; `davinci_client::networks::{OPTIMISM, NetworkDa, OP_L1_BLOCK}`. Negative replays can encode `submitStateTransitionL1` with `davinci_client`'s `ProcessRegistry` bindings (`submitStateTransitionL1Call`).
- **Logs a benchmark can time:** "blobs posted on L1" (nonce, tx, block, blobs), "OP's origin reached the blobs before the proof; origin recorded", "L1-blob transition sent" (L1 block, header count, OP tx), "settlement mined; committing at its event", "transition settled".
- **To fill after the deployment:** `client/src/networks.rs` `OPTIMISM.registry` and `start_block` (the config test `optimism_preset_is_l1_mode_and_needs_its_registry` covers both states), the provisional RPC and beacon lists (verify that the send endpoint takes 1.5 MB bodies and that the beacons serve `blobs/{slot}?versioned_hashes=` after PeerDAS), the "OP Mainnet (pending)" section of `docs/deployments.md` and its Networks row in `docs/configuration.md`.
- **CI:** the new `ANVIL=1` tests need davinci-contracts artifacts with `L1BlobVerifier` and `MockL1Block`; `CONTRACTS_REF` must point at a commit that has them until `feat/l1-blob-da` is merged.
- **Behaviour the e2e kill-and-restart scenario should expect:** a node killed between posting and settling re-proves after the restart (the batch secrets are never persisted) and its first post replaces the pending blob transaction at the same nonce with doubled fees; a transaction already mined is wasted.

## Self-review

- **Spec and notes coverage.** §5 configuration: Tasks 2, 11 (with the notes' change: no `--l1-beacon-url`, `--blob-source` names Ethereum beacons in L1 mode). Mode detection and refusal: Tasks 3, 8 (`mode_error`), 11. Boot checks (L1 chain id, `L1Block`, sidecar version and blob cap from Ethereum): Tasks 2 (`check_l1`), 3 (`blob_support`), 5, 11. L1 blob sender (window 1, fees, 3-block replacement with every fee doubled, fee cap, no gap, record before send, restart replacement, schema 9): Tasks 5, 6, 7. Actor flow (post while proving, record once, receipt and re-post, head ≥ n + 1 and origin ≥ n, 16/64 thresholds, build plus `verify_local`, simulate, send with estimate + 20 %, bounded `L1OriginMismatch` with fresh reads): Tasks 8, 10. Restart and reorgs (the notes' re-prove with a post record replaces the spec's resume): Tasks 6, 7, 10. Proof builder: Plan 1, called from Tasks 5 and 7. Sync (`ProcessStateTransitionedL1` pairing rules, `blobs_for_l1` by beacon slot or anvil scan, unpaired transitions fail in L1 mode): Tasks 4, 9, 10. Vanished settlement event: Task 10 (commit at the event, roll back after `LANDED_EVENT_WAIT`). Client `verify_registry` reports the mode: Task 1. Presets: Task 1. §6 tests: unit (Tasks 1-9, 11), fake chain for post, origin lag, record, mismatch, dropped post, lost race, restart (Tasks 7, 8, 10), two anvils with a real settlement, a recorded anchor, a reorg re-post and a dropped-post replacement (Task 12). Docs: Task 13. The e2e scenarios, deployment and benchmarks are Plan 3.
- **Placeholder scan.** No TBD/TODO steps; every code step carries its code. The `optimism` preset's zero registry and the "pending" deployment table are deliberate, refused at boot and documented, filled by Plan 3.
- **Type consistency.** `L1Origin { number, hash }`, `L1Blobs { blocks, vhs }`, `L1Fees { max_fee, tip, blob_fee }`, `L1Market { base_fee, tip, blob_base_fee }`, `Posted { tx, nonce, block }`, `L1Job { pid, root_before, blobs, deadline, cancel }`, `Posting { post, recorded }`, `Settled::{Landed, Reverted, Lost, Transient, Failed}`, `RegistryDa::{Blobs, L1Blobs { verifier, l1_block }}` and `NetworkDa::{Blobs, L1Blobs { l1_chain_id, l1_block, l1_rpc_urls }}` are used with the same names and fields in every task; `Chain`'s four L1 methods have one signature (`pid: &[u8; 31], snark: &PlonkSnark, blobs: &TransitionBlobs, proof: &L1BlobProof`) in the trait, `Contracts` and `FakeChain`.
- **Review Focus.** Each of the five lines names tests that exist in the owning tasks (Tasks 5, 7, 8, 9, 10).
