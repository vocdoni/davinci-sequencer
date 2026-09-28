import { describe, expect, it, vi } from 'vitest'
import { encodeFunctionData, keccak256, toHex, type PublicClient } from 'viem'
import { processRegistryAbi } from '~contracts/abis'
import { demoFixture } from '~fixtures/demo'
import { Indexer, type IndexerConfig } from './indexer'
import { memoryStore, loadStore } from './persist'
import { networkStats, rootChain } from './selectors'
import type { Hex, IndexedEvent, ProcessState } from './types'

const fixture = demoFixture({ scale: 0.25 })
const source = fixture.store
const HEAD = source.chain.headBlock
const CODE = '0x6080604052' as Hex

const STATUS = ['ready', 'ended', 'canceled', 'paused', 'results']
const ORIGIN = ['unknown', 'merkle-static', 'merkle-dynamic', 'onchain-dynamic', 'csp']
const MODE = ['sequencer', 'dkg-automatic', 'dkg-locked']

function rawProcess(s: ProcessState) {
  return {
    status: STATUS.indexOf(s.status),
    organizationId: s.organizer,
    encryptionKey: s.encryptionKey,
    latestStateRoot: s.latestStateRoot,
    result: s.result,
    startTime: BigInt(s.startTime),
    duration: BigInt(s.duration),
    maxVoters: BigInt(s.maxVoters),
    votersCount: BigInt(s.votersCount),
    overwrittenVotesCount: BigInt(s.overwrittenVotesCount),
    creationBlock: BigInt(s.creationBlock),
    batchNumber: BigInt(s.batchNumber),
    metadataURI: s.metadataURI,
    ballotMode: s.ballotMode,
    census: {
      censusOrigin: ORIGIN.indexOf(s.census.origin),
      censusRoot: s.census.root,
      contractAddress: s.census.contractAddress,
      censusURI: s.census.uri,
      onchainAllowAnyValidRoot: false,
    },
    keyMode: MODE.indexOf(s.keyMode),
    dkgEpochId: s.dkg?.epochId ?? '0x000000000000000000000000',
    dkgFirstIndex: s.dkg?.firstIndex ?? 0,
    dkgCount: s.dkg?.count ?? 0,
    dkgZeroSkipped: s.dkg?.zeroSkipped ?? 0,
    dkgResultsRequested: s.dkg?.resultsRequested ?? false,
    dkgAid: s.dkg?.aid ?? `0x${'00'.repeat(32)}`,
  }
}

function rawArgs(ev: IndexedEvent): Record<string, unknown> {
  const base = { processId: ev.processId }
  switch (ev.name) {
    case 'ProcessCreated':
      return { ...base, creator: ev.data.creator }
    case 'ProcessStatusChanged':
      return { ...base, oldStatus: STATUS.indexOf(ev.data.oldStatus), newStatus: STATUS.indexOf(ev.data.newStatus) }
    case 'ProcessStateTransitioned':
      return {
        ...base,
        ...ev.data,
        newVotersCount: BigInt(ev.data.newVotersCount),
        newOverwrittenVotesCount: BigInt(ev.data.newOverwrittenVotesCount),
        nBlobs: BigInt(ev.data.nBlobs),
      }
    case 'ProcessResultsSet':
      return { ...base, ...ev.data }
    case 'ProcessDurationChanged':
      return { ...base, duration: BigInt(ev.data.duration) }
    case 'ProcessMaxVotersChanged':
      return { ...base, maxVoters: BigInt(ev.data.maxVoters) }
    default:
      return { ...base, ...ev.data }
  }
}

interface FakeOptions {
  chainId?: number
  head?: number
  maxSpan?: number
}

