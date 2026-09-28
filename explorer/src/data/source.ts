// The contract between the pages and whatever feeds them: the live indexer
// (viem against a real chain) or the demo fixture (`?demo=1`, no RPC at all).
// Both are external stores, so every hook is a `useSyncExternalStore` over
// the same snapshot shape and no page knows which one it talks to.

import { Indexer, type IndexerConfig } from '~indexer/indexer'
import type { Hex, IndexerSnapshot } from '~indexer/types'

export type DataSourceKind = 'live' | 'demo'

export interface DataSource {
  readonly kind: DataSourceKind
  /** `useSyncExternalStore` pair. */
  subscribe(listener: () => void): () => void
  getSnapshot(): IndexerSnapshot
  /** Begin scanning / advancing. Idempotent. */
  start(): void
  stop(): void
  /** Force one poll. */
  refresh(): Promise<void>
  /** Re-read a process's `getProcess` on the next poll. */
  ensureProcessState(processId: string): void
  /** Resolve these settlement transactions first. */
  ensureTxDetails(hashes: Array<Hex | null | undefined>): void
  /** Drop the IndexedDB cache of this deployment. */
  clearCache(): Promise<void>
}

/** Wraps a live `Indexer` as a `DataSource`. */
export function createLiveDataSource(config: IndexerConfig): DataSource & { indexer: Indexer } {
  const indexer = new Indexer(config)
  return {
    kind: 'live',
    indexer,
    subscribe: indexer.subscribe,
    getSnapshot: indexer.getSnapshot,
    start: () => indexer.start(),
    stop: () => indexer.stop(),
    refresh: () => indexer.refresh(),
    ensureProcessState: (id) => indexer.ensureProcessState(id),
    ensureTxDetails: (hashes) => indexer.ensureTxDetails(hashes),
    clearCache: () => indexer.clearCache(),
  }
}
