// The indexer: one scan, one poll loop, one snapshot.
//
//   start() → check the RPC's chain id against the config, load the
//             IndexedDB cache, read the registry immutables, backfill from
//             the cursor (or the start block) in adaptive chunks, then poll.
//   tick()  → head block; if it moved, check that the last indexed block
//             still has the hash it was indexed with (a reorg below the lag
//             reindexes from the start block), scan the new range, re-read
//             the `getProcess` of every process the new events touched (one
//             multicall at the last indexed block, so the state and the
//             events describe the same chain), resolve settlement
//             transactions and block times in small batches, persist.
//
// It is an external store: `subscribe` / `getSnapshot` plug straight into
// `useSyncExternalStore`, and every publish produces a fresh top-level store
// object so memoised selectors invalidate once per change.

import type { Address, PublicClient } from 'viem'
import { REGISTRY_EVENT_ABIS } from '~contracts/abis'
import { clearStore, createIdbStore, loadStore, saveStore, type KVStore } from './persist'
import {
  applyBlockTimes,
  applyEvents,
  applyGenesisRoot,
  applyHead,
  applyProcessState,
  applyRegistryInfo,
  applyTxDetails,
  blocksMissingTime,
  bumpStore,
  createEmptyStore,
  txsMissingDetails,
} from './reduce'
import { DEFAULT_CHUNK, scanRange } from './scan'
import { registryIncomplete, StateReader } from './state'
import {
  processKey,
  txKey,
  type Hex,
  type IndexerError,
  type IndexerSnapshot,
  type IndexerStatus,
  type IndexerStore,
} from './types'

export interface IndexerConfig {
  client: PublicClient
  chainId: number
  networkName?: string
  registryAddress: Address
  /** Registry deployment block: the floor of every log scan. */
  startBlock: number
  /** Poll interval in ms. */
  pollIntervalMs?: number
  /** Blocks behind head the scan stays, so a shallow reorg never lands in the store. */
  confirmations?: number
  /** Initial `getLogs` window; the scan adapts from here. */
  chunkSize?: number
  blockTimeSeconds?: number
  /** Persistence backend; `null` disables the cache. Defaults to IndexedDB. */
  kv?: KVStore | null
  /** Minimum ms between cache writes during a backfill. */
  persistIntervalMs?: number
  /** Settlement transactions resolved per tick. */
  txPerTick?: number
  /** Block timestamps resolved per tick (only for logs that carried none). */
  blocksPerTick?: number
  /** Blocks after which every non-final process is re-read even without an event. */
  stateStaleBlocks?: number
}

const MAX_ERRORS = 20
const PUBLISH_THROTTLE_MS = 200
/** Failed lookups after which a transaction stops holding a place at the front of the queue. */
const TX_SKIP_AFTER = 3
/** Backoff of a skipped transaction's retries: doubles from the first to the cap. */
const TX_RETRY_MS = 30_000
const TX_RETRY_MAX_MS = 10 * 60_000

function emptyStatus(fromBlock: number): IndexerStatus {
  return {
    phase: 'idle',
    scanning: false,
    fromBlock,
    lastBlock: fromBlock,
    headBlock: fromBlock,
    progress: 0,
    eventCount: 0,
    requests: 0,
    lastPollAt: null,
    errors: [],
    skippedTx: [],
    chainMismatch: null,
  }
}

type Resolved = Required<
  Pick<
    IndexerConfig,
    'pollIntervalMs' | 'confirmations' | 'persistIntervalMs' | 'txPerTick' | 'blocksPerTick' | 'stateStaleBlocks'
  >
> &
  IndexerConfig

export class Indexer {
  readonly config: Resolved
  private store: IndexerStore
  private status: IndexerStatus
  private snapshot: IndexerSnapshot
  private listeners = new Set<() => void>()

  private readonly kv: KVStore | null
  private readonly reader: StateReader
  private chunkSize: number

  private started = false
  /** Bumped by every start(), so a loop left over from a stop/start pair exits. */
  private generation = 0
  private timer: ReturnType<typeof setTimeout> | null = null
  private abort: AbortController | null = null
  private running: Promise<void> | null = null
  private bootstrapped = false

