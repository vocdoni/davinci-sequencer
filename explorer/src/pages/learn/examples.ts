// Real examples from this deployment for the guide's "see it" links: the
// newest process with a settled transition, one with results, one on a DKG
// key. On an empty registry every example is null and the guide links to the
// list pages instead.

import { useMemo } from 'react'
import { useProcesses } from '~data/hooks'
import type { ProcessRow } from '~indexer/selectors'

export interface LearnExamples {
  /** Newest process with at least one transition. */
  active: ProcessRow | null
  withResults: ProcessRow | null
  dkg: ProcessRow | null
  /** Newest process of any kind. */
  newest: ProcessRow | null
}

export function pickExamples(rows: ProcessRow[]): LearnExamples {
  return {
    active: rows.find((r) => r.transitions > 0) ?? null,
    withResults: rows.find((r) => r.hasResults) ?? null,
    dkg: rows.find((r) => r.keyMode === 'dkg-automatic' || r.keyMode === 'dkg-locked') ?? null,
    newest: rows[0] ?? null,
  }
}

export function useLearnExamples(): LearnExamples {
  const rows = useProcesses()
  return useMemo(() => pickExamples(rows), [rows])
}
