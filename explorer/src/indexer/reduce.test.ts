import { describe, expect, it } from 'vitest'
import { normalizeLog } from './events'
import {
  applyBlockTimes,
  applyEvents,
  applyProcessState,
  blocksMissingTime,
  bumpStore,
  createEmptyStore,
  txsMissingDetails,
} from './reduce'
import type { Address, Hex, IndexedEvent, ProcessState } from './types'

const REGISTRY = '0x3cde68c39e26ecf94bd029b6ed3b9f945441daf3' as Address
const PID = '0x42fc20654efd78c6887ff0bd1cc50c9ec1dab58980c5bb9300000000000004' as Hex
const SEQ = '0xfa971da5a813f3ecb55142692c7415b37733845d' as Address
const root = (n: number) => `0x${n.toString(16).padStart(64, '0')}` as Hex
const tx = (n: number) => `0x${n.toString(16).padStart(64, 'a')}` as Hex

function transitioned(
  block: number,
  logIndex: number,
  before: number,
  after: number,
  voters: number,
  overwrites: number
): IndexedEvent {
  return {
    name: 'ProcessStateTransitioned',
    block,
    tx: tx(block),
    logIndex,
    timestamp: null,
    processId: PID,
    data: {
      sender: SEQ,
      oldStateRoot: root(before),
      newStateRoot: root(after),
      newVotersCount: voters,
      newOverwrittenVotesCount: overwrites,
      nBlobs: 1,
    },
  }
}

function baseEvents(): IndexedEvent[] {
  return [
    {
      name: 'ProcessCreated',
      block: 100,
      tx: tx(100),
      logIndex: 0,
      timestamp: 1_000,
      processId: PID,
      data: { creator: '0x42fc20654efd78c6887ff0bd1cc50c9ec1dab589' },
    },
    transitioned(110, 3, 1, 2, 5, 0),
    transitioned(120, 1, 2, 3, 12, 2),
    {
      name: 'ProcessStatusChanged',
      block: 130,
      tx: tx(130),
      logIndex: 0,
      timestamp: null,
      processId: PID,
      data: { oldStatus: 'ready', newStatus: 'ended' },
    },
    {
      name: 'ProcessResultsSet',
      block: 140,
      tx: tx(140),
      logIndex: 0,
      timestamp: null,
      processId: PID,
      data: { sender: SEQ, result: [3n, 9n] },
    },
  ]
}

function state(overrides: Partial<ProcessState> = {}): ProcessState {
  return {
    status: 'ready',
    organizer: '0x42fc20654efd78c6887ff0bd1cc50c9ec1dab589',
    encryptionKey: { x: 1n, y: 2n },
    latestStateRoot: root(1),
    result: [],
    startTime: 0,
    duration: 3600,
    maxVoters: 100,
    votersCount: 0,
    overwrittenVotesCount: 0,
    creationBlock: 100,
    batchNumber: 0,
    metadataURI: '',
    ballotMode: {
      uniqueValues: false,
      numFields: 2,
      groupSize: 0,
      costExponent: 1,
      maxValue: 1n,
      minValue: 0n,
      maxValueSum: 2n,
      minValueSum: 0n,
    },
    census: {
      origin: 'merkle-static',
      root: root(77),
      contractAddress: '0x0000000000000000000000000000000000000000',
      uri: 'x',
    },
    keyMode: 'sequencer',
    dkg: null,
    ...overrides,
  }
}

