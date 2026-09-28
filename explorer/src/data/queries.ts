// On-demand hooks over the services: blobs, sequencer APIs, DKG state and
// metadata. TanStack Query caches them per deployment; nothing here goes into
// the persisted store.

import { useEffect, useMemo, useRef, useState } from 'react'
import { useQueries, useQuery, useQueryClient, type QueryClient, type UseQueryResult } from '@tanstack/react-query'
import { useRuntimeConfig } from '~config/config-context'
import { onchainRoots } from '~indexer/selectors'
import { processKey, transitionKey, txKey, type IndexerStore } from '~indexer/types'
import { decodeTransitionBlobs, type TransitionData } from '~protocol/blob'
import type { Hex } from '~protocol/bytes'
import type { SequencerInfo, VoteStatusResponse } from '~protocol/sequencer-api'
import { verifyTracker, type TrackerProof } from '~protocol/tracker'
import { useServices } from './context'
import { useSnapshot, useStore } from './hooks'
import type { DkgApplicationView, ExplorerServices, SequencerEndpoint, TransitionBlobs } from './services'

export interface DecodedTransitionBlobs extends TransitionBlobs {
  /** The cells decoded at the process's `numFields`; null when decoding failed. */
  decoded: TransitionData | null
  decodeError: string | null
}

function useDeploymentKey(): string {
  const config = useRuntimeConfig()
  return `${config.demo ? 'demo' : config.chainId}:${config.registryAddress.toLowerCase()}`
}

interface BlobJob {
  processId: Hex
  index: number
  /** The settlement block's exact time: the beacon slot comes from it. */
  timestamp: number
  versionedHashes: Hex[]
  numFields: number
}

/**
 * What fetching a transition's blobs needs from the store; null while any of
 * it is missing. An estimated block time could name the wrong beacon slot,
 * so the exact one is waited for like the transaction and the field count.
 */
function blobJob(store: IndexerStore, pid: string, index: number): BlobJob | null {
  const t = store.transitions[transitionKey(pid, index)]
  const p = store.processes[processKey(pid)]
  if (!t || !p) return null
  const tx = t.tx ? store.txDetails[txKey(t.tx)] : undefined
  const timestamp = t.timestamp ?? store.blockTimes[t.block] ?? null
  const numFields = p.state?.ballotMode.numFields ?? null
  if (!tx || timestamp == null || numFields == null) return null
  return {
    processId: t.processId,
    index,
    timestamp,
    versionedHashes: tx.blobVersionedHashes ?? [],
    numFields,
  }
}

async function loadBlobs(
  services: ExplorerServices,
  job: BlobJob,
  signal?: AbortSignal
): Promise<DecodedTransitionBlobs> {
  const result = await services.fetchTransitionBlobs(job, signal)
  try {
    return {
      ...result,
      decoded: decodeTransitionBlobs(
        result.blobs.map((b) => b.data),
        job.numFields
      ),
      decodeError: null,
    }
  } catch (err) {
    return { ...result, decoded: null, decodeError: err instanceof Error ? err.message : String(err) }
  }
}

/** Every input of `loadBlobs`, so a result is never served for other inputs. */
const blobsKey = (deployment: string, pid: string, index: number, job: BlobJob | null) =>
  [
    'transition-blobs',
    deployment,
    pid.toLowerCase(),
    index,
    job?.timestamp ?? null,
    job?.numFields ?? null,
    job?.versionedHashes.join(',') ?? null,
  ] as const

/**
 * The blobs of one transition, fetched from the beacon API (or a sequencer)
 * and decoded into vote ids, slot updates and the accumulator. Waits until
 * the indexer has the settlement transaction (its versioned hashes), the
 * block's exact time and the process's field count.
 */
export function useTransitionBlobs(
  pid: string | undefined,
  index: number | undefined,
  options: { enabled?: boolean } = {}
): UseQueryResult<DecodedTransitionBlobs> {
  const services = useServices()
  const store = useStore()
  const deployment = useDeploymentKey()
  const job = pid != null && index != null ? blobJob(store, pid, index) : null
  return useQuery({
    queryKey: blobsKey(deployment, pid ?? '', index ?? -1, job),
    queryFn: ({ signal }) => loadBlobs(services, job!, signal),
    enabled: (options.enabled ?? true) && job != null,
    staleTime: Infinity,
    gcTime: 10 * 60_000,
    retry: 1,
  })
}

export interface VoteInclusion {
  state: 'idle' | 'searching' | 'found' | 'not-found' | 'error'
  /** Transition whose blob lists the vote id. */
  transitionIndex: number | null
  checked: number
  total: number
  errors: string[]
}

