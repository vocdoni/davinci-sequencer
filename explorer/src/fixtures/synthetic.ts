// Deterministic synthetic network for demo mode and the tests.
//
// Same store shape as the live indexer: it is built by pushing a generated
// ProcessRegistry event stream through the same reducers, then applying
// `getProcess` states, genesis roots, settlement transactions and the
// registry immutables exactly as the indexer would. It covers:
//
//   every status (ready: upcoming / open / past its end, paused, ended,
//   canceled, results), every key mode (sequencer, DKG automatic, DKG
//   locked), every census origin (fixed and updatable Merkle trees, on-chain
//   census contract, CSP), transitions with one and several blobs, results
//   from the zkVM and from the DKG committee, and a DKG-locked process whose
//   tally waits for the organizer's reveal.
//
// State roots after each transition are the roots of a sparse Merkle tree
// over the process's vote ids (see `smt.ts`), so the demo sequencers' tracker
// proofs verify. The blob bytes are generated on demand from the recorded
// transition data with the real cell encoder.

import type { Address, Hex } from 'viem'
import { KNOWN_RELEASES } from '~protocol/releases'
import { B8, addPoints, type Point } from '~protocol/babyjubjub'
import { blobCount, blobsDigest, blobsFromCells, transitionCells, versionedHash, type Ciphertext } from '~protocol/blob'
import { bigIntToBe, reverseBytes, toBytes, toHex } from '~protocol/bytes'
import { BALLOT_MIN, VOTE_ID_MIN, requiredRefresh } from '~protocol/limits'
import { computeProcessId, processIdPrefix } from '~protocol/process-id'
import { PUBLIC_VALUES_LENGTH } from '~protocol/publics'
import type { CensusOriginName, KeyModeName, ProcessStatusName } from '~protocol/types'
import {
  applyEvents,
  applyGenesisRoot,
  applyHead,
  applyProcessState,
  applyRegistryInfo,
  applyTxDetails,
  createEmptyStore,
} from '~indexer/reduce'
import type { IndexedEvent, IndexerStore, ProcessState, RegistryInfo, TxDetails } from '~indexer/types'
import { voteIdTreeRoot } from './smt'

// ── deterministic randomness ─────────────────────────────────────────────────

export class Rng {
  private s: number
  constructor(seed: number) {
    this.s = seed >>> 0 || 1
  }
  /** mulberry32 */
  next(): number {
    this.s = (this.s + 0x6d2b79f5) >>> 0
    let t = this.s
    t = Math.imul(t ^ (t >>> 15), t | 1)
    t ^= t + Math.imul(t ^ (t >>> 7), t | 61)
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296
  }
  int(min: number, max: number): number {
    return min + Math.floor(this.next() * (max - min + 1))
  }
  bytes(n: number): Uint8Array {
    const out = new Uint8Array(n)
    for (let i = 0; i < n; i++) out[i] = Math.floor(this.next() * 256)
    return out
  }
  hex(n: number): Hex {
    return toHex(this.bytes(n))
  }
  u64(): bigint {
    return (BigInt(Math.floor(this.next() * 2 ** 32)) << 32n) | BigInt(Math.floor(this.next() * 2 ** 32))
  }
  pick<T>(items: readonly T[]): T {
    return items[Math.floor(this.next() * items.length)]!
  }
  sample<T>(items: readonly T[], n: number): T[] {
    const copy = [...items]
    const out: T[] = []
    while (out.length < n && copy.length > 0) out.push(copy.splice(Math.floor(this.next() * copy.length), 1)[0]!)
    return out
  }
}

// ── a pool of curve points, so ballots and keys are real points ─────────────

let pointPool: Point[] | null = null

/** 64 multiples of the generator, computed once. */
export function demoPoints(): Point[] {
  if (!pointPool) {
    const pts: Point[] = [B8]
    for (let i = 1; i < 64; i++) pts.push(addPoints(pts[i - 1]!, B8))
    pointPool = pts
  }
  return pointPool
}

function ciphertexts(rng: Rng, nf: number): Ciphertext[] {
  const pool = demoPoints()
  return Array.from({ length: nf }, () => ({ c1: rng.pick(pool), c2: rng.pick(pool) }))
}

// ── fixture ──────────────────────────────────────────────────────────────────

