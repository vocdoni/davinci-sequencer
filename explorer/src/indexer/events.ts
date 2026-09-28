// ProcessRegistry log normalisation: viem's decoded logs into the flat
// `IndexedEvent` union the reducer understands.

import { REGISTRY_EVENT_NAMES, type RegistryEventName } from '~contracts/abis'
import { processStatusName } from '~protocol/types'
import type { Address, Hex, IndexedEvent } from './types'

const KNOWN = new Set<string>(REGISTRY_EVENT_NAMES)

export function isRegistryEvent(name: string | undefined): name is RegistryEventName {
  return name != null && KNOWN.has(name)
}

/** A viem log after decoding, loosened to what we read. */
export interface RawLog {
  eventName?: string
  args?: Record<string, unknown> | readonly unknown[]
  blockNumber?: bigint | null
  transactionHash?: Hex | null
  logIndex?: number | null
  /** Some RPCs (Gnosis among them) return the block time with each log. */
  blockTimestamp?: bigint | number | Hex | null
}

function num(v: unknown): number {
  if (typeof v === 'bigint') return Number(v)
  if (typeof v === 'number') return v
  return Number(v ?? 0)
}

function big(v: unknown): bigint {
  if (typeof v === 'bigint') return v
  if (typeof v === 'number') return BigInt(v)
  if (typeof v === 'string' && v.length > 0) return BigInt(v)
  return 0n
}

function addr(v: unknown): Address {
  return String(v ?? '0x0000000000000000000000000000000000000000').toLowerCase() as Address
}

function hex(v: unknown): Hex {
  return String(v ?? '0x').toLowerCase() as Hex
}

function timestampOf(v: RawLog['blockTimestamp']): number | null {
  if (v == null) return null
  const n = typeof v === 'string' ? Number.parseInt(v, 16) : Number(v)
  return Number.isFinite(n) && n > 0 ? n : null
}

/**
 * One decoded log as an `IndexedEvent`, or null when it is not a registry
 * event this build knows (an event added after it, or a topic collision).
 */
export function normalizeLog(log: RawLog): IndexedEvent | null {
  const name = log.eventName
  if (!isRegistryEvent(name)) return null
  const a = (log.args ?? {}) as Record<string, unknown>
  if (a.processId == null) return null
  const envelope = {
    block: num(log.blockNumber),
    tx: log.transactionHash ? hex(log.transactionHash) : null,
    logIndex: log.logIndex ?? 0,
    timestamp: timestampOf(log.blockTimestamp),
    processId: hex(a.processId),
  }

  switch (name) {
    case 'ProcessCreated':
      return { ...envelope, name, data: { creator: addr(a.creator) } }
    case 'ProcessStatusChanged':
      return {
        ...envelope,
        name,
        data: { oldStatus: processStatusName(num(a.oldStatus)), newStatus: processStatusName(num(a.newStatus)) },
      }
    case 'ProcessStateTransitioned':
      return {
        ...envelope,
        name,
        data: {
          sender: addr(a.sender),
          oldStateRoot: hex(a.oldStateRoot),
          newStateRoot: hex(a.newStateRoot),
          newVotersCount: num(a.newVotersCount),
          newOverwrittenVotesCount: num(a.newOverwrittenVotesCount),
          nBlobs: num(a.nBlobs),
        },
      }
    case 'ProcessResultsSet':
      return {
        ...envelope,
        name,
        data: { sender: addr(a.sender), result: ((a.result as unknown[]) ?? []).map(big) },
      }
    case 'ProcessDurationChanged':
      return { ...envelope, name, data: { duration: num(a.duration) } }
    case 'ProcessMaxVotersChanged':
      return { ...envelope, name, data: { maxVoters: num(a.maxVoters) } }
    case 'CensusUpdated':
      return { ...envelope, name, data: { censusRoot: hex(a.censusRoot), censusURI: String(a.censusURI ?? '') } }
    case 'ResultsDecryptionRequested':
      return {
        ...envelope,
        name,
        data: {
          epochId: hex(a.epochId),
          aid: hex(a.aid),
          firstIndex: num(a.firstIndex),
          count: num(a.count),
        },
      }
    default:
      return null
  }
}

/** Chronological order: block, then log index within the block. */
export function compareEvents(a: { block: number; logIndex: number }, b: { block: number; logIndex: number }): number {
  return a.block - b.block || a.logIndex - b.logIndex
}
