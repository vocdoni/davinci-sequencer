import { describe, expect, it } from 'vitest'
import { sha256 } from 'viem'
import { concatBytes, toHex } from './bytes'
import { VOTE_ID_MIN } from './limits'
import { verifyTracker, voteIdLeaf } from './tracker'

describe('verifyTracker', () => {
  const a = VOTE_ID_MIN | 2n // bit 0 = 0
  const b = VOTE_ID_MIN | 3n // bit 0 = 1

  it('accepts a single-leaf tree (root = leaf)', () => {
    const root = toHex(voteIdLeaf(a))
    expect(verifyTracker({ processId: '0x00', voteId: a, root, siblings: [] }, root)).toBe(true)
  })

  it('walks a two-leaf tree on bit 0', () => {
    const la = voteIdLeaf(a)
    const lb = voteIdLeaf(b)
    const root = toHex(sha256(concatBytes(la, lb), 'bytes'))
    expect(verifyTracker({ processId: '0x00', voteId: a, root, siblings: [toHex(lb)] }, root)).toBe(true)
    expect(verifyTracker({ processId: '0x00', voteId: b, root, siblings: [toHex(la)] }, root)).toBe(true)
    // The wrong side, a different root, or an id outside the namespace all fail.
    expect(verifyTracker({ processId: '0x00', voteId: b, root, siblings: [toHex(lb)] }, root)).toBe(false)
    expect(verifyTracker({ processId: '0x00', voteId: a, root, siblings: [toHex(lb)] }, toHex(la))).toBe(false)
    expect(verifyTracker({ processId: '0x00', voteId: 2n, root, siblings: [toHex(lb)] }, root)).toBe(false)
  })
})
