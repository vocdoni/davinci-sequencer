import { describe, expect, it, vi } from 'vitest'
import { act, renderHook, waitFor } from '@testing-library/react'
import { QueryClient, QueryClientProvider } from '@tanstack/react-query'
import type { ReactNode } from 'react'
import { ConfigContext } from '~config/config-context'
import { DEMO_CONFIG } from '~config/runtime-config'
import { createDemoServices, demoFixture } from '~fixtures/demo'
import { bumpStore } from '~indexer/reduce'
import { processKey, transitionKey, txKey, type IndexerStatus, type IndexerStore } from '~indexer/types'
import { DataProvider } from './DataProvider'
import { useDkgApplication, useTrackerProof, useTransitionBlobs, useVoteInclusion } from './queries'
import type { ExplorerServices } from './services'
import type { DataSource } from './source'

const fixture = demoFixture()

const STATUS: IndexerStatus = {
  phase: 'live',
  scanning: false,
  fromBlock: fixture.store.chain.startBlock,
  lastBlock: fixture.store.lastIndexedBlock,
  headBlock: fixture.store.chain.headBlock,
  progress: 1,
  eventCount: fixture.store.events.length,
  requests: 0,
  lastPollAt: null,
  errors: [],
  skippedTx: [],
  chainMismatch: null,
}

/** A data source over a copy of the demo store that the test edits between renders. */
function editableSource(edit: (store: IndexerStore) => void = () => {}, status: Partial<IndexerStatus> = {}) {
  const store = structuredClone(fixture.store)
  edit(store)
  let snapshot = { store: bumpStore(store), status: { ...STATUS, ...status } }
  const listeners = new Set<() => void>()
  const source: DataSource = {
    kind: 'live',
    subscribe: (listener) => {
      listeners.add(listener)
      return () => listeners.delete(listener)
    },
    getSnapshot: () => snapshot,
    start() {},
    stop() {},
    async refresh() {},
    ensureProcessState() {},
    ensureTxDetails() {},
    async clearCache() {},
  }
  const update = (change: (store: IndexerStore) => void) =>
    act(() => {
      change(snapshot.store)
      snapshot = { store: bumpStore(snapshot.store), status: snapshot.status }
      for (const listener of listeners) listener()
    })
  return { source, update }
}

function wrapper(source: DataSource, services: ExplorerServices) {
  const client = new QueryClient({ defaultOptions: { queries: { retry: false } } })
  return ({ children }: { children: ReactNode }) => (
    <ConfigContext.Provider value={DEMO_CONFIG}>
      <QueryClientProvider client={client}>
        <DataProvider source={source} services={services} autoStart={false}>
          {children}
        </DataProvider>
      </QueryClientProvider>
    </ConfigContext.Provider>
  )
}

const settle = () => new Promise((resolve) => setTimeout(resolve, 20))

describe('useTrackerProof', () => {
  it('refuses a proof the sequencer served for another vote', async () => {
    const { processId, voteId } = fixture.featured.settledVote
    const other = fixture.transitionData.get(transitionKey(processId, 1))!.voteIds[0]!
    const services = createDemoServices(fixture)
    const api = services.sequencers[0]!.api
    services.sequencers[0] = {
      ...services.sequencers[0]!,
      api: { ...api, trackerProof: (pid, _voteId, signal) => api.trackerProof(pid, other, signal) },
    }
    const { source } = editableSource()
    const { result } = renderHook(() => useTrackerProof(processId, voteId), { wrapper: wrapper(source, services) })
    await waitFor(() => expect(result.current.isSuccess).toBe(true))
    expect(result.current.data!.proof.voteId).toBe(other)
    // Its own path does reach an on-chain root: only the vote id gives it away.
    expect(result.current.data).toMatchObject({ otherVote: true, valid: false, rootOnChain: true })
  })
})

