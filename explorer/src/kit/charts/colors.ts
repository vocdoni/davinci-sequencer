// Chart palette. Marks reference the theme tokens as CSS variables, so a chart
// drawn once follows the light/dark switch without re-rendering. The hex
// helpers remain for callers that need to interpolate between two literal
// colours (a CSS variable cannot be interpolated in JavaScript).

export const CHART_COLORS = {
  emerald: 'var(--color-series-1)',
  teal: 'var(--color-series-2)',
  slate: 'var(--color-series-3)',
  warmGray: 'var(--color-series-4)',
  pewter: 'var(--color-series-5)',
  grid: 'var(--color-charcoal)',
  axis: 'var(--color-ash)',
  hover: 'var(--color-onyx)',
  text: 'var(--color-ghost)',
  amber: 'var(--color-amber)',
  red: 'var(--color-red)',
} as const

/** Series order: emerald first, then the companions, then the greys. */
export const SERIES_COLORS: string[] = [
  CHART_COLORS.emerald,
  CHART_COLORS.teal,
  CHART_COLORS.slate,
  CHART_COLORS.warmGray,
  CHART_COLORS.pewter,
]

export function seriesColor(index: number): string {
  return SERIES_COLORS[((index % SERIES_COLORS.length) + SERIES_COLORS.length) % SERIES_COLORS.length] as string
}

interface Rgb {
  r: number
  g: number
  b: number
}

export function hexToRgb(hex: string): Rgb {
  const value = hex.replace('#', '')
  const full =
    value.length === 3
      ? value
          .split('')
          .map((c) => c + c)
          .join('')
      : value
  const int = Number.parseInt(full, 16)
  return { r: (int >> 16) & 255, g: (int >> 8) & 255, b: int & 255 }
}

export function rgbToHex({ r, g, b }: Rgb): string {
  const hex = (n: number) =>
    Math.round(Math.min(Math.max(n, 0), 255))
      .toString(16)
      .padStart(2, '0')
  return `#${hex(r)}${hex(g)}${hex(b)}`
}

/** Linear mix of two hex colours; `t=0` → `a`, `t=1` → `b`. */
export function mix(a: string, b: string, t: number): string {
  const clamped = Math.min(Math.max(t, 0), 1)
  const from = hexToRgb(a)
  const to = hexToRgb(b)
  return rgbToHex({
    r: from.r + (to.r - from.r) * clamped,
    g: from.g + (to.g - from.g) * clamped,
    b: from.b + (to.b - from.b) * clamped,
  })
}
