import { describe, expect, it } from 'vitest'
import { demoFixture } from '~fixtures/demo'
import { processRow, transitionRows } from '~indexer/selectors'
import type { BallotMode } from '~indexer/types'
import { B8, isOnCurve } from '~protocol/babyjubjub'
import { BN254_FR } from '~protocol/limits'
import { describeBallotMode } from './ballot-mode'
import { BJJ_K, BJJ_K_INV, reducedToCircom } from '~protocol/babyjubjub'
import { dkgApplicationUrl, dkgEpochUrl } from './dkg-links'
import { toJson } from './json'
import { browsableUri, localized, metadataChoices, metadataTitle } from './metadata'
import { tallyRows } from './tally'
import { processLifecycle } from './timeline'

const mode = (over: Partial<BallotMode>): BallotMode => ({
  uniqueValues: false,
  numFields: 4,
  groupSize: 0,
  costExponent: 1,
  maxValue: 1n,
  minValue: 0n,
  maxValueSum: 1n,
  minValueSum: 0n,
  ...over,
})

describe('describeBallotMode', () => {
  it('single choice, blank allowed or not', () => {
    expect(describeBallotMode(mode({})).kind).toBe('single-choice')
    expect(describeBallotMode(mode({})).summary).toContain('or none')
    expect(describeBallotMode(mode({ minValueSum: 1n })).summary).toBe('Each voter picks exactly one of 4 options.')
  })

  it('multiple choice and approval', () => {
    expect(describeBallotMode(mode({ maxValueSum: 2n })).summary).toBe('Each voter picks up to 2 of 4 options.')
    expect(describeBallotMode(mode({ maxValueSum: 4n })).kind).toBe('approval')
    expect(describeBallotMode(mode({ maxValueSum: 0n })).summary).toContain('census weight')
  })

  it('points, quadratic and ranking', () => {
    const points = describeBallotMode(mode({ maxValue: 10n, maxValueSum: 20n }))
    expect(points.kind).toBe('points')
    expect(points.summary).toBe('Each voter distributes up to 20 points among 4 options, at most 10 per option.')
    const quad = describeBallotMode(mode({ costExponent: 2, maxValue: 10n, maxValueSum: 100n }))
    expect(quad.kind).toBe('quadratic')
    expect(quad.summary).toContain('v² credits')
    expect(quad.rules.join(' ')).toContain('sum of the squares of the values is at most 100')
    const rank = describeBallotMode(mode({ uniqueValues: true, minValue: 1n, maxValue: 4n, maxValueSum: 10n }))
    expect(rank.kind).toBe('ranking')
    expect(rank.rules).toContain('No two fields may carry the same value.')
  })

  it('a budget that never binds reads as a rating', () => {
    const rating = describeBallotMode(mode({ maxValue: 5n, maxValueSum: 20n }))
    expect(rating.kind).toBe('rating')
    expect(rating.summary).toBe('Each voter rates each of 4 options from 0 to 5.')
    expect(describeBallotMode(mode({ costExponent: 2, maxValue: 3n, maxValueSum: 36n })).kind).toBe('rating')
    expect(describeBallotMode(mode({ costExponent: 2, maxValue: 3n, maxValueSum: 35n })).kind).toBe('quadratic')
  })

  it('unique values with fewer allowed values than fields cannot be satisfied', () => {
    const d = describeBallotMode(mode({ uniqueValues: true }))
    expect(d.kind).toBe('unsatisfiable')
    expect(d.label).toBe('Cannot be satisfied')
    expect(d.summary).toBe(
      'No ballot can meet these rules: the 4 fields must all differ, but only 2 values are allowed.'
    )
    expect(describeBallotMode(mode({ uniqueValues: true, numFields: 2 })).kind).toBe('ranking')
  })

  it('always states the bounds and the tally rule', () => {
    const d = describeBallotMode(mode({ numFields: 1, maxValue: 5n, maxValueSum: 5n, groupSize: 2 }))
    expect(d.kind).toBe('custom')
    expect(d.rules[0]).toContain('between 0 and 5')
    expect(d.rules.some((r) => r.includes('groups of 2'))).toBe(true)
    expect(describeBallotMode(mode({ maxValue: 5n, maxValueSum: 0n })).summary).toBe(
      'Each voter distributes as many points as their census weight among 4 options, at most 5 per option.'
    )
    expect(d.rules[d.rules.length - 1]).toContain('sum of the values voters gave it')
  })
})

