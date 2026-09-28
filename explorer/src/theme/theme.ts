// Theme preference: system, light or dark, persisted in localStorage. The
// inline script in index.html applies the same rule before first paint;
// keep the key, the rule and the canvas colours in sync with it.

export type ThemePreference = 'system' | 'light' | 'dark'
export type ResolvedTheme = 'light' | 'dark'

export const THEME_STORAGE_KEY = 'davinci-explorer:theme'
export const THEME_PREFERENCES: ThemePreference[] = ['system', 'light', 'dark']

/** Canvas colour per theme, for `<meta name="theme-color">`. */
export const THEME_CANVAS: Record<ResolvedTheme, string> = { dark: '#050507', light: '#f6f5f3' }

export function isThemePreference(value: unknown): value is ThemePreference {
  return value === 'system' || value === 'light' || value === 'dark'
}

export function readPreference(storage: Pick<Storage, 'getItem'> | null = safeStorage()): ThemePreference {
  try {
    const value = storage?.getItem(THEME_STORAGE_KEY)
    return isThemePreference(value) ? value : 'system'
  } catch {
    return 'system'
  }
}

export function writePreference(pref: ThemePreference, storage: Pick<Storage, 'setItem'> | null = safeStorage()): void {
  try {
    storage?.setItem(THEME_STORAGE_KEY, pref)
  } catch {
    // Private mode or storage disabled: the choice lasts for the session.
  }
}

/** System follows `prefers-color-scheme`; without a light preference the explorer is dark. */
export function resolveTheme(pref: ThemePreference, systemPrefersLight: boolean): ResolvedTheme {
  if (pref === 'system') return systemPrefersLight ? 'light' : 'dark'
  return pref
}

export function systemPrefersLight(): boolean {
  return typeof window !== 'undefined' && !!window.matchMedia?.('(prefers-color-scheme: light)').matches
}

export function applyTheme(
  theme: ResolvedTheme,
  doc: Document | null = typeof document === 'undefined' ? null : document
): void {
  if (!doc) return
  const root = doc.documentElement
  root.setAttribute('data-theme', theme)
  root.style.colorScheme = theme
  doc.querySelector('meta[name="theme-color"]')?.setAttribute('content', THEME_CANVAS[theme])
}

function safeStorage(): Storage | null {
  try {
    return typeof window === 'undefined' ? null : window.localStorage
  } catch {
    return null
  }
}
