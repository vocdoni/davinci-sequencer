// EIP-4844 DA blobs of a transition. Port of davinci-zkvm
// `rust-sdk/src/blob.rs`; layout in `circuit/CIRCUIT.md` §8:
//
//   enc(n_vids) enc(vid)...                                vote ids ascending
//   enc(n_updates) [enc(key) pack(c1_0) pack(c2_0) ...]... ascending by key
//   pack(acc_c1_0) pack(acc_c2_0) ...                      new accumulator
//   zero cells to the end of the last blob
//   enc(v) = BE32(v), pack(P) = compressed point, only fields < nf.
//
// New votes, overwrites and silent refreshes are all "slot updates" here: the
// blob does not mark them, so an overwrite looks like a refresh. A slot's first
// appearance across the blobs is still a new vote.

import { sha256 } from 'viem'
import { compressPoint, decompressPoint, isPackedCellShape, PointError, type Point } from './babyjubjub'
import { beToBigInt, bigIntToBe, concatBytes, reverseBytes, toBytes, toHex, type Hex } from './bytes'
import {
  BALLOT_MAX,
  BALLOT_MIN,
  BLOB_SIZE,
  BLS_MODULUS,
  BYTES_PER_CELL,
  CELLS_PER_BLOB,
  MAX_BLOBS,
  NUM_FIELDS,
  U64_MAX,
  VOTE_ID_MIN,
} from './limits'

export class BlobError extends Error {}

export interface Ciphertext {
  c1: Point
  c2: Point
}

export interface SlotUpdate {
  /** Ballot slot key (`0x10 ≤ key < 2^63`). */
  key: bigint
  /** The `2·nf` packed point cells, `c1_0 c2_0 c1_1 …`. Decompress with {@link unpackBallot}. */
  cells: Hex[]
  /** The decompressed ciphertexts, when decoded with `points: 'full'`. */
  ballot?: Ciphertext[]
}

export interface TransitionData {
  /** Vote identifiers, ascending. */
  voteIds: bigint[]
  /** Every slot the batch wrote, ascending by key. */
  updates: SlotUpdate[]
  /** The results accumulator after the batch, `nf` ciphertexts. */
  accumulator: Ciphertext[]
  numFields: number
}

export interface DecodeOptions {
  /**
   * `lazy` (default) checks each update cell's shape and leaves the square
   * roots to {@link unpackBallot}; `full` decompresses every point, which
   * also checks it is on the curve. The accumulator is always decompressed.
   */
  points?: 'lazy' | 'full'
}

function encU64(v: bigint): Uint8Array {
  if (v < 0n || v > U64_MAX) throw new BlobError('integer does not fit in 64 bits')
  return bigIntToBe(v, 32)
}

/** Cells needed for a transition: `2 + n_vids + n_updates·(1 + 2nf) + 2nf`. */
export function totalCells(nVoteIds: number, nUpdates: number, nf: number): number {
  return 2 + nVoteIds + nUpdates * (1 + 2 * nf) + 2 * nf
}

/** `ceil(T / 4096)`. */
export function blobCount(nVoteIds: number, nUpdates: number, nf: number): number {
  return Math.ceil(totalCells(nVoteIds, nUpdates, nf) / CELLS_PER_BLOB)
}

export interface TransitionInput {
  voteIds: bigint[]
  updates: Array<{ key: bigint; ballot: Ciphertext[] }>
  accumulator: Ciphertext[]
  numFields: number
}

/** The cell stream in guest order (vote ids and updates sorted here). */
export function transitionCells(t: TransitionInput): Uint8Array[] {
  const nf = Math.min(t.numFields, NUM_FIELDS)
  const vids = [...t.voteIds].sort((a, b) => (a < b ? -1 : a > b ? 1 : 0))
  const updates = [...t.updates].sort((a, b) => (a.key < b.key ? -1 : a.key > b.key ? 1 : 0))
  const out: Uint8Array[] = [encU64(BigInt(vids.length))]
  for (const v of vids) out.push(encU64(v))
  out.push(encU64(BigInt(updates.length)))
  for (const u of updates) {
    out.push(encU64(u.key))
    for (const ct of u.ballot.slice(0, nf)) out.push(compressPoint(ct.c1), compressPoint(ct.c2))
  }
  for (const ct of t.accumulator.slice(0, nf)) out.push(compressPoint(ct.c1), compressPoint(ct.c2))
  return out
}

