import { describe, expect, it } from 'vitest'
import { demoFixture } from '~fixtures/demo'
import type { SequencerInfo, SequencerProcess } from '~protocol/sequencer-api'
import { infoChecks, settledBy, settlers, syncState } from './model'

const fixture = demoFixture()
const store = fixture.store

describe('settlers', () => {
  it('counts every transition once, by sender, busiest first', () => {
    const rows = settlers(store)
    expect(rows.reduce((n, r) => n + r.transitions, 0)).toBe(store.transitionOrder.length)
    for (let i = 1; i < rows.length; i++) expect(rows[i - 1]!.transitions).toBeGreaterThanOrEqual(rows[i]!.transitions)
    const last = Math.max(...store.transitionOrder.map((k) => store.transitions[k]!.block))
    expect(Math.max(...rows.map((r) => r.lastBlock))).toBe(last)
  })

  it('finds a sequencer’s own row, whatever the address case', () => {
    const rows = settlers(store)
    const first = rows[0]!
    expect(settledBy(rows, first.address.toUpperCase().replace('0X', '0x'))).toEqual(first)
    expect(settledBy(rows, null)).toBeNull()
  })

  it('is empty for a registry with no transition', () => {
    expect(settlers({ ...store, transitionOrder: [], transitions: {} })).toEqual([])
  })
})

describe('infoChecks', () => {
  const r = store.chain.registry!
  const info: SequencerInfo = {
    sequencerAddress: null,
    chainId: store.chain.chainId,
    processRegistry: store.chain.registryAddress.toUpperCase().replace('0X', '0x') as `0x${string}`,
    ballotVkHash: r.ballotVKHash,
    batchProgramVk: r.batchProgramVK,
    resultsProgramVk: r.resultsProgramVK,
    observer: true,
    settledBySelf: 0,
    syncedFromOthers: 0,
    lostRaces: 0,
  }

  it('passes for a node of this deployment', () => {
    expect(infoChecks(info, store.chain).every((c) => c.state === 'pass')).toBe(true)
  })

  it('fails the fields of another deployment', () => {
    const checks = infoChecks({ ...info, chainId: 1, batchProgramVk: '0x00' }, store.chain)
    const state = Object.fromEntries(checks.map((c) => [c.id, c.state]))
    expect(state).toMatchObject({ chain: 'fail', 'batch-vk': 'fail', registry: 'pass' })
  })

  it('is unknown before the registry is read', () => {
    const checks = infoChecks(info, { ...store.chain, registry: null })
    expect(checks.find((c) => c.id === 'batch-vk')!.state).toBe('unknown')
  })
})

describe('syncState', () => {
  const view = { localStateRoot: '0xAB' } as unknown as SequencerProcess
  it('compares the node’s root with the registry’s', () => {
    expect(syncState(view, '0xab')).toBe('in-sync')
    expect(syncState(view, '0xcd')).toBe('differs')
    expect(syncState(undefined, '0xab')).toBe('unknown')
    expect(syncState(view, null)).toBe('unknown')
  })
})
