import { describe, expect, it } from 'vitest'
import { render, screen, within } from '@testing-library/react'
import { QueryClient, QueryClientProvider } from '@tanstack/react-query'
import { MemoryRouter, Route, Routes } from 'react-router-dom'
import { ConfigContext } from '~config/config-context'
import { DEMO_CONFIG } from '~config/runtime-config'
import { createExplorerData } from '~data/create'
import { DataProvider } from '~data/DataProvider'
import type { SequencerApi } from '~data/services'
import { demoFixture } from '~fixtures/demo'
import { transitionKey } from '~indexer/types'
import { TooltipProvider } from '~kit'
import { formatVoteId } from '~protocol/blob'
import { patterns, paths } from '~routes/paths'
import { VotesPage } from '.'

const fixture = demoFixture()

describe('VotesPage', () => {
  it('says so when the sequencer answers with a proof for another vote', async () => {
    const { processId, voteId } = fixture.featured.settledVote
    const other = fixture.transitionData.get(transitionKey(processId, 1))!.voteIds[0]!
    const data = createExplorerData({ config: DEMO_CONFIG, demoOptions: { blockIntervalMs: 0 } })
    const api = data.services.sequencers[0]!.api
    const trackerProof: SequencerApi['trackerProof'] = (pid, _voteId, signal) => api.trackerProof(pid, other, signal)
    const sequencers = [{ ...data.services.sequencers[0]!, api: { ...api, trackerProof } }]
    const client = new QueryClient({ defaultOptions: { queries: { retry: false } } })
    render(
      <ConfigContext.Provider value={DEMO_CONFIG}>
        <QueryClientProvider client={client}>
          <DataProvider source={data.source} services={{ ...data.services, sequencers }}>
            <MemoryRouter
              initialEntries={[paths.vote(processId, formatVoteId(voteId))]}
              future={{ v7_startTransition: true, v7_relativeSplatPath: true }}
            >
              <TooltipProvider>
                <Routes>
                  <Route path={patterns.vote} element={<VotesPage />} />
                </Routes>
              </TooltipProvider>
            </MemoryRouter>
          </DataProvider>
        </QueryClientProvider>
      </ConfigContext.Provider>
    )
    const panel = await screen.findByTestId('tracker-proof')
    expect(await within(panel).findByText('The sequencer answered with a proof for another vote')).toBeInTheDocument()
    expect(within(panel).getByText(new RegExp(`it names vote ${formatVoteId(other)}`))).toBeInTheDocument()
  })
})