describe('applyEvents', () => {
  it('builds the process, its transitions and their deltas', () => {
    const store = createEmptyStore({ chainId: 100, registryAddress: REGISTRY, startBlock: 50 })
    expect(applyEvents(store, baseEvents())).toBe(5)
    const p = store.processes[PID]!
    expect(p.createdBlock).toBe(100)
    expect(p.createdAt).toBe(1_000)
    expect(p.transitions).toEqual([`${PID}:0`, `${PID}:1`])
    const [t0, t1] = p.transitions.map((k) => store.transitions[k]!)
    expect([t0!.index, t0!.newVoters, t0!.overwrites]).toEqual([0, 5, 0])
    expect([t1!.index, t1!.newVoters, t1!.overwrites, t1!.votersCount]).toEqual([1, 7, 2, 12])
    expect(p.statusChanges.map((c) => c.to)).toEqual(['ended'])
    expect(p.results?.values).toEqual([3n, 9n])
    expect(store.transitionOrder).toHaveLength(2)
  })

  it('publishes fresh collections, so memos keyed on them invalidate', () => {
    const events = baseEvents()
    let store = bumpStore(createEmptyStore({ chainId: 100, registryAddress: REGISTRY, startBlock: 50 }))
    applyEvents(store, events.slice(0, 2))
    store = bumpStore(store)
    const [order, p] = [store.processOrder, store.processes[PID]!]
    const before = [...p.transitions]
    applyEvents(store, events.slice(2))
    const next = bumpStore(store)
    expect(next.processOrder).not.toBe(order)
    expect(next.processes[PID]).not.toBe(p)
    expect(next.processes[PID]!.transitions).not.toBe(p.transitions)
    expect(next.processes[PID]!.transitions.length).toBeGreaterThan(before.length)
  })

  it('is idempotent: a re-scanned chunk changes nothing', () => {
    const store = createEmptyStore({ chainId: 100, registryAddress: REGISTRY, startBlock: 50 })
    applyEvents(store, baseEvents())
    expect(applyEvents(store, baseEvents())).toBe(0)
    expect(store.events).toHaveLength(5)
    expect(store.processes[PID]!.transitions).toHaveLength(2)
  })

  it('orders out-of-order input by block and log index', () => {
    const store = createEmptyStore({ chainId: 100, registryAddress: REGISTRY, startBlock: 50 })
    applyEvents(store, [...baseEvents()].reverse())
    expect(store.events.map((e) => e.block)).toEqual([100, 110, 120, 130, 140])
  })

  it('applies event values on top of an older state read, not a newer one', () => {
    const store = createEmptyStore({ chainId: 100, registryAddress: REGISTRY, startBlock: 50 })
    const events = baseEvents()
    applyEvents(store, events.slice(0, 1))
    applyProcessState(store, PID, state(), 105)
    applyEvents(store, events.slice(1))
    const s = store.processes[PID]!.state!
    expect(s.status).toBe('ended')
    expect(s.latestStateRoot).toBe(root(3))
    expect([s.votersCount, s.overwrittenVotesCount, s.batchNumber]).toEqual([12, 2, 2])
    expect(s.result).toEqual([3n, 9n])

    // A read at block 200 already includes everything; an older read is ignored.
    applyProcessState(store, PID, state({ status: 'results' }), 200)
    applyProcessState(store, PID, state({ status: 'paused' }), 150)
    expect(store.processes[PID]!.state!.status).toBe('results')
  })

  it('keeps the DKG decryption fields of one request together', () => {
    const dkgState = () =>
      state({
        keyMode: 'dkg-automatic',
        dkg: {
          epochId: `0x${'0e'.repeat(12)}` as Hex,
          aid: root(9),
          firstIndex: 0,
          count: 0,
          zeroSkipped: 0,
          resultsRequested: false,
        },
      })
    const requested = (count: number): IndexedEvent => ({
      name: 'ResultsDecryptionRequested',
      block: 150,
      tx: tx(150),
      logIndex: 0,
      timestamp: null,
      processId: PID,
      data: { epochId: `0x${'0e'.repeat(12)}` as Hex, aid: root(9), firstIndex: 7, count },
    })
    const after = (count: number) => {
      const store = createEmptyStore({ chainId: 100, registryAddress: REGISTRY, startBlock: 50 })
      applyEvents(store, baseEvents().slice(0, 1))
      applyProcessState(store, PID, dkgState(), 105)
      applyEvents(store, [requested(count)])
      return store.processes[PID]!.state!.dkg!
    }
    // Two fields: one submitted leaves which one was skipped to the next read.
    expect(after(1)).toMatchObject({ firstIndex: 7, count: 1, zeroSkipped: null, resultsRequested: true })
    expect(after(2)).toMatchObject({ count: 2, zeroSkipped: 0 })
    expect(after(0)).toMatchObject({ count: 0, zeroSkipped: 0b11 })
  })

  it('creates a placeholder for a process whose creation it never saw', () => {
    const store = createEmptyStore({ chainId: 100, registryAddress: REGISTRY, startBlock: 50 })
    applyEvents(store, [transitioned(110, 0, 1, 2, 1, 0)])
    expect(store.processes[PID]!.organizer).toBe('0x42fc20654efd78c6887ff0bd1cc50c9ec1dab589')
  })
})

describe('lazy reads', () => {
  it('lists the transactions and block times still missing', () => {
    const store = createEmptyStore({ chainId: 100, registryAddress: REGISTRY, startBlock: 50 })
    applyEvents(store, baseEvents())
    expect(txsMissingDetails(store, 10)).toEqual([tx(100), tx(110), tx(120), tx(140)])
    expect(blocksMissingTime(store, 10)).toEqual([110, 120, 130, 140])
    applyBlockTimes(store, { 110: 2_000, 120: 2_050 })
    expect(store.transitions[`${PID}:0`]!.timestamp).toBe(2_000)
    expect(store.events[1]!.timestamp).toBe(2_000)
    expect(blocksMissingTime(store, 10)).toEqual([130, 140])
  })

  it('leaves out the transactions it is told to skip', () => {
    const store = createEmptyStore({ chainId: 100, registryAddress: REGISTRY, startBlock: 50 })
    applyEvents(store, baseEvents())
    expect(txsMissingDetails(store, 2, new Set([tx(100), tx(110)]))).toEqual([tx(120), tx(140)])
  })
})

describe('normalizeLog', () => {
  it('reads a decoded viem log, block time included', () => {
    const ev = normalizeLog({
      eventName: 'ProcessStateTransitioned',
      args: {
        processId: PID,
        sender: '0xFA971Da5A813F3eCb55142692C7415B37733845D',
        oldStateRoot: root(1),
        newStateRoot: root(2),
        newVotersCount: 6n,
        newOverwrittenVotesCount: 0n,
        nBlobs: 1n,
      },
      blockNumber: 48477140n,
      transactionHash: tx(1),
      logIndex: 7,
      blockTimestamp: 1790572080n,
    })
    expect(ev).toMatchObject({ name: 'ProcessStateTransitioned', block: 48477140, timestamp: 1790572080, logIndex: 7 })
    expect(ev?.name === 'ProcessStateTransitioned' && ev.data.sender).toBe(SEQ)
  })

  it('maps status ordinals and ignores foreign events', () => {
    const ev = normalizeLog({ eventName: 'ProcessStatusChanged', args: { processId: PID, oldStatus: 0, newStatus: 4 } })
    expect(ev?.name === 'ProcessStatusChanged' && ev.data).toEqual({ oldStatus: 'ready', newStatus: 'results' })
    expect(normalizeLog({ eventName: 'Transfer', args: {} })).toBeNull()
  })
})
