// The glossary. Definitions are plain text with `backtick` code spans, so the
// page can filter them; `id` is the anchor (`#term-<id>`) the guide links to.

import { paths } from '~routes/paths'

export interface GlossaryEntry {
  id: string
  term: string
  text: string
  /** Where the explorer shows it. */
  see?: { label: string; to: string }
}

const contracts = (section: string) => `${paths.contracts()}#${section}`

export const GLOSSARY: GlossaryEntry[] = [
  {
    id: 'accumulator',
    term: 'Accumulator (results)',
    text: 'State leaf `0x04`: 16 ElGamal ciphertexts holding the encrypted sum of every counted ballot, one per ballot field. Each batch adds its new ballots, subtracts the ones they overwrite and adds the encryptions of zero of its silent refreshes. Only the final sum is decrypted by the protocol. The holder of the election key could decrypt any intermediate accumulator or ballot in the blobs.',
    see: { label: 'How results are produced', to: paths.learn('results') },
  },
  {
    id: 'application-id',
    term: 'Application id (aid)',
    text: 'The davinci-dkg application a DKG-mode process registers: `keccak256(chainid, registry, process id) mod Q`, never zero. It scopes the committee’s decryptions to that process.',
  },
  {
    id: 'ballot',
    term: 'Ballot',
    text: '16 ElGamal ciphertexts on the BabyJubJub curve, encrypted under the process key, one per field. Fields beyond the ballot mode’s `numFields` carry the identity.',
  },
  {
    id: 'ballot-mode',
    term: 'Ballot mode',
    text: 'The shape of a valid ballot: how many fields (1 to 16), the minimum and maximum value of a field, the bounds on their sum, whether values must be unique, the group size and the cost exponent. Packed into state leaf `0x02`.',
    see: { label: 'Processes', to: paths.processes() },
  },
  {
    id: 'ballot-proof',
    term: 'Ballot proof',
    text: 'The Groth16 proof, of the davinci-circom ballot circuit, that a ballot is a correct encryption for the process. Its public inputs are the voter’s address, the vote id and an inputs hash over the process id, ballot mode, key, ciphertexts and weight.',
  },
  {
    id: 'ballot-vk-hash',
    term: 'Ballot VK hash',
    text: 'sha256 of the ballot proof’s verification key: a registry immutable, written into every genesis state as leaf `0x07`. The guest only accepts ballot proofs under the key with this hash.',
    see: { label: 'Pinned values', to: contracts('parameters') },
  },
  {
    id: 'batch',
    term: 'Batch',
    text: 'The votes a sequencer proves together as one transition: up to 1024, at most one per ballot slot.',
  },
  {
    id: 'blob',
    term: 'Blob',
    text: 'An EIP-4844 data blob: 4096 cells of 32 bytes carried next to a transaction. A transition’s blobs publish its vote ids, the slots it wrote and the new encrypted tally. Beacon nodes prune them after about 15 days on Gnosis Chain (16384 epochs of 80 s) and about 18 on Ethereum mainnet.',
    see: { label: 'Data availability', to: paths.learn('blobs') },
  },
  {
    id: 'blob-digest',
    term: 'Blob digest',
    text: '`sha256(commitment₀ ‖ y₀ ‖ commitment₁ ‖ y₁ ‖ …)` over a transition’s blobs, where each `y` is the blob’s evaluation at its bound point. The guest publishes it in registers 28 to 35; the registry recomputes it from the transaction.',
  },
  {
    id: 'census',
    term: 'Census',
    text: 'Who may vote and with what weight: a Merkle tree (fixed, updatable by the organizer, or held by a contract) or the signatures of a credential service provider.',
    see: { label: 'Census origins', to: paths.learn('census') },
  },
  {
    id: 'census-origin',
    term: 'Census origin',
    text: 'Which kind of census a process uses: 1 fixed Merkle tree, 2 updatable Merkle tree, 3 on-chain census contract, 4 credential service provider. Chosen at creation, stored in state leaf `0x06`.',
  },
  {
    id: 'census-root',
    term: 'Census root',
    text: 'What a batch’s census proofs lead to: the lean-IMT root of a Merkle census, or the CSP signer’s address. The guest publishes it and the registry compares it with the roots the process accepts.',
  },
  {
    id: 'chaum-pedersen-proof',
    term: 'Chaum–Pedersen proof',
    text: 'A proof that a value is the correct decryption of a ciphertext under a public key, without revealing the secret. The results guest checks 16 of them; each DKG partial decryption proves the same relation for one member’s share.',
  },
  {
    id: 'committee',
    term: 'Committee',
    text: 'The davinci-dkg operators of one epoch, drawn by an on-chain lottery. A threshold of them can decrypt under the epoch’s pool keys; fewer cannot.',
    see: { label: 'The DKG committee', to: contracts('dkg') },
  },
  {
    id: 'csp',
    term: 'CSP (credential service provider)',
    text: 'Census origin 4: an authority that signs each voter’s credential with an ECDSA (secp256k1) key. Its address is the census root; the signed index sets the voter’s slot.',
  },
  {
    id: 'dkg',
    term: 'DKG',
    text: 'Distributed key generation. davinci-dkg’s committee jointly generates keys no single member knows and decrypts under them on demand, each step proven with Groth16 on chain.',
  },
  {
    id: 'epoch',
    term: 'Epoch',
    text: 'One DKG run: a committee is drawn and deals 16 pool keys; once finalized the epoch is Live and applications can claim keys. Epochs are created at a fixed cadence, or early when the newest pool is nearly spent or the epoch aborted.',
    see: { label: 'The DKG committee', to: contracts('dkg') },
  },
  {
    id: 'fail-mask',
    term: 'Fail mask',
    text: 'Register 1 of the public values: one bit per kind of check the guest failed. The registry requires `ok = 1` and a zero fail mask.',
  },
  {
    id: 'genesis-root',
    term: 'Genesis root',
    text: 'The state root before the first transition, computed by the registry at creation from six leaves: the process id, the ballot mode, the encryption key hash, an empty accumulator, the census origin and the ballot VK hash.',
  },
  {
    id: 'guest',
    term: 'Guest',
    text: 'A program the zkVM proves. The vote-batch guest proves a transition; the results guest proves a sequencer-key tally.',
  },
  {
    id: 'key-mode',
    term: 'Key mode',
    text: 'Who holds the key the ballots are encrypted under: one sequencer, a davinci-dkg committee (automatic), or the committee plus an organizer secret (locked).',
    see: { label: 'Key modes', to: paths.learn('key-modes') },
  },
  {
    id: 'kzg-commitment',
    term: 'KZG commitment',
    text: 'A 48-byte commitment to a blob’s polynomial. The versioned hash is derived from it, and the point-evaluation precompile checks an opening of the blob against it.',
  },
  {
    id: 'observer',
    term: 'Observer',
    text: 'A sequencer node without a key: it follows every process, replays every transition from its blobs and serves reads and tracker proofs, but never settles.',
    see: { label: 'Sequencers', to: paths.sequencers() },
  },
  {
    id: 'occupied-before',
    term: 'occupied_before',
    text: 'Register 42: how many ballot slots were written before the batch. The guest cannot see the tree, so the registry checks it against its own voter count.',
  },
  {
    id: 'organizer',
    term: 'Organizer',
    text: 'The account that created a process. Only it can change the process’s status, extend its duration, change its voter limit or, for an updatable census, replace the census.',
  },
  {
    id: 'organizer-secret',
    term: 'Organizer secret',
    text: 'In DKG locked mode, the organizer’s half of the process key. The committee cannot decrypt until it is revealed, and losing it loses the results.',
  },
  {
    id: 'overwrite',
    term: 'Overwrite (revote)',
    text: 'A vote for a slot that already holds a ballot. The new ballot replaces the old one in the tally. The blob does not show which occupied slot it was: an overwrite and a silent refresh look the same.',
    see: { label: 'Silent revoting', to: paths.learn('silent-revoting') },
  },
  {
    id: 'plonk-proof',
    term: 'PLONK proof',
    text: 'The succinct proof the registry verifies: a guest’s ZisK proof, wrapped. 768 bytes of proof and 512 bytes of public values at every batch size.',
  },
  {
    id: 'pool-key',
    term: 'Pool key',
    text: 'One of the 16 keys an epoch’s committee deals. Each application, and so each DKG-mode process, claims one.',
  },
  {
    id: 'process',
    term: 'Process',
    text: 'One election on the registry: its parameters, census, key and state root while it runs, and its results at the end.',
    see: { label: 'Processes', to: paths.processes() },
  },
  {
    id: 'process-id',
    term: 'Process id',
    text: 'A `bytes31`: the organizer’s address (20 bytes), the registry’s 4-byte prefix and a 7-byte per-organizer nonce. An id from another registry or chain is refused.',
    see: { label: 'Processes', to: paths.processes() },
  },
  {
    id: 'program-vk',
    term: 'Program vk',
    text: 'The verification key of a guest program: what `cargo-zisk setup` prints as its root hash. The registry pins one for the vote-batch guest and one for the results guest, and every proof is checked against its program’s.',
    see: { label: 'Pinned values', to: contracts('parameters') },
  },
  {
    id: 'public-values',
    term: 'Public values (publics)',
    text: 'The guest’s 64 output registers, each as an 8-byte little-endian word: 512 bytes holding the roots, counts, census root, blob digest and verdict. The verifier binds the proof to them.',
  },
  {
    id: 're-encryption',
    term: 'Re-encryption',
    text: 'Adding an encryption of zero with a fresh scalar to a ciphertext: the same plaintext, a new ciphertext. Every stored ballot is re-encrypted with scalars derived from the batch’s secret seed. Without the seed nobody can match a sent ballot to the stored one; it does not hide whose slot it is.',
    see: { label: 'Silent revoting', to: paths.learn('silent-revoting') },
  },
  {
    id: 'root-c-vadcop-final',
    term: 'rootCVadcopFinal',
    text: 'The root of the ZisK vadcop-final setup the proofs are wrapped with, pinned by the registry next to the program vks. It changes with the ZisK setup, not with the guests.',
    see: { label: 'Pinned values', to: contracts('parameters') },
  },
  {
    id: 'sequencer',
    term: 'Sequencer',
    text: 'A node that collects ballots, proves them in batches and settles them on the registry. Settlement is permissionless, so any number can serve a process.',
    see: { label: 'Sequencers', to: paths.sequencers() },
  },
  {
    id: 'silent-refresh',
    term: 'Silent refresh',
    text: 'A re-encryption of an occupied slot the batch did not write. Every batch carries enough of them that an overwrite looks like routine noise. A slot’s first write stays public, since refreshes only touch occupied slots.',
    see: { label: 'Silent revoting', to: paths.learn('silent-revoting') },
  },
  {
    id: 'slot',
    term: 'Slot',
    text: 'The key of a voter’s ballot in the state tree, between `0x10` and 2^63: derived from the voter’s address for a Merkle census, `0x10` plus the signed index for a CSP.',
    see: { label: 'Ballot slots', to: paths.learn('census') },
  },
  {
    id: 'state-root',
    term: 'State root',
    text: 'The root of a process’s state tree, a SHA-256 sparse Merkle tree of 64 levels holding the process configuration, every vote id, every ballot and the encrypted tally. The registry keeps the latest one; each transition moves it.',
  },
  {
    id: 'state-transition',
    term: 'State transition',
    text: 'One settled batch: the root before, the root after, the proof and the blobs, sent in one `submitStateTransition` transaction.',
    see: { label: 'What the registry checks', to: paths.learn('settlement') },
  },
  {
    id: 'threshold',
    term: 'Threshold',
    text: 'How many committee members it takes to decrypt, `t` of `n`. The design trusts that no threshold of the committee colludes.',
  },
  {
    id: 'tracker-proof',
    term: 'Tracker proof',
    text: 'The path from a vote id’s leaf to a state root. When that root is one the registry held for the process, the vote was recorded as cast.',
    see: { label: 'Vote lookup', to: paths.votes() },
  },
  {
    id: 'versioned-hash',
    term: 'Versioned hash',
    text: 'What the chain keeps of a blob: `0x01` followed by the last 31 bytes of `sha256(commitment)`. The `BLOBHASH` opcode returns it, and the registry checks each blob against it.',
  },
  {
    id: 'vote-id',
    term: 'Vote id',
    text: 'A 64-bit identifier of one ballot, at least 2^63, derived from a hash by the voter’s client. The voter signs it, the ballot proof takes it as a public input, and a settled transition inserts it in the state tree and publishes it in its blob.',
    see: { label: 'Vote lookup', to: paths.votes() },
  },
  {
    id: 'voters-count',
    term: 'Voters count',
    text: 'The distinct ballot slots written so far, `votersCount` on the registry. Overwrites are counted apart, in `overwrittenVotesCount`.',
  },
  {
    id: 'zkvm',
    term: 'zkVM (ZisK)',
    text: 'A virtual machine whose executions can be proven. DAVINCI’s guests are RISC-V programs proven with ZisK and wrapped in PLONK for the chain.',
  },
]

/** Entries whose term or text contains every word of `query`, alphabetical. */
export function filterGlossary(query: string, entries: GlossaryEntry[] = GLOSSARY): GlossaryEntry[] {
  const words = query.toLowerCase().split(/\s+/).filter(Boolean)
  return entries
    .filter((e) => {
      const hay = `${e.term} ${e.text}`.toLowerCase()
      return words.every((w) => hay.includes(w))
    })
    .sort((a, b) => a.term.localeCompare(b.term, 'en', { sensitivity: 'base' }))
}
