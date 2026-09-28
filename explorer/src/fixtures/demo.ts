// Demo data source and services: the synthetic fixture behind the same
// interfaces as the live indexer, with a head block that keeps moving so the
// clocks, "time ago" strings and the chain pill behave as on a real chain.

import { bumpStore } from '~indexer/reduce'
import type { IndexerSnapshot, IndexerStatus, IndexerStore } from '~indexer/types'
import type { DataSource } from '~data/source'
import type { DkgApplicationView, ExplorerServices, SequencerApi, SequencerEndpoint } from '~data/services'
import { ServiceError } from '~data/services'
import type { FetchedBlob } from '~protocol/beacon'
import { SequencerApiError, type SequencerProcess, type SequencerTransition } from '~protocol/sequencer-api'
import type { Hex } from '~protocol/bytes'
import { KNOWN_RELEASES } from '~protocol/releases'
import { buildFixture, demoTransitionBlobs, type Fixture, type FixtureOptions } from './synthetic'
import { voteIdTreeProof } from './smt'
import { circomToReduced } from '~protocol/babyjubjub'

export interface DemoOptions extends FixtureOptions {
  /** Wall-clock ms between fake blocks; 0 freezes the chain. */
  blockIntervalMs?: number
}

const cache = new Map<string, Fixture>()

/** The fixture, built once per option set (building it takes a moment). */
export function demoFixture(options: FixtureOptions = {}): Fixture {
  const key = JSON.stringify(options)
  let f = cache.get(key)
  if (!f) {
    f = buildFixture(options)
    cache.set(key, f)
  }
  return f
}

export function createDemoDataSource(options: DemoOptions = {}): DataSource {
  const { blockIntervalMs = 5_000, ...fixtureOptions } = options
  // Each source advances its own head, so it works on a copy of the fixture store.
  let store: IndexerStore =
    typeof structuredClone === 'function'
      ? structuredClone(demoFixture(fixtureOptions).store)
      : buildFixture(fixtureOptions).store
  let status: IndexerStatus = {
    phase: 'live',
    scanning: false,
    fromBlock: store.chain.startBlock,
    lastBlock: store.lastIndexedBlock,
    headBlock: store.chain.headBlock,
    progress: 1,
    eventCount: store.events.length,
    requests: 0,
    lastPollAt: Date.now(),
    errors: [],
    skippedTx: [],
    chainMismatch: null,
  }
  let snapshot: IndexerSnapshot = { store, status }
  const listeners = new Set<() => void>()
  let timer: ReturnType<typeof setInterval> | null = null

  const publish = () => {
    store = bumpStore(store)
    snapshot = { store, status }
    for (const listener of listeners) listener()
  }

  const advance = () => {
    const head = store.chain.headBlock + 1
    const ts = (store.chain.headTimestamp ?? 0) + store.chain.blockTimeSeconds
    store.chain = { ...store.chain, headBlock: head, headTimestamp: ts }
    store.blockTimes[head] = ts
    store.lastIndexedBlock = head
    status = { ...status, headBlock: head, lastBlock: head, lastPollAt: Date.now() }
    publish()
  }

  return {
    kind: 'demo',
    subscribe(listener) {
      listeners.add(listener)
      return () => {
        listeners.delete(listener)
      }
    },
    getSnapshot: () => snapshot,
    start() {
      if (timer || blockIntervalMs <= 0) return
      timer = setInterval(advance, blockIntervalMs)
    },
    stop() {
      if (timer) clearInterval(timer)
      timer = null
    },
    async refresh() {
      advance()
    },
    ensureProcessState() {},
    ensureTxDetails() {},
    async clearCache() {},
  }
}

