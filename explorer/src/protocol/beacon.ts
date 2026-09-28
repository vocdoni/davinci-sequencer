// Blob retrieval from a consensus-layer beacon API. The slot of an execution
// block is `(timestamp - genesis_time) / SECONDS_PER_SLOT`, as davinci-node
// and the sequencer derive it. `blob_sidecars/{slot}` first: it returns every
// blob of the block with its KZG commitment, and a sidecar belongs to a
// transaction when `0x01 ‖ sha256(commitment)[1..]` is one of the
// transaction's versioned hashes. On any other answer (beacons past Fulu may
// drop sidecars), `blobs/{slot}?versioned_hashes=…`, which returns bare blobs
// the beacon matched itself. Neither path lets the browser check the bytes
// against the commitment (that needs a KZG library), but the sidecar path at
// least shows the commitment the beacon claims. Beacons prune blobs after the retention window (about 18 days on
// Ethereum), so an old slot answers 404.

import { versionedHash } from './blob'
import { toBytes, type Hex } from './bytes'
import { BLOB_SIZE } from './limits'

export class BlobFetchError extends Error {
  constructor(
    message: string,
    readonly status?: number
  ) {
    super(message)
  }
}

export interface FetchedBlob {
  /** Position of the blob in the transaction. */
  index: number
  versionedHash: Hex
  /** KZG commitment as the source reported it (the beacon sidecar); null when unknown. */
  commitment: Hex | null
  data: Uint8Array
  /**
   * How the blob was tied to the transaction:
   * `commitment`: the sidecar's commitment hashes to the versioned hash;
   * `beacon-filter`: the beacon selected it by versioned hash;
   * `sequencer`: served by a sequencer node for that transition index.
   * None of them recomputes the KZG commitment from the bytes.
   */
  binding: 'commitment' | 'beacon-filter' | 'sequencer'
  source: string
}

export interface BeaconTiming {
  genesisTime: number
  secondsPerSlot: number
}

type FetchLike = (input: string, init?: RequestInit) => Promise<Response>

interface Sidecar {
  index: string
  blob: Hex
  kzg_commitment: Hex
}

export class BeaconClient {
  private timingPromise: Promise<BeaconTiming> | null = null

  constructor(
    readonly baseUrl: string,
    private readonly fetchImpl: FetchLike = (input, init) => fetch(input, init)
  ) {}

  private url(path: string): string {
    return `${this.baseUrl.replace(/\/+$/, '')}${path}`
  }

  private async getJson<T>(path: string, signal?: AbortSignal, query = ''): Promise<T> {
    let res: Response
    try {
      res = await this.fetchImpl(this.url(path + query), { headers: { accept: 'application/json' }, signal })
    } catch (err) {
      throw new BlobFetchError(`beacon ${path}: ${err instanceof Error ? err.message : String(err)}`)
    }
    if (!res.ok) {
      const why = res.status === 404 ? 'not found (pruned or not yet available)' : `HTTP ${res.status}`
      throw new BlobFetchError(`beacon ${path}: ${why}`, res.status)
    }
    return (await res.json()) as T
  }

  /** Genesis time and slot length, read once. */
  timing(signal?: AbortSignal): Promise<BeaconTiming> {
    if (!this.timingPromise) {
      this.timingPromise = (async () => {
        const genesis = await this.getJson<{ data: { genesis_time: string } }>('/eth/v1/beacon/genesis', signal)
        const spec = await this.getJson<{ data: Record<string, string> }>('/eth/v1/config/spec', signal)
        const genesisTime = Number(genesis.data.genesis_time)
        const secondsPerSlot = Number(spec.data.SECONDS_PER_SLOT)
        if (!Number.isFinite(genesisTime) || !(secondsPerSlot > 0))
          throw new BlobFetchError('beacon: bad genesis or spec')
        return { genesisTime, secondsPerSlot }
      })().catch((err) => {
        this.timingPromise = null
        throw err
      })
    }
    return this.timingPromise
  }

