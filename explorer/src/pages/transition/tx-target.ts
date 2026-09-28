// Where a transaction hash leads in the explorer.

import { transitionByTx } from '~indexer/selectors'
import type { IndexerStore } from '~indexer/types'
import { paths } from '~routes/paths'

/** 0x and 64 hex digits. */
export function isTxHash(value: string): boolean {
  return /^0x[0-9a-fA-F]{64}$/.test(value.trim())
}

/** Where a registry transaction leads: its transition, the process it created or touched. */
export function txTarget(store: IndexerStore, hash: string): string | null {
  const h = hash.toLowerCase()
  const t = transitionByTx(store, h)
  if (t) return paths.transition(t.processId, t.index)
  for (const key of store.processOrder) {
    const p = store.processes[key]!
    if (p.createdTx === h) return paths.process(p.id)
    if (p.results?.tx === h || p.decryptionRequest?.tx === h) return paths.process(p.id, 'results')
  }
  // Any other registry event: a status, census, duration or max-voters change.
  const ev = store.events.find((e) => e.tx === h)
  return ev ? paths.process(ev.processId) : null
}
