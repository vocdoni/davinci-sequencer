// Pure presentation helpers. Tiny and side-effect free.

/** Truncate a 0x-prefixed hex string to "0xABCD…1234" form. */
export function shortHash(value: string | undefined | null, head = 6, tail = 4): string {
  if (!value) return ''
  if (!value.startsWith('0x')) return value
  if (value.length <= head + tail + 2) return value
  return `${value.slice(0, 2 + head)}…${value.slice(-tail)}`
}

/** Truncate an Ethereum address to "0xABCD…1234" form. */
export function shortAddress(addr: string | undefined | null): string {
  return shortHash(addr, 4, 4)
}

/** A bigint as 0x-prefixed, zero-padded 64-hex-digit string. */
export function bigIntToHex(value: bigint): `0x${string}` {
  return `0x${value.toString(16).padStart(64, '0')}`
}

const INT = new Intl.NumberFormat('en-US')

/** Thousands separators; bigints and numbers alike. */
export function formatNumber(value: number | bigint | null | undefined): string {
  if (value == null) return '—'
  return INT.format(value)
}

/** Seconds as a coarse duration: "45 s", "12 min", "3 h 5 min", "4 d 2 h". */
export function formatDuration(seconds: number | bigint | null | undefined): string {
  if (seconds == null) return '—'
  const s = Math.max(0, Math.floor(Number(seconds)))
  if (s < 60) return `${s} s`
  const m = Math.floor(s / 60)
  if (m < 60) return `${m} min`
  const h = Math.floor(m / 60)
  if (h < 24) return m % 60 ? `${h} h ${m % 60} min` : `${h} h`
  const d = Math.floor(h / 24)
  return h % 24 ? `${d} d ${h % 24} h` : `${d} d`
}

/** Unix seconds as an ISO-like UTC timestamp: "2026-09-28 14:03 UTC". */
export function formatTimestamp(unixSeconds: number | bigint | null | undefined): string {
  if (unixSeconds == null) return '—'
  const d = new Date(Number(unixSeconds) * 1000)
  if (Number.isNaN(d.getTime())) return '—'
  return `${d.toISOString().slice(0, 16).replace('T', ' ')} UTC`
}

/** "5 min ago", "in 3 h": relative to `now` (unix seconds). */
export function timeAgo(unixSeconds: number | null | undefined, now = Date.now() / 1000): string {
  if (unixSeconds == null) return '—'
  const delta = Math.round(now - unixSeconds)
  if (Math.abs(delta) < 10) return 'just now'
  const text = formatDuration(Math.abs(delta))
  return delta > 0 ? `${text} ago` : `in ${text}`
}

/** Byte counts: "512 B", "1.5 KiB", "128 KiB". */
export function formatBytes(bytes: number | null | undefined): string {
  if (bytes == null) return '—'
  if (bytes < 1024) return `${bytes} B`
  const kib = bytes / 1024
  if (kib < 1024) return `${kib % 1 === 0 ? kib : kib.toFixed(1)} KiB`
  const mib = kib / 1024
  return `${mib.toFixed(mib < 10 ? 2 : 1)} MiB`
}

/**
 * A wei amount in the chain's native unit with up to `digits` fraction
 * digits, trailing zeros trimmed: 123400000000000n → "0.0001234".
 */
export function formatWei(wei: bigint | null | undefined, digits = 6): string {
  if (wei == null) return '—'
  const negative = wei < 0n
  const abs = negative ? -wei : wei
  const whole = abs / 10n ** 18n
  const frac = (abs % 10n ** 18n).toString().padStart(18, '0').slice(0, digits).replace(/0+$/, '')
  const text = frac ? `${INT.format(whole)}.${frac}` : INT.format(whole)
  if (text === '0' && abs > 0n) return `<0.${'0'.repeat(Math.max(0, digits - 1))}1`
  return negative ? `-${text}` : text
}

/** A wei price in gwei with two decimals: 1_500_000_000n → "1.50". */
export function formatGwei(wei: bigint | null | undefined): string {
  if (wei == null) return '—'
  const hundredths = (wei + 5_000_000n) / 10_000_000n
  return `${INT.format(hundredths / 100n)}.${(hundredths % 100n).toString().padStart(2, '0')}`
}

/** Share as a percentage with one decimal; `null` total gives a dash. */
export function formatShare(part: number, total: number): string {
  if (!(total > 0)) return '—'
  return `${((part / total) * 100).toFixed(1)}%`
}
