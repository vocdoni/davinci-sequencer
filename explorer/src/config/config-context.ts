import { createContext, useContext } from 'react'
import type { RuntimeConfig } from './runtime-config'

export const ConfigContext = createContext<RuntimeConfig | null>(null)

/**
 * The deployment the explorer is pointed at. Every page reads the chain,
 * registry, endpoints and the demo flag from here; nothing imports
 * `config.json` directly.
 */
export function useRuntimeConfig(): RuntimeConfig {
  const config = useContext(ConfigContext)
  if (!config) throw new Error('useRuntimeConfig must be used inside <ConfigProvider>')
  return config
}

/** True when the app runs off the synthetic fixture. */
export function useIsDemo(): boolean {
  return useRuntimeConfig().demo
}