export interface FixtureOptions {
  seed?: number
  chainId?: number
  networkName?: string
  registryAddress?: Address
  startBlock?: number
  headBlock?: number
  /** Unix seconds of the head block. */
  headTimestamp?: number
  blockTimeSeconds?: number
  /** Multiplies the number of transitions of the busy processes (tests use less). */
  scale?: number
}

export const DEFAULT_FIXTURE = {
  seed: 0xda7c1,
  chainId: 100,
  networkName: 'Demo network',
  registryAddress: '0xde30000000000000000000000000000000000001' as Address,
  headBlock: 48_600_000,
  headTimestamp: 1_790_600_000,
  blockTimeSeconds: 5,
  scale: 1,
}

export interface DemoTransitionData {
  processId: Hex
  index: number
  numFields: number
  voteIds: bigint[]
  /** Every slot written: new voters, overwrites and silent refreshes. */
  updateKeys: bigint[]
  /** Seed of the ciphertexts in the blob cells. */
  ballotSeed: number
}

export interface DemoDkgApplication {
  epochId: Hex
  aid: Hex
  poolIndex: number
  poolKey: Point
  organizerPK: Point
  /** 0 while sealed; always 0 for an automatic application. */
  organizerSecret: bigint
  revealBlock: number | null
  applicationKey: Point
  ciphertexts: Array<{ index: number; completed: boolean; plaintext: bigint }>
}

export interface DemoSequencerVote {
  processId: Hex
  voteId: bigint
  status: 'pending' | 'aggregated' | 'processed' | 'settled' | 'error'
  error?: string
}

export interface DemoSequencer {
  url: string
  address: Address | null
  observer: boolean
  settledBySelf: number
  syncedFromOthers: number
  lostRaces: number
  /** Votes this node holds that are not settled yet. */
  votes: DemoSequencerVote[]
}

export interface Fixture {
  store: IndexerStore
  transitionData: Map<string, DemoTransitionData>
  dkg: Map<Hex, DemoDkgApplication>
  metadata: Map<string, unknown>
  sequencers: DemoSequencer[]
  /** Handy entities for docs, tests and the Playwright suite. */
  featured: {
    openProcess: Hex
    resultsProcess: Hex
    awaitingReveal: Hex
    multiBlob: { processId: Hex; index: number }
    settledVote: { processId: Hex; voteId: bigint }
    pendingVote: { processId: Hex; voteId: bigint }
  }
}

type End = 'upcoming' | 'open' | 'closed' | 'paused' | 'ended' | 'canceled' | 'results' | 'awaiting-reveal'

interface Spec {
  title: string
  keyMode: KeyModeName
  census: CensusOriginName
  numFields: number
  /** Days before the head the process was created. */
  createdDaysAgo: number
  /** Voting window, days. */
  durationDays: number
  /** Start delay after creation, days (upcoming processes start in the future). */
  startDelayDays?: number
  transitions: number
  votes: [number, number]
  overwriteShare: number
  end: End
  censusUpdates?: number
  /** One transition big enough to need several blobs. */
  bigTransition?: boolean
  organizer: number
}