describe('metadata', () => {
  const doc = {
    title: { default: 'Board election' },
    questions: [
      {
        title: 'Who?',
        choices: [
          { title: { default: 'B' }, value: 1 },
          { title: { default: 'A' }, value: 0 },
        ],
      },
    ],
  }

  it('reads titles and choices', () => {
    expect(metadataTitle(doc)).toBe('Board election')
    expect(localized({ ca: 'Hola' })).toBe('Hola')
    expect(localized(3)).toBeNull()
    expect(metadataChoices(doc, 2)).toEqual(['A', 'B'])
    expect(metadataChoices(doc, 3)).toBeNull()
    expect(metadataChoices('nope', 2)).toBeNull()
  })

  it('links only what a browser opens', () => {
    expect(browsableUri('ipfs://bafy')).toBe('https://ipfs.io/ipfs/bafy')
    expect(browsableUri('https://x.org/a.json')).toBe('https://x.org/a.json')
    expect(browsableUri('javascript:alert(1)')).toBeNull()
  })
})

describe('small helpers', () => {
  it('toJson writes bigints as decimal strings', () => {
    expect(toJson({ a: 10n ** 30n, b: [1n] })).toBe(
      '{\n  "a": "1000000000000000000000000000000",\n  "b": [\n    "1"\n  ]\n}'
    )
  })

  it('tallyRows shares and bar lengths', () => {
    const rows = tallyRows([1n, 3n, 0n], ['x', 'y', 'z'])
    expect(rows.map((r) => r.share)).toEqual([0.25, 0.75, 0])
    expect(rows.map((r) => r.ofMax)).toEqual([0.333333, 1, 0])
    expect(tallyRows([0n, 0n])[1]).toMatchObject({ label: 'Field 2', share: 0, ofMax: 0 })
  })

  it('dkg links', () => {
    expect(dkgEpochUrl('https://dkg.example.org/', '0xAB')).toBe('https://dkg.example.org/epochs/0xab')
    expect(dkgApplicationUrl('https://d', '0xE', '0xA')).toBe('https://d/applications/0xe/0xa')
    expect(dkgApplicationUrl(undefined, '0xe', '0xa')).toBeNull()
  })
})

describe('BabyJubJub forms', () => {
  it('K matches BjjFormLib', () => {
    expect((BJJ_K * BJJ_K) % BN254_FR).toBe(BN254_FR - 168700n)
    expect((BJJ_K * BJJ_K_INV) % BN254_FR).toBe(1n)
  })

  it('maps a reduced point onto the circomlib curve', () => {
    const te = B8
    const reduced = { x: (te.x * BJJ_K) % BN254_FR, y: te.y }
    expect(reducedToCircom(reduced)).toEqual(te)
    expect(isOnCurve(reducedToCircom(reduced))).toBe(true)
  })
})

describe('processLifecycle', () => {
  const f = demoFixture()
  const now = f.store.chain.headTimestamp
  const lifecycle = (pid: string) => {
    const process = f.store.processes[pid]!
    return processLifecycle(
      { process, row: processRow(f.store, process), transitions: transitionRows(f.store, pid) },
      now
    )
  }
  const states = (pid: string) => lifecycle(pid).map((s) => s.state)

  it('an open process is collecting transitions', () => {
    expect(states(f.featured.openProcess)).toEqual(['done', 'done', 'current', 'upcoming', 'upcoming'])
  })

  it('a process with results is done throughout', () => {
    const steps = lifecycle(f.featured.resultsProcess)
    expect(steps.map((s) => s.state)).toEqual(['done', 'done', 'done', 'done', 'done'])
    expect(steps[4]!.tx).toBe(f.store.processes[f.featured.resultsProcess]!.results!.tx)
  })

  it('a tally waiting for the reveal shows the decryption request', () => {
    const steps = lifecycle(f.featured.awaitingReveal)
    expect(steps[4]).toMatchObject({ state: 'current', detail: 'Decryption requested' })
  })

  it('a canceled process skips the end and the results', () => {
    const canceled = f.store.processOrder.find((k) => f.store.processes[k]!.state?.status === 'canceled')!
    expect(states(canceled).slice(3)).toEqual(['skipped', 'skipped'])
  })
})
