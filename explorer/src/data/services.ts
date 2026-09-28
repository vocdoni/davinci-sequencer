// On-demand reads that do not belong in the indexed store: blob bytes (large,
// fetched per transition), sequencer node APIs, DKG application state and
// metadata documents. The live implementation talks to the beacon API, the
// configured sequencers and the chain; the demo one serves the fixture.

import type { Address, PublicClient } from 'viem'
import { dkgAppManagerAbi, dkgManagerAbi } from '~contracts/abis'
import type { Point } from '~protocol/babyjubjub'
import { BeaconClient, BlobFetchError, type FetchedBlob } from '~protocol/beacon'
import type { Hex } from '~protocol/bytes'
import { SequencerClient } from '~protocol/sequencer-api'
import type { ProcessEntity, RegistryInfo } from '~indexer/types'
import type { RuntimeConfig } from '~config/runtime-config'

/** The sequencer API surface the explorer uses; the demo implements it too. */
export type SequencerApi = Pick<
  SequencerClient,
  'ping' | 'info' | 'processes' | 'process' | 'transitions' | 'transitionBlobs' | 'voteStatus' | 'trackerProof'
>

export interface SequencerEndpoint {
  index: number
  /** URL the browser calls (may be a same-origin proxy path). */
  url: string
  /** The node's own URL, for display. */
  upstream: string
  api: SequencerApi
}

export interface BlobRequest {
  processId: Hex
  /** 0-based transition index. */
  index: number
  /** Unix seconds of the settlement block. */
  timestamp: number | null
  /** The settlement transaction's `blobVersionedHashes`, in order. */
  versionedHashes: Hex[]
}

export interface BlobAttempt {
  source: 'beacon' | 'sequencer'
  url: string
  error: string
}

export interface TransitionBlobs {
  blobs: FetchedBlob[]
  source: 'beacon' | 'sequencer' | 'demo'
  sourceUrl: string
  /** Sources tried before the one that answered. */
  attempts: BlobAttempt[]
}

export interface DkgCiphertextView {
  /** DKG ciphertext index. */
  index: number
  /** Ballot field this ciphertext carries. */
  field: number
  completed: boolean
  plaintext: bigint
}

export interface DkgApplicationView {
  manager: Address
  appManager: Address
  epochId: Hex
  aid: Hex
  creator: Address
  poolIndex: number
  poolKey: Point | null
  /** PK_org in the DKG's reduced form; the identity (0, 1) when automatic. */
  organizerPK: Point
  /** Revealed organizer secret; 0 while sealed and always for automatic. */
  organizerSecret: bigint
  revealed: boolean
  /** Key the ballots are encrypted to, in the DKG's reduced form. */
  applicationKey: Point
  createdAtBlock: number
  /** The decryption requests of the tally, when requested. */
  ciphertexts: DkgCiphertextView[]
}

export interface ExplorerServices {
  kind: 'live' | 'demo'
  sequencers: SequencerEndpoint[]
  /** The blobs of a transition: the beacon first, then each sequencer. */
  fetchTransitionBlobs(request: BlobRequest, signal?: AbortSignal): Promise<TransitionBlobs>
  /** DKG application state of a DKG-mode process; null in sequencer mode. */
  readDkgApplication(
    process: ProcessEntity,
    registry: RegistryInfo | null,
    signal?: AbortSignal
  ): Promise<DkgApplicationView | null>
  /** A JSON document (process metadata, census files). */
  fetchJson(url: string, signal?: AbortSignal): Promise<unknown>
}

export class ServiceError extends Error {}

const errorText = (err: unknown) => (err instanceof Error ? err.message : String(err))

