import { describe, expect, it } from 'vitest'
import { addPoints, B8, compressPoint, IDENTITY, type Point } from '~protocol/babyjubjub'
import { blobsFromCells, decodeTransitionBlobs, transitionCells, type Ciphertext } from '~protocol/blob'
import { bigIntToBe, toHex } from '~protocol/bytes'
import { CELLS_PER_BLOB, NUM_FIELDS, VOTE_ID_MIN } from '~protocol/limits'
import { cellHex, countsOf, describeCell, sectionSpans, sectionStarts } from './cells'

function multiples(n: number): Point[] {
  const out: Point[] = []
  let p = B8
  for (let i = 0; i < n; i++) {
    out.push(p)
    p = addPoints(p, B8)
  }
  return out
}

const pts = multiples(12)
const ct = (i: number): Ciphertext => ({ c1: pts[i % pts.length]!, c2: pts[(i + 1) % pts.length]! })
const ballot = (seed: number): Ciphertext[] =>
  Array.from({ length: NUM_FIELDS }, (_, f) => (f < 2 ? ct(seed + f) : { c1: IDENTITY, c2: IDENTITY }))

const input = {
  voteIds: [VOTE_ID_MIN + 9n, VOTE_ID_MIN + 3n, VOTE_ID_MIN + 5n],
  updates: [
    { key: 0x40n, ballot: ballot(1) },
    { key: 0x11n, ballot: ballot(4) },
  ],
  accumulator: ballot(7),
  numFields: 2,
}
const blobs = blobsFromCells(transitionCells(input))
const decoded = decodeTransitionBlobs(blobs, 2)
const counts = countsOf(decoded)
const enc = (v: bigint) => toHex(bigIntToBe(v, 32))

describe('describeCell', () => {
  it('labels every used cell with what the guest put there', () => {
    const { used } = sectionStarts(counts)
    expect(used).toBe(2 + 3 + 2 * (1 + 4) + 4)
    for (let i = 0; i < used; i++) {
      const info = describeCell(counts, i)
      const hex = cellHex(blobs, i)
      switch (info.kind) {
        case 'vote-id-count':
          expect(hex).toBe(enc(3n))
          break
        case 'vote-id':
          expect(hex).toBe(enc(decoded.voteIds[info.item!]!))
          break
        case 'update-count':
          expect(hex).toBe(enc(2n))
          break
        case 'slot-key':
          expect(hex).toBe(enc(decoded.updates[info.item!]!.key))
          break
        case 'ballot-c1':
        case 'ballot-c2':
          expect(hex).toBe(decoded.updates[info.item!]!.cells[info.field! * 2 + (info.kind === 'ballot-c1' ? 0 : 1)])
          break
        case 'acc-c1':
          expect(hex).toBe(toHex(compressPoint(decoded.accumulator[info.field!]!.c1)))
          break
        case 'acc-c2':
          expect(hex).toBe(toHex(compressPoint(decoded.accumulator[info.field!]!.c2)))
          break
        default:
          throw new Error(`unexpected ${info.kind} at ${i}`)
      }
    }
  })

  it('reads sorted data and zero padding', () => {
    expect(describeCell(counts, 1).label).toBe('Vote id #0')
    expect(cellHex(blobs, 1)).toBe(enc(VOTE_ID_MIN + 3n))
    expect(describeCell(counts, 5)).toMatchObject({ kind: 'slot-key', item: 0 })
    expect(cellHex(blobs, 5)).toBe(enc(0x11n))
    expect(describeCell(counts, 7)).toMatchObject({ kind: 'ballot-c2', item: 0, field: 0 })
    const pad = describeCell(counts, 100)
    expect(pad).toMatchObject({ kind: 'padding', section: 'padding' })
    expect(cellHex(blobs, 100)).toBe(enc(0n))
    expect(cellHex(blobs, CELLS_PER_BLOB)).toBeNull()
  })

  it('places cells in their blob', () => {
    expect(describeCell(counts, CELLS_PER_BLOB + 3)).toMatchObject({ blob: 1, offset: 3 })
  })
})

describe('sectionSpans', () => {
  it('covers the blobs without gaps', () => {
    const spans = sectionSpans(counts, CELLS_PER_BLOB)
    expect(spans.map((s) => s.section)).toEqual(['vote-ids', 'updates', 'accumulator', 'padding'])
    for (let i = 1; i < spans.length; i++) expect(spans[i]!.start).toBe(spans[i - 1]!.end)
    expect(spans[spans.length - 1]!.end).toBe(CELLS_PER_BLOB)
  })

  it('drops empty sections', () => {
    const none = { voteIds: 0, updates: 0, numFields: 1 }
    expect(sectionSpans(none, 4).map((s) => s.section)).toEqual(['vote-ids', 'updates', 'accumulator'])
  })
})