  private dirtyProcesses = new Set<string>()
  private priorityTx = new Set<string>()
  private processCountDirty = false
  /** Failed lookups per tx key, for this session only, and when a skipped one is due again. */
  private txFailures = new Map<string, { count: number; retryAt: number }>()
  /** Head seen by the last reorg check; an unchanged head needs no new check. */
  private lastHead: { block: number; hash: Hex | null } | null = null

  private pendingPersist = false
  private lastPublish = 0
  private publishTimer: ReturnType<typeof setTimeout> | null = null
  private lastPersist = 0

  constructor(config: IndexerConfig) {
    this.config = {
      pollIntervalMs: 10_000,
      confirmations: 2,
      persistIntervalMs: 5_000,
      txPerTick: 40,
      blocksPerTick: 100,
      stateStaleBlocks: 1_000,
      ...config,
    }
    this.kv = config.kv === null ? null : (config.kv ?? createIdbStore())
    this.chunkSize = config.chunkSize ?? DEFAULT_CHUNK
    this.store = this.emptyStore()
    this.status = emptyStatus(config.startBlock)
    this.snapshot = { store: this.store, status: this.status }
    this.reader = new StateReader({ client: config.client, registryAddress: config.registryAddress })
  }

  // ── external store ─────────────────────────────────────────────────────────

  subscribe = (listener: () => void): (() => void) => {
    this.listeners.add(listener)
    return () => {
      this.listeners.delete(listener)
    }
  }

  getSnapshot = (): IndexerSnapshot => this.snapshot

  // ── lifecycle ──────────────────────────────────────────────────────────────

  start(): void {
    if (this.started) return
    this.started = true
    this.generation += 1
    this.abort = new AbortController()
    void this.loop(this.generation)
  }

  stop(): void {
    this.started = false
    this.abort?.abort()
    this.abort = null
    if (this.timer) clearTimeout(this.timer)
    this.timer = null
    if (this.publishTimer) clearTimeout(this.publishTimer)
    this.publishTimer = null
  }

  /** One tick now (also the "refresh" button). */
  async refresh(): Promise<void> {
    if (this.running) return this.running
    this.running = this.tick().finally(() => {
      this.running = null
    })
    return this.running
  }

  /** Drops the cached store for this deployment; the next start re-scans. */
  async clearCache(): Promise<void> {
    if (this.kv) await clearStore(this.kv, this.config.chainId, this.config.registryAddress)
  }

  /** Re-read a process's `getProcess` on the next tick. */
  ensureProcessState(id: string): void {
    this.dirtyProcesses.add(processKey(id))
    if (this.started && !this.running) void this.refresh()
  }

  /** Resolve these transactions first. */
  ensureTxDetails(hashes: Array<Hex | null | undefined>): void {
    let added = false
    for (const hash of hashes) {
      if (!hash) continue
      const key = txKey(hash)
      if (this.store.txDetails[key] || this.priorityTx.has(key)) continue
      this.priorityTx.add(key)
      added = true
    }
    if (added && this.started && !this.running) void this.refresh()
  }

  // ── internals ──────────────────────────────────────────────────────────────

  private emptyStore(): IndexerStore {
    return createEmptyStore({
      chainId: this.config.chainId,
      networkName: this.config.networkName,
      registryAddress: this.config.registryAddress,
      startBlock: this.config.startBlock,
      blockTimeSeconds: this.config.blockTimeSeconds,
    })
  }

  private async loop(generation: number): Promise<void> {
    const current = () => this.started && this.generation === generation
    while (current()) {
      await this.refresh()
      if (!current() || this.status.chainMismatch) break
      await new Promise<void>((resolve) => {
        this.timer = setTimeout(resolve, this.config.pollIntervalMs)
      })
    }
  }

