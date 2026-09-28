// Tracker proofs: a sequencer's proof that a vote id is a leaf of the state
// tree whose root is on-chain. Port of davinci-sequencer
// `client/src/api.rs::verify_tracker`: leaf sha256(vid_le8 ‖ 0^32 ‖ 0x01),
// nodes sha256(l ‖ r), siblings root to leaf, path bits LSB-first.

import { sha256 } from 'viem'
import { concatBytes, equalBytes, toBytes, type Hex } from './bytes'
import { SMT_LEVELS, VOTE_ID_MIN } from './limits'

export interface TrackerProof {
  processId: Hex
  voteId: bigint
  /** Raw arbo root, as the registry stores it. */
  root: Hex
  siblings: Hex[]
}

function le8(v: bigint): Uint8Array {
  const out = new Uint8Array(8)
  let x = v
  for (let i = 0; i < 8; i++) {
    out[i] = Number(x & 0xffn)
    x >>= 8n
  }
  return out
}

/** Leaf hash of a vote id (value 0). */
export function voteIdLeaf(voteId: bigint): Uint8Array {
  return sha256(concatBytes(le8(voteId), new Uint8Array(32), Uint8Array.of(1)), 'bytes')
}

/** The root a tracker proof computes, or null when its shape is invalid. */
export function trackerRoot(proof: TrackerProof): Uint8Array | null {
  if (proof.siblings.length > SMT_LEVELS || proof.voteId < VOTE_ID_MIN) return null
  let node = voteIdLeaf(proof.voteId)
  for (let i = proof.siblings.length - 1; i >= 0; i--) {
    const s = toBytes(proof.siblings[i]!)
    node =
      ((proof.voteId >> BigInt(i)) & 1n) === 1n
        ? sha256(concatBytes(s, node), 'bytes')
        : sha256(concatBytes(node, s), 'bytes')
  }
  return node
}

/** Recorded-as-cast: true when the proof reaches `onchainRoot`. */
export function verifyTracker(proof: TrackerProof, onchainRoot: Hex): boolean {
  const root = toBytes(onchainRoot)
  if (!equalBytes(toBytes(proof.root), root)) return false
  const computed = trackerRoot(proof)
  return computed != null && equalBytes(computed, root)
}
