import { describe, expect, it } from 'vitest'
import { loadJsonBig } from '../test/vectors'
import { B8, addPoints, compressPoint, decompressPoint, IDENTITY, isOnCurve, type Point } from './babyjubjub'
import {
  blobCount,
  blobEvaluationPoint,
  blobsDigest,
  blobsFromCells,
  decodeTransitionBlobs,
  formatVoteId,
  parseVoteId,
  transitionCells,
  unpackBallot,
  versionedHash,
  type Ciphertext,
} from './blob'
import { bigIntToBe, toBytes, toHex } from './bytes'
import { BLOB_SIZE, VOTE_ID_MIN } from './limits'

interface Case {
  nf: string | number
  process_id: string
  root_before: string
  vote_ids: string[]
  updates: Array<{ key: number | string; ballot: string[] }>
  accumulator: string[]
  cells: string[]
  commitments: string[]
  versioned_hashes: string[]
  zs: string[]
  ys: string[]
  digest: string
}

const cases = loadJsonBig<Case[]>('blob.json')

const ballot = (coords: string[]): Ciphertext[] => {
  const out: Ciphertext[] = []
  for (let i = 0; i < 16; i++) {
    out.push({
      c1: { x: BigInt(coords[4 * i]!), y: BigInt(coords[4 * i + 1]!) },
      c2: { x: BigInt(coords[4 * i + 2]!), y: BigInt(coords[4 * i + 3]!) },
    })
  }
  return out
}

const hex = (s: string) => (s.startsWith('0x') ? s : `0x${s}`) as `0x${string}`
const cellsOf = (c: Case) => c.cells.map((x) => toBytes(hex(x)))
const pid = (c: Case) => toHex(bigIntToBe(BigInt(c.process_id), 31))

describe('DA blob codec (rust-sdk vectors)', () => {
  it('lays out the same cells as the guest', () => {
    for (const c of cases) {
      const nf = Number(c.nf)
      const cells = transitionCells({
        voteIds: c.vote_ids.map(BigInt),
        updates: c.updates.map((u) => ({ key: BigInt(u.key), ballot: ballot(u.ballot) })),
        accumulator: ballot(c.accumulator),
        numFields: nf,
      })
      expect(cells.map((x) => toHex(x).slice(2))).toEqual(c.cells)
      expect(blobCount(c.vote_ids.length, c.updates.length, nf)).toBe(c.commitments.length)
    }
  })

  it('decodes the blobs back to the sorted transition', () => {
    let multi = false
    for (const c of cases) {
      const nf = Number(c.nf)
      const blobs = blobsFromCells(cellsOf(c))
      multi ||= blobs.length > 1
      expect(blobs.length).toBe(c.commitments.length)
      const t = decodeTransitionBlobs(blobs, nf, { points: 'full' })
      const vids = c.vote_ids.map(BigInt).sort((a, b) => (a < b ? -1 : 1))
      expect(t.voteIds).toEqual(vids)
      const updates = [...c.updates].sort((a, b) => (BigInt(a.key) < BigInt(b.key) ? -1 : 1))
      expect(t.updates.map((u) => u.key)).toEqual(updates.map((u) => BigInt(u.key)))
      updates.forEach((u, i) => expect(t.updates[i]!.ballot).toEqual(ballot(u.ballot).slice(0, nf)))
      expect(t.accumulator).toEqual(ballot(c.accumulator).slice(0, nf))
      // Lazy decoding keeps the cells; unpacking them gives the same ballots.
      const lazy = decodeTransitionBlobs(blobs, nf)
      expect(lazy.updates[0]!.ballot).toBeUndefined()
      expect(unpackBallot(lazy.updates[0]!.cells)).toEqual(t.updates[0]!.ballot)
    }
    expect(multi).toBe(true)
  })

  it('derives versioned hashes, evaluation points and the digest', () => {
    for (const c of cases) {
      expect(c.commitments.map((x) => versionedHash(hex(x)).slice(2))).toEqual(c.versioned_hashes)
      expect(c.commitments.map((x) => blobEvaluationPoint(pid(c), hex(c.root_before), hex(x)).slice(2))).toEqual(c.zs)
      expect(blobsDigest(c.commitments.map(hex), c.ys.map(hex)).slice(2)).toBe(c.digest)
    }
  })
})

// A small transition built from real curve points, for the rejection cases.
function base() {
  const pts: Point[] = [B8]
  for (let i = 1; i < 12; i++) pts.push(addPoints(pts[i - 1]!, B8))
  const ct = (k: number): Ciphertext => ({ c1: pts[k]!, c2: pts[k + 1]! })
  const id: Ciphertext = { c1: IDENTITY, c2: IDENTITY }
  const t = {
    voteIds: [VOTE_ID_MIN | 5n, VOTE_ID_MIN | 1n, (1n << 64n) - 1n],
    updates: [
      { key: 0x30n, ballot: [ct(3), ct(5)] },
      { key: 0x11n, ballot: [ct(7), ct(9)] },
    ],
    accumulator: [ct(1), id],
    numFields: 2,
  }
  return { t, cells: transitionCells(t) }
}