/** A PublicClient backed by the demo fixture, counting calls per method. */
function fakeClient(opts: FakeOptions = {}) {
  const calls: Record<string, number> = {}
  const count = (m: string) => (calls[m] = (calls[m] ?? 0) + 1)
  let head = opts.head ?? HEAD
  const registry = source.chain.registry!
  const tsOf = (b: number) => source.chain.headTimestamp! - (HEAD - b) * source.chain.blockTimeSeconds

  const client = {
    async getChainId() {
      count('getChainId')
      return opts.chainId ?? source.chain.chainId
    },
    async getBlock(args: { blockTag?: string; blockNumber?: bigint }) {
      count('getBlock')
      const n = args.blockNumber != null ? Number(args.blockNumber) : head
      return { number: BigInt(n), timestamp: BigInt(tsOf(n)) }
    },
    async getLogs({ fromBlock, toBlock }: { fromBlock: bigint; toBlock: bigint }) {
      count('getLogs')
      if (opts.maxSpan && Number(toBlock - fromBlock) + 1 > opts.maxSpan) throw new Error('block range too large')
      return source.events
        .filter((e) => e.block >= Number(fromBlock) && e.block <= Number(toBlock))
        .map((e, i) => ({
          eventName: e.name,
          args: rawArgs(e),
          blockNumber: BigInt(e.block),
          transactionHash: e.tx,
          logIndex: e.logIndex,
          // Half the logs carry their block time, like some RPCs do.
          ...(i % 2 === 0 ? { blockTimestamp: BigInt(e.timestamp!) } : {}),
        }))
    },
    async getCode() {
      count('getCode')
      return CODE
    },
    async multicall({ contracts }: { contracts: Array<{ functionName: string; args?: unknown[] }> }) {
      count('multicall')
      return contracts.map((c) => {
        const ok = (result: unknown) => ({ status: 'success', result })
        switch (c.functionName) {
          case 'chainID':
            return ok(registry.chainID)
          case 'pidPrefix':
            return ok(registry.pidPrefix)
          case 'processCount':
            return ok(source.processOrder.length)
          case 'batchProgramVK':
          case 'resultsProgramVK':
          case 'rootCVadcopFinal':
          case 'ballotVKHash':
          case 'ziskVerifier':
          case 'dkgAdapter':
            return ok(registry[c.functionName])
          case 'manager':
            return ok(registry.dkgManager)
          case 'appManager':
            return ok(registry.dkgAppManager)
          case 'getProcess':
            return ok(rawProcess(source.processes[String(c.args![0])]!.state!))
          case 'genesisRoot':
            return ok(source.processes[String(c.args![0])]!.genesisRoot)
          default:
            return { status: 'failure', error: new Error(c.functionName) }
        }
      })
    },
    async getTransaction({ hash }: { hash: Hex }) {
      count('getTransaction')
      const d = source.txDetails[hash]!
      const input =
        d.functionName === 'submitStateTransition'
          ? encodeFunctionData({
              abi: processRegistryAbi,
              functionName: 'submitStateTransition',
              args: [
                source.transitions[source.transitionOrder.find((k) => source.transitions[k]!.tx === hash)!]!.processId,
                d.publicValues!,
                d.proofBytes!,
                d.commitments,
                d.ys,
                d.kzgProofs,
              ],
            })
          : '0x'
      return {
        hash,
        from: d.from,
        to: d.to,
        input,
        blockNumber: BigInt(d.blockNumber),
        blobVersionedHashes: d.blobVersionedHashes,
      }
    },
    async getTransactionReceipt({ hash }: { hash: Hex }) {
      count('getTransactionReceipt')
      const d = source.txDetails[hash]!
      return {
        status: 'success',
        gasUsed: d.gasUsed,
        effectiveGasPrice: d.effectiveGasPrice,
        blobGasUsed: d.blobGasUsed,
        blobGasPrice: d.blobGasPrice,
        blockNumber: BigInt(d.blockNumber),
      }
    },
  }
  return {
    client: client as unknown as PublicClient,
    calls,
    setHead: (n: number) => {
      head = n
    },
  }
}

function indexer(client: PublicClient, kv = memoryStore()) {
  return new Indexer({
    client,
    chainId: source.chain.chainId,
    registryAddress: source.chain.registryAddress,
    startBlock: source.chain.startBlock,
    confirmations: 0,
    txPerTick: 1_000,
    blocksPerTick: 1_000,
    kv,
  })
}

