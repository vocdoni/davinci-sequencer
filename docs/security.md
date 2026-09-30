# Security

The zkVM circuits and the registry enforce the protocol. The node's part is to keep votes the
circuit would reject away from the prover, to keep the protocol's secrets, and to settle only
proofs that match what it computed itself.

## Properties

- **One bad ballot cannot sink a batch.** `davinci_state::validate_vote` repeats every per-vote
  check the circuit makes (ballot proof, signature, inputs hash, census binding, ballot encoding,
  weight), and the actor applies the stateful rules (unused vote id, slot queue depth,
  `maxVoters`). The batch builder mirrors the contract's `maxVoters` check and the blob limit.
- **Proofs are checked against pins.** A batch settles only when the proof's program key and
  `rootCVadcopFinal` equal the SDK release pins and every public output equals what the node
  computed. The startup check refuses a registry or verifier that pins anything else. On-chain,
  the verifier's public input is `sha256(programVK ‖ publicValues ‖ rootCVadcopFinal)`, so a proof
  of another program or another setup does not verify. The ballot verification key is bound too:
  the circuit hashes the key it is given and requires it to equal the state leaf written at
  genesis from the registry's `ballotVKHash`.
- **The batch seed and the refresh selection stay secret.** Both come from the OS random number
  generator and live in memory for one batch only. They are never logged or written to disk, and
  the prover deletes the job input that holds them when it runs with `DAVINCI_KEEP_INPUTS=0`. A
  crash therefore loses an unfinished batch by design; its votes go back to pending.
- **Silent revoting.** Every batch also re-randomizes occupied slots it did not write, a uniform
  sample that nothing public determines, so an observer cannot tell an overwrite from a routine
  refresh. Participation is not hidden: a refresh only touches an occupied slot, so a slot's first
  write is a new vote, and with a Merkle census the slot follows from the voter's address.
- **Re-sealing a batch reveals nothing.** A sealed batch may reach the chain even if the node later
  drops it (a lost race, a prover failure, a restart). Once a batch is sealed, every slot it
  changed joins a persisted per-process exposed set, and every later batch refreshes each of those
  slots it does not write. Otherwise two attempts with different refresh draws could be compared,
  and a vote dropped between them would stand out as an overwrite. The set holds exactly the slot
  keys the blob already publishes, without marking which were writes, so persisting it leaks
  nothing new. It is cleared once every exposed vote has settled or errored. If it no longer fits
  one transition, the node errors the exposed pending votes and clears it rather than publish a
  transition that covers only part of it.
- **Election keys are derived, not stored.** A sequencer key is
  `HMAC-SHA256(master, "davinci-election-key-v1" ‖ chainId ‖ registry ‖ processId)`, expanded and
  reduced into `[1, l)`. `POST /processes/keys` writes nothing, so key requests cannot fill the
  disk, and a key handed out for one process id is useless under any other.
- **State is rebuilt, not trusted.** A transition from another node is applied only when its blobs
  match the transaction's versioned hashes and replaying them on the committed root gives the
  event's `newRoot`. An observer does this for every transition, which checks the tally
  independently of the sequencers.
- **Recorded as cast.** A tracker proof is the vote id's leaf under an on-chain root, and
  `davinci_client::api::verify_tracker` checks it against the registry.
- **Results are bound to the final root.** With a sequencer key the results program proves the key
  and accumulator leaves under the final root and the 16 Chaum–Pedersen decryptions, and the
  registry checks that root against its own. With a DKG key the registry checks the accumulator's
  inclusion itself and the committee proves every decryption share.
- **External input is hostile.** Census URIs come from on-chain data, so downloads are restricted
  (see [census.md](census.md#census-files)). The API decodes strictly, caps bodies at 256 KiB,
  bounds every queue and times requests out after 60 s.

## Trust assumptions

**Sequencer keys.** The node that answered `POST /processes/keys` holds the only secret. It can
decrypt every ballot published in the blobs and is the only party able to publish the results. Use
a DKG mode when no single node should hold the key.

- A process whose key is not the one this node derives for its id has no decryptor. The client's
  organizer checks the created process id: if another `newProcess` from the same account lands
  between the key request and the creation, the process is still created and the organizer gets
  `Error::WrongProcessId`. It must cancel that process, since no node can finalize it.
- The SDK's scalar multiplication is not constant-time, so run key-holding nodes on dedicated
  hosts.
- `POST /processes/keys` is unauthenticated and stores nothing; its per-IP rate limit only bounds
  CPU. The limit keys on the full peer address, so an IPv6 client can rotate through a /64, and
  behind a reverse proxy every request shares the proxy's address (the node does not read
  `X-Forwarded-For`). If key-generation abuse matters, terminate at a proxy with its own per-client
  limit, or expose the port directly and firewall IPv6 at /64 granularity.

**DKG keys.**

- A threshold of an epoch's committee can decrypt every ballot of the processes keyed on that
  epoch (with `sk_org` as well, in locked mode). The design trusts the threshold not to collude.
- A process's key belongs to one epoch's committee, and there is no resharing. If more than
  n − t of its members leave before the process ends, its results are lost. Operators must keep a
  committee up for the life of every process on it.
- The committee's discrete-log search stops at 2^50 per field. DAVINCI caps results well below
  that.
- Anyone can create DKG-mode processes or register applications on the DKG, and each takes a pool
  key. When an epoch's pool is spent the committee opens the next one at once, and DKG-mode
  creation pauses for one epoch setup. The attacker pays more gas than the committee; a fee or an
  allowlist is the answer if that stops being enough.
- Between the end and the first `requestResultsDecryption`, the organizer can still cancel a ready
  or paused process without having seen the tally, the same power it has in sequencer mode.
  Sequencers request on their first heartbeat after the grace end to keep that window short.
- `revealProcessKey` works at any time. A locked organizer that reveals during voting drops that
  process to the automatic trust model.
- `registrationEpoch` looks back 8 epochs. If all 8 are spent or dead, automatic mode reverts
  `NoLiveEpoch` until a new epoch is live. Locked mode names its epoch and is unaffected.

## Known limitations

- **Blob retention.** Sync and late bootstrap read blobs from the beacon API, which keeps them for
  about 18 days on Ethereum mainnet. A node started after an election's blobs were pruned cannot
  rebuild that election. Nodes archive and serve the blobs they saw, but no node fetches from
  another node's archive yet. Keep a node, or a blob archiver, running for the whole election.
- **Reorgs.** Other sequencers' transitions are applied only once they are `--confirmations`
  blocks deep, but the node's own commits follow the transaction receipt. An own transaction
  whose event never confirms is rolled back after 90 s and its votes requeued. If a fully
  committed own transition is reorged out, the node's copy of that process stays ahead of the
  chain and stops following it.
- **Scalar bias.** Re-encryption scalars are SHA-256 digests reduced modulo the BN254 scalar
  field, which leaves them 2^-7.6 from uniform modulo the subgroup order. Closing the gap needs a
  512-bit reduction, which changes the batch circuit and its program key.
- **Not supported:** key resharing across epochs, fees against pool draining, and the chained
  (folded) proof mode of davinci-zkvm with dynamic censuses or DKG keys.
