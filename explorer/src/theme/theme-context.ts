import { createContext, useContext } from 'react'
import type { ResolvedTheme, ThemePreference } from './theme'

export interface ThemeApi {
  preference: ThemePreference
  resolved: ResolvedTheme
  setPreference: (pref: ThemePreference) => void
}

export const ThemeContext = createContext<ThemeApi | null>(null)

export function useTheme(): ThemeApi {
  const api = useContext(ThemeContext)
  if (!api) throw new Error('useTheme must be used inside <ThemeProvider>')
  return api
}