const SPECS: Spec[] = [
  {
    title: 'Board election 2026',
    keyMode: 'sequencer',
    census: 'merkle-static',
    numFields: 4,
    createdDaysAgo: 28,
    durationDays: 7,
    transitions: 9,
    votes: [8, 30],
    overwriteShare: 0.1,
    end: 'results',
    organizer: 0,
  },
  {
    title: 'Budget allocation',
    keyMode: 'dkg-automatic',
    census: 'csp',
    numFields: 6,
    createdDaysAgo: 24,
    durationDays: 6,
    transitions: 7,
    votes: [5, 25],
    overwriteShare: 0.15,
    end: 'results',
    organizer: 1,
  },
  {
    title: 'Statute reform',
    keyMode: 'dkg-locked',
    census: 'merkle-dynamic',
    numFields: 3,
    createdDaysAgo: 20,
    durationDays: 8,
    transitions: 8,
    votes: [6, 24],
    overwriteShare: 0.2,
    end: 'awaiting-reveal',
    censusUpdates: 1,
    organizer: 2,
  },
  {
    title: 'Community fund round',
    keyMode: 'sequencer',
    census: 'onchain-dynamic',
    numFields: 16,
    createdDaysAgo: 18,
    durationDays: 30,
    transitions: 40,
    votes: [10, 40],
    overwriteShare: 0.12,
    end: 'open',
    bigTransition: true,
    organizer: 0,
  },
  {
    title: 'Park renovation poll',
    keyMode: 'dkg-automatic',
    census: 'merkle-static',
    numFields: 2,
    createdDaysAgo: 5,
    durationDays: 14,
    transitions: 6,
    votes: [3, 15],
    overwriteShare: 0.1,
    end: 'open',
    organizer: 1,
  },
  {
    title: 'Union ballot',
    keyMode: 'sequencer',
    census: 'csp',
    numFields: 5,
    createdDaysAgo: 12,
    durationDays: 20,
    transitions: 5,
    votes: [4, 16],
    overwriteShare: 0.1,
    end: 'paused',
    organizer: 2,
  },
  {
    title: 'Next season schedule',
    keyMode: 'dkg-locked',
    census: 'merkle-static',
    numFields: 4,
    createdDaysAgo: 1,
    durationDays: 10,
    startDelayDays: 3,
    transitions: 0,
    votes: [0, 0],
    overwriteShare: 0,
    end: 'upcoming',
    organizer: 0,
  },
  {
    title: 'Logo contest',
    keyMode: 'sequencer',
    census: 'merkle-dynamic',
    numFields: 8,
    createdDaysAgo: 15,
    durationDays: 10,
    transitions: 2,
    votes: [3, 9],
    overwriteShare: 0,
    end: 'canceled',
    censusUpdates: 1,
    organizer: 1,
  },
  {
    title: 'Annual assembly minutes',
    keyMode: 'sequencer',
    census: 'merkle-static',
    numFields: 1,
    createdDaysAgo: 10,
    durationDays: 9,
    transitions: 4,
    votes: [5, 12],
    overwriteShare: 0.1,
    end: 'ended',
    organizer: 2,
  },
  {
    title: 'Tooling survey',
    keyMode: 'dkg-automatic',
    census: 'onchain-dynamic',
    numFields: 3,
    createdDaysAgo: 9,
    durationDays: 4,
    transitions: 3,
    votes: [4, 10],
    overwriteShare: 0.1,
    end: 'closed',
    organizer: 0,
  },
]

const ORGANIZERS: Address[] = [
  '0x42fc20654efd78c6887ff0bd1cc50c9ec1dab589',
  '0x7e5f4552091a69125d5dfcb7b8c2659029395bdf',
  '0x2b5ad5c4795c026514f8317c7a215e218dccd6cf',
]

const SEQUENCER_ADDRESSES: Address[] = [
  '0xfa971da5a813f3ecb55142692c7415b37733845d',
  '0x6813eb9362372eef6200f3b1dbc3f819671cba69',
]

const ZERO_ADDRESS = '0x0000000000000000000000000000000000000000' as Address
const DAY = 86_400

function registersToPublicValues(regs: number[]): Hex {
  const out = new Uint8Array(PUBLIC_VALUES_LENGTH)
  const view = new DataView(out.buffer)
  regs.forEach((r, i) => view.setUint32(i * 8, r >>> 0, true))
  return toHex(out)
}

function setBytes32(regs: number[], base: number, bytes: Uint8Array): void {
  const view = new DataView(bytes.buffer, bytes.byteOffset, 32)
  for (let i = 0; i < 8; i++) regs[base + i] = view.getUint32(i * 4, true)
}