/** Packs cells into zero-padded 128 KiB blobs. */
export function blobsFromCells(cells: Uint8Array[]): Uint8Array[] {
  const n = Math.max(1, Math.ceil(cells.length / CELLS_PER_BLOB))
  if (n > MAX_BLOBS) throw new BlobError(`${n} blobs, at most ${MAX_BLOBS}`)
  const blobs: Uint8Array[] = []
  for (let b = 0; b < n; b++) {
    const blob = new Uint8Array(BLOB_SIZE)
    const start = b * CELLS_PER_BLOB
    const end = Math.min(start + CELLS_PER_BLOB, cells.length)
    for (let i = start; i < end; i++) blob.set(cells[i]!, (i - start) * BYTES_PER_CELL)
    blobs.push(blob)
  }
  return blobs
}

/** EIP-4844 versioned hash: `0x01 ‖ sha256(commitment)[1..]`. */
export function versionedHash(commitment: Hex | Uint8Array): Hex {
  const h = sha256(toBytes(commitment), 'bytes')
  h[0] = 0x01
  return toHex(h)
}

/**
 * Evaluation point the guest binds a blob to:
 * `sha256(pid_BE32 ‖ root_before_BE32 ‖ commitment) mod r_bls`.
 * `rootBefore` is the raw arbo digest (the registry's root), reversed here.
 */
export function blobEvaluationPoint(processId: Hex, rootBefore: Hex, commitment: Hex | Uint8Array): Hex {
  const pid = bigIntToBe(beToBigInt(toBytes(processId)), 32)
  const root = reverseBytes(toBytes(rootBefore))
  const h = beToBigInt(sha256(concatBytes(pid, root, toBytes(commitment)), 'bytes'))
  return toHex(bigIntToBe(h % BLS_MODULUS, 32))
}

/** `sha256(com_0 ‖ y_0 ‖ …)`: registers 28..35 of the batch publics. */
export function blobsDigest(commitments: Array<Hex | Uint8Array>, ys: Array<Hex | Uint8Array>): Hex {
  if (commitments.length !== ys.length) throw new BlobError('commitments and evaluations differ in length')
  const parts: Uint8Array[] = []
  commitments.forEach((c, i) => parts.push(toBytes(c), toBytes(ys[i]!)))
  return sha256(concatBytes(...parts))
}

class Cells {
  next = 0
  constructor(private readonly blobs: Uint8Array[]) {}

  get total(): number {
    return this.blobs.length * CELLS_PER_BLOB
  }

  cell(i: number): Uint8Array | null {
    const blob = this.blobs[Math.floor(i / CELLS_PER_BLOB)]
    if (!blob) return null
    const off = (i % CELLS_PER_BLOB) * BYTES_PER_CELL
    return blob.subarray(off, off + BYTES_PER_CELL)
  }

  take(): Uint8Array {
    const c = this.cell(this.next)
    if (!c) throw new BlobError('truncated blob data')
    this.next += 1
    return c
  }

  u64(): bigint {
    const c = this.take()
    for (let i = 0; i < 24; i++) if (c[i] !== 0) throw new BlobError('integer cell above 64 bits')
    return beToBigInt(c.subarray(24))
  }

  point(): Point {
    try {
      return decompressPoint(this.take())
    } catch (err) {
      if (err instanceof PointError) throw new BlobError(`bad point cell: ${err.message}`)
      throw err
    }
  }

  /** A count that must fit in the cells left. */
  count(perItem: number): number {
    const n = this.u64()
    const left = BigInt(Math.max(0, this.total - this.next))
    if (n * BigInt(perItem) > left) throw new BlobError('count exceeds the blob data')
    return Number(n)
  }
}

/**
 * Parses a transition's blobs (in order) at the process's `numFields`.
 * Rejects truncated or oversized counts, unsorted or out-of-namespace keys,
 * malformed points, non-zero padding and extra blobs.
 *
 * These are layout checks, not provenance: trust the output only for blobs
 * whose versioned hashes match the settlement transaction.
 */
