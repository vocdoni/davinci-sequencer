// Client for a davinci-sequencer node's HTTP API (README "HTTP API"). Every
// route the explorer uses is read-only. Wire conventions: camelCase fields,
// field elements as decimal strings, bytes as 0x hex, vote ids as 0x + 16 hex.

import { toBytes, type Hex } from './bytes'
import { formatVoteId } from './blob'
import type { TrackerProof } from './tracker'

export class SequencerApiError extends Error {
  constructor(
    message: string,
    readonly status: number,
    readonly code?: number
  ) {
    super(message)
  }
}

export interface SequencerInfo {
  /** Settling account; null for an observer node. */
  sequencerAddress: Hex | null
  chainId: number
  processRegistry: Hex
  ballotVkHash: Hex
  batchProgramVk: Hex
  resultsProgramVk: Hex
  observer: boolean
  settledBySelf: number
  syncedFromOthers: number
  lostRaces: number
}

export type SequencerProcessStatus = 'ready' | 'ended' | 'canceled' | 'paused' | 'results' | 'unknown'

export interface SequencerProcess {
  id: Hex
  status: SequencerProcessStatus
  isAcceptingVotes: boolean
  organizationId: Hex
  encryptionKey: { x: string; y: string }
  ballotMode: Record<string, unknown>
  census: { censusOrigin: number; censusRoot: string; censusURI: string }
  stateRoot: Hex
  localStateRoot?: Hex
  votersCount: number
  overwrittenVotesCount: number
  maxVoters: number
  startTime: number
  duration: number
  result?: number[]
  ignored?: boolean
  note?: string
}

export interface SequencerTransition {
  index: number
  oldRoot: Hex
  newRoot: Hex
  txHash: Hex
  blockNumber: number
  sender: Hex
  voters: number
  overwrites: number
  nBlobs: number
}

/** pending → aggregated (in a batch) → processed (proven) → settled (on-chain), or error. */
export type VoteStatus = 'pending' | 'aggregated' | 'processed' | 'settled' | 'error'

export interface VoteStatusResponse {
  status: VoteStatus
  error?: string
}

type FetchLike = (input: string, init?: RequestInit) => Promise<Response>

export class SequencerClient {
  constructor(
    readonly baseUrl: string,
    private readonly fetchImpl: FetchLike = (input, init) => fetch(input, init)
  ) {}

  private async get<T>(path: string, signal?: AbortSignal): Promise<T> {
    let res: Response
    try {
      res = await this.fetchImpl(`${this.baseUrl.replace(/\/+$/, '')}${path}`, {
        headers: { accept: 'application/json' },
        signal,
      })
    } catch (err) {
      throw new SequencerApiError(`${path}: ${err instanceof Error ? err.message : String(err)}`, 0)
    }
    if (!res.ok) {
      let message = `HTTP ${res.status}`
      let code: number | undefined
      try {
        const body = (await res.json()) as { error?: string; code?: number }
        if (body.error) message = body.error
        code = body.code
      } catch {
        // Not JSON: keep the status line.
      }
      throw new SequencerApiError(`${path}: ${message}`, res.status, code)
    }
    const text = await res.text()
    return (text === 'pong' ? text : JSON.parse(text)) as T
  }

  ping(signal?: AbortSignal): Promise<string> {
    return this.get<string>('/ping', signal)
  }

  info(signal?: AbortSignal): Promise<SequencerInfo> {
    return this.get<SequencerInfo>('/info', signal)
  }

  async processes(signal?: AbortSignal): Promise<Hex[]> {
    return (await this.get<{ processes: Hex[] }>('/processes', signal)).processes
  }

  process(pid: Hex, signal?: AbortSignal): Promise<SequencerProcess> {
    return this.get<SequencerProcess>(`/processes/${pid}`, signal)
  }

  async transitions(pid: Hex, signal?: AbortSignal): Promise<SequencerTransition[]> {
    return (await this.get<{ transitions: SequencerTransition[] }>(`/processes/${pid}/transitions`, signal)).transitions
  }

  /** Raw EIP-4844 blobs of transition `index` (0-based), in transaction order. */
  async transitionBlobs(pid: Hex, index: number, signal?: AbortSignal): Promise<Uint8Array[]> {
    const body = await this.get<{ blobs: Hex[] }>(`/processes/${pid}/transitions/${index}/blobs`, signal)
    return body.blobs.map((b) => toBytes(b))
  }

  voteStatus(pid: Hex, voteId: bigint, signal?: AbortSignal): Promise<VoteStatusResponse> {
    return this.get<VoteStatusResponse>(`/votes/${pid}/voteId/${formatVoteId(voteId)}`, signal)
  }

  async trackerProof(pid: Hex, voteId: bigint, signal?: AbortSignal): Promise<TrackerProof> {
    const body = await this.get<{ processId: Hex; voteId: string; root: Hex; siblings: Hex[] }>(
      `/votes/${pid}/voteId/${formatVoteId(voteId)}/proof`,
      signal
    )
    return { processId: body.processId, voteId: BigInt(body.voteId), root: body.root, siblings: body.siblings }
  }
}
