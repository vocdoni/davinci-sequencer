// What each 32-byte cell of a transition's blobs holds, from the decoded
// counts (circuit/CIRCUIT.md §8). Pure, so the cell view can label any cell
// without walking the stream.
//
//   enc(n_vids) enc(vid)...                        vote ids, ascending
//   enc(n_updates) [enc(key) pack(c1_0) pack(c2_0) ...]...
//   pack(acc_c1_0) pack(acc_c2_0) ...              the new accumulator
//   zero cells to the end of the last blob

import type { TransitionData } from '~protocol/blob'
import { toHex, type Hex } from '~protocol/bytes'
import { BYTES_PER_CELL, CELLS_PER_BLOB } from '~protocol/limits'

export type CellSection = 'vote-ids' | 'updates' | 'accumulator' | 'padding'

export type CellKind =
  | 'vote-id-count'
  | 'vote-id'
  | 'update-count'
  | 'slot-key'
  | 'ballot-c1'
  | 'ballot-c2'
  | 'acc-c1'
  | 'acc-c2'
  | 'padding'

export interface CellInfo {
  /** Position in the whole stream. */
  index: number
  /** Blob the cell lives in, and its position there. */
  blob: number
  offset: number
  kind: CellKind
  section: CellSection
  /** Slot update (for `slot-key` and ballot cells) or vote id position. */
  item: number | null
  /** Ballot field, for point cells. */
  field: number | null
  label: string
}

export interface CellCounts {
  voteIds: number
  updates: number
  numFields: number
}

export function countsOf(t: TransitionData): CellCounts {
  return { voteIds: t.voteIds.length, updates: t.updates.length, numFields: t.numFields }
}

/** First cell of each section and the number of cells the data uses (`T`). */
export function sectionStarts(c: CellCounts) {
  const updates = 1 + c.voteIds
  const accumulator = updates + 1 + c.updates * (1 + 2 * c.numFields)
  const used = accumulator + 2 * c.numFields
  return { voteIds: 0, updates, accumulator, used }
}

/** Describes cell `index` of a transition with these counts. */
export function describeCell(c: CellCounts, index: number): CellInfo {
  const s = sectionStarts(c)
  const base = { index, blob: Math.floor(index / CELLS_PER_BLOB), offset: index % CELLS_PER_BLOB }
  if (index === 0) {
    return { ...base, kind: 'vote-id-count', section: 'vote-ids', item: null, field: null, label: 'Vote id count' }
  }
  if (index < s.updates) {
    const k = index - 1
    return { ...base, kind: 'vote-id', section: 'vote-ids', item: k, field: null, label: `Vote id #${k}` }
  }
  if (index === s.updates) {
    return { ...base, kind: 'update-count', section: 'updates', item: null, field: null, label: 'Slot update count' }
  }
  if (index < s.accumulator) {
    const stride = 1 + 2 * c.numFields
    const rel = index - s.updates - 1
    const item = Math.floor(rel / stride)
    const pos = rel % stride
    if (pos === 0) {
      return { ...base, kind: 'slot-key', section: 'updates', item, field: null, label: `Update #${item}: slot key` }
    }
    const field = Math.floor((pos - 1) / 2)
    const half = (pos - 1) % 2 === 0 ? 'c1' : 'c2'
    return {
      ...base,
      kind: half === 'c1' ? 'ballot-c1' : 'ballot-c2',
      section: 'updates',
      item,
      field,
      label: `Update #${item}: field ${field} ${half}`,
    }
  }
  if (index < s.used) {
    const rel = index - s.accumulator
    const field = Math.floor(rel / 2)
    const half = rel % 2 === 0 ? 'c1' : 'c2'
    return {
      ...base,
      kind: half === 'c1' ? 'acc-c1' : 'acc-c2',
      section: 'accumulator',
      item: null,
      field,
      label: `Accumulator field ${field} ${half}`,
    }
  }
  return { ...base, kind: 'padding', section: 'padding', item: null, field: null, label: 'Padding (zero)' }
}

export interface SectionSpan {
  section: CellSection
  start: number
  /** Exclusive. */
  end: number
}

/** The four sections as cell ranges over `totalCells` (every blob's 4096 cells). */
export function sectionSpans(c: CellCounts, totalCells: number): SectionSpan[] {
  const s = sectionStarts(c)
  const spans: SectionSpan[] = [
    { section: 'vote-ids', start: 0, end: s.updates },
    { section: 'updates', start: s.updates, end: s.accumulator },
    { section: 'accumulator', start: s.accumulator, end: s.used },
    { section: 'padding', start: s.used, end: Math.max(s.used, totalCells) },
  ]
  return spans.filter((x) => x.end > x.start)
}

/** The raw 32 bytes of cell `index` across a transition's blobs, or null past the end. */
export function cellBytes(blobs: Uint8Array[], index: number): Uint8Array | null {
  const blob = blobs[Math.floor(index / CELLS_PER_BLOB)]
  if (!blob) return null
  const off = (index % CELLS_PER_BLOB) * BYTES_PER_CELL
  if (off + BYTES_PER_CELL > blob.length) return null
  return blob.subarray(off, off + BYTES_PER_CELL)
}

export function cellHex(blobs: Uint8Array[], index: number): Hex | null {
  const b = cellBytes(blobs, index)
  return b ? toHex(b) : null
}

/** A ballot slot key as the explorer prints it: `0x` + 16 hex digits. */
export function formatSlotKey(key: bigint): string {
  return `0x${key.toString(16).padStart(16, '0')}`
}
