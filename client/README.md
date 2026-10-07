# davinci-client

Rust client for a DAVINCI sequencer and its `ProcessRegistry`, for organizers who create elections
and voters who cast ballots.

## Overview

- `api`: the HTTP wire types of every node route, and `verify_tracker` for recorded-as-cast
  checks.
- `SequencerClient`: a client for every route.
- `networks`: the known deployments, shared with the node's `--network` presets.
- `organizer`: the process lifecycle on the registry (`Organizer`), read-only access
  (`RegistryReader`), and `verify_registry`, which checks that a registry pins what this release
  proves.
- `voter`: ballot encryption, vote id, inputs hash, circuit inputs and the vote-id signature.
- `prover` (feature `prover`, on by default): the Groth16 ballot prover over the davinci-circom
  `ballot_proof.wasm` and `ballot_proof_pkey.zkey`. It pulls in ark-circom (arkworks 0.6) and
  wasmer; build with `default-features = false` when only the wire types and the client are
  needed.

The protocol primitives come from `davinci-zkvm-sdk` (davinci-zkvm `rust-sdk`), a path
dependency. The node's API is described in [docs/api.md](../docs/api.md), key modes in
[docs/lifecycle.md](../docs/lifecycle.md#key-modes) and census origins in
[docs/census.md](../docs/census.md).

## Usage

### Casting a vote

```rust
use davinci_client::SequencerClient;
use davinci_client::api::verify_tracker;
use davinci_client::organizer::{RegistryReader, verify_registry};
use davinci_client::prover::BallotProver;
use davinci_client::voter::Voter;
use davinci_zkvm_sdk::census::CensusWitness;
use rand::rngs::OsRng;

// The registry must pin the release this crate was built against.
verify_registry(rpc_url, registry).await?;
// Build ballots from the registry's copy of the process, never from a
// sequencer's view: a sequencer could hand out its own key or census.
let reader = RegistryReader::connect(rpc_url, registry)?;
let process = reader.process(&pid).await?;

let seq = SequencerClient::new("https://sequencer.example");
let voter = Voter::new(signing_key);
let (proof, weight) = seq.participant(&pid, &voter.address()).await?;

let prover = BallotProver::load(&wasm, &zkey)?;
let (vote, _k) = voter.build_vote(
    &prover,
    &process,
    &[1, 0, 2, 1],
    CensusWitness::Merkle(proof),
    weight,
    &mut OsRng,
)?;
seq.submit_vote(&vote).await?;

// Once the vote is settled, its id is in an on-chain root.
let tracker = seq.vote_id_proof(&pid, vote.vote_id).await?;
let root = reader.process(&pid).await?.state_root;
assert!(verify_tracker(&tracker, &root));
```

For a CSP census the census witness is the CSP's attestation (`CensusWitness::Csp`) instead of
the participant proof. `prepare_vote` and `finish_vote` split `build_vote` for callers that prove
elsewhere.

### Creating a process

`Organizer::create_process` takes a `NewProcess` whose `process_id` is the registry's
`next_process_id()`. For a sequencer key, fetch the key for that id with
`SequencerClient::new_key` first. For a DKG key, pick `KeyMode::DkgAutomatic` or `KeyMode::DkgLocked`, and keep the
`organizer_secret` a locked process returns: `reveal_process_key` needs it to unlock the results.
For a Council key, use `KeyMode::Council(ceremony_id)` where `ceremony_id` is the 12-byte id
(`[u8; 12]`) from the Council ceremony; results stay locked until the ceremony opens decryption.

`NewProcess::metadata` is the URI of the metadata document (title, question, what each ballot
field stands for) and `metadata_hash` is `organizer::metadata_hash(&document)`, the SHA-256 of the
exact bytes served at that URI, with no JSON canonicalisation. The registry refuses an empty URI
or a zero hash, so serve the document byte for byte: reformatting it changes the hash. Until the
process ends, `Organizer::set_process_metadata(pid, uri, hash)` points it at a new document
(organizer only, while ready or paused). A client checks a fetched document against
`OnchainProcess::metadata_hash`.

## Development

```bash
cargo test -p davinci-client                                   # offline tests
ANVIL=1 cargo test -p davinci-client --test organizer          # against real contracts on anvil
CIRCOM_ARTIFACTS=<dir> cargo test -p davinci-client --test prover
```

The anvil tests need `forge build` output in `DAVINCI_CONTRACTS_DIR` (default
`../davinci-contracts`). See [docs/testing.md](../docs/testing.md).

## License

GNU Affero General Public License v3.0 or later. See [LICENSE](../LICENSE).