const decodeCells = (cells: Uint8Array[], nf: number) =>
  decodeTransitionBlobs(blobsFromCells(cells), nf, { points: 'full' })
const clone = (cells: Uint8Array[]): Uint8Array[] => cells.map((c) => c.slice())
const u64Cell = (v: bigint) => bigIntToBe(v, 32)

describe('decodeTransitionBlobs rejects malformed blobs', () => {
  it('accepts the honest layout', () => {
    const { cells } = base()
    const t = decodeCells(cells, 2)
    expect(t.voteIds.length).toBe(3)
    expect(t.updates.map((u) => u.key)).toEqual([0x11n, 0x30n])
  })

  it('refuses a wrong field count', () => {
    const { cells } = base()
    expect(() => decodeCells(cells, 1)).toThrow()
    expect(() => decodeCells(cells, 0)).toThrow()
    expect(() => decodeCells(cells, 17)).toThrow()
    expect(() => decodeTransitionBlobs([], 2)).toThrow()
  })

  it('refuses truncated or oversized counts', () => {
    const { cells } = base()
    let bad = clone(cells)
    bad[0]![31] = 200
    expect(() => decodeCells(bad, 2)).toThrow()
    bad = clone(cells)
    bad[0] = u64Cell((1n << 64n) - 1n)
    expect(() => decodeCells(bad, 2)).toThrow()
    bad = clone(cells)
    bad[4]![31] = 9 // the update count
    expect(() => decodeCells(bad, 2)).toThrow()
    bad = clone(cells)
    bad[0]![0] = 1
    expect(() => decodeCells(bad, 2)).toThrow(/above 64 bits/)
  })

  it('refuses unsorted or out-of-namespace keys', () => {
    const { cells } = base()
    let bad = clone(cells)
    bad[1] = u64Cell(5n) // a vote id below 2^63
    expect(() => decodeCells(bad, 2)).toThrow(/vote-id namespace/)
    bad = clone(cells)
    bad[2] = cells[1]!.slice() // duplicate vote id
    expect(() => decodeCells(bad, 2)).toThrow(/ascending/)
    bad = clone(cells)
    bad[5] = u64Cell(0x0fn) // slot key below 0x10
    expect(() => decodeCells(bad, 2)).toThrow(/ballot namespace/)
    bad = clone(cells)
    bad[5] = u64Cell(0x40n) // first key above the second
    expect(() => decodeCells(bad, 2)).toThrow(/ascending/)
  })

  it('refuses bad points', () => {
    const { cells } = base()
    const firstPoint = 6
    let bad = clone(cells)
    bad[firstPoint] = toBytes('0x30644e72e131a029b85045b68181585d2833e84879b9709143e1f593f0000001')
    expect(() => decodeCells(bad, 2)).toThrow(/y >= p/)
    bad = clone(cells)
    bad[firstPoint]![0]! |= 0x80
    expect(() => decodeCells(bad, 2)).toThrow(/bit 255/)
    let offCurve: Uint8Array | null = null
    for (let y = 2n; y < 60n && !offCurve; y++) {
      try {
        decompressPoint(u64Cell(y))
      } catch {
        offCurve = u64Cell(y)
      }
    }
    bad = clone(cells)
    bad[firstPoint] = offCurve!
    expect(() => decodeCells(bad, 2)).toThrow(/not on the curve/)
  })

  it('refuses trailing data and extra blobs', () => {
    const { cells } = base()
    const blobs = blobsFromCells(cells)
    const dirty = blobs[0]!.slice()
    dirty[BLOB_SIZE - 1] = 1
    expect(() => decodeTransitionBlobs([dirty], 2)).toThrow(/non-zero/)
    expect(() => decodeTransitionBlobs([blobs[0]!, new Uint8Array(BLOB_SIZE)], 2)).toThrow(/more blobs/)
    expect(() => decodeTransitionBlobs([blobs[0]!.slice(0, 100)], 2)).toThrow(/bytes/)
  })
})

describe('points and ids', () => {
  it('compresses and decompresses curve points, identity included', () => {
    let p = B8
    for (let i = 0; i < 8; i++) {
      expect(isOnCurve(p)).toBe(true)
      expect(decompressPoint(compressPoint(p))).toEqual(p)
      p = addPoints(p, B8)
    }
    expect(toHex(compressPoint(IDENTITY))).toBe(toHex(u64Cell(1n)))
    expect(decompressPoint(compressPoint(IDENTITY))).toEqual(IDENTITY)
  })

  it('formats and parses vote ids', () => {
    const id = VOTE_ID_MIN + 255n
    expect(formatVoteId(id)).toBe('0x80000000000000ff')
    expect(parseVoteId('0x80000000000000ff')).toBe(id)
    expect(parseVoteId(id.toString())).toBe(id)
    expect(parseVoteId('0x10')).toBeNull()
    expect(parseVoteId('nope')).toBeNull()
  })
})
