// Byte helpers shared by the decoders. Hex strings are lowercase `0x…`.

import { bytesToHex, hexToBytes, type Hex } from 'viem'

export type { Hex }

export function toBytes(value: Hex | Uint8Array): Uint8Array {
  return typeof value === 'string' ? hexToBytes(value) : value
}

export function toHex(bytes: Uint8Array): Hex {
  return bytesToHex(bytes)
}

export function beToBigInt(bytes: Uint8Array): bigint {
  let v = 0n
  for (const b of bytes) v = (v << 8n) | BigInt(b)
  return v
}

export function bigIntToBe(value: bigint, length: number): Uint8Array {
  const out = new Uint8Array(length)
  let v = value
  for (let i = length - 1; i >= 0; i--) {
    out[i] = Number(v & 0xffn)
    v >>= 8n
  }
  if (v !== 0n) throw new RangeError(`value does not fit in ${length} bytes`)
  return out
}

export function reverseBytes(bytes: Uint8Array): Uint8Array {
  return bytes.slice().reverse()
}

export function concatBytes(...parts: Uint8Array[]): Uint8Array {
  const out = new Uint8Array(parts.reduce((n, p) => n + p.length, 0))
  let offset = 0
  for (const p of parts) {
    out.set(p, offset)
    offset += p.length
  }
  return out
}

export function equalBytes(a: Uint8Array, b: Uint8Array): boolean {
  if (a.length !== b.length) return false
  for (let i = 0; i < a.length; i++) if (a[i] !== b[i]) return false
  return true
}
