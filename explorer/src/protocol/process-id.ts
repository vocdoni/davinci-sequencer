// Process ids. Layout (davinci-contracts `ProcessIdLib`), a bytes31:
//   [0..19]  organizer address
//   [20..23] registry prefix: last 4 bytes of keccak256(chainId_u32 ‖ registry)
//   [24..30] nonce (uint56)

import { encodePacked, getAddress, keccak256, type Address, type Hex } from 'viem'

export const PROCESS_ID_RE = /^0x[0-9a-fA-F]{62}$/

export function isProcessId(value: string): value is Hex {
  return PROCESS_ID_RE.test(value.trim())
}

/** Lowercase canonical form, the store key. */
export function normalizeProcessId(value: string): Hex {
  return value.trim().toLowerCase() as Hex
}

export interface ParsedProcessId {
  organizer: Address
  prefix: number
  nonce: bigint
}

export function parseProcessId(pid: string): ParsedProcessId {
  if (!isProcessId(pid)) throw new Error('not a process id (0x + 62 hex digits)')
  const hex = pid.trim().slice(2)
  return {
    organizer: getAddress(`0x${hex.slice(0, 40)}`),
    prefix: Number.parseInt(hex.slice(40, 48), 16),
    nonce: BigInt(`0x${hex.slice(48)}`),
  }
}

/** The registry's `pidPrefix` for a chain and registry address. */
export function processIdPrefix(chainId: number, registry: Address): number {
  const h = keccak256(encodePacked(['uint32', 'address'], [chainId, registry]))
  return Number.parseInt(h.slice(-8), 16)
}

/** The id `newProcess` assigns: organizer ‖ prefix ‖ nonce. */
export function computeProcessId(prefix: number, organizer: Address, nonce: bigint): Hex {
  const n = nonce & ((1n << 56n) - 1n)
  return `0x${organizer.slice(2).toLowerCase()}${prefix.toString(16).padStart(8, '0')}${n.toString(16).padStart(14, '0')}`
}
