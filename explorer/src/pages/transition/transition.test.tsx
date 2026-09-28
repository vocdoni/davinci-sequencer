import { describe, expect, it } from 'vitest'
import { render, screen, waitFor, within } from '@testing-library/react'
import { QueryClient, QueryClientProvider } from '@tanstack/react-query'
import type { ReactElement } from 'react'
import { MemoryRouter, Route, Routes } from 'react-router-dom'
import { ConfigContext } from '~config/config-context'
import { DEMO_CONFIG } from '~config/runtime-config'
import { createExplorerData } from '~data/create'
import { DataProvider } from '~data/DataProvider'
import { ServiceError, type ExplorerServices } from '~data/services'
import { demoFixture } from '~fixtures/demo'
import { TooltipProvider } from '~kit'
import { formatVoteId } from '~protocol/blob'
import { patterns, paths } from '~routes/paths'
import { VotesPage } from '~pages/votes'
import { TransitionPage } from '.'

const fixture = demoFixture()

/** The demo network with a beacon that pruned everything and no sequencer. */
function renderAt(url: string, element: ReactElement, pattern: string, services?: Partial<ExplorerServices>) {
  const data = createExplorerData({ config: DEMO_CONFIG, demoOptions: { blockIntervalMs: 0 } })
  const client = new QueryClient({ defaultOptions: { queries: { retry: false } } })
  return render(
    <ConfigContext.Provider value={DEMO_CONFIG}>
      <QueryClientProvider client={client}>
        <DataProvider source={data.source} services={{ ...data.services, ...services }}>
          <MemoryRouter initialEntries={[url]} future={{ v7_startTransition: true, v7_relativeSplatPath: true }}>
            <TooltipProvider>
              <Routes>
                <Route path={pattern} element={element} />
              </Routes>
            </TooltipProvider>
          </MemoryRouter>
        </DataProvider>
      </QueryClientProvider>
    </ConfigContext.Provider>
  )
}

const pruned: Partial<ExplorerServices> = {
  sequencers: [],
  fetchTransitionBlobs: async () => {
    throw new ServiceError(
      'beacon https://beacon.invalid: beacon /eth/v1/beacon/blob_sidecars/7: not found (pruned or not yet available)'
    )
  },
}

describe('TransitionPage', () => {
  it('explains pruned blobs and still shows every check', async () => {
    const { processId, index } = fixture.featured.multiBlob
    renderAt(paths.transition(processId, index), <TransitionPage />, patterns.transition, pruned)
    expect(await screen.findByText(/no sequencer is configured/i, {}, { timeout: 5_000 })).toBeInTheDocument()
    expect(screen.getByText(/The settlement is not in doubt/)).toBeInTheDocument()
    const verify = screen.getByTestId('verify')
    expect(within(verify).getByTestId('check-plonk')).toBeInTheDocument()
    expect(within(verify).getByTestId('check-kzg-openings')).toBeInTheDocument()
    expect(screen.getByTestId('blob-list').querySelectorAll('tbody tr')).toHaveLength(4)
  })

  it('decodes the blobs when they are available', async () => {
    const { processId, index } = fixture.featured.multiBlob
    renderAt(paths.transition(processId, index), <TransitionPage />, patterns.transition)
    const content = await screen.findByTestId('blob-content', {}, { timeout: 10_000 })
    expect(within(content).getByTestId('vote-id-list')).toBeInTheDocument()
    expect(screen.getByTestId('transition-summary')).toHaveTextContent(/vote ids/)
  })

  it('says when there is no such transition', async () => {
    renderAt(paths.transition(fixture.featured.openProcess, 999), <TransitionPage />, patterns.transition)
    expect(await screen.findByText(/No transition found/)).toBeInTheDocument()
  })
})

describe('VotesPage', () => {
  it('works without a sequencer', async () => {
    const { processId, voteId } = fixture.featured.settledVote
    renderAt(paths.vote(processId, formatVoteId(voteId)), <VotesPage />, patterns.vote, { sequencers: [] })
    expect(screen.getAllByText(/No sequencer is configured/)).toHaveLength(2)
    await waitFor(() => expect(screen.getByTestId('vote-summary')).toHaveTextContent(/found in transition/), {
      timeout: 15_000,
    })
  })
})
