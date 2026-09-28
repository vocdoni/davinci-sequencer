// A sparse Merkle tree over vote ids only, with the arbo rules the tracker
// proof uses: leaf sha256(vid_le8 ‖ 0^32 ‖ 0x01), node sha256(l ‖ r), empty
// subtree 0^32, a lone leaf sits at the depth where its path becomes unique,
// path bits LSB-first. The demo network uses it so its roots and tracker
// proofs verify with the real `verifyTracker`.

import { sha256 } from 'viem'
import { concatBytes, toHex, type Hex } from '~protocol/bytes'
import { voteIdLeaf } from '~protocol/tracker'

const ZERO = new Uint8Array(32)

function node(keys: bigint[], depth: number, leaves: Map<bigint, Uint8Array>): Uint8Array {
  if (keys.length === 0) return ZERO
  if (keys.length === 1) return leaves.get(keys[0]!)!
  const bit = BigInt(depth)
  const left = keys.filter((k) => ((k >> bit) & 1n) === 0n)
  const right = keys.filter((k) => ((k >> bit) & 1n) === 1n)
  return sha256(concatBytes(node(left, depth + 1, leaves), node(right, depth + 1, leaves)), 'bytes')
}

function leavesOf(keys: bigint[]): Map<bigint, Uint8Array> {
  return new Map(keys.map((k) => [k, voteIdLeaf(k)]))
}

export function voteIdTreeRoot(keys: bigint[]): Hex {
  return toHex(node(keys, 0, leavesOf(keys)))
}

/** Siblings from the root down to `key`'s leaf. Throws when `key` is not in the tree. */
export function voteIdTreeProof(keys: bigint[], key: bigint): Hex[] {
  if (!keys.includes(key)) throw new Error('vote id not in the tree')
  const leaves = leavesOf(keys)
  const siblings: Hex[] = []
  let set = keys
  let depth = 0
  while (set.length > 1) {
    const bit = BigInt(depth)
    const mine = (key >> bit) & 1n
    const same = set.filter((k) => ((k >> bit) & 1n) === mine)
    const other = set.filter((k) => ((k >> bit) & 1n) !== mine)
    siblings.push(toHex(node(other, depth + 1, leaves)))
    set = same
    depth += 1
  }
  return siblings
}
