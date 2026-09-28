import { useEffect, type ReactNode } from 'react'
import { DataSourceContext, ServicesContext } from './context'
import type { ExplorerServices } from './services'
import type { DataSource } from './source'

export interface DataProviderProps {
  source: DataSource
  services: ExplorerServices
  /** Start the source with the provider (default true). */
  autoStart?: boolean
  children: ReactNode
}

/** Owns the source's lifetime: starts it on mount, stops it on unmount. Mount once, above the router. */
export function DataProvider({ source, services, autoStart = true, children }: DataProviderProps) {
  useEffect(() => {
    if (!autoStart) return
    source.start()
    return () => source.stop()
  }, [source, autoStart])

  return (
    <DataSourceContext.Provider value={source}>
      <ServicesContext.Provider value={services}>{children}</ServicesContext.Provider>
    </DataSourceContext.Provider>
  )
}
