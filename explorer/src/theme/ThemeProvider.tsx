import { useCallback, useEffect, useMemo, useState, type ReactNode } from 'react'
import { ThemeContext, type ThemeApi } from './theme-context'
import {
  applyTheme,
  readPreference,
  resolveTheme,
  systemPrefersLight,
  writePreference,
  type ThemePreference,
} from './theme'

/** Owns the theme preference; follows the OS setting live while on "system". */
export function ThemeProvider({ children }: { children: ReactNode }) {
  const [preference, setPreferenceState] = useState<ThemePreference>(() => readPreference())
  const [prefersLight, setPrefersLight] = useState<boolean>(() => systemPrefersLight())

  useEffect(() => {
    const mq = window.matchMedia?.('(prefers-color-scheme: light)')
    if (!mq) return
    const onChange = () => setPrefersLight(mq.matches)
    mq.addEventListener?.('change', onChange)
    return () => mq.removeEventListener?.('change', onChange)
  }, [])

  const resolved = resolveTheme(preference, prefersLight)
  useEffect(() => applyTheme(resolved), [resolved])

  const setPreference = useCallback((pref: ThemePreference) => {
    writePreference(pref)
    setPreferenceState(pref)
  }, [])

  const api = useMemo<ThemeApi>(() => ({ preference, resolved, setPreference }), [preference, resolved, setPreference])
  return <ThemeContext.Provider value={api}>{children}</ThemeContext.Provider>
}
