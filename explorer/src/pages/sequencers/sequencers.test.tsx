import { describe, expect, it } from 'vitest'
import { render, screen, waitFor, within } from '@testing-library/react'
import { QueryClient, QueryClientProvider } from '@tanstack/react-query'
import { MemoryRouter } from 'react-router-dom'
import { ConfigContext } from '~config/config-context'
import { DEMO_CONFIG } from '~config/runtime-config'
import { DataProvider } from '~data/DataProvider'
import { TooltipProvider } from '~kit'
import { renderWithProviders, testData } from '../../test-utils'
import { SequencersPage } from './index'

describe('SequencersPage', () => {
  it('shows each demo sequencer, its checks and the settling accounts', async () => {
    renderWithProviders(<SequencersPage />, { route: '/sequencers' })
    const first = await screen.findByTestId('sequencer-0')
    await waitFor(() => expect(within(first).getByText('Signer')).toBeInTheDocument())
    const checks = within(first).getByTestId('sequencer-info-checks')
    expect(within(checks).getAllByRole('img', { name: 'passed' })).toHaveLength(5)
    await waitFor(() => expect(within(screen.getByTestId('sequencer-1')).getByText('Observer')).toBeInTheDocument())
    expect(within(screen.getByTestId('settlers')).getAllByRole('row').length).toBeGreaterThan(1)
  })

  it('explains an explorer with no sequencer configured and still lists the settling accounts', () => {
    const config = { ...DEMO_CONFIG, sequencers: [] }
    const data = testData(config)
    render(
      <ConfigContext.Provider value={config}>
        <QueryClientProvider client={new QueryClient()}>
          <DataProvider source={data.source} services={{ ...data.services, sequencers: [] }}>
            <MemoryRouter future={{ v7_startTransition: true, v7_relativeSplatPath: true }}>
              <TooltipProvider>
                <SequencersPage />
              </TooltipProvider>
            </MemoryRouter>
          </DataProvider>
        </QueryClientProvider>
      </ConfigContext.Provider>
    )
    expect(screen.getByText('No sequencer API configured')).toBeInTheDocument()
    expect(screen.queryByTestId('sequencer-0')).toBeNull()
    expect(screen.getByTestId('settlers')).toBeInTheDocument()
  })
})