  private async bootstrap(): Promise<boolean> {
    if (this.bootstrapped) return true
    this.status = { ...this.status, phase: 'loading' }
    this.publish(true)

    const actual = await this.config.client.getChainId()
    this.status = { ...this.status, requests: this.status.requests + 1 }
    if (actual !== this.config.chainId) {
      this.status = {
        ...this.status,
        phase: 'error',
        chainMismatch: { expected: this.config.chainId, actual },
      }
      this.pushError('chain', new Error(`RPC is on chain ${actual}, the config expects ${this.config.chainId}`))
      this.publish(true)
      return false
    }
    this.bootstrapped = true

    if (this.kv) {
      const cached = await loadStore(this.kv, this.config.chainId, this.config.registryAddress)
      if (cached) {
        cached.chain = {
          ...cached.chain,
          networkName: this.config.networkName ?? cached.chain.networkName,
          startBlock: this.config.startBlock,
          blockTimeSeconds: this.config.blockTimeSeconds ?? cached.chain.blockTimeSeconds,
        }
        this.store = cached
        // Processes read before the last session may have moved on.
        for (const key of cached.processOrder) this.dirtyProcesses.add(key)
        this.status = {
          ...this.status,
          fromBlock: cached.lastIndexedBlock + 1,
          lastBlock: cached.lastIndexedBlock,
          eventCount: cached.events.length,
        }
        this.publish(true)
      }
    }
    return true
  }

  private async tick(): Promise<void> {
    try {
      if (!(await this.bootstrap())) return
      const head = await this.config.client.getBlock({ blockTag: 'latest' })
      const headBlock = Number(head.number)
      this.status = { ...this.status, headBlock, lastPollAt: Date.now(), requests: this.status.requests + 1 }
      if (await this.reorged(headBlock, head.hash)) await this.reindex()
      applyHead(this.store, { block: headBlock, timestamp: Number(head.timestamp) })

      // Fields whose read failed stay null and are read again here; a zero
      // DKG adapter is a value, not a failure.
      const registry = this.store.chain.registry
      if (!registry || registryIncomplete(registry)) {
        try {
          const info = registry
            ? await this.reader.completeRegistryInfo(registry)
            : await this.reader.readRegistryInfo(headBlock)
          if (info !== registry) {
            applyRegistryInfo(this.store, info)
            this.pendingPersist = true
          }
        } catch (err) {
          this.pushError('state', err)
        }
      }

      const target = headBlock - this.config.confirmations
      const from = Math.max(this.config.startBlock, this.store.lastIndexedBlock + 1)
      // The cursor moves chunk by chunk inside scan(), so an aborted scan
      // resumes where it stopped.
      if (target >= from) await this.scan(from, target)

      // At the scan target, not the head: a state read ahead of the events
      // would show a transition settled inside the lag as a broken root chain.
      await this.refreshState(this.store.lastIndexedBlock)
      await this.resolveTransactions()
      await this.resolveBlockTimes()

      this.status = {
        ...this.status,
        phase: 'live',
        scanning: false,
        progress: 1,
        lastBlock: this.store.lastIndexedBlock,
        eventCount: this.store.events.length,
        requests: this.status.requests + this.reader.requests,
        skippedTx: [...this.txFailures]
          .filter(([key, f]) => f.count >= TX_SKIP_AFTER && !this.store.txDetails[key])
          .map(([key]) => key),
      }
      this.reader.requests = 0
      this.publish(true)
      if (this.pendingPersist) {
        this.pendingPersist = false
        await this.persist()
      }
    } catch (err) {
      this.pushError('poll', err)
      this.status = { ...this.status, phase: 'error', scanning: false }
      this.publish(true)
    }
  }

  /**
   * True when the last indexed block now has another hash than the one it
   * was indexed with: a reorg deeper than the lag. Skipped while the head is
   * the block the previous check saw.
   */
  private async reorged(headBlock: number, headHash: Hex | null | undefined): Promise<boolean> {
    const head = { block: headBlock, hash: (headHash?.toLowerCase() as Hex | undefined) ?? null }
    const { lastIndexedBlock, lastIndexedHash } = this.store
    const unchanged = this.lastHead?.block === head.block && this.lastHead.hash === head.hash
    if (!unchanged && lastIndexedHash && lastIndexedBlock <= headBlock) {
      const block = await this.config.client.getBlock({ blockNumber: BigInt(lastIndexedBlock) })
      this.status = { ...this.status, requests: this.status.requests + 1 }
      if (block.hash?.toLowerCase() !== lastIndexedHash) return true
    }
    this.lastHead = head
    return false
  }

