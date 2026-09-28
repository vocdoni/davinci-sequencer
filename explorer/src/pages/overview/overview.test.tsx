import { describe, expect, it } from 'vitest'
import { render, screen } from '@testing-library/react'
import { QueryClient, QueryClientProvider } from '@tanstack/react-query'
import type { ReactNode } from 'react'
import { MemoryRouter } from 'react-router-dom'
import { ConfigContext } from '~config/config-context'
import { DEMO_CONFIG, type RuntimeConfig } from '~config/runtime-config'
import { DataProvider } from '~data/DataProvider'
import type { DataSource } from '~data/source'
import { createDemoServices, demoFixture } from '~fixtures/demo'
import { applyHead, applyRegistryInfo, createEmptyStore } from '~indexer/reduce'
import type { IndexerSnapshot } from '~indexer/types'
import { TooltipProvider } from '~kit'
import { ProcessesPage } from '~pages/processes'
import { ThemeProvider } from '~theme/ThemeProvider'
import { OverviewPage } from '.'

// A registry that is deployed and indexed but has no process yet, like a
// fresh deployment: every panel must say what will appear, not look broken.
function emptySource(): DataSource {
  const fixture = demoFixture()
  const store = createEmptyStore({
    chainId: 100,
    networkName: 'Fresh network',
    registryAddress: '0x48a5091b64434a6690aea32455712bd2b7ee3e77',
    startBlock: 48_483_867,
  })
  applyRegistryInfo(store, { ...fixture.store.chain.registry!, processCount: 0 })
  applyHead(store, { block: 48_500_000, timestamp: 1_790_600_000 })
  store.lastIndexedBlock = 48_500_000
  const snapshot: IndexerSnapshot = {
    store,
    status: {
      phase: 'live',
      scanning: false,
      fromBlock: 48_483_867,
      lastBlock: 48_500_000,
      headBlock: 48_500_000,
      progress: 1,
      eventCount: 0,
      requests: 3,
      lastPollAt: Date.now(),
      errors: [],
      skippedTx: [],
      chainMismatch: null,
    },
  }
  return {
    kind: 'live',
    subscribe: () => () => {},
    getSnapshot: () => snapshot,
    start() {},
    stop() {},
    refresh: async () => {},
    ensureProcessState() {},
    ensureTxDetails() {},
    clearCache: async () => {},
  }
}

function renderEmpty(ui: ReactNode) {
  const config: RuntimeConfig = { ...DEMO_CONFIG, demo: false }
  return render(
    <ThemeProvider>
      <ConfigContext.Provider value={config}>
        <QueryClientProvider client={new QueryClient()}>
          <DataProvider source={emptySource()} services={createDemoServices()}>
            <MemoryRouter future={{ v7_startTransition: true, v7_relativeSplatPath: true }}>
              <TooltipProvider>{ui}</TooltipProvider>
            </MemoryRouter>
          </DataProvider>
        </QueryClientProvider>
      </ConfigContext.Provider>
    </ThemeProvider>
  )
}

describe('an empty registry', () => {
  it('the overview says what will appear', () => {
    renderEmpty(<OverviewPage />)
    expect(screen.getByTestId('empty-registry')).toHaveTextContent('No processes on this registry yet')
    expect(screen.getByText('No activity yet')).toBeInTheDocument()
    expect(screen.getByText('No ballots settled in the last 30 days')).toBeInTheDocument()
    // The deployment itself is still checkable.
    expect(screen.getByText(/matches davinci-zkvm/)).toBeInTheDocument()
    expect(screen.getByTestId('role-cards')).toBeInTheDocument()
  })

  it('the processes list explains itself', () => {
    renderEmpty(<ProcessesPage />)
    expect(screen.getByTestId('process-count')).toHaveTextContent('0 processes')
    expect(screen.getByText('No processes yet')).toBeInTheDocument()
  })
})
