import { describe, expect, it } from 'vitest'
import { applyTheme, readPreference, resolveTheme, THEME_STORAGE_KEY, writePreference } from './theme'

function memoryStorage(initial: Record<string, string> = {}) {
  const map = new Map(Object.entries(initial))
  return {
    getItem: (k: string) => map.get(k) ?? null,
    setItem: (k: string, v: string) => void map.set(k, v),
  }
}

describe('theme preference', () => {
  it('defaults to system and ignores garbage', () => {
    expect(readPreference(memoryStorage())).toBe('system')
    expect(readPreference(memoryStorage({ [THEME_STORAGE_KEY]: 'purple' }))).toBe('system')
  })

  it('round-trips through storage', () => {
    const s = memoryStorage()
    writePreference('light', s)
    expect(readPreference(s)).toBe('light')
  })

  it('resolves system from the OS preference, dark without one', () => {
    expect(resolveTheme('system', true)).toBe('light')
    expect(resolveTheme('system', false)).toBe('dark')
    expect(resolveTheme('dark', true)).toBe('dark')
    expect(resolveTheme('light', false)).toBe('light')
  })

  it('applies the theme to <html> and the theme-color meta', () => {
    const meta = document.createElement('meta')
    meta.setAttribute('name', 'theme-color')
    document.head.appendChild(meta)
    applyTheme('light')
    expect(document.documentElement.getAttribute('data-theme')).toBe('light')
    expect(document.documentElement.style.colorScheme).toBe('light')
    expect(meta.getAttribute('content')).toBe('#f6f5f3')
    applyTheme('dark')
    expect(document.documentElement.getAttribute('data-theme')).toBe('dark')
  })
})