  /** Drops the store, in memory and in the cache, and indexes again from the start block. */
  private async reindex(): Promise<void> {
    // A deep reorg reindexes everything; rewinding from the fork point by block ranges is the upgrade if they stop being rare.
    const message = `block ${this.store.lastIndexedBlock} changed hash, a reorg deeper than ${this.config.confirmations} blocks: reindexing from block ${this.config.startBlock}`
    console.warn(`davinci-explorer: ${message}`)
    this.pushError('scan', new Error(message))
    await this.clearCache()
    this.store = this.emptyStore()
    this.dirtyProcesses.clear()
    this.processCountDirty = false
    this.txFailures.clear()
    this.lastHead = null
    this.status = {
      ...this.status,
      fromBlock: this.config.startBlock,
      lastBlock: this.store.lastIndexedBlock,
      eventCount: 0,
      progress: 0,
    }
  }

  private async scan(from: number, to: number): Promise<void> {
    const span = to - from + 1
    const backfill = span > this.chunkSize
    if (backfill) {
      this.status = { ...this.status, phase: 'scanning', scanning: true, fromBlock: from, progress: 0 }
      this.publish(true)
    }
    // Read before the logs: a reorg in between then shows up as a changed
    // hash on the next tick instead of passing unnoticed.
    const target = await this.config.client.getBlock({ blockNumber: BigInt(to) })
    const targetHash = (target.hash?.toLowerCase() as Hex | undefined) ?? null
    this.status = { ...this.status, requests: this.status.requests + 1 }
    const result = await scanRange({
      client: this.config.client,
      addresses: [this.config.registryAddress],
      events: REGISTRY_EVENT_ABIS,
      fromBlock: from,
      toBlock: to,
      chunkSize: this.chunkSize,
      signal: this.abort?.signal,
      onChunk: async (chunk) => {
        const applied = applyEvents(this.store, chunk.events)
        if (applied > 0) this.pendingPersist = true
        for (const ev of chunk.events) {
          this.dirtyProcesses.add(processKey(ev.processId))
          if (ev.name === 'ProcessCreated') this.processCountDirty = true
        }
        this.store.lastIndexedBlock = Math.max(this.store.lastIndexedBlock, chunk.to)
        this.store.lastIndexedHash = chunk.to === to ? targetHash : null
        this.status = {
          ...this.status,
          lastBlock: this.store.lastIndexedBlock,
          eventCount: this.store.events.length,
          progress: span <= 0 ? 1 : Math.min(1, (chunk.to - from + 1) / span),
          requests: this.status.requests + 1,
        }
        this.publish()
        if (backfill) await this.persistThrottled()
      },
    })
    this.chunkSize = result.chunkSize
  }

  /** Contract state at block `at`, the last indexed block. */
  private async refreshState(at: number): Promise<void> {
    const stale = (block: number) => block === 0 || at - block >= this.config.stateStaleBlocks
    for (const key of this.store.processOrder) {
      const p = this.store.processes[key]!
      const final = p.state?.status === 'results' || p.state?.status === 'canceled'
      if (!p.state || (!final && stale(p.stateBlock))) this.dirtyProcesses.add(key)
    }

    const ids = [...this.dirtyProcesses].filter((key) => this.store.processes[key])
    this.dirtyProcesses.clear()
    if (ids.length > 0) {
      try {
        const states = await this.reader.readProcesses(ids as Hex[], at)
        for (const [id, state] of states) applyProcessState(this.store, id, state, at)
        this.pendingPersist = true
      } catch (err) {
        for (const id of ids) this.dirtyProcesses.add(id)
        this.pushError('state', err)
      }
    }

    const wantGenesis = this.store.processOrder
      .map((key) => this.store.processes[key]!)
      .filter((p) => p.state && !p.genesisRoot)
    if (wantGenesis.length > 0) {
      try {
        const roots = await this.reader.readGenesisRoots(wantGenesis.map((p) => ({ id: p.id, state: p.state! })))
        for (const [id, root] of roots) applyGenesisRoot(this.store, id, root)
        this.pendingPersist = true
      } catch (err) {
        this.pushError('state', err)
      }
    }

    const registry = this.store.chain.registry
    if (registry && (this.processCountDirty || stale(registry.readAtBlock))) {
      this.processCountDirty = false
      try {
        const count = await this.reader.readProcessCount(at)
        if (count != null) applyRegistryInfo(this.store, { ...registry, processCount: count, readAtBlock: at })
      } catch (err) {
        this.pushError('state', err)
      }
    }
  }

