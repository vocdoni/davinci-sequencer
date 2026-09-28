// The processes list's filter lives in the URL query (`paths.processes`).
// Unknown values are dropped rather than matched, so a stale link shows the
// full list instead of an empty one.

import type { ProcessFilter, ProcessPhase } from '~indexer/selectors'
import type { ProcessListFilter } from '~routes/paths'
import { CENSUS_ORIGIN_INFO, KEY_MODE_INFO, type CensusOriginName, type KeyModeName } from '~protocol/types'

export interface FilterOption<T extends string> {
  value: T
  label: string
}

/** Phases a user filters by, in lifecycle order. `ready` covers upcoming, open and closed. */
export const PHASE_OPTIONS: FilterOption<ProcessPhase | 'ready'>[] = [
  { value: 'upcoming', label: 'Upcoming' },
  { value: 'open', label: 'Open' },
  { value: 'paused', label: 'Paused' },
  { value: 'closed', label: 'Voting closed' },
  { value: 'ready', label: 'Ready (any time)' },
  { value: 'ended', label: 'Ended' },
  { value: 'results', label: 'Results' },
  { value: 'canceled', label: 'Canceled' },
]

export const KEY_MODE_OPTIONS: FilterOption<KeyModeName>[] = (
  ['sequencer', 'dkg-automatic', 'dkg-locked'] as const
).map((value) => ({ value, label: KEY_MODE_INFO[value].label }))

export const CENSUS_OPTIONS: FilterOption<CensusOriginName>[] = (
  ['merkle-static', 'merkle-dynamic', 'onchain-dynamic', 'csp'] as const
).map((value) => ({ value, label: CENSUS_ORIGIN_INFO[value].label }))

const pick = <T extends string>(options: FilterOption<T>[], raw: string | null): T | undefined =>
  options.find((o) => o.value === raw)?.value

/** The URL query as a list filter (for links) and a selector filter (for `useProcesses`). */
export function readProcessFilter(params: URLSearchParams): { list: ProcessListFilter; filter: ProcessFilter } {
  const status = pick(PHASE_OPTIONS, params.get('status'))
  const keyMode = pick(KEY_MODE_OPTIONS, params.get('keyMode'))
  const census = pick(CENSUS_OPTIONS, params.get('census'))
  const organizer = /^0x[0-9a-fA-F]{40}$/.test(params.get('organizer')?.trim() ?? '')
    ? params.get('organizer')!.trim().toLowerCase()
    : undefined
  const q = params.get('q')?.trim() || undefined
  return {
    list: { status, keyMode, census, organizer, q },
    filter: { status, keyMode, censusOrigin: census, organizer, query: q },
  }
}

/** True when any filter narrows the list. */
export function isFiltered(list: ProcessListFilter): boolean {
  return Object.values(list).some((v) => v != null && v !== '')
}
