// JSON for the raw views: bigints as decimal strings, two-space indent.

export function toJson(value: unknown): string {
  return JSON.stringify(value, (_key, v: unknown) => (typeof v === 'bigint' ? v.toString() : v), 2) ?? 'null'
}
