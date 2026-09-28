// Pure reducers: events and contract reads folded into the entity store. No
// RPC, no React, no side effects. The live indexer and the demo fixture both
// go through here, which keeps the two store shapes identical.

import { parseProcessId } from '~protocol/process-id'
import { compareEvents } from './events'
import {
  processKey,
  STORE_VERSION,
  transitionKey,
  txKey,
  type Address,
  type ChainMeta,
  type Hex,
  type IndexedEvent,
  type IndexerStore,
  type ProcessEntity,
  type ProcessState,
  type RegistryInfo,
  type TransitionEntity,
  type TxDetails,
} from './types'

export interface StoreSeed {
  chainId: number
  networkName?: string
  registryAddress: Address
  startBlock: number
  blockTimeSeconds?: number
}

/** Seconds per block of the chains we know; 12 otherwise. */
export function defaultBlockTime(chainId: number): number {
  return chainId === 100 || chainId === 10200 ? 5 : 12
}

export function createEmptyStore(seed: StoreSeed): IndexerStore {
  const chain: ChainMeta = {
    chainId: seed.chainId,
    networkName: seed.networkName ?? '',
    registryAddress: seed.registryAddress.toLowerCase() as Address,
    startBlock: seed.startBlock,
    headBlock: seed.startBlock,
    headTimestamp: null,
    blockTimeSeconds: seed.blockTimeSeconds ?? defaultBlockTime(seed.chainId),
    registry: null,
  }
  return {
    version: STORE_VERSION,
    chain,
    lastIndexedBlock: Math.max(0, seed.startBlock - 1),
    lastIndexedHash: null,
    processes: {},
    processOrder: [],
    transitions: {},
    transitionOrder: [],
    txDetails: {},
    blockTimes: {},
    events: [],
  }
}

/**
 * A new snapshot for `useSyncExternalStore`. The reducers mutate in place, so
 * the collections and the entities (with their arrays) are copied too: a memo
 * keyed on any of them sees the change.
 */
export function bumpStore(store: IndexerStore): IndexerStore {
  // Copies every entity per (throttled) publish; copy-on-write in the reducers is the upgrade if stores get large.
  return {
    ...store,
    processes: mapValues(store.processes, copyEntity),
    processOrder: [...store.processOrder],
    transitions: mapValues(store.transitions, copyEntity),
    transitionOrder: [...store.transitionOrder],
    txDetails: { ...store.txDetails },
    blockTimes: { ...store.blockTimes },
    events: [...store.events],
  }
}

function copyEntity<T extends object>(entity: T): T {
  const out = { ...entity } as Record<string, unknown>
  for (const [k, v] of Object.entries(out)) if (Array.isArray(v)) out[k] = [...v]
  return out as T
}

function mapValues<T>(record: Record<string, T>, f: (v: T) => T): Record<string, T> {
  const out: Record<string, T> = {}
  for (const k in record) out[k] = f(record[k])
  return out
}

function organizerOf(id: string): Address {
  try {
    return parseProcessId(id).organizer.toLowerCase() as Address
  } catch {
    return '0x0000000000000000000000000000000000000000'
  }
}

export function ensureProcess(store: IndexerStore, id: string, block: number): ProcessEntity {
  const key = processKey(id)
  let p = store.processes[key]
  if (!p) {
    p = {
      id: key as Hex,
      organizer: organizerOf(key),
      createdBlock: block,
      createdTx: null,
      createdAt: null,
      state: null,
      stateBlock: 0,
      genesisRoot: null,
      transitions: [],
      statusChanges: [],
      durationChanges: [],
      maxVotersChanges: [],
      censusUpdates: [],
      results: null,
      decryptionRequest: null,
      events: [],
      lastActivityBlock: block,
    }
    store.processes[key] = p
    store.processOrder.push(key)
  }
  return p
}

/**
 * Folds events into the store. Events at or before the newest one already
 * applied are skipped, so a re-scanned chunk is harmless. Returns how many
 * were applied.
 */
