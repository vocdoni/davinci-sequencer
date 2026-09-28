import { describe, expect, it } from 'vitest'
import { renderHook, waitFor } from '@testing-library/react'
import { QueryClient, QueryClientProvider } from '@tanstack/react-query'
import type { ReactNode } from 'react'
import { ConfigContext } from '~config/config-context'
import { DEMO_CONFIG } from '~config/runtime-config'
import { demoFixture } from '~fixtures/demo'
import { DataProvider } from './DataProvider'
import { createExplorerData } from './create'
import { useNetworkStats, useProcess, useProcesses, useTransition } from './hooks'
import {
  useDkgApplication,
  useJsonDocument,
  useSequencers,
  useTrackerProof,
  useTransitionBlobs,
  useVoteInclusion,
  useVoteStatus,
} from './queries'

const fixture = demoFixture()

function wrapper() {
  const data = createExplorerData({ config: DEMO_CONFIG, demoOptions: { blockIntervalMs: 0 } })
  const client = new QueryClient({ defaultOptions: { queries: { retry: false } } })
  return ({ children }: { children: ReactNode }) => (
    <ConfigContext.Provider value={DEMO_CONFIG}>
      <QueryClientProvider client={client}>
        <DataProvider source={data.source} services={data.services}>
          {children}
        </DataProvider>
      </QueryClientProvider>
    </ConfigContext.Provider>
  )
}

describe('store hooks', () => {
  it('read the demo network', () => {
    const { result } = renderHook(
      () => ({
        stats: useNetworkStats(),
        rows: useProcesses({ keyMode: 'sequencer' }),
        view: useProcess(fixture.featured.openProcess),
      }),
      { wrapper: wrapper() }
    )
    expect(result.current.stats.processes).toBe(fixture.store.processOrder.length)
    expect(result.current.rows.every((r) => r.keyMode === 'sequencer')).toBe(true)
    expect(result.current.view?.transitions.length).toBeGreaterThan(10)
    expect(result.current.view?.rootChain.gaps).toBe(0)
  })

  it('returns null for unknown ids instead of throwing', () => {
    const { result } = renderHook(() => ({ p: useProcess(`0x${'00'.repeat(31)}`), t: useTransition(undefined, 0) }), {
      wrapper: wrapper(),
    })
    expect(result.current.p).toBeNull()
    expect(result.current.t).toBeNull()
  })
})

describe('on-demand hooks', () => {
  it('fetch and decode a multi-blob transition', async () => {
    const { processId, index } = fixture.featured.multiBlob
    const { result } = renderHook(() => useTransitionBlobs(processId, index), { wrapper: wrapper() })
    await waitFor(() => expect(result.current.isSuccess).toBe(true))
    const t = fixture.store.transitions[`${processId}:${index}`]!
    expect(result.current.data!.blobs).toHaveLength(t.nBlobs)
    expect(result.current.data!.decoded!.voteIds.length).toBe(t.newVoters + t.overwrites)
  })

  it('find the transition a vote was settled in, and its tracker proof', async () => {
    const { processId, voteId } = fixture.featured.settledVote
    const { result } = renderHook(
      () => ({
        inclusion: useVoteInclusion(processId, voteId),
        status: useVoteStatus(processId, voteId),
        tracker: useTrackerProof(processId, voteId),
      }),
      { wrapper: wrapper() }
    )
    await waitFor(() => expect(result.current.inclusion.state).toBe('found'))
    expect(result.current.inclusion.transitionIndex).toBe(0)
    await waitFor(() => expect(result.current.tracker.isSuccess).toBe(true))
    expect(result.current.tracker.data).toMatchObject({ valid: true, rootOnChain: true })
    await waitFor(() => expect(result.current.status[0]!.status.data?.status).toBe('settled'))
  })

  it('report a pending vote and an unknown one', async () => {
    const { processId, voteId } = fixture.featured.pendingVote
    const { result } = renderHook(() => useVoteStatus(processId, voteId), { wrapper: wrapper() })
    await waitFor(() => expect(result.current[0]!.status.data?.status).toBe('pending'))
    await waitFor(() => expect(result.current[1]!.status.isError).toBe(true))
  })

  it('read sequencers, the DKG application and metadata', async () => {
    const pid = fixture.featured.awaitingReveal
    const uri = fixture.store.processes[pid]!.state!.metadataURI
    const { result } = renderHook(
      () => ({ seq: useSequencers(), dkg: useDkgApplication(pid), meta: useJsonDocument(uri) }),
      { wrapper: wrapper() }
    )
    await waitFor(() => expect(result.current.dkg.isSuccess).toBe(true))
    expect(result.current.dkg.data).toMatchObject({ revealed: false })
    expect(result.current.dkg.data!.ciphertexts.every((c) => !c.completed)).toBe(true)
    await waitFor(() => expect(result.current.seq.every((s) => s.info.isSuccess)).toBe(true))
    expect(result.current.seq[1]!.info.data?.observer).toBe(true)
    await waitFor(() => expect(result.current.meta.isSuccess).toBe(true))
  })
})