  async slotFor(timestamp: number, signal?: AbortSignal): Promise<number> {
    const { genesisTime, secondsPerSlot } = await this.timing(signal)
    return slotForTimestamp(timestamp, genesisTime, secondsPerSlot)
  }

  async blobSidecars(slot: number, signal?: AbortSignal): Promise<Sidecar[]> {
    const body = await this.getJson<{ data: Sidecar[] }>(`/eth/v1/beacon/blob_sidecars/${slot}`, signal)
    return body.data
  }

  /**
   * The blobs of a transaction, in `versionedHashes` order, from the block
   * mined at `timestamp`: `blob_sidecars/{slot}` matched by commitment, and
   * `blobs/{slot}` filtered by all the hashes in one request when that fails.
   */
  async fetchBlobs(
    { timestamp, versionedHashes }: { timestamp: number; versionedHashes: Hex[] },
    signal?: AbortSignal
  ): Promise<FetchedBlob[]> {
    const slot = await this.slotFor(timestamp, signal)
    let sidecars: string
    try {
      return matchSidecars(await this.blobSidecars(slot, signal), versionedHashes, this.baseUrl)
    } catch (err) {
      if (signal?.aborted) throw err
      sidecars = err instanceof Error ? err.message : String(err)
    }
    try {
      const query = `?${versionedHashes.map((h) => `versioned_hashes=${h}`).join('&')}`
      const body = await this.getJson<{ data: Hex[] }>(`/eth/v1/beacon/blobs/${slot}`, signal, query)
      return filteredBlobs(body.data, versionedHashes, this.baseUrl)
    } catch (err) {
      if (signal?.aborted) throw err
      const message = err instanceof Error ? err.message : String(err)
      throw new BlobFetchError(`${sidecars}; ${message}`, err instanceof BlobFetchError ? err.status : undefined)
    }
  }
}

/**
 * The answer of `blobs/{slot}?versioned_hashes=…`: exactly one blob per
 * requested hash, in the block's order, which for one transaction's blobs is
 * the order of its versioned hashes. Anything else is refused.
 */
export function filteredBlobs(data: Hex[] | undefined, versionedHashes: Hex[], source: string): FetchedBlob[] {
  const n = Array.isArray(data) ? data.length : 0
  if (n !== versionedHashes.length || new Set(versionedHashes.map((h) => h.toLowerCase())).size !== n) {
    throw new BlobFetchError(`beacon: ${n} blobs for ${versionedHashes.length} versioned hashes`)
  }
  return versionedHashes.map((hash, index) => ({
    index,
    versionedHash: hash.toLowerCase() as Hex,
    commitment: null,
    data: checkBlob(data![index]!),
    binding: 'beacon-filter',
    source,
  }))
}

export function slotForTimestamp(timestamp: number, genesisTime: number, secondsPerSlot: number): number {
  if (timestamp < genesisTime) throw new BlobFetchError('block before the beacon genesis')
  return Math.floor((timestamp - genesisTime) / secondsPerSlot)
}

function checkBlob(hex: Hex): Uint8Array {
  const data = toBytes(hex)
  if (data.length !== BLOB_SIZE) throw new BlobFetchError(`blob of ${data.length} bytes, want ${BLOB_SIZE}`)
  return data
}

/** Picks the sidecars whose commitments hash to `versionedHashes`, in that order. */
export function matchSidecars(sidecars: Sidecar[], versionedHashes: Hex[], source: string): FetchedBlob[] {
  const byHash = new Map<string, Sidecar>()
  for (const s of sidecars) byHash.set(versionedHash(s.kzg_commitment).toLowerCase(), s)
  return versionedHashes.map((hash, index) => {
    const s = byHash.get(hash.toLowerCase())
    if (!s) throw new BlobFetchError(`beacon: no sidecar for versioned hash ${hash}`)
    return {
      index,
      versionedHash: hash.toLowerCase() as Hex,
      commitment: s.kzg_commitment.toLowerCase() as Hex,
      data: checkBlob(s.blob),
      binding: 'commitment',
      source,
    }
  })
}