/**
 * Finds the transition whose blob carries `voteId`, newest first. Fetches and
 * decodes one transition's blobs at a time (shared with `useTransitionBlobs`'
 * cache) and stops at the first hit. A transition whose settlement
 * transaction the indexer gave up reading counts as an error, not a wait.
 */
export function useVoteInclusion(pid: string | undefined, voteId: bigint | null): VoteInclusion {
  const services = useServices()
  const { store, status } = useSnapshot()
  const queryClient = useQueryClient()
  const deployment = useDeploymentKey()
  const p = pid ? store.processes[processKey(pid)] : undefined
  const total = p?.transitions.length ?? 0
  const skipped = new Set(status.skippedTx)
  const unreadable = (p?.transitions ?? []).map((k) => {
    const tx = store.transitions[k]!.tx
    return tx != null && skipped.has(txKey(tx)) && !store.txDetails[txKey(tx)]
  })
  const ready = p != null && unreadable.every((u, i) => u || blobJob(store, pid!, i) != null)
  // The search reruns when a transaction is skipped or a skipped one resolves.
  const unreadableKey = unreadable.flatMap((u, i) => (u ? [i] : [])).join(',')
  const [result, setResult] = useState<VoteInclusion>({
    state: 'idle',
    transitionIndex: null,
    checked: 0,
    total,
    errors: [],
  })
  const storeRef = useRef(store)
  storeRef.current = store

  useEffect(() => {
    if (!pid || voteId == null || !ready) {
      setResult({ state: 'idle', transitionIndex: null, checked: 0, total, errors: [] })
      return
    }
    let cancelled = false
    const controller = new AbortController()
    ;(async () => {
      const errors: string[] = []
      setResult({ state: 'searching', transitionIndex: null, checked: 0, total, errors })
      for (let i = total - 1, checked = 1; i >= 0; i--, checked++) {
        if (cancelled) return
        const job = blobJob(storeRef.current, pid, i)
        if (!job) {
          errors.push(`#${i}: the settlement transaction could not be read from the RPC`)
          if (!cancelled) setResult({ state: 'searching', transitionIndex: null, checked, total, errors: [...errors] })
          continue
        }
        try {
          const blobs = await fetchBlobsCached(queryClient, services, deployment, job, controller.signal)
          if (blobs.decoded?.voteIds.includes(voteId)) {
            if (!cancelled) setResult({ state: 'found', transitionIndex: i, checked, total, errors })
            return
          }
          if (blobs.decodeError) errors.push(`#${i}: ${blobs.decodeError}`)
        } catch (err) {
          errors.push(`#${i}: ${err instanceof Error ? err.message : String(err)}`)
        }
        if (!cancelled) setResult({ state: 'searching', transitionIndex: null, checked, total, errors: [...errors] })
      }
      if (!cancelled) {
        setResult({
          state: errors.length === total && total > 0 ? 'error' : 'not-found',
          transitionIndex: null,
          checked: total,
          total,
          errors,
        })
      }
    })()
    return () => {
      cancelled = true
      controller.abort()
    }
  }, [pid, voteId, ready, total, unreadableKey, services, queryClient, deployment])

  return result
}

function fetchBlobsCached(
  queryClient: QueryClient,
  services: ExplorerServices,
  deployment: string,
  job: BlobJob,
  signal: AbortSignal
): Promise<DecodedTransitionBlobs> {
  return queryClient.fetchQuery({
    queryKey: blobsKey(deployment, job.processId, job.index, job),
    queryFn: () => loadBlobs(services, job, signal),
    staleTime: Infinity,
  })
}

export interface SequencerState {
  endpoint: SequencerEndpoint
  info: UseQueryResult<SequencerInfo>
  processes: UseQueryResult<Hex[]>
}

/** Every configured sequencer with its `/info` and process list, polled every 30 s. */
export function useSequencers(): SequencerState[] {
  const services = useServices()
  const deployment = useDeploymentKey()
  const infos = useQueries({
    queries: services.sequencers.map((s) => ({
      queryKey: ['sequencer-info', deployment, s.index],
      queryFn: ({ signal }: { signal: AbortSignal }) => s.api.info(signal),
      refetchInterval: 30_000,
      retry: 0,
    })),
  })
  const processes = useQueries({
    queries: services.sequencers.map((s) => ({
      queryKey: ['sequencer-processes', deployment, s.index],
      queryFn: ({ signal }: { signal: AbortSignal }) => s.api.processes(signal),
      refetchInterval: 60_000,
      retry: 0,
    })),
  })
  return services.sequencers.map((endpoint, i) => ({ endpoint, info: infos[i]!, processes: processes[i]! }))
}

