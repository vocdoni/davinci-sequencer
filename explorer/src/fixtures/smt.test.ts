import { describe, expect, it } from 'vitest'
import { verifyTracker } from '~protocol/tracker'
import { VOTE_ID_MIN } from '~protocol/limits'
import { voteIdTreeProof, voteIdTreeRoot } from './smt'

describe('vote-id tree', () => {
  it('produces proofs the tracker verifier accepts', () => {
    const keys = Array.from({ length: 37 }, (_, i) => VOTE_ID_MIN + BigInt(i * 7919 + 13))
    const root = voteIdTreeRoot(keys)
    for (const key of keys.slice(0, 10)) {
      const siblings = voteIdTreeProof(keys, key)
      expect(verifyTracker({ processId: '0x00', voteId: key, root, siblings }, root)).toBe(true)
    }
    const siblings = voteIdTreeProof(keys, keys[0]!)
    expect(verifyTracker({ processId: '0x00', voteId: keys[1]!, root, siblings }, root)).toBe(false)
  })
})