function sequencerApi(fixture: Fixture, index: number): SequencerApi {
  const node = fixture.sequencers[index]!
  const store = fixture.store
  const release = KNOWN_RELEASES[0]!
  const transitionsOf = (pid: Hex) =>
    store.processes[pid.toLowerCase()]?.transitions.map((k) => store.transitions[k]!) ?? []
  const notFound = (what: string): never => {
    throw new SequencerApiError(`${what}: not found`, 404, 40401)
  }
  const voteIdsOf = (pid: Hex) => transitionsOf(pid).flatMap((t) => fixture.transitionData.get(t.key)?.voteIds ?? [])

  return {
    async ping() {
      return 'pong'
    },
    async info() {
      return {
        sequencerAddress: node.address,
        chainId: store.chain.chainId,
        processRegistry: store.chain.registryAddress,
        ballotVkHash: release.ballotVKHash,
        batchProgramVk: release.batchProgramVK,
        resultsProgramVk: release.resultsProgramVK,
        observer: node.observer,
        settledBySelf: node.settledBySelf,
        syncedFromOthers: node.syncedFromOthers,
        lostRaces: node.lostRaces,
      }
    },
    async processes() {
      return store.processOrder as Hex[]
    },
    async process(pid) {
      const p = store.processes[pid.toLowerCase()]
      if (!p?.state) return notFound(pid)
      const s = p.state
      const view: SequencerProcess = {
        id: p.id,
        status: s.status,
        isAcceptingVotes: s.status === 'ready' && (store.chain.headTimestamp ?? 0) < s.startTime + s.duration,
        organizationId: s.organizer,
        encryptionKey: { x: s.encryptionKey.x.toString(), y: s.encryptionKey.y.toString() },
        ballotMode: { numFields: s.ballotMode.numFields },
        census: {
          censusOrigin: ['unknown', 'merkle-static', 'merkle-dynamic', 'onchain-dynamic', 'csp'].indexOf(
            s.census.origin
          ),
          censusRoot: BigInt(s.census.root).toString(),
          censusURI: s.census.uri,
        },
        stateRoot: s.latestStateRoot,
        localStateRoot: s.latestStateRoot,
        votersCount: s.votersCount,
        overwrittenVotesCount: s.overwrittenVotesCount,
        maxVoters: s.maxVoters,
        startTime: s.startTime,
        duration: s.duration,
        ...(s.result.length ? { result: s.result.map(Number) } : {}),
      }
      return view
    },
    async transitions(pid) {
      return transitionsOf(pid).map((t): SequencerTransition => ({
        index: t.index,
        oldRoot: t.rootBefore,
        newRoot: t.rootAfter,
        txHash: t.tx ?? '0x',
        blockNumber: t.block,
        sender: t.sender,
        voters: t.newVoters + t.overwrites,
        overwrites: t.overwrites,
        nBlobs: t.nBlobs,
      }))
    },
    async transitionBlobs(pid, i) {
      const data = fixture.transitionData.get(`${pid.toLowerCase()}:${i}`)
      if (!data) return notFound(`transition ${i}`)
      return demoTransitionBlobs(data)
    },
    async voteStatus(pid, voteId) {
      const pending = node.votes.find((v) => v.processId === pid.toLowerCase() && v.voteId === voteId)
      if (pending) return pending.error ? { status: pending.status, error: pending.error } : { status: pending.status }
      if (voteIdsOf(pid).includes(voteId)) return { status: 'settled' }
      return notFound('vote')
    },
    async trackerProof(pid, voteId) {
      const p = store.processes[pid.toLowerCase()]
      const ids = voteIdsOf(pid)
      if (!p?.state || !ids.includes(voteId)) return notFound('vote')
      return { processId: p.id, voteId, root: p.state.latestStateRoot, siblings: voteIdTreeProof(ids, voteId) }
    },
  }
}

export function createDemoServices(fixture: Fixture = demoFixture()): ExplorerServices {
  const sequencers: SequencerEndpoint[] = fixture.sequencers.map((s, index) => ({
    index,
    url: s.url,
    upstream: index === 0 ? 'https://sequencer-1.demo.invalid' : 'https://observer.demo.invalid',
    api: sequencerApi(fixture, index),
  }))

  return {
    kind: 'demo',
    sequencers,
    async fetchTransitionBlobs(request) {
      const key = `${request.processId.toLowerCase()}:${request.index}`
      const data = fixture.transitionData.get(key)
      const t = fixture.store.transitions[key]
      if (!data || !t) throw new ServiceError(`no demo transition ${key}`)
      const tx = t.tx ? fixture.store.txDetails[t.tx] : undefined
      const blobs: FetchedBlob[] = demoTransitionBlobs(data).map((bytes, index) => ({
        index,
        versionedHash: request.versionedHashes[index] ?? tx?.blobVersionedHashes?.[index] ?? '0x',
        commitment: tx?.commitments[index] ?? null,
        data: bytes,
        binding: 'commitment',
        source: 'demo',
      }))
      return { blobs, source: 'demo', sourceUrl: 'demo://beacon', attempts: [] }
    },
    async readDkgApplication(process, registry) {
      const d = fixture.dkg.get(process.id)
      const dkg = process.state?.dkg
      if (!d || !dkg || !registry?.dkgManager || !registry.dkgAppManager) return null
      const view: DkgApplicationView = {
        manager: registry.dkgManager,
        appManager: registry.dkgAppManager,
        epochId: d.epochId,
        aid: d.aid,
        creator: registry.dkgAdapter!,
        poolIndex: d.poolIndex,
        poolKey: circomToReduced(d.poolKey),
        organizerPK: circomToReduced(d.organizerPK),
        organizerSecret: d.organizerSecret,
        revealed: d.organizerSecret !== 0n,
        applicationKey: circomToReduced(d.applicationKey),
        createdAtBlock: process.createdBlock,
        ciphertexts: dkg.resultsRequested
          ? d.ciphertexts.map((c, i) => ({ index: c.index, field: i, completed: c.completed, plaintext: c.plaintext }))
          : [],
      }
      return view
    },
    async fetchJson(url) {
      if (fixture.metadata.has(url)) return fixture.metadata.get(url)
      throw new ServiceError(`${url}: not part of the demo network`)
    },
  }
}