describe('useTransitionBlobs', () => {
  const { processId, index } = fixture.featured.multiBlob
  const t = fixture.store.transitions[transitionKey(processId, index)]!

  it('waits for the exact block time and fetches the slot of that time', async () => {
    const services = createDemoServices(fixture)
    const fetch = vi.spyOn(services, 'fetchTransitionBlobs')
    const { source, update } = editableSource((store) => {
      store.transitions[t.key]!.timestamp = null
      delete store.blockTimes[t.block]
    })
    const { result } = renderHook(() => useTransitionBlobs(processId, index), { wrapper: wrapper(source, services) })
    await settle()
    expect(fetch).not.toHaveBeenCalled()
    expect(result.current.fetchStatus).toBe('idle')

    update((store) => {
      store.blockTimes[t.block] = t.timestamp!
    })
    await waitFor(() => expect(result.current.isSuccess).toBe(true))
    expect(fetch).toHaveBeenCalledTimes(1)
    expect(fetch.mock.calls[0]![0].timestamp).toBe(t.timestamp)
  })

  it('waits for the field count instead of caching an undecoded result', async () => {
    const services = createDemoServices(fixture)
    const state = fixture.store.processes[processKey(processId)]!.state!
    const { source, update } = editableSource((store) => {
      store.processes[processKey(processId)]!.state = null
    })
    const { result } = renderHook(() => useTransitionBlobs(processId, index), { wrapper: wrapper(source, services) })
    await settle()
    expect(result.current.fetchStatus).toBe('idle')

    update((store) => {
      store.processes[processKey(processId)]!.state = structuredClone(state)
    })
    await waitFor(() => expect(result.current.data?.decoded).toBeTruthy())
    expect(result.current.data!.decoded!.voteIds.length).toBe(t.newVoters + t.overwrites)
  })
})

describe('useVoteInclusion', () => {
  it('searches past a transaction the indexer could not read, reporting it', async () => {
    const { processId, voteId } = fixture.featured.settledVote
    const p = fixture.store.processes[processKey(processId)]!
    const last = p.transitions.length - 1
    expect(last).toBeGreaterThan(0)
    const tx = fixture.store.transitions[p.transitions[last]!]!.tx!
    const { source } = editableSource(
      (store) => {
        delete store.txDetails[txKey(tx)]
      },
      { skippedTx: [txKey(tx)] }
    )
    const { result } = renderHook(() => useVoteInclusion(processId, voteId), {
      wrapper: wrapper(source, createDemoServices(fixture)),
    })
    await waitFor(() => expect(result.current.state).toBe('found'))
    expect(result.current.transitionIndex).toBe(0)
    expect(result.current.errors).toEqual([`#${last}: the settlement transaction could not be read from the RPC`])
  })
})

describe('useDkgApplication', () => {
  it('reads again when the request fields change, not only the count', async () => {
    const pid = fixture.featured.awaitingReveal
    const services = createDemoServices(fixture)
    const read = vi.spyOn(services, 'readDkgApplication')
    const { source, update } = editableSource()
    const { result } = renderHook(() => useDkgApplication(pid), { wrapper: wrapper(source, services) })
    await waitFor(() => expect(result.current.isSuccess).toBe(true))
    expect(read).toHaveBeenCalledTimes(1)

    const dkg = () => source.getSnapshot().store.processes[pid]!.state!.dkg!
    const count = dkg().count
    update((store) => {
      const s = store.processes[pid]!.state!
      s.dkg = { ...s.dkg!, firstIndex: s.dkg!.firstIndex + 10, zeroSkipped: 1 }
    })
    await waitFor(() => expect(read).toHaveBeenCalledTimes(2))
    expect(read.mock.calls[1]![0].state!.dkg).toMatchObject({ count, zeroSkipped: 1 })

    // An unknown skipped-field mask is waited for.
    update((store) => {
      const s = store.processes[pid]!.state!
      s.dkg = { ...s.dkg!, zeroSkipped: null }
    })
    await settle()
    expect(read).toHaveBeenCalledTimes(2)
  })
})