export function applyEvents(store: IndexerStore, events: IndexedEvent[]): number {
  const sorted = [...events].sort(compareEvents)
  let applied = 0
  for (const ev of sorted) {
    const last = store.events[store.events.length - 1]
    if (last && compareEvents(ev, last) <= 0) continue
    const timestamp = ev.timestamp ?? store.blockTimes[ev.block] ?? null
    const event = { ...ev, timestamp } as IndexedEvent
    store.events.push(event)
    applyOne(store, event, store.events.length - 1)
    applied += 1
  }
  return applied
}

// State fields an event carries are applied on top of a snapshot read before
// it, so the page is right between the event and the next `getProcess`.
function newerThanState(p: ProcessEntity, block: number): ProcessState | null {
  return p.state && block > p.stateBlock ? p.state : null
}

function applyOne(store: IndexerStore, ev: IndexedEvent, index: number): void {
  const p = ensureProcess(store, ev.processId, ev.block)
  p.events.push(index)
  p.lastActivityBlock = Math.max(p.lastActivityBlock, ev.block)
  const at = { block: ev.block, tx: ev.tx, timestamp: ev.timestamp }

  switch (ev.name) {
    case 'ProcessCreated': {
      p.organizer = ev.data.creator
      p.createdBlock = ev.block
      p.createdTx = ev.tx
      p.createdAt = ev.timestamp
      break
    }
    case 'ProcessStatusChanged': {
      p.statusChanges.push({ ...at, from: ev.data.oldStatus, to: ev.data.newStatus })
      const s = newerThanState(p, ev.block)
      if (s) s.status = ev.data.newStatus
      break
    }
    case 'ProcessStateTransitioned': {
      const prevKey = p.transitions[p.transitions.length - 1]
      const prev = prevKey ? store.transitions[prevKey] : undefined
      const i = p.transitions.length
      const t: TransitionEntity = {
        key: transitionKey(p.id, i),
        processId: p.id,
        index: i,
        block: ev.block,
        tx: ev.tx,
        logIndex: ev.logIndex,
        timestamp: ev.timestamp,
        sender: ev.data.sender,
        rootBefore: ev.data.oldStateRoot,
        rootAfter: ev.data.newStateRoot,
        votersCount: ev.data.newVotersCount,
        overwrittenVotesCount: ev.data.newOverwrittenVotesCount,
        newVoters: ev.data.newVotersCount - (prev?.votersCount ?? 0),
        overwrites: ev.data.newOverwrittenVotesCount - (prev?.overwrittenVotesCount ?? 0),
        nBlobs: ev.data.nBlobs,
      }
      store.transitions[t.key] = t
      store.transitionOrder.push(t.key)
      p.transitions.push(t.key)
      const s = newerThanState(p, ev.block)
      if (s) {
        s.latestStateRoot = t.rootAfter
        s.votersCount = t.votersCount
        s.overwrittenVotesCount = t.overwrittenVotesCount
        s.batchNumber = Math.max(s.batchNumber, i + 1)
      }
      break
    }
    case 'ProcessResultsSet': {
      p.results = { ...at, sender: ev.data.sender, values: ev.data.result }
      const s = newerThanState(p, ev.block)
      if (s) s.result = ev.data.result
      break
    }
    case 'ProcessDurationChanged': {
      p.durationChanges.push({ ...at, value: ev.data.duration })
      const s = newerThanState(p, ev.block)
      if (s) s.duration = ev.data.duration
      break
    }
    case 'ProcessMaxVotersChanged': {
      p.maxVotersChanges.push({ ...at, value: ev.data.maxVoters })
      const s = newerThanState(p, ev.block)
      if (s) s.maxVoters = ev.data.maxVoters
      break
    }
    case 'CensusUpdated': {
      p.censusUpdates.push({ ...at, value: { root: ev.data.censusRoot, uri: ev.data.censusURI } })
      const s = newerThanState(p, ev.block)
      if (s) s.census = { ...s.census, root: ev.data.censusRoot, uri: ev.data.censusURI }
      break
    }
    case 'ResultsDecryptionRequested': {
      p.decryptionRequest = { ...at, ...ev.data }
      const s = newerThanState(p, ev.block)
      if (s?.dkg) {
        // The event does not carry the skipped-field mask; the count settles it
        // only when every field or none was submitted.
        const numFields = s.ballotMode.numFields
        const { epochId, aid, firstIndex, count } = ev.data
        const zeroSkipped = count === numFields ? 0 : count === 0 ? (1 << numFields) - 1 : null
        s.dkg = { ...s.dkg, epochId, aid, firstIndex, count, zeroSkipped, resultsRequested: true }
      }
      break
    }
  }
}