  /**
   * Resolves the oldest unresolved transactions. One whose lookup failed
   * `TX_SKIP_AFTER` times leaves the queue for this session, so it cannot
   * starve the newer ones, and is retried with backoff once they are done.
   */
  private async resolveTransactions(): Promise<void> {
    const limit = this.config.txPerTick
    const now = Date.now()
    const skipped = new Set<string>()
    const due: string[] = []
    for (const [key, f] of this.txFailures) {
      if (f.count < TX_SKIP_AFTER || this.store.txDetails[key]) continue
      skipped.add(key)
      if (f.retryAt <= now) due.push(key)
    }
    const priority = [...this.priorityTx]
      .filter((key) => !this.store.txDetails[key] && !skipped.has(key))
      .slice(0, limit)
    for (const key of priority) this.priorityTx.delete(key)
    const rest = txsMissingDetails(this.store, limit, skipped).filter((h) => !priority.includes(txKey(h)))
    const batch = [...priority, ...rest, ...due].slice(0, limit) as Hex[]
    if (batch.length === 0) return
    try {
      const details = await this.reader.readTxDetails(batch)
      if (details.length > 0) {
        applyTxDetails(this.store, details)
        this.pendingPersist = true
      }
      const resolved = new Set(details.map((d) => txKey(d.hash)))
      for (const hash of batch) {
        const key = txKey(hash)
        if (resolved.has(key)) {
          this.txFailures.delete(key)
          continue
        }
        const count = (this.txFailures.get(key)?.count ?? 0) + 1
        const backoff =
          count < TX_SKIP_AFTER ? 0 : Math.min(TX_RETRY_MAX_MS, TX_RETRY_MS * 2 ** (count - TX_SKIP_AFTER))
        this.txFailures.set(key, { count, retryAt: now + backoff })
      }
    } catch (err) {
      this.pushError('tx', err)
    }
  }

  private async resolveBlockTimes(): Promise<void> {
    const blocks = blocksMissingTime(this.store, this.config.blocksPerTick)
    if (blocks.length === 0) return
    try {
      const times = await this.reader.readBlockTimes(blocks)
      if (Object.keys(times).length > 0) {
        applyBlockTimes(this.store, times)
        this.pendingPersist = true
      }
    } catch (err) {
      this.pushError('chain', err)
    }
  }

  private pushError(scope: IndexerError['scope'], err: unknown): void {
    const message = (err instanceof Error ? err.message : String(err)).split('\n')[0]!
    const errors = [...this.status.errors, { at: Date.now(), scope, message }].slice(-MAX_ERRORS)
    this.status = { ...this.status, errors }
  }

  private async persist(): Promise<void> {
    if (!this.kv) return
    try {
      this.lastPersist = Date.now()
      await saveStore(this.kv, this.store)
    } catch (err) {
      this.pushError('persist', err)
    }
  }

  private async persistThrottled(): Promise<void> {
    if (Date.now() - this.lastPersist < this.config.persistIntervalMs) return
    await this.persist()
  }

  private publish(immediate = false): void {
    this.store = bumpStore(this.store)
    this.snapshot = { store: this.store, status: this.status }
    const now = Date.now()
    if (!immediate && now - this.lastPublish < PUBLISH_THROTTLE_MS) {
      if (!this.publishTimer) {
        this.publishTimer = setTimeout(() => {
          this.publishTimer = null
          this.publish(true)
        }, PUBLISH_THROTTLE_MS)
      }
      return
    }
    if (this.publishTimer) {
      clearTimeout(this.publishTimer)
      this.publishTimer = null
    }
    this.lastPublish = now
    for (const listener of this.listeners) listener()
  }
}

export function createIndexer(config: IndexerConfig): Indexer {
  return new Indexer(config)
}
