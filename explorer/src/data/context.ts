// Contexts of the data layer: the data source (live indexer or demo) and
// the on-demand services. `DataProvider` (DataProvider.tsx) fills both.

import { createContext, useContext } from 'react'
import type { ExplorerServices } from './services'
import type { DataSource } from './source'

export const DataSourceContext = createContext<DataSource | null>(null)
export const ServicesContext = createContext<ExplorerServices | null>(null)

export function useDataSource(): DataSource {
  const source = useContext(DataSourceContext)
  if (!source) throw new Error('useDataSource must be used inside <DataProvider>')
  return source
}

/** Null instead of throwing, for components that may render outside the provider. */
export function useOptionalDataSource(): DataSource | null {
  return useContext(DataSourceContext)
}

export function useServices(): ExplorerServices {
  const services = useContext(ServicesContext)
  if (!services) throw new Error('useServices must be used inside <DataProvider>')
  return services
}
