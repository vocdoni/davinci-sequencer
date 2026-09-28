import { describe, expect, it } from 'vitest'
import { decodeTransitionBlobs, versionedHash } from '~protocol/blob'
import { decodeBatchPublicValues } from '~protocol/publics'
import { verifyTracker } from '~protocol/tracker'
import { networkStats, rootChain, transitionDetail } from '~indexer/selectors'
import { buildFixture, demoTransitionBlobs } from './synthetic'
import { createDemoServices, demoFixture } from './demo'

const fixture = demoFixture()
const { store } = fixture

describe('synthetic network', () => {
  it('is deterministic', () => {
    const again = buildFixture()
    expect(again.store.processOrder).toEqual(store.processOrder)
    expect(again.store.transitionOrder.length).toBe(store.transitionOrder.length)
    const key = store.transitionOrder[5]!
    expect(again.store.transitions[key]).toEqual(store.transitions[key])
  })

  it('covers every status, key mode and census origin', () => {
    const stats = networkStats(store)
    for (const status of ['ready', 'ended', 'canceled', 'paused', 'results'] as const) {
      expect(stats.byStatus[status], status).toBeGreaterThan(0)
    }
    for (const phase of ['upcoming', 'open', 'closed', 'paused', 'ended', 'canceled', 'results'] as const) {
      expect(stats.byPhase[phase], phase).toBeGreaterThan(0)
    }
    for (const mode of ['sequencer', 'dkg-automatic', 'dkg-locked'] as const) {
      expect(stats.byKeyMode[mode], mode).toBeGreaterThan(0)
    }
    for (const origin of ['merkle-static', 'merkle-dynamic', 'onchain-dynamic', 'csp'] as const) {
      expect(stats.byCensusOrigin[origin], origin).toBeGreaterThan(0)
    }
    expect(stats.transitions).toBeGreaterThan(50)
    expect(stats.withResults).toBeGreaterThan(1)
  })

  it('keeps every root chain continuous up to the registry root', () => {
    for (const pid of store.processOrder) {
      const chain = rootChain(store, pid)
      expect(chain.gaps, pid).toBe(0)
      if (chain.links.length > 0) expect(chain.headMatches, pid).toBe(true)
    }
  })

  it('carries publics that pass every settlement check', () => {
    for (const key of store.transitionOrder) {
      const t = store.transitions[key]!
      const d = transitionDetail(store, t.processId, t.index)!
      for (const c of d.checks) {
        if (c.id === 'census-root' && d.process.state?.census.origin === 'onchain-dynamic') continue
        expect(c.state, `${key} ${c.id}`).toBe('pass')
      }
    }
  })

  it('generates blobs the real decoder accepts', () => {
    const { processId, index } = fixture.featured.multiBlob
    const data = fixture.transitionData.get(`${processId}:${index}`)!
    const blobs = demoTransitionBlobs(data)
    const t = store.transitions[`${processId}:${index}`]!
    expect(blobs.length).toBe(t.nBlobs)
    expect(blobs.length).toBeGreaterThan(1)
    const decoded = decodeTransitionBlobs(blobs, data.numFields)
    expect(decoded.voteIds.length).toBe(t.newVoters + t.overwrites)
    const publics = decodeBatchPublicValues(store.txDetails[t.tx!]!.publicValues!)
    expect(decoded.voteIds.length).toBe(publics.voters)
  })

  it('has a DKG-locked process whose tally waits for the reveal', () => {
    const pid = fixture.featured.awaitingReveal
    const p = store.processes[pid]!
    expect(p.state?.keyMode).toBe('dkg-locked')
    expect(p.decryptionRequest).not.toBeNull()
    expect(p.results).toBeNull()
    const app = fixture.dkg.get(pid)!
    expect(app.organizerSecret).toBe(0n)
    expect(app.ciphertexts.every((c) => !c.completed)).toBe(true)
  })

  it('serves tracker proofs that verify against the on-chain root', async () => {
    const services = createDemoServices(fixture)
    const { processId, voteId } = fixture.featured.settledVote
    const proof = await services.sequencers[0]!.api.trackerProof(processId, voteId)
    expect(proof.root).toBe(store.processes[processId]!.state!.latestStateRoot)
    expect(verifyTracker(proof, proof.root)).toBe(true)
    expect(await services.sequencers[0]!.api.voteStatus(processId, voteId)).toEqual({ status: 'settled' })
  })

  it('ties each blob to its transaction by versioned hash', () => {
    for (const key of store.transitionOrder.slice(0, 20)) {
      const tx = store.txDetails[store.transitions[key]!.tx!]!
      expect(tx.commitments.map((c) => versionedHash(c))).toEqual(tx.blobVersionedHashes)
    }
  })
})