export function createLiveServices(config: RuntimeConfig, client: PublicClient | null): ExplorerServices {
  const beacon = config.beaconUrl ? new BeaconClient(config.beaconUrl) : null
  const sequencers: SequencerEndpoint[] = config.sequencers.map((s, index) => ({
    index,
    url: s.url,
    upstream: s.upstream ?? s.url,
    api: new SequencerClient(s.url),
  }))

  return {
    kind: 'live',
    sequencers,

    async fetchTransitionBlobs(request, signal) {
      const attempts: BlobAttempt[] = []
      if (beacon && request.timestamp != null && request.versionedHashes.length > 0) {
        try {
          const blobs = await beacon.fetchBlobs(
            { timestamp: request.timestamp, versionedHashes: request.versionedHashes },
            signal
          )
          return { blobs, source: 'beacon', sourceUrl: beacon.baseUrl, attempts }
        } catch (err) {
          attempts.push({ source: 'beacon', url: beacon.baseUrl, error: errorText(err) })
        }
      }
      for (const s of sequencers) {
        try {
          const data = await s.api.transitionBlobs(request.processId, request.index, signal)
          if (data.length !== request.versionedHashes.length && request.versionedHashes.length > 0) {
            throw new BlobFetchError(`${data.length} blobs, the transaction carries ${request.versionedHashes.length}`)
          }
          const blobs: FetchedBlob[] = data.map((bytes, i) => ({
            index: i,
            versionedHash: request.versionedHashes[i] ?? ('0x' as Hex),
            commitment: null,
            data: bytes,
            binding: 'sequencer',
            source: s.upstream,
          }))
          return { blobs, source: 'sequencer', sourceUrl: s.upstream, attempts }
        } catch (err) {
          attempts.push({ source: 'sequencer', url: s.upstream, error: errorText(err) })
        }
      }
      if (!beacon && sequencers.length === 0) throw new ServiceError('No beacon API or sequencer is configured')
      throw new ServiceError(attempts.map((a) => `${a.source} ${a.url}: ${a.error}`).join('; ') || 'No blob source')
    },

    async readDkgApplication(process, registry) {
      const dkg = process.state?.dkg
      if (!dkg || !client) return null
      if (!registry?.dkgManager || !registry.dkgAppManager) throw new ServiceError('The registry has no DKG adapter')
      const manager = registry.dkgManager
      const appManager = registry.dkgAppManager
      const epochId = dkg.epochId
      const aid = dkg.aid
      const [app, key] = await Promise.all([
        client.readContract({
          address: appManager,
          abi: dkgAppManagerAbi,
          functionName: 'getApplication',
          args: [epochId, aid],
        }),
        client.readContract({
          address: appManager,
          abi: dkgAppManagerAbi,
          functionName: 'getApplicationKey',
          args: [epochId, aid],
        }),
      ])
      let poolKey: Point | null = null
      try {
        const [x, y] = await client.readContract({
          address: manager,
          abi: dkgManagerAbi,
          functionName: 'getPoolKey',
          args: [epochId, app.poolIndex],
        })
        poolKey = { x, y }
      } catch {
        poolKey = null
      }
      const ciphertexts: DkgCiphertextView[] = []
      if (dkg.resultsRequested && dkg.count > 0) {
        const zeroSkipped = dkg.zeroSkipped
        if (zeroSkipped == null) throw new ServiceError('The skipped ballot fields are not known yet')
        const fields = Array.from({ length: process.state!.ballotMode.numFields }, (_, i) => i).filter(
          (i) => ((zeroSkipped >> i) & 1) === 0
        )
        const records = await Promise.all(
          fields.slice(0, dkg.count).map((_, j) =>
            client.readContract({
              address: manager,
              abi: dkgManagerAbi,
              functionName: 'getCombinedDecryption',
              args: [epochId, aid, dkg.firstIndex + j],
            })
          )
        )
        records.forEach((r, j) =>
          ciphertexts.push({
            index: dkg.firstIndex + j,
            field: fields[j]!,
            completed: r.completed,
            plaintext: r.plaintext,
          })
        )
      }
      return {
        manager,
        appManager,
        epochId,
        aid,
        creator: app.creator.toLowerCase() as Address,
        poolIndex: app.poolIndex,
        poolKey,
        organizerPK: { x: app.organizerPK.x, y: app.organizerPK.y },
        organizerSecret: app.organizerSecret,
        revealed: app.organizerSecret !== 0n,
        applicationKey: { x: key[0], y: key[1] },
        createdAtBlock: Number(app.createdAtBlock),
        ciphertexts,
      }
    },

    async fetchJson(url, signal) {
      const res = await fetch(resolveUri(url), { signal, headers: { accept: 'application/json' } })
      if (!res.ok) throw new ServiceError(`${url}: HTTP ${res.status}`)
      return res.json()
    },
  }
}

/** ipfs:// URIs through a public gateway; everything else unchanged. */
export function resolveUri(uri: string): string {
  if (uri.startsWith('ipfs://')) return `https://ipfs.io/ipfs/${uri.slice('ipfs://'.length)}`
  return uri
}