export function decodeTransitionBlobs(
  blobs: Array<Uint8Array | Hex>,
  numFields: number,
  options: DecodeOptions = {}
): TransitionData {
  const nf = numFields
  if (!Number.isInteger(nf) || nf < 1 || nf > NUM_FIELDS) throw new BlobError(`num_fields ${nf} not in 1..=16`)
  if (blobs.length === 0 || blobs.length > MAX_BLOBS) {
    throw new BlobError(`${blobs.length} blobs, want 1..=${MAX_BLOBS}`)
  }
  const raw = blobs.map(toBytes)
  for (const b of raw) if (b.length !== BLOB_SIZE) throw new BlobError(`blob of ${b.length} bytes, want ${BLOB_SIZE}`)
  const c = new Cells(raw)
  const full = options.points === 'full'

  const nVids = c.count(1)
  const voteIds: bigint[] = []
  for (let i = 0; i < nVids; i++) {
    const v = c.u64()
    const prev = voteIds[voteIds.length - 1]
    if (v < VOTE_ID_MIN || (prev !== undefined && prev >= v)) {
      throw new BlobError('vote ids not ascending in the vote-id namespace')
    }
    voteIds.push(v)
  }

  const nUpdates = c.count(1 + 2 * nf)
  const updates: SlotUpdate[] = []
  for (let i = 0; i < nUpdates; i++) {
    const key = c.u64()
    const prev = updates[updates.length - 1]
    if (key < BALLOT_MIN || key > BALLOT_MAX || (prev && prev.key >= key)) {
      throw new BlobError('slot keys not ascending in the ballot namespace')
    }
    const cells: Hex[] = []
    const ballot: Ciphertext[] = []
    for (let f = 0; f < nf; f++) {
      if (full) {
        const start = c.next
        const c1 = c.point()
        const c2 = c.point()
        ballot.push({ c1, c2 })
        cells.push(toHex(c.cell(start)!), toHex(c.cell(start + 1)!))
      } else {
        for (let k = 0; k < 2; k++) {
          const cell = c.take()
          if (!isPackedCellShape(cell)) throw new BlobError('bad point cell: not a packed coordinate')
          cells.push(toHex(cell))
        }
      }
    }
    updates.push(full ? { key, cells, ballot } : { key, cells })
  }

  const accumulator: Ciphertext[] = []
  for (let f = 0; f < nf; f++) accumulator.push({ c1: c.point(), c2: c.point() })

  const used = c.next
  if (Math.ceil(used / CELLS_PER_BLOB) !== raw.length) throw new BlobError('more blobs than the data needs')
  for (let i = used; i < c.total; i++) {
    const cell = c.cell(i)
    if (!cell || cell.some((b) => b !== 0)) throw new BlobError('non-zero data after the accumulator')
  }
  return { voteIds, updates, accumulator, numFields: nf }
}

/** Decompresses the packed cells of one slot update into its ciphertexts. */
export function unpackBallot(cells: Hex[]): Ciphertext[] {
  if (cells.length % 2 !== 0) throw new BlobError('odd number of point cells')
  const out: Ciphertext[] = []
  for (let i = 0; i < cells.length; i += 2) {
    out.push({ c1: decompressPoint(toBytes(cells[i]!)), c2: decompressPoint(toBytes(cells[i + 1]!)) })
  }
  return out
}

/** Cells used by the data in a set of blobs (`T`), for the cell view. */
export function usedCells(t: TransitionData): number {
  return totalCells(t.voteIds.length, t.updates.length, t.numFields)
}

/** A vote id as the API and the explorer print it: `0x` + 16 hex digits. */
export function formatVoteId(id: bigint): Hex {
  return `0x${id.toString(16).padStart(16, '0')}`
}

/** Parses `0x…` (up to 16 hex digits) or a decimal vote id; null when it is not one. */
export function parseVoteId(input: string): bigint | null {
  const s = input.trim()
  let v: bigint
  if (/^0x[0-9a-fA-F]{1,16}$/.test(s)) v = BigInt(s)
  else if (/^\d{1,20}$/.test(s)) v = BigInt(s)
  else return null
  return v >= VOTE_ID_MIN && v <= U64_MAX ? v : null
}
