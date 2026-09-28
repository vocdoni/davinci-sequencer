import { describe, expect, it } from 'vitest'
import { CHART_COLORS, hexToRgb, mix, rgbToHex, seriesColor } from './colors'

describe('hex helpers', () => {
  it('round-trips a colour', () => {
    expect(rgbToHex(hexToRgb('#00d992'))).toBe('#00d992')
  })
  it('expands 3-digit hex', () => {
    expect(hexToRgb('#0f0')).toEqual({ r: 0, g: 255, b: 0 })
  })
  it('mixes endpoints exactly and clamps t', () => {
    expect(mix('#000000', '#ffffff', 0)).toBe('#000000')
    expect(mix('#000000', '#ffffff', 1)).toBe('#ffffff')
    expect(mix('#000000', '#ffffff', 0.5)).toBe('#808080')
    expect(mix('#000000', '#ffffff', 2)).toBe('#ffffff')
    expect(mix('#000000', '#ffffff', -1)).toBe('#000000')
  })
})

describe('seriesColor', () => {
  it('starts on the accent and cycles', () => {
    expect(seriesColor(0)).toBe(CHART_COLORS.emerald)
    expect(seriesColor(5)).toBe(seriesColor(0))
    expect(seriesColor(-1)).toBeTruthy()
  })

  it('references theme tokens, so both themes apply', () => {
    for (let i = 0; i < 5; i++) expect(seriesColor(i)).toMatch(/^var\(--color-series-\d\)$/)
  })
})
