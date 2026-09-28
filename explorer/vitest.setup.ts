import '@testing-library/jest-dom/vitest'

// jsdom has neither: Radix reads `matchMedia`, charts measure with a
// ResizeObserver, and the theme follows `prefers-color-scheme`.
if (typeof window !== 'undefined') {
  if (!window.matchMedia) {
    window.matchMedia = ((query: string) => ({
      matches: false,
      media: query,
      onchange: null,
      addListener: () => {},
      removeListener: () => {},
      addEventListener: () => {},
      removeEventListener: () => {},
      dispatchEvent: () => false,
    })) as typeof window.matchMedia
  }
  // ScrollRestoration scrolls on every navigation; jsdom does not implement it.
  window.scrollTo = (() => {}) as typeof window.scrollTo
  if (!window.ResizeObserver) {
    window.ResizeObserver = class {
      observe() {}
      unobserve() {}
      disconnect() {}
    } as unknown as typeof window.ResizeObserver
  }
}

// React Router's data router builds a `Request` with an AbortSignal; Node's
// Request rejects jsdom's AbortSignal class. Tests never abort navigations,
// so drop the signal.
if (typeof globalThis.Request === 'function') {
  const NativeRequest = globalThis.Request
  globalThis.Request = class extends NativeRequest {
    constructor(input: RequestInfo | URL, init?: RequestInit) {
      const { signal: _signal, ...rest } = init ?? {}
      super(input, rest)
    }
  } as typeof Request
}
