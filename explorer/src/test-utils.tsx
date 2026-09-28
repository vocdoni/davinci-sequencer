import { render, type RenderOptions, type RenderResult } from '@testing-library/react'
import { QueryClient, QueryClientProvider } from '@tanstack/react-query'
import { MemoryRouter } from 'react-router-dom'
import type { ReactElement, ReactNode } from 'react'
import { ConfigContext } from '~config/config-context'
import { DEMO_CONFIG, type RuntimeConfig } from '~config/runtime-config'
import { DataProvider } from '~data/DataProvider'
import { createExplorerData } from '~data/create'
import { TooltipProvider } from '~kit'
import { ThemeProvider } from '~theme/ThemeProvider'

/** Frozen demo data: no timers, the same fixture every test. */
export function testData(config: RuntimeConfig = DEMO_CONFIG) {
  return createExplorerData({ config: { ...config, demo: true }, demoOptions: { blockIntervalMs: 0 } })
}

/**
 * Renders a component with everything a page may reach for: theme, runtime
 * config, a query client, the demo data source and services, a router and
 * the tooltip provider.
 */
export function renderWithProviders(
  ui: ReactElement,
  options: RenderOptions & { config?: Partial<RuntimeConfig>; route?: string } = {}
): RenderResult {
  const { config, route = '/', ...rest } = options
  const value: RuntimeConfig = { ...DEMO_CONFIG, ...config }
  const data = testData(value)
  const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } })
  const Wrapper = ({ children }: { children: ReactNode }) => (
    <ThemeProvider>
      <ConfigContext.Provider value={value}>
        <QueryClientProvider client={queryClient}>
          <DataProvider source={data.source} services={data.services}>
            <MemoryRouter initialEntries={[route]} future={{ v7_startTransition: true, v7_relativeSplatPath: true }}>
              <TooltipProvider>{children}</TooltipProvider>
            </MemoryRouter>
          </DataProvider>
        </QueryClientProvider>
      </ConfigContext.Provider>
    </ThemeProvider>
  )
  return render(ui, { wrapper: Wrapper, ...rest })
}