describe('Indexer against a fake chain', () => {
  it('rebuilds the fixture store from logs and contract reads', async () => {
    const fake = fakeClient({ maxSpan: 200_000 })
    const ix = indexer(fake.client)
    await ix.refresh()
    const { store, status } = ix.getSnapshot()
    expect(status.phase).toBe('live')
    expect(status.errors).toEqual([])
    expect(store.processOrder).toEqual(source.processOrder)
    expect(store.transitionOrder).toEqual(source.transitionOrder)
    for (const pid of store.processOrder) {
      expect(store.processes[pid]!.state).toEqual(source.processes[pid]!.state)
      expect(rootChain(store, pid).gaps).toBe(0)
    }
    const stats = networkStats(store)
    const expected = networkStats(source)
    expect([stats.processes, stats.ballots, stats.transitions, stats.blobs]).toEqual([
      expected.processes,
      expected.ballots,
      expected.transitions,
      expected.blobs,
    ])
    // Settlement transactions were read and their calldata decoded.
    const t = store.transitions[store.transitionOrder[0]!]!
    expect(store.txDetails[t.tx!]!.publicValues).toBe(source.txDetails[t.tx!]!.publicValues)
    // Registry immutables and the verifier code hash.
    expect(store.chain.registry?.batchProgramVK).toBe(source.chain.registry!.batchProgramVK)
    expect(store.chain.registry?.ziskVerifierCodeHash).toBe(keccak256(CODE))
    // Every event has a time, from the log or a block read.
    expect(store.events.every((e) => e.timestamp != null)).toBe(true)
  })

  it('costs one request per poll when the chain is idle', async () => {
    const fake = fakeClient()
    const ix = indexer(fake.client)
    await ix.refresh()
    const before = { ...fake.calls }
    await ix.refresh()
    const delta = Object.fromEntries(Object.entries(fake.calls).map(([k, v]) => [k, v - (before[k] ?? 0)]))
    expect(Object.values(delta).reduce((a, b) => a + b, 0)).toBe(1)
    expect(delta.getBlock).toBe(1)
  })

  it('stops with a clear error when the RPC is on another chain', async () => {
    const fake = fakeClient({ chainId: 1 })
    const ix = indexer(fake.client)
    await ix.refresh()
    const { status } = ix.getSnapshot()
    expect(status.chainMismatch).toEqual({ expected: 100, actual: 1 })
    expect(status.phase).toBe('error')
    expect(fake.calls.getLogs).toBeUndefined()
  })

  it('resumes from the IndexedDB cache', async () => {
    const kv = memoryStore()
    const early = HEAD - 5_000
    const first = fakeClient({ head: early })
    const a = indexer(first.client, kv)
    await a.refresh()
    const cached = await loadStore(kv, source.chain.chainId, source.chain.registryAddress)
    expect(cached?.lastIndexedBlock).toBe(early)

    const second = fakeClient()
    const b = indexer(second.client, kv)
    await b.refresh()
    const { store, status } = b.getSnapshot()
    expect(status.fromBlock).toBe(early + 1)
    expect(store.transitionOrder).toEqual(source.transitionOrder)
    expect(second.calls.getLogs).toBe(1)
  })
})

// ── a small chain whose history can change ──────────────────────────────────

const START = 1_000
const MINI_PID = source.processOrder.find((k) => source.processes[k]!.state!.keyMode === 'sequencer')! as Hex
const MINI_STATE = source.processes[MINI_PID]!.state!
const miniRoot = (n: number) => `0x${n.toString(16).padStart(64, '0')}` as Hex
const miniTx = (n: number) => `0x${n.toString(16).padStart(64, 'b')}` as Hex
const ZERO = '0x0000000000000000000000000000000000000000' as Hex

/**
 * One process on a chain of a few hundred blocks. Block hashes change from
 * `forkAt` on when `fork` is bumped, `getProcess` answers the state of the
 * block it is asked at, and some reads can be made to fail.
 */
