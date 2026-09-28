// BabyJubJub in circomlib twisted Edwards form: a*x^2 + y^2 = 1 + d*x^2*y^2,
// a = 168700, d = 168696, over the BN254 scalar field. Port of the parts of
// davinci-zkvm `rust-sdk/src/crypto/babyjubjub.rs` the explorer needs: the
// blob point encoding, the curve check and affine addition.

import { Field } from '@noble/curves/abstract/modular'
import { BN254_FR } from './limits'

export const Fp = Field(BN254_FR)
const A = 168700n
const D = 168696n

export interface Point {
  x: bigint
  y: bigint
}

export const IDENTITY: Point = { x: 0n, y: 1n }

/** Generator B8 of the prime-order subgroup. */
export const B8: Point = {
  x: 5299619240641551281634865583518297030282874472190772894086521144482721001553n,
  y: 16950150798460657717958625567821834550301663161624707787222815936182638968203n,
}

export class PointError extends Error {}

export function isOnCurve(p: Point): boolean {
  if (p.x >= BN254_FR || p.y >= BN254_FR || p.x < 0n || p.y < 0n) return false
  const x2 = Fp.sqr(p.x)
  const y2 = Fp.sqr(p.y)
  return Fp.eql(Fp.add(Fp.mul(A, x2), y2), Fp.add(1n, Fp.mul(D, Fp.mul(x2, y2))))
}

export function isIdentity(p: Point): boolean {
  return p.x === 0n && p.y === 1n
}

export function pointEquals(a: Point, b: Point): boolean {
  return a.x === b.x && a.y === b.y
}

/** Affine addition (complete on this curve). */
export function addPoints(p: Point, q: Point): Point {
  const x1y2 = Fp.mul(p.x, q.y)
  const y1x2 = Fp.mul(p.y, q.x)
  const y1y2 = Fp.mul(p.y, q.y)
  const x1x2 = Fp.mul(p.x, q.x)
  const dxxyy = Fp.mul(D, Fp.mul(x1x2, y1y2))
  return {
    x: Fp.div(Fp.add(x1y2, y1x2), Fp.add(1n, dxxyy)),
    y: Fp.div(Fp.sub(y1y2, Fp.mul(A, x1x2)), Fp.sub(1n, dxxyy)),
  }
}

/**
 * The 32-byte blob cell of a point: `y` big-endian with the parity of `x` in
 * bit 254 (0x40 of the first byte). The identity `(0, 1)` packs to 1.
 */
export function compressPoint(p: Point): Uint8Array {
  const out = new Uint8Array(32)
  let y = p.y
  for (let i = 31; i >= 0; i--) {
    out[i] = Number(y & 0xffn)
    y >>= 8n
  }
  if (p.x & 1n) out[0]! |= 0x40
  return out
}

function beToBigInt(bytes: Uint8Array): bigint {
  let v = 0n
  for (const b of bytes) v = (v << 8n) | BigInt(b)
  return v
}

/**
 * Inverse of {@link compressPoint}. Rejects bit 255 set, `y >= p`, a `y` with
 * no point on the curve, and `x = 0` with the parity bit set.
 */
export function decompressPoint(cell: Uint8Array): Point {
  if (cell.length !== 32) throw new PointError('cell is not 32 bytes')
  if (cell[0]! & 0x80) throw new PointError('bit 255 set')
  const parity = (cell[0]! & 0x40) !== 0
  const yBytes = cell.slice()
  yBytes[0]! &= 0x3f
  const y = beToBigInt(yBytes)
  if (y >= BN254_FR) throw new PointError('y >= p')
  const y2 = Fp.sqr(y)
  const den = Fp.sub(A, Fp.mul(D, y2))
  if (den === 0n) throw new PointError('no x for this y')
  const x2 = Fp.div(Fp.sub(1n, y2), den)
  let x: bigint
  try {
    x = Fp.sqrt(x2)
  } catch {
    throw new PointError('not on the curve')
  }
  if (((x & 1n) === 1n) !== parity) {
    if (x === 0n) throw new PointError('non-canonical zero x')
    x = Fp.neg(x)
  }
  return { x, y }
}

/** Cheap shape check of a packed cell (no square root): bit 255 clear, y < p. */
export function isPackedCellShape(cell: Uint8Array): boolean {
  if (cell.length !== 32 || cell[0]! & 0x80) return false
  const yBytes = cell.slice()
  yBytes[0]! &= 0x3f
  return beToBigInt(yBytes) < BN254_FR
}

// BabyJubJub point forms. The DKG works in reduced twisted Edwards form
// (a = -1); DAVINCI and circomlib use a = 168700. The same point has the same
// y in both, and x_te = x_rte · K⁻¹ mod p with K² = -168700 (davinci-contracts
// `BjjFormLib`). The registry stores a DKG key after this conversion.

/** Scale factor K of `BjjFormLib`: K² = -168700 mod p. */
export const BJJ_K = 15527681003928902128179717624703512672403908117992798440346960750464748824729n
/** K⁻¹ mod p. */
export const BJJ_K_INV = 1911982854305225074381251344103329931637610209014896889891168275855466657090n

/** x of a reduced-form point in circomlib form (`BjjFormLib.toTE`). */
export function reducedToCircomX(x: bigint): bigint {
  return (x * BJJ_K_INV) % BN254_FR
}

/** A DKG application key (reduced form) as the registry would store it. */
export function reducedToCircom(p: Point): Point {
  return { x: reducedToCircomX(p.x), y: p.y }
}

/** A circomlib point in the DKG's reduced form (`BjjFormLib.fromTE`). */
export function circomToReduced(p: Point): Point {
  return { x: (p.x * BJJ_K) % BN254_FR, y: p.y }
}