export interface VoteStatusBySequencer {
  endpoint: SequencerEndpoint
  status: UseQueryResult<VoteStatusResponse>
}

/** A vote's status as each configured sequencer reports it. */
export function useVoteStatus(pid: string | undefined, voteId: bigint | null): VoteStatusBySequencer[] {
  const services = useServices()
  const deployment = useDeploymentKey()
  const results = useQueries({
    queries: services.sequencers.map((s) => ({
      queryKey: ['vote-status', deployment, s.index, pid?.toLowerCase(), voteId?.toString()],
      queryFn: ({ signal }: { signal: AbortSignal }) => s.api.voteStatus(pid as Hex, voteId!, signal),
      enabled: pid != null && voteId != null,
      retry: 0,
      refetchInterval: (q: { state: { data?: VoteStatusResponse } }) =>
        q.state.data && q.state.data.status !== 'settled' && q.state.data.status !== 'error' ? 10_000 : false,
    })),
  })
  return services.sequencers.map((endpoint, i) => ({ endpoint, status: results[i]! }))
}

export interface TrackerCheck {
  /** As served: its vote id and process id are the sequencer's, not the request's. */
  proof: TrackerProof
  sequencer: SequencerEndpoint
  /** The answer names another vote id or process than the one asked for. */
  otherVote: boolean
  /** The proof is for the requested vote and its path reaches its root. */
  valid: boolean
  /** That root is one the registry has held for this process. */
  rootOnChain: boolean
}

/**
 * The first tracker proof a sequencer serves for the vote, checked in the
 * browser: the path from the requested vote id's leaf must reach the proof's
 * root and that root must be an on-chain root of the process. An answer for
 * another vote id or process is invalid, however well its own path hashes.
 */
export function useTrackerProof(pid: string | undefined, voteId: bigint | null): UseQueryResult<TrackerCheck | null> {
  const services = useServices()
  const store = useStore()
  const deployment = useDeploymentKey()
  const roots = useMemo(() => (pid ? onchainRoots(store, pid) : new Set<Hex>()), [store, pid])
  return useQuery({
    queryKey: ['tracker-proof', deployment, pid?.toLowerCase(), voteId?.toString(), [...roots].join(',')],
    enabled: pid != null && voteId != null && services.sequencers.length > 0,
    retry: 0,
    queryFn: async ({ signal }) => {
      const errors: string[] = []
      for (const s of services.sequencers) {
        try {
          const proof = await s.api.trackerProof(pid as Hex, voteId!, signal)
          const otherVote = proof.voteId !== voteId || proof.processId?.toLowerCase() !== pid!.toLowerCase()
          return {
            proof,
            sequencer: s,
            otherVote,
            valid: !otherVote && verifyTracker({ ...proof, processId: pid as Hex, voteId: voteId! }, proof.root),
            rootOnChain: roots.has(proof.root.toLowerCase() as Hex),
          }
        } catch (err) {
          errors.push(err instanceof Error ? err.message : String(err))
        }
      }
      if (errors.every((e) => /not found|404/i.test(e))) return null
      throw new Error(errors.join('; '))
    },
  })
}

/** DKG application state behind a DKG-mode process (null in sequencer mode). */
export function useDkgApplication(pid: string | undefined): UseQueryResult<DkgApplicationView | null> {
  const services = useServices()
  const store = useStore()
  const deployment = useDeploymentKey()
  const process = pid ? store.processes[processKey(pid)] : undefined
  const dkg = process?.state?.dkg
  const registry = store.chain.registry
  return useQuery({
    // Every input of readDkgApplication.
    queryKey: [
      'dkg-application',
      deployment,
      pid?.toLowerCase(),
      dkg?.epochId,
      dkg?.aid,
      dkg?.firstIndex,
      dkg?.count,
      dkg?.zeroSkipped,
      dkg?.resultsRequested,
      process?.state?.ballotMode.numFields,
      registry?.dkgAdapter,
      registry?.dkgManager,
      registry?.dkgAppManager,
    ],
    // Which fields the ciphertexts carry is unknown until the mask is read.
    enabled: process != null && dkg != null && dkg.zeroSkipped != null,
    queryFn: ({ signal }) => services.readDkgApplication(process!, store.chain.registry, signal),
    refetchInterval: 60_000,
  })
}

/** A JSON document such as a process's metadata URI. */
export function useJsonDocument(url: string | null | undefined): UseQueryResult<unknown> {
  const services = useServices()
  return useQuery({
    queryKey: ['json-document', url],
    enabled: !!url,
    queryFn: ({ signal }) => services.fetchJson(url!, signal),
    staleTime: 10 * 60_000,
    retry: 1,
  })
}
