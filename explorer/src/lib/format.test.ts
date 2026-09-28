import { describe, expect, it } from 'vitest'
import {
  bigIntToHex,
  formatBytes,
  formatDuration,
  formatGwei,
  formatNumber,
  formatShare,
  formatTimestamp,
  formatWei,
  shortAddress,
  shortHash,
  timeAgo,
} from './format'

describe('shortHash', () => {
  it('is empty-safe and leaves non-hex alone', () => {
    expect(shortHash(null)).toBe('')
    expect(shortHash('hello')).toBe('hello')
    expect(shortHash('0xabcd')).toBe('0xabcd')
  })

  it('truncates in the middle', () => {
    expect(shortHash(`0x${'a'.repeat(62)}`)).toBe('0xaaaaaa…aaaa')
    expect(shortAddress('0x1234567890abcdef1234567890abcdef12345678')).toBe('0x1234…5678')
  })
})

describe('numbers and units', () => {
  it('formats counts', () => {
    expect(formatNumber(1234567)).toBe('1,234,567')
    expect(formatNumber(10n ** 12n)).toBe('1,000,000,000,000')
    expect(formatNumber(null)).toBe('—')
  })

  it('pads a bigint to 32 bytes', () => {
    expect(bigIntToHex(255n)).toBe(`0x${'0'.repeat(62)}ff`)
  })

  it('formats durations', () => {
    expect(formatDuration(42)).toBe('42 s')
    expect(formatDuration(600)).toBe('10 min')
    expect(formatDuration(3600 + 300)).toBe('1 h 5 min')
    expect(formatDuration(86400 * 2)).toBe('2 d')
    expect(formatDuration(86400 + 3600 * 3)).toBe('1 d 3 h')
  })

  it('formats timestamps in UTC', () => {
    expect(formatTimestamp(0)).toBe('1970-01-01 00:00 UTC')
    expect(formatTimestamp(null)).toBe('—')
  })

  it('reads relative times both ways', () => {
    expect(timeAgo(1000, 1000)).toBe('just now')
    expect(timeAgo(1000, 1600)).toBe('10 min ago')
    expect(timeAgo(1600, 1000)).toBe('in 10 min')
  })

  it('formats byte counts', () => {
    expect(formatBytes(512)).toBe('512 B')
    expect(formatBytes(131072)).toBe('128 KiB')
    expect(formatBytes(1536)).toBe('1.5 KiB')
  })

  it('formats wei and gwei', () => {
    expect(formatWei(10n ** 18n)).toBe('1')
    expect(formatWei(1234n * 10n ** 14n)).toBe('0.1234')
    expect(formatWei(1n)).toBe('<0.000001')
    expect(formatWei(0n)).toBe('0')
    expect(formatGwei(1_500_000_000n)).toBe('1.50')
  })

  it('formats shares', () => {
    expect(formatShare(1, 3)).toBe('33.3%')
    expect(formatShare(1, 0)).toBe('—')
  })
})