/** A `getProcess` read at `block`. Older reads than the one held are ignored. */
export function applyProcessState(store: IndexerStore, id: string, state: ProcessState, block: number): void {
  const p = ensureProcess(store, id, block)
  if (block < p.stateBlock) return
  p.state = state
  p.stateBlock = block
  if (state.organizer !== '0x0000000000000000000000000000000000000000') {
    p.organizer = state.organizer.toLowerCase() as Address
  }
}

export function applyGenesisRoot(store: IndexerStore, id: string, root: Hex): void {
  const p = store.processes[processKey(id)]
  if (p) p.genesisRoot = root.toLowerCase() as Hex
}

export function applyRegistryInfo(store: IndexerStore, info: RegistryInfo): void {
  store.chain = { ...store.chain, registry: info }
}

export function applyHead(store: IndexerStore, head: { block: number; timestamp: number | null }): void {
  store.chain = {
    ...store.chain,
    headBlock: Math.max(store.chain.headBlock, head.block),
    headTimestamp: head.timestamp ?? store.chain.headTimestamp,
  }
  if (head.timestamp != null) store.blockTimes[head.block] = head.timestamp
}

export function applyTxDetails(store: IndexerStore, details: TxDetails[]): void {
  for (const d of details) store.txDetails[txKey(d.hash)] = d
}

/** Block times read for events whose logs carried none; backfills the entities. */
export function applyBlockTimes(store: IndexerStore, times: Record<number, number>): void {
  const blocks = Object.keys(times)
  if (blocks.length === 0) return
  for (const b of blocks) store.blockTimes[b] = times[Number(b)]!
  for (const ev of store.events) {
    if (ev.timestamp == null && times[ev.block] != null) ev.timestamp = times[ev.block]!
  }
  for (const key of store.transitionOrder) {
    const t = store.transitions[key]!
    if (t.timestamp == null && times[t.block] != null) t.timestamp = times[t.block]!
  }
  for (const key of store.processOrder) {
    const p = store.processes[key]!
    if (p.createdAt == null && times[p.createdBlock] != null) p.createdAt = times[p.createdBlock]!
    for (const list of [p.statusChanges, p.durationChanges, p.maxVotersChanges, p.censusUpdates]) {
      for (const c of list as Array<{ block: number; timestamp: number | null }>) {
        if (c.timestamp == null && times[c.block] != null) c.timestamp = times[c.block]!
      }
    }
    if (p.results && p.results.timestamp == null && times[p.results.block] != null) {
      p.results.timestamp = times[p.results.block]!
    }
    if (p.decryptionRequest && p.decryptionRequest.timestamp == null && times[p.decryptionRequest.block] != null) {
      p.decryptionRequest.timestamp = times[p.decryptionRequest.block]!
    }
  }
}

/** Blocks of events that still have no timestamp, oldest first. */
export function blocksMissingTime(store: IndexerStore, limit: number): number[] {
  const out = new Set<number>()
  for (const ev of store.events) {
    if (ev.timestamp == null && store.blockTimes[ev.block] == null) out.add(ev.block)
    if (out.size >= limit) break
  }
  return [...out]
}

/** Transactions the store wants details for and does not have, oldest first, leaving out `skip` (tx keys). */
export function txsMissingDetails(store: IndexerStore, limit: number, skip?: ReadonlySet<string>): Hex[] {
  const out: Hex[] = []
  for (const ev of store.events) {
    if (out.length >= limit) break
    if (!ev.tx || store.txDetails[txKey(ev.tx)] || skip?.has(txKey(ev.tx))) continue
    if (ev.name === 'ProcessStateTransitioned' || ev.name === 'ProcessResultsSet' || ev.name === 'ProcessCreated') {
      if (!out.includes(ev.tx)) out.push(ev.tx)
    }
  }
  return out
}
