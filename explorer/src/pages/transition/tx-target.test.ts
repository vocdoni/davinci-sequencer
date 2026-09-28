import { describe, expect, it } from 'vitest'
import { demoFixture } from '~fixtures/demo'
import { paths } from '~routes/paths'
import { isTxHash, txTarget } from './tx-target'

const { store, featured } = demoFixture()

describe('txTarget', () => {
  it('sends a settlement to its transition', () => {
    const t = store.transitions[store.transitionOrder[3]!]!
    expect(txTarget(store, t.tx!.toUpperCase().replace('0X', '0x'))).toBe(paths.transition(t.processId, t.index))
  })

  it('sends a creation to the process and results to its tab', () => {
    const p = store.processes[featured.resultsProcess]!
    expect(txTarget(store, p.createdTx!)).toBe(paths.process(p.id))
    expect(txTarget(store, p.results!.tx!)).toBe(paths.process(p.id, 'results'))
  })

  it('sends any other registry event to its process', () => {
    const ev = store.events.find((e) => {
      const p = store.processes[e.processId]!
      return e.name === 'ProcessStatusChanged' && e.tx !== p.results?.tx && e.tx !== p.decryptionRequest?.tx
    })
    expect(ev).toBeDefined()
    expect(txTarget(store, ev!.tx!)).toBe(paths.process(ev!.processId))
  })

  it('knows nothing else', () => {
    expect(txTarget(store, `0x${'ab'.repeat(32)}`)).toBeNull()
  })
})

describe('isTxHash', () => {
  it('wants 32 bytes of hex', () => {
    expect(isTxHash(`0x${'ab'.repeat(32)}`)).toBe(true)
    expect(isTxHash('0xabc')).toBe(false)
    expect(isTxHash(`${'ab'.repeat(32)}`)).toBe(false)
  })
})