/** Builds the synthetic network. Deterministic for a given `seed`. */
export function buildFixture(options: FixtureOptions = {}): Fixture {
  const o = { ...DEFAULT_FIXTURE, ...options }
  const rng = new Rng(o.seed)
  const blocksPerDay = Math.floor(DAY / o.blockTimeSeconds)
  const startBlock = o.startBlock ?? o.headBlock - 32 * blocksPerDay
  const registry = o.registryAddress.toLowerCase() as Address
  const prefix = processIdPrefix(o.chainId, registry)
  const tsOf = (block: number) => o.headTimestamp - (o.headBlock - block) * o.blockTimeSeconds
  const blockOf = (ts: number) => o.headBlock - Math.ceil((o.headTimestamp - ts) / o.blockTimeSeconds)
  const release = KNOWN_RELEASES[0]!
  const pool = demoPoints()

  const events: IndexedEvent[] = []
  const txs: TxDetails[] = []
  const states = new Map<Hex, ProcessState>()
  const genesis = new Map<Hex, Hex>()
  const transitionData = new Map<string, DemoTransitionData>()
  const dkg = new Map<Hex, DemoDkgApplication>()
  const metadata = new Map<string, unknown>()
  const nonces = new Map<Address, bigint>()
  const pendingVotes: DemoSequencerVote[] = []
  let logIndex = 0
  let settledVote: Fixture['featured']['settledVote'] | null = null
  let multiBlob: Fixture['featured']['multiBlob'] | null = null
  let epochNonce = 40n

  const emit = <E extends IndexedEvent>(ev: Omit<E, 'logIndex' | 'timestamp'>) => {
    events.push({ ...ev, logIndex: logIndex++, timestamp: tsOf(ev.block) } as E)
  }

  const addTx = (hash: Hex, from: Address, block: number, extra: Partial<TxDetails> = {}) => {
    const gasUsed = BigInt(extra.gasUsed ?? 90_000 + rng.int(0, 40_000))
    const price = BigInt(rng.int(1_000_000_000, 3_000_000_000))
    const blobGasUsed = extra.blobGasUsed ?? null
    const blobGasPrice = blobGasUsed != null ? 1_000_000_000n : null
    txs.push({
      hash,
      from,
      to: registry,
      blockNumber: block,
      status: 'success',
      gasUsed,
      effectiveGasPrice: price,
      blobGasUsed,
      blobGasPrice,
      fee: gasUsed * price + (blobGasUsed ?? 0n) * (blobGasPrice ?? 0n),
      blobVersionedHashes: [],
      inputSize: 4 + 32 * 4,
      functionName: null,
      publicValues: null,
      proofBytes: null,
      commitments: [],
      ys: [],
      kzgProofs: [],
      initialCensusRoot: null,
      decodeError: null,
      ...extra,
    })
  }

  SPECS.forEach((spec, specIndex) => {
    const organizer = ORGANIZERS[spec.organizer]!
    const nonce = nonces.get(organizer) ?? 0n
    nonces.set(organizer, nonce + 1n)
    const pid = computeProcessId(prefix, organizer, nonce)
    const createdBlock = o.headBlock - Math.floor(spec.createdDaysAgo * blocksPerDay) - rng.int(0, 500)
    const createdTs = tsOf(createdBlock)
    const startTime = createdTs + Math.floor((spec.startDelayDays ?? 0.02) * DAY)
    const duration = spec.durationDays * DAY
    const endTime = startTime + duration
    const now = o.headTimestamp
    const nf = spec.numFields
    const censusRoots: Hex[] = [
      spec.census === 'csp' ? toHex(bigIntToBe(BigInt(rng.hex(20)), 32)) : toHex(bigIntToBe(BigInt(rng.hex(31)), 32)),
    ]
    const createdTx = rng.hex(32)
    emit({ name: 'ProcessCreated', block: createdBlock, tx: createdTx, processId: pid, data: { creator: organizer } })
    addTx(createdTx, organizer, createdBlock, {
      functionName: 'newProcess',
      gasUsed: 480_000n,
      initialCensusRoot: censusRoots[0]!,
    })
    const encryptionKey = rng.pick(pool)
    const genesisRoot = rng.hex(32)
    genesis.set(pid, genesisRoot)

    // DKG application behind the key.
    let dkgInfo: DemoDkgApplication | null = null
    if (spec.keyMode !== 'sequencer') {
      epochNonce += 1n
      const epochId = toHex(Uint8Array.from([0x2f, 0x11, 0x05, 0xe9, ...bigIntToBe(epochNonce, 8)]))
      const poolKey = rng.pick(pool)
      const organizerPK = spec.keyMode === 'dkg-locked' ? rng.pick(pool) : { x: 0n, y: 1n }
      dkgInfo = {
        epochId,
        aid: toHex(bigIntToBe(BigInt(rng.hex(30)), 32)),
        poolIndex: rng.int(0, 15),
        poolKey,
        organizerPK,
        organizerSecret: 0n,
        revealBlock: null,
        applicationKey: spec.keyMode === 'dkg-locked' ? addPoints(poolKey, organizerPK) : poolKey,
        ciphertexts: [],
      }
      dkg.set(pid, dkgInfo)
    }

    // Transitions spread over the part of the window that has passed.
    const windowEnd = Math.min(endTime, now - 600)
    const occupied: bigint[] = []
    const allVoteIds: bigint[] = []
    let root = genesisRoot
    let voters = 0
    let overwritten = 0
    let censusIdx = 0
    const censusUpdateAt = spec.censusUpdates ? Math.floor(spec.transitions / 2) : -1
    const nTransitions =
      spec.end === 'upcoming' ? 0 : Math.max(0, Math.round(spec.transitions * (spec.transitions > 10 ? o.scale : 1)))
    let lastBlock = createdBlock
    for (let i = 0; i < nTransitions; i++) {
      if (i === censusUpdateAt) {
        const updateBlock = lastBlock + rng.int(20, 200)
        const newRoot = toHex(bigIntToBe(BigInt(rng.hex(31)), 32))
        censusRoots.push(newRoot)
        censusIdx += 1
        const tx = rng.hex(32)
        emit({
          name: 'CensusUpdated',
          block: updateBlock,
          tx,
          processId: pid,
          data: {
            censusRoot: newRoot,
            censusURI: `https://census.example.org/${pid.slice(2, 10)}/v${censusIdx + 1}.json`,
          },
        })
        addTx(tx, organizer, updateBlock, { functionName: 'setProcessCensus' })
        lastBlock = updateBlock
      }
      const t0 = Math.max(startTime, createdTs) + 60
      const ts = t0 + Math.floor(((windowEnd - t0) * (i + 1)) / (nTransitions + 1)) + rng.int(0, 300)
      const block = Math.max(lastBlock + 3, blockOf(ts))
      lastBlock = block

      const big = spec.bigTransition && i === nTransitions - 3
      const n = big ? 200 : rng.int(spec.votes[0], spec.votes[1])
      const w = Math.min(Math.floor(n * spec.overwriteShare), occupied.length)
      const newCount = n - w
      const overwriteKeys = rng.sample(occupied, w)
      const newKeys: bigint[] = []
      while (newKeys.length < newCount) {
        const k = BALLOT_MIN + (rng.u64() % ((1n << 62n) - 1n))
        if (!occupied.includes(k) && !newKeys.includes(k)) newKeys.push(k)
      }
      const refresh = requiredRefresh(n, w, occupied.length)
      const batchKeys = new Set([...overwriteKeys, ...newKeys])
      const refreshKeys = rng.sample(
        occupied.filter((k) => !batchKeys.has(k)),
        refresh
      )
      const voteIds: bigint[] = []
      while (voteIds.length < n) {
        const v = VOTE_ID_MIN | rng.u64()
        if (!allVoteIds.includes(v) && !voteIds.includes(v)) voteIds.push(v)
      }
      const updateKeys = [...newKeys, ...overwriteKeys, ...refreshKeys]
      const nBlobs = blobCount(n, updateKeys.length, nf)
      const occupiedBefore = occupied.length
      occupied.push(...newKeys)
      allVoteIds.push(...voteIds)
      voters += newCount
      overwritten += w

      const rootBefore = root
      const rootAfter = voteIdTreeRoot(allVoteIds)
      root = rootAfter
      const tx = rng.hex(32)
      const sender = SEQUENCER_ADDRESSES[i % 3 === 2 ? 1 : 0]!
      emit({
        name: 'ProcessStateTransitioned',
        block,
        tx,
        processId: pid,
        data: {
          sender,
          oldStateRoot: rootBefore,
          newStateRoot: rootAfter,
          newVotersCount: voters,
          newOverwrittenVotesCount: overwritten,
          nBlobs,
        },
      })
      transitionData.set(`${pid}:${i}`, {
        processId: pid,
        index: i,
        numFields: nf,
        voteIds,
        updateKeys,
        ballotSeed: rng.int(1, 2 ** 30),
      })

      // The calldata: publics, a proof-sized blob of bytes, commitments and evaluations.
      const commitments = Array.from({ length: nBlobs }, () => rng.hex(48))
      const ys = Array.from({ length: nBlobs }, () => {
        const y = rng.bytes(32)
        y[0] = y[0]! & 0x3f
        return toHex(y)
      })
      const regs = new Array<number>(64).fill(0)
      regs[0] = 1
      setBytes32(regs, 2, toBytes(rootBefore))
      setBytes32(regs, 10, toBytes(rootAfter))
      regs[18] = n
      regs[19] = w
      setBytes32(regs, 20, reverseBytes(toBytes(censusRoots[censusIdx]!)))
      setBytes32(regs, 28, toBytes(blobsDigest(commitments, ys)))
      regs[36] = nBlobs
      regs[40] = 1
      regs[41] = 1
      regs[42] = occupiedBefore
      regs[43] = n
      regs[44] = 3
      regs[45] = Math.floor(Math.log2(Math.max(1, n)))
      addTx(tx, sender, block, {
        functionName: 'submitStateTransition',
        gasUsed: BigInt(498_000 + 56_000 * (nBlobs - 1) + rng.int(0, 9_000)),
        blobGasUsed: 131_072n * BigInt(nBlobs),
        blobVersionedHashes: commitments.map((c) => versionedHash(c)),
        inputSize: 4 + 32 * 9 + 512 + 768 + nBlobs * (48 + 32 + 48 + 96),
        publicValues: registersToPublicValues(regs),
        proofBytes: rng.hex(768),
        commitments,
        ys,
        kzgProofs: Array.from({ length: nBlobs }, () => rng.hex(48)),
      })
      if (nBlobs > 1 && !multiBlob) multiBlob = { processId: pid, index: i }
      if (!settledVote && spec.end === 'open') settledVote = { processId: pid, voteId: voteIds[0]! }
    }

    // Lifecycle after the transitions.
    let status: ProcessStatusName = 'ready'
    let finalDuration = duration
    const result: bigint[] = []
    const tally = () => Array.from({ length: nf }, () => BigInt(rng.int(0, Math.max(1, voters) * 3)))
    const afterEnd = Math.max(lastBlock + 10, blockOf(endTime) + rng.int(30, 400))
    switch (spec.end) {
      case 'paused': {
        status = 'paused'
        const block = lastBlock + rng.int(50, 400)
        const tx = rng.hex(32)
        emit({
          name: 'ProcessStatusChanged',
          block,
          tx,
          processId: pid,
          data: { oldStatus: 'ready', newStatus: 'paused' },
        })
        addTx(tx, organizer, block, { functionName: 'setProcessStatus' })
        break
      }
      case 'canceled': {
        status = 'canceled'
        const block = lastBlock + rng.int(50, 400)
        const tx = rng.hex(32)
        emit({
          name: 'ProcessStatusChanged',
          block,
          tx,
          processId: pid,
          data: { oldStatus: 'ready', newStatus: 'canceled' },
        })
        addTx(tx, organizer, block, { functionName: 'setProcessStatus' })
        break
      }
      case 'ended': {
        status = 'ended'
        const block = lastBlock + rng.int(50, 400)
        finalDuration = Math.max(0, tsOf(block) - startTime)
        const tx = rng.hex(32)
        emit({ name: 'ProcessDurationChanged', block, tx, processId: pid, data: { duration: finalDuration } })
        emit({
          name: 'ProcessStatusChanged',
          block,
          tx,
          processId: pid,
          data: { oldStatus: 'ready', newStatus: 'ended' },
        })
        addTx(tx, organizer, block, { functionName: 'setProcessStatus' })
        break
      }
      case 'results': {
        result.push(...tally())
        if (spec.keyMode === 'sequencer') {
          const tx = rng.hex(32)
          emit({
            name: 'ProcessStatusChanged',
            block: afterEnd,
            tx,
            processId: pid,
            data: { oldStatus: 'ready', newStatus: 'results' },
          })
          emit({
            name: 'ProcessResultsSet',
            block: afterEnd,
            tx,
            processId: pid,
            data: { sender: SEQUENCER_ADDRESSES[0]!, result },
          })
          const words = new Array<number>(64).fill(0)
          words[0] = 1
          setBytes32(words, 2, toBytes(root))
          result.forEach((v, i) => {
            words[10 + 2 * i] = Number(v & 0xffffffffn)
            words[11 + 2 * i] = Number(v >> 32n)
          })
          words[42] = 0xffffffff
          addTx(tx, SEQUENCER_ADDRESSES[0]!, afterEnd, {
            functionName: 'setProcessResults',
            gasUsed: 402_000n,
            publicValues: registersToPublicValues(words),
            proofBytes: rng.hex(768),
          })
        } else {
          const requestTx = rng.hex(32)
          const d = dkg.get(pid)!
          d.ciphertexts = result.map((v, i) => ({ index: 1 + i, completed: true, plaintext: v }))
          emit({
            name: 'ProcessStatusChanged',
            block: afterEnd,
            tx: requestTx,
            processId: pid,
            data: { oldStatus: 'ready', newStatus: 'ended' },
          })
          emit({
            name: 'ResultsDecryptionRequested',
            block: afterEnd,
            tx: requestTx,
            processId: pid,
            data: { epochId: d.epochId, aid: d.aid, firstIndex: 1, count: nf },
          })
          addTx(requestTx, SEQUENCER_ADDRESSES[1]!, afterEnd, {
            functionName: 'requestResultsDecryption',
            gasUsed: 910_000n,
          })
          const finalBlock = afterEnd + rng.int(20, 80)
          const finalTx = rng.hex(32)
          emit({
            name: 'ProcessStatusChanged',
            block: finalBlock,
            tx: finalTx,
            processId: pid,
            data: { oldStatus: 'ended', newStatus: 'results' },
          })
          emit({
            name: 'ProcessResultsSet',
            block: finalBlock,
            tx: finalTx,
            processId: pid,
            data: { sender: SEQUENCER_ADDRESSES[1]!, result },
          })
          addTx(finalTx, SEQUENCER_ADDRESSES[1]!, finalBlock, {
            functionName: 'finalizeResultsFromDKG',
            gasUsed: 160_000n,
          })
        }
        status = 'results'
        break
      }
      case 'awaiting-reveal': {
        const d = dkg.get(pid)!
        const values = tally()
        d.ciphertexts = values.map((_, i) => ({ index: 1 + i, completed: false, plaintext: 0n }))
        const tx = rng.hex(32)
        emit({
          name: 'ProcessStatusChanged',
          block: afterEnd,
          tx,
          processId: pid,
          data: { oldStatus: 'ready', newStatus: 'ended' },
        })
        emit({
          name: 'ResultsDecryptionRequested',
          block: afterEnd,
          tx,
          processId: pid,
          data: { epochId: d.epochId, aid: d.aid, firstIndex: 1, count: nf },
        })
        addTx(tx, SEQUENCER_ADDRESSES[1]!, afterEnd, { functionName: 'requestResultsDecryption', gasUsed: 880_000n })
        status = 'ended'
        break
      }
      case 'open': {
        if (specIndex === 3) {
          const block = createdBlock + 200
          const tx = rng.hex(32)
          emit({ name: 'ProcessMaxVotersChanged', block, tx, processId: pid, data: { maxVoters: 5_000 } })
          addTx(tx, organizer, block, { functionName: 'setProcessMaxVoters' })
        }
        break
      }
      default:
        break
    }

    // Unsettled votes held by the demo sequencers.
    if (spec.end === 'open') {
      for (const st of ['pending', 'aggregated', 'processed'] as const) {
        pendingVotes.push({ processId: pid, voteId: VOTE_ID_MIN | rng.u64(), status: st })
      }
      pendingVotes.push({
        processId: pid,
        voteId: VOTE_ID_MIN | rng.u64(),
        status: 'error',
        error: 'census proof: not a member',
      })
    }

    const metadataURI = `https://metadata.example.org/${pid}.json`
    metadata.set(metadataURI, {
      title: { default: spec.title },
      description: { default: `${spec.title}: a synthetic process of the demo network.` },
      questions: [
        {
          title: { default: spec.title },
          choices: Array.from({ length: nf }, (_, i) => ({ title: { default: `Option ${i + 1}` }, value: i })),
        },
      ],
    })

    states.set(pid, {
      status,
      organizer,
      encryptionKey: dkgInfo ? dkgInfo.applicationKey : encryptionKey,
      latestStateRoot: root,
      result,
      startTime,
      duration: finalDuration,
      maxVoters: specIndex === 3 ? 5_000 : 1_000,
      votersCount: voters,
      overwrittenVotesCount: overwritten,
      creationBlock: createdBlock,
      batchNumber: nTransitions,
      metadataURI,
      ballotMode: {
        uniqueValues: spec.numFields > 1 && specIndex % 2 === 0,
        numFields: nf,
        groupSize: 0,
        costExponent: specIndex === 1 ? 2 : 1,
        maxValue: BigInt(specIndex === 1 ? 10 : 1),
        minValue: 0n,
        maxValueSum: BigInt(specIndex === 1 ? 20 : nf),
        minValueSum: 0n,
      },
      census: {
        origin: spec.census,
        root: censusRoots[censusRoots.length - 1]!,
        contractAddress:
          spec.census === 'onchain-dynamic' ? '0xce0500000000000000000000000000000000c3a5' : ZERO_ADDRESS,
        uri:
          spec.census === 'csp'
            ? 'https://csp.example.org/'
            : `https://census.example.org/${pid.slice(2, 10)}/v${censusRoots.length}.json`,
      },
      keyMode: spec.keyMode,
      dkg: dkgInfo
        ? {
            epochId: dkgInfo.epochId,
            aid: dkgInfo.aid,
            firstIndex: spec.end === 'results' || spec.end === 'awaiting-reveal' ? 1 : 0,
            count: spec.end === 'results' || spec.end === 'awaiting-reveal' ? nf : 0,
            zeroSkipped: 0,
            resultsRequested: spec.end === 'results' || spec.end === 'awaiting-reveal',
          }
        : null,
    })
  })

  const store = createEmptyStore({
    chainId: o.chainId,
    networkName: o.networkName,
    registryAddress: registry,
    startBlock,
    blockTimeSeconds: o.blockTimeSeconds,
  })
  applyEvents(store, events)
  for (const [pid, state] of states) applyProcessState(store, pid, state, o.headBlock)
  for (const [pid, root] of genesis) applyGenesisRoot(store, pid, root)
  applyTxDetails(store, txs)
  const registryInfo: RegistryInfo = {
    chainID: o.chainId,
    pidPrefix: prefix,
    processCount: SPECS.length,
    batchProgramVK: release.batchProgramVK,
    resultsProgramVK: release.resultsProgramVK,
    rootCVadcopFinal: release.rootCVadcopFinal,
    ballotVKHash: release.ballotVKHash,
    ziskVerifier: '0xde30000000000000000000000000000000000002',
    ziskVerifierCodeHash: release.ziskVerifierCodeHash,
    dkgAdapter: '0xde30000000000000000000000000000000000003',
    dkgManager: '0xde30000000000000000000000000000000000004',
    dkgAppManager: '0xde30000000000000000000000000000000000005',
    readAtBlock: o.headBlock,
  }
  applyRegistryInfo(store, registryInfo)
  applyHead(store, { block: o.headBlock, timestamp: o.headTimestamp })
  store.lastIndexedBlock = o.headBlock

  const pids = store.processOrder as Hex[]
  const sequencers: DemoSequencer[] = [
    {
      url: 'demo://sequencer/0',
      address: SEQUENCER_ADDRESSES[0]!,
      observer: false,
      settledBySelf: store.transitionOrder.filter((k) => store.transitions[k]!.sender === SEQUENCER_ADDRESSES[0])
        .length,
      syncedFromOthers: store.transitionOrder.filter((k) => store.transitions[k]!.sender !== SEQUENCER_ADDRESSES[0])
        .length,
      lostRaces: 2,
      votes: pendingVotes,
    },
    {
      url: 'demo://sequencer/1',
      address: null,
      observer: true,
      settledBySelf: 0,
      syncedFromOthers: store.transitionOrder.length,
      lostRaces: 0,
      votes: [],
    },
  ]

  const openProcess = pids[3]!
  return {
    store,
    transitionData,
    dkg,
    metadata,
    sequencers,
    featured: {
      openProcess,
      resultsProcess: pids[0]!,
      awaitingReveal: pids[2]!,
      multiBlob: multiBlob ?? { processId: openProcess, index: 0 },
      settledVote: settledVote ?? { processId: openProcess, voteId: VOTE_ID_MIN },
      pendingVote: pendingVotes[0] ?? { processId: openProcess, voteId: VOTE_ID_MIN },
    },
  }
}

/** The blobs of a demo transition, generated with the real cell encoder. */
export function demoTransitionBlobs(data: DemoTransitionData, accumulatorSeed = 7): Uint8Array[] {
  const rng = new Rng(data.ballotSeed)
  const cells = transitionCells({
    voteIds: data.voteIds,
    updates: data.updateKeys.map((key) => ({ key, ballot: ciphertexts(rng, data.numFields) })),
    accumulator: ciphertexts(new Rng(accumulatorSeed + data.index), data.numFields),
    numFields: data.numFields,
  })
  return blobsFromCells(cells)
}
