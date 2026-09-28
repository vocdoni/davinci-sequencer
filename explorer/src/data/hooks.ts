// Store hooks. Each is a `useSyncExternalStore` on the data source's snapshot
// plus a memoised selector: a page never holds derived state and never
// refetches. When the indexer publishes, the selectors re-run once.

import { useCallback, useEffect, useMemo, useRef, useSyncExternalStore } from 'react'
import {
  activityFeed,
  networkStats,
  processRow,
  processRows,
  releaseCheck,
  rootChain,
  searchStore,
  transitionByTx,
  transitionDetail,
  transitionRows,
  votesPerDay,
  type DayBucket,
  type FeedEntry,
  type NetworkStats,
  type ProcessFilter,
  type ProcessRow,
  type RootChain,
  type SearchHit,
  type TransitionDetail,
  type TransitionRow,
} from '~indexer/selectors'
import {
  processKey,
  type ChainMeta,
  type IndexerSnapshot,
  type IndexerStatus,
  type IndexerStore,
  type ProcessEntity,
  type TransitionEntity,
} from '~indexer/types'
import type { ReleaseMatch } from '~protocol/releases'
import { useDataSource } from './context'
import type { DataSourceKind } from './source'

/** The raw snapshot. Prefer the specific hooks below. */
export function useSnapshot(): IndexerSnapshot {
  const source = useDataSource()
  return useSyncExternalStore(source.subscribe, source.getSnapshot, source.getSnapshot)
}

export function useStore(): IndexerStore {
  return useSnapshot().store
}

export interface IndexerHandle {
  status: IndexerStatus
  kind: DataSourceKind
  /** A backfill is running: render skeletons, not "nothing found". */
  scanning: boolean
  /** Nothing indexed yet: the first load or the first scan, before any event arrived. */
  loading: boolean
  headBlock: number
  lastBlock: number
  progress: number
  refresh: () => Promise<void>
  clearCache: () => Promise<void>
}

/** Scan progress, chain head, errors, the chain-id check and a manual refresh. */
export function useIndexer(): IndexerHandle {
  const source = useDataSource()
  const { status } = useSnapshot()
  return useMemo(
    () => ({
      status,
      kind: source.kind,
      scanning: status.scanning,
      loading: status.eventCount === 0 && (status.phase === 'idle' || status.phase === 'loading' || status.scanning),
      headBlock: status.headBlock,
      lastBlock: status.lastBlock,
      progress: status.progress,
      refresh: () => source.refresh(),
      clearCache: () => source.clearCache(),
    }),
    [source, status]
  )
}

/** Chain head, registry address, immutables. */
export function useChain(): ChainMeta {
  return useStore().chain
}

/** The chain's "now": the head block's unix time (null before the first poll). */
export function useChainNow(): number | null {
  return useStore().chain.headTimestamp
}

export function useNetworkStats(): NetworkStats {
  const store = useStore()
  return useMemo(() => networkStats(store), [store])
}

/** Processes, newest first. */
export function useProcesses(filter: ProcessFilter = {}): ProcessRow[] {
  const store = useStore()
  const { status, keyMode, censusOrigin, organizer, query } = filter
  return useMemo(
    () => processRows(store, { status, keyMode, censusOrigin, organizer, query }),
    [store, status, keyMode, censusOrigin, organizer, query]
  )
}

export interface ProcessView {
  process: ProcessEntity
  row: ProcessRow
  transitions: TransitionRow[]
  rootChain: RootChain
}

/** One process with its transitions and root chain; re-reads its state when opened. */
export function useProcess(pid: string | undefined): ProcessView | null {
  const source = useDataSource()
  const store = useStore()
  useEffect(() => {
    if (pid) source.ensureProcessState(pid)
  }, [source, pid])
  return useMemo(() => {
    if (!pid) return null
    const process = store.processes[processKey(pid)]
    if (!process) return null
    return {
      process,
      row: processRow(store, process),
      transitions: transitionRows(store, pid),
      rootChain: rootChain(store, pid),
    }
  }, [store, pid])
}

export function useTransitions(pid: string | undefined): TransitionRow[] {
  const store = useStore()
  return useMemo(() => (pid ? transitionRows(store, pid) : []), [store, pid])
}

/** One transition with its decoded publics and the settlement checks. */
export function useTransition(pid: string | undefined, index: number | undefined): TransitionDetail | null {
  const source = useDataSource()
  const store = useStore()
  const detail = useMemo(
    () => (pid != null && index != null && Number.isInteger(index) ? transitionDetail(store, pid, index) : null),
    [store, pid, index]
  )
  const tx = detail?.transition.tx ?? null
  const hasTx = detail?.tx != null
  useEffect(() => {
    if (tx && !hasTx) source.ensureTxDetails([tx])
  }, [source, tx, hasTx])
  return detail
}

export function useTransitionByTx(hash: string | undefined): TransitionEntity | null {
  const store = useStore()
  return useMemo(() => (hash ? transitionByTx(store, hash) : null), [store, hash])
}

/** Newest events first, network-wide or for one process. */
export function useActivityFeed(limit = 20, pid?: string): FeedEntry[] {
  const store = useStore()
  return useMemo(() => activityFeed(store, limit, pid), [store, limit, pid])
}

/** Settled votes per UTC day, oldest first. */
export function useVotesPerDay(days = 30): DayBucket[] {
  const store = useStore()
  return useMemo(() => votesPerDay(store, days), [store, days])
}

/** The deployment's pins against the known davinci-zkvm releases. */
export function useReleaseCheck(): ReleaseMatch {
  const store = useStore()
  return useMemo(() => releaseCheck(store), [store])
}

export function useStoreSearch(query: string, limit = 8): SearchHit[] {
  const store = useStore()
  return useMemo(() => searchStore(store, query, limit), [store, query, limit])
}

/**
 * A resolver for the shell's global search box, backed by the store: it
 * knows which ids exist. The return type is structurally the shell's
 * `SearchTarget`.
 */
export function useIndexerSearchResolver(): (query: string) => { kind: 'route'; path: string; label: string } | null {
  const store = useStore()
  const latest = useRef(store)
  latest.current = store
  return useCallback((query: string) => {
    const [best] = searchStore(latest.current, query, 1)
    return best ? { kind: 'route' as const, path: best.href, label: best.label } : null
  }, [])
}
