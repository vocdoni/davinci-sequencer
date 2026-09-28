import type { CellSection } from './cells'

/** Label and swatch of each blob section, shared by the layout bar and the cell view. */
export const SECTION_STYLE: Record<CellSection, { label: string; swatch: string }> = {
  'vote-ids': { label: 'Vote ids', swatch: 'bg-series-1' },
  updates: { label: 'Slot updates', swatch: 'bg-series-3' },
  accumulator: { label: 'Accumulator', swatch: 'bg-amber' },
  padding: { label: 'Zero padding', swatch: 'bg-onyx' },
}
