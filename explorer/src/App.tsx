import { useMemo, type ReactNode } from 'react'
import { QueryClient, QueryClientProvider } from '@tanstack/react-query'
import { RouterProvider } from 'react-router-dom'
import { ConfigProvider } from '~config/ConfigProvider'
import { useRuntimeConfig } from '~config/config-context'
import { DataProvider } from '~data/DataProvider'
import { createExplorerData } from '~data/create'
import { router } from '~routes/router'
import { ThemeProvider } from '~theme/ThemeProvider'

// Provider order:
//   Theme:       no dependencies; the config error screen is themed too.
//   Config:      gates on /config.json; nothing chain-aware mounts before it.
//   QueryClient: on-demand reads (blobs, sequencers, DKG).
//   Data:        the indexer (or the demo fixture) and the services, built from the config.
//   Router:      last, so route elements can use all of the above.
const queryClient = new QueryClient({
  defaultOptions: {
    queries: {
      staleTime: 10_000,
      retry: 1,
      refetchOnWindowFocus: false,
    },
  },
})

function Data({ children }: { children: ReactNode }) {
  const config = useRuntimeConfig()
  const data = useMemo(() => createExplorerData({ config }), [config])
  return (
    <DataProvider source={data.source} services={data.services}>
      {children}
    </DataProvider>
  )
}

export function App() {
  return (
    <ThemeProvider>
      <ConfigProvider>
        <QueryClientProvider client={queryClient}>
          <Data>
            <RouterProvider router={router} future={{ v7_startTransition: true }} />
          </Data>
        </QueryClientProvider>
      </ConfigProvider>
    </ThemeProvider>
  )
}