function miniChain() {
  const registry = source.chain.registry!
  const chain = {
    head: START + 100,
    fork: 0,
    forkAt: Number.POSITIVE_INFINITY,
    events: [] as IndexedEvent[],
    /** `getProcess` answers the newest entry at or before the block it is read at. */
    states: [] as Array<{ block: number; state: ProcessState }>,
    failingTx: new Set<string>(),
    codeDown: false,
    managerDown: false,
    dkgAdapter: registry.dkgAdapter as Hex,
    calls: {} as Record<string, number>,
  }
  const count = (m: string) => (chain.calls[m] = (chain.calls[m] ?? 0) + 1)
  const hashOf = (n: number) => keccak256(toHex(`${n >= chain.forkAt ? chain.fork : 0}:${n}`))
  const tsOf = (n: number) => 1_700_000_000 + n * 5
  const txBlock = (hash: string) => chain.events.find((e) => e.tx === hash)?.block ?? 0

  const client = {
    async getChainId() {
      return source.chain.chainId
    },
    async getBlock(args: { blockTag?: string; blockNumber?: bigint }) {
      count('getBlock')
      const n = args.blockNumber != null ? Number(args.blockNumber) : chain.head
      return { number: BigInt(n), timestamp: BigInt(tsOf(n)), hash: hashOf(n) }
    },
    async getLogs({ fromBlock, toBlock }: { fromBlock: bigint; toBlock: bigint }) {
      count('getLogs')
      return chain.events
        .filter((e) => e.block >= Number(fromBlock) && e.block <= Number(toBlock))
        .map((e) => ({
          eventName: e.name,
          args: rawArgs(e),
          blockNumber: BigInt(e.block),
          transactionHash: e.tx,
          logIndex: e.logIndex,
          blockTimestamp: BigInt(tsOf(e.block)),
        }))
    },
    async getCode({ address }: { address: Hex }) {
      count('getCode')
      if (address.toLowerCase() === registry.ziskVerifier && chain.codeDown) throw new Error('rpc hiccup')
      return CODE
    },
    async multicall({
      contracts,
      blockNumber,
    }: {
      contracts: Array<{ functionName: string; args?: unknown[] }>
      blockNumber?: bigint
    }) {
      count('multicall')
      const at = blockNumber != null ? Number(blockNumber) : chain.head
      return contracts.map((c) => {
        const ok = (result: unknown) => ({ status: 'success', result })
        const fail = { status: 'failure', error: new Error(c.functionName) }
        switch (c.functionName) {
          case 'chainID':
          case 'pidPrefix':
          case 'batchProgramVK':
          case 'resultsProgramVK':
          case 'rootCVadcopFinal':
          case 'ballotVKHash':
          case 'ziskVerifier':
            return ok(registry[c.functionName])
          case 'processCount':
            return ok(1)
          case 'dkgAdapter':
            return ok(chain.dkgAdapter)
          case 'manager':
            return chain.managerDown ? fail : ok(registry.dkgManager)
          case 'appManager':
            return chain.managerDown ? fail : ok(registry.dkgAppManager)
          case 'getProcess': {
            const state = [...chain.states].reverse().find((s) => s.block <= at)?.state
            return state ? ok(rawProcess(state)) : fail
          }
          case 'genesisRoot':
            return ok(miniRoot(1))
          default:
            return fail
        }
      })
    },
    async getTransaction({ hash }: { hash: Hex }) {
      count('getTransaction')
      if (chain.failingTx.has(hash)) throw new Error('transaction not found')
      return { hash, from: ZERO, to: registryAddress, input: '0x', blockNumber: BigInt(txBlock(hash)) }
    },
    async getTransactionReceipt({ hash }: { hash: Hex }) {
      count('getTransactionReceipt')
      if (chain.failingTx.has(hash)) throw new Error('receipt not found')
      return { status: 'success', gasUsed: 1n, effectiveGasPrice: 1n, blockNumber: BigInt(txBlock(hash)) }
    },
  }

  const created = (block: number): IndexedEvent => ({
    name: 'ProcessCreated',
    block,
    tx: miniTx(block),
    logIndex: 0,
    timestamp: null,
    processId: MINI_PID,
    data: { creator: MINI_STATE.organizer },
  })
  /** Transition `i` (roots i+1 → i+2) at `block`, settled by `tx`. */
  const transitioned = (block: number, i: number, tx: Hex = miniTx(block)): IndexedEvent => ({
    name: 'ProcessStateTransitioned',
    block,
    tx,
    logIndex: 0,
    timestamp: null,
    processId: MINI_PID,
    data: {
      sender: ZERO,
      oldStateRoot: miniRoot(i + 1),
      newStateRoot: miniRoot(i + 2),
      newVotersCount: i + 1,
      newOverwrittenVotesCount: 0,
      nBlobs: 1,
    },
  })
  /** The state after `transitions` transitions, from `block` on. */
  const settle = (block: number, transitions: number) =>
    chain.states.push({
      block,
      state: {
        ...MINI_STATE,
        latestStateRoot: miniRoot(transitions + 1),
        votersCount: transitions,
        batchNumber: transitions,
      },
    })

  return { chain, client: client as unknown as PublicClient, created, transitioned, settle }
}

const registryAddress = source.chain.registryAddress as Hex

function miniIndexer(client: PublicClient, options: Partial<IndexerConfig> = {}) {
  return new Indexer({
    client,
    chainId: source.chain.chainId,
    registryAddress,
    startBlock: START,
    kv: null,
    ...options,
  })
}

