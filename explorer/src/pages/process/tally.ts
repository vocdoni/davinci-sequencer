// Tally rows for the results bars: each field's value, its share of the sum
// and its length relative to the largest field.

export interface TallyRow {
  field: number
  label: string
  value: bigint
  /** value / sum of all fields, 0 when the sum is 0. */
  share: number
  /** value / the largest value, for the bar length. */
  ofMax: number
}

const ratio = (a: bigint, b: bigint) => (b > 0n ? Number((a * 1_000_000n) / b) / 1_000_000 : 0)

export function tallyRows(values: bigint[], labels: string[] | null = null): TallyRow[] {
  const total = values.reduce((sum, v) => sum + v, 0n)
  const max = values.reduce((m, v) => (v > m ? v : m), 0n)
  return values.map((value, field) => ({
    field,
    label: labels?.[field] ?? `Field ${field + 1}`,
    value,
    share: ratio(value, total),
    ofMax: ratio(value, max),
  }))
}