describe('Indexer against a changing chain', () => {
  it('reads getProcess at the last indexed block, so a settlement inside the lag is no broken chain', async () => {
    const { chain, client, created, transitioned, settle } = miniChain()
    chain.events.push(created(START + 10), transitioned(START + 20, 0), transitioned(chain.head - 1, 1))
    settle(START + 10, 0)
    settle(START + 20, 1)
    settle(chain.head - 1, 2)
    const ix = miniIndexer(client, { confirmations: 2 })
    await ix.refresh()
    const { store } = ix.getSnapshot()
    const p = store.processes[MINI_PID]!
    expect(store.lastIndexedBlock).toBe(chain.head - 2)
    expect(p.transitions).toHaveLength(1)
    expect(p.stateBlock).toBe(chain.head - 2)
    expect(p.state!.latestStateRoot).toBe(miniRoot(2))
    expect(rootChain(store, MINI_PID)).toMatchObject({ gaps: 0, headMatches: true })
  })

  it('reads a registry field again while its read fails, and a zero DKG adapter never', async () => {
    const { chain, client } = miniChain()
    chain.codeDown = true
    chain.managerDown = true
    const ix = miniIndexer(client)
    await ix.refresh()
    const first = ix.getSnapshot().store.chain.registry!
    expect([first.ziskVerifierCodeHash, first.dkgManager, first.dkgAppManager]).toEqual([null, null, null])
    await ix.refresh()
    expect(ix.getSnapshot().store.chain.registry!.ziskVerifierCodeHash).toBeNull()

    chain.codeDown = false
    chain.managerDown = false
    await ix.refresh()
    const registry = ix.getSnapshot().store.chain.registry!
    expect(registry.ziskVerifierCodeHash).toBe(keccak256(CODE))
    expect([registry.dkgManager, registry.dkgAppManager]).toEqual([
      source.chain.registry!.dkgManager,
      source.chain.registry!.dkgAppManager,
    ])

    const plain = miniChain()
    plain.chain.dkgAdapter = ZERO
    const other = miniIndexer(plain.client)
    await other.refresh()
    expect(other.getSnapshot().store.chain.registry!.dkgAdapter).toBeNull()
    const calls = { ...plain.chain.calls }
    await other.refresh()
    expect(plain.chain.calls).toEqual({ ...calls, getBlock: calls.getBlock! + 1 })
  })

  it('reindexes from the start block when a block below the lag changes hash', async () => {
    const warn = vi.spyOn(console, 'warn').mockImplementation(() => {})
    const kv = memoryStore()
    const { chain, client, created, transitioned, settle } = miniChain()
    const late = miniTx(0xbeef)
    chain.events.push(created(START + 10), transitioned(START + 50, 0), transitioned(START + 60, 1, late))
    settle(START + 10, 0)
    settle(START + 50, 1)
    settle(START + 60, 2)
    const ix = miniIndexer(client, { confirmations: 2, kv })
    await ix.refresh()
    expect(ix.getSnapshot().store.transitionOrder).toHaveLength(2)

    // Blocks from START + 55 are replaced: the second settlement lands again,
    // later, and the old one is gone.
    chain.fork = 1
    chain.forkAt = START + 55
    chain.events = chain.events.filter((e) => e.block < START + 55)
    chain.events.push(transitioned(START + 101, 1, late))
    chain.states = chain.states.filter((s) => s.block < START + 55)
    settle(START + 101, 2)
    chain.head = START + 105
    await ix.refresh()

    const { store, status } = ix.getSnapshot()
    expect(store.transitionOrder.map((k) => [store.transitions[k]!.block, store.transitions[k]!.tx])).toEqual([
      [START + 50, miniTx(START + 50)],
      [START + 101, late],
    ])
    expect(rootChain(store, MINI_PID)).toMatchObject({ gaps: 0, headMatches: true })
    expect(status.errors.some((e) => /reorg/.test(e.message))).toBe(true)
    expect(warn).toHaveBeenCalled()
    const cached = await loadStore(kv, source.chain.chainId, registryAddress)
    expect(cached?.transitionOrder).toHaveLength(2)
    expect(cached?.lastIndexedHash).toBe(keccak256(toHex(`1:${START + 103}`)))
    warn.mockRestore()
  })

  it('moves past transactions it cannot read and retries them with backoff', async () => {
    let now = 1_000_000
    const clock = vi.spyOn(Date, 'now').mockImplementation(() => now)
    const { chain, client, created, transitioned, settle } = miniChain()
    chain.events.push(created(START + 10), ...[0, 1, 2, 3].map((i) => transitioned(START + 20 + i, i)))
    settle(START + 10, 4)
    const [creation, t0, t1, t2, t3] = chain.events.map((e) => e.tx!)
    chain.failingTx = new Set([creation!, t0!])
    const ix = miniIndexer(client, { txPerTick: 2 })
    for (let i = 0; i < 5; i++) await ix.refresh()
    const { store, status } = ix.getSnapshot()
    expect([t1, t2, t3].every((h) => store.txDetails[h!])).toBe(true)
    expect(status.skippedTx).toEqual([creation, t0])

    // Not retried before the backoff, retried after it.
    chain.failingTx.delete(t0!)
    await ix.refresh()
    expect(ix.getSnapshot().store.txDetails[t0!]).toBeUndefined()
    now += 31_000
    await ix.refresh()
    expect(ix.getSnapshot().store.txDetails[t0!]).toBeDefined()
    expect(ix.getSnapshot().status.skippedTx).toEqual([creation])
    clock.mockRestore()
  })
})
