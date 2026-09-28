// Contract state the events do not carry: `getProcess` structs, genesis
// roots, the registry immutables, the verifier code hash, settlement
// transactions and block times.
//
// Contract reads go through `multicall` against the canonical Multicall3
// address (deployed on Gnosis, Ethereum and pre-deployed by Anvil); a chain
// without it falls back to bounded-concurrency `eth_call`s. Transaction and
// block reads run concurrently, which the batching HTTP transport folds into
// a few JSON-RPC batch requests.

import { keccak256, type Abi, type PublicClient } from 'viem'
import { dkgAdapterAbi, processRegistryAbi } from '~contracts/abis'
import { decodeRegistryCall } from '~protocol/calldata'
import { censusOriginName, keyModeName, processStatusName } from '~protocol/types'
import type { Address, Hex, ProcessState, RegistryInfo, TxDetails } from './types'

/** Canonical Multicall3 deployment, identical on every supported chain. */
export const MULTICALL3_ADDRESS = '0xcA11bde05977b3631167028862bE2a173976CA11' as Address

const ZERO_ADDRESS = '0x0000000000000000000000000000000000000000'

type CallSpec = {
  address: Address
  abi: Abi
  functionName: string
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  args?: any[]
}

type CallResult = { ok: true; value: unknown } | { ok: false; error: unknown }

export interface StateReaderOptions {
  client: PublicClient
  registryAddress: Address
  /** Calls per multicall request. */
  batchSize?: number
  /** Parallel requests when multicall is unavailable, and for tx/block reads. */
  concurrency?: number
  multicallAddress?: Address
}

const num = (v: unknown): number => (typeof v === 'bigint' ? Number(v) : Number(v ?? 0))
const big = (v: unknown): bigint => (typeof v === 'bigint' ? v : BigInt(String(v ?? 0)))
const lower = <T extends string>(v: unknown): T => String(v ?? '').toLowerCase() as T

/** Normalises a decoded `getProcess` tuple. */
export function normalizeProcess(raw: Record<string, unknown>): ProcessState {
  const bm = (raw.ballotMode ?? {}) as Record<string, unknown>
  const census = (raw.census ?? {}) as Record<string, unknown>
  const key = (raw.encryptionKey ?? {}) as Record<string, unknown>
  const keyMode = keyModeName(num(raw.keyMode))
  const epochId = lower<Hex>(raw.dkgEpochId ?? '0x')
  return {
    status: processStatusName(num(raw.status)),
    organizer: lower<Address>(raw.organizationId),
    encryptionKey: { x: big(key.x), y: big(key.y) },
    latestStateRoot: lower<Hex>(raw.latestStateRoot),
    result: ((raw.result as unknown[]) ?? []).map(big),
    startTime: num(raw.startTime),
    duration: num(raw.duration),
    maxVoters: num(raw.maxVoters),
    votersCount: num(raw.votersCount),
    overwrittenVotesCount: num(raw.overwrittenVotesCount),
    creationBlock: num(raw.creationBlock),
    batchNumber: num(raw.batchNumber),
    metadataURI: String(raw.metadataURI ?? ''),
    ballotMode: {
      uniqueValues: Boolean(bm.uniqueValues),
      numFields: num(bm.numFields),
      groupSize: num(bm.groupSize),
      costExponent: num(bm.costExponent),
      maxValue: big(bm.maxValue),
      minValue: big(bm.minValue),
      maxValueSum: big(bm.maxValueSum),
      minValueSum: big(bm.minValueSum),
    },
    census: {
      origin: censusOriginName(num(census.censusOrigin)),
      root: lower<Hex>(census.censusRoot),
      contractAddress: lower<Address>(census.contractAddress ?? ZERO_ADDRESS),
      uri: String(census.censusURI ?? ''),
    },
    keyMode,
    dkg:
      keyMode === 'sequencer'
        ? null
        : {
            epochId,
            aid: lower<Hex>(raw.dkgAid),
            firstIndex: num(raw.dkgFirstIndex),
            count: num(raw.dkgCount),
            zeroSkipped: num(raw.dkgZeroSkipped),
            resultsRequested: Boolean(raw.dkgResultsRequested),
          },
  }
}

/** A registry read left a field null because its read failed (a zero `dkgAdapter()` is final). */
export function registryIncomplete(info: RegistryInfo): boolean {
  return (
    info.ziskVerifierCodeHash == null ||
    (info.dkgAdapter != null && (info.dkgManager == null || info.dkgAppManager == null))
  )
}

export class StateReader {
  readonly client: PublicClient
  readonly registryAddress: Address
  private readonly batchSize: number
  private readonly concurrency: number
  private readonly multicallAddress: Address
  private multicallSupported = true
  private multicallProbed = false
  /** Requests issued (multicall batches, calls, tx and block reads). */
  requests = 0

  constructor(options: StateReaderOptions) {
    this.client = options.client
    this.registryAddress = options.registryAddress.toLowerCase() as Address
    this.batchSize = options.batchSize ?? 64
    this.concurrency = options.concurrency ?? 8
    this.multicallAddress = options.multicallAddress ?? MULTICALL3_ADDRESS
  }

  /** Runs `calls`, never throwing per call: failures come back as `ok: false`. */
  async read(calls: CallSpec[], blockNumber?: number): Promise<CallResult[]> {
    const out: CallResult[] = []
    for (let i = 0; i < calls.length; i += this.batchSize) {
      out.push(...(await this.readBatch(calls.slice(i, i + this.batchSize), blockNumber)))
    }
    return out
  }

  private async readBatch(batch: CallSpec[], blockNumber?: number): Promise<CallResult[]> {
    if (this.multicallSupported && !this.multicallProbed) {
      // viem reports a missing Multicall3 as per-call failures rather than a
      // rejection, so probe for code once.
      this.multicallProbed = true
      if (typeof this.client.getCode === 'function') {
        const code = await this.client.getCode({ address: this.multicallAddress }).catch(() => undefined)
        if (code === '0x' || code === undefined) this.multicallSupported = false
      }
    }
    if (this.multicallSupported) {
      try {
        this.requests += 1
        const results = await this.client.multicall({
          // eslint-disable-next-line @typescript-eslint/no-explicit-any
          contracts: batch as any,
          allowFailure: true,
          multicallAddress: this.multicallAddress,
          blockNumber: blockNumber != null ? BigInt(blockNumber) : undefined,
        })
        return results.map((r) =>
          r.status === 'success' ? { ok: true as const, value: r.result } : { ok: false as const, error: r.error }
        )
      } catch {
        // A provider that rejects the batch: degrade once for the session.
        this.multicallSupported = false
      }
    }
    return this.parallel(batch, async (call) => {
      try {
        this.requests += 1
        const value = await this.client.readContract({
          ...call,
          blockNumber: blockNumber != null ? BigInt(blockNumber) : undefined,
        } as Parameters<PublicClient['readContract']>[0])
        return { ok: true as const, value }
      } catch (error) {
        return { ok: false as const, error }
      }
    })
  }

  private async parallel<T, R>(items: T[], fn: (item: T) => Promise<R>): Promise<R[]> {
    const out: R[] = new Array(items.length)
    let cursor = 0
    const worker = async () => {
      for (;;) {
        const i = cursor++
        if (i >= items.length) return
        out[i] = await fn(items[i]!)
      }
    }
    await Promise.all(Array.from({ length: Math.min(this.concurrency, items.length) }, worker))
    return out
  }

  private registry(functionName: string, args?: unknown[]): CallSpec {
    return { address: this.registryAddress, abi: processRegistryAbi, functionName, args }
  }

  /** Immutables, counters and what they point at. Throws when the registry answers nothing. */
  async readRegistryInfo(blockNumber: number): Promise<RegistryInfo> {
    const names = [
      'chainID',
      'pidPrefix',
      'processCount',
      'batchProgramVK',
      'resultsProgramVK',
      'rootCVadcopFinal',
      'ballotVKHash',
      'ziskVerifier',
      'dkgAdapter',
    ] as const
    const results = await this.read(names.map((n) => this.registry(n)))
    const value = (n: (typeof names)[number]) => {
      const r = results[names.indexOf(n)]!
      if (!r.ok)
        throw new Error(`registry ${n}() failed: ${String((r.error as Error)?.message ?? r.error).split('\n')[0]}`)
      return r.value
    }
    const ziskVerifier = lower<Address>(value('ziskVerifier'))
    const adapter = lower<Address>(value('dkgAdapter'))
    const dkgAdapter = adapter === ZERO_ADDRESS ? null : adapter
    const ziskVerifierCodeHash = await this.readCodeHash(ziskVerifier)
    const { dkgManager, dkgAppManager } = dkgAdapter
      ? await this.readDkgLinks(dkgAdapter)
      : { dkgManager: null, dkgAppManager: null }

    return {
      chainID: num(value('chainID')),
      pidPrefix: num(value('pidPrefix')),
      processCount: num(value('processCount')),
      batchProgramVK: lower<Hex>(value('batchProgramVK')),
      resultsProgramVK: lower<Hex>(value('resultsProgramVK')),
      rootCVadcopFinal: lower<Hex>(value('rootCVadcopFinal')),
      ballotVKHash: lower<Hex>(value('ballotVKHash')),
      ziskVerifier,
      ziskVerifierCodeHash,
      dkgAdapter,
      dkgManager,
      dkgAppManager,
      readAtBlock: blockNumber,
    }
  }

  /** keccak256 of the code at `address` (of empty code when there is none); null when the read fails. */
  async readCodeHash(address: Address): Promise<Hex | null> {
    try {
      this.requests += 1
      const code = await this.client.getCode({ address })
      return keccak256(code ?? '0x')
    } catch {
      return null
    }
  }

  /** The DKG adapter's `manager()` and `appManager()`; null for a read that failed. */
  async readDkgLinks(adapter: Address): Promise<Pick<RegistryInfo, 'dkgManager' | 'dkgAppManager'>> {
    const [manager, appManager] = await this.read([
      { address: adapter, abi: dkgAdapterAbi, functionName: 'manager' },
      { address: adapter, abi: dkgAdapterAbi, functionName: 'appManager' },
    ])
    return {
      dkgManager: manager?.ok ? lower<Address>(manager.value) : null,
      dkgAppManager: appManager?.ok ? lower<Address>(appManager.value) : null,
    }
  }

  /** Re-reads the fields an earlier `readRegistryInfo` left null because their read failed. */
  async completeRegistryInfo(info: RegistryInfo): Promise<RegistryInfo> {
    let out = info
    if (info.ziskVerifierCodeHash == null) {
      const ziskVerifierCodeHash = await this.readCodeHash(info.ziskVerifier)
      if (ziskVerifierCodeHash) out = { ...out, ziskVerifierCodeHash }
    }
    if (info.dkgAdapter && (info.dkgManager == null || info.dkgAppManager == null)) {
      const links = await this.readDkgLinks(info.dkgAdapter)
      const dkgManager = out.dkgManager ?? links.dkgManager
      const dkgAppManager = out.dkgAppManager ?? links.dkgAppManager
      if (dkgManager !== out.dkgManager || dkgAppManager !== out.dkgAppManager) {
        out = { ...out, dkgManager, dkgAppManager }
      }
    }
    return out
  }

  async readProcessCount(blockNumber?: number): Promise<number | null> {
    const [r] = await this.read([this.registry('processCount')], blockNumber)
    return r?.ok ? num(r.value) : null
  }

  /** `getProcess` for each id, at one block. Missing entries failed. */
  async readProcesses(ids: Hex[], blockNumber?: number): Promise<Map<Hex, ProcessState>> {
    const results = await this.read(
      ids.map((id) => this.registry('getProcess', [id])),
      blockNumber
    )
    const out = new Map<Hex, ProcessState>()
    results.forEach((r, i) => {
      if (r.ok && r.value && typeof r.value === 'object')
        out.set(ids[i]!, normalizeProcess(r.value as Record<string, unknown>))
    })
    return out
  }

  /** The root a process starts from: `genesisRoot(pid, ballotMode, key, censusOrigin)`. */
  async readGenesisRoots(processes: Array<{ id: Hex; state: ProcessState }>): Promise<Map<Hex, Hex>> {
    const originCode = (o: ProcessState['census']['origin']) =>
      ['unknown', 'merkle-static', 'merkle-dynamic', 'onchain-dynamic', 'csp'].indexOf(o)
    const results = await this.read(
      processes.map(({ id, state }) =>
        this.registry('genesisRoot', [
          id,
          {
            uniqueValues: state.ballotMode.uniqueValues,
            numFields: state.ballotMode.numFields,
            groupSize: state.ballotMode.groupSize,
            costExponent: state.ballotMode.costExponent,
            maxValue: state.ballotMode.maxValue,
            minValue: state.ballotMode.minValue,
            maxValueSum: state.ballotMode.maxValueSum,
            minValueSum: state.ballotMode.minValueSum,
          },
          { x: state.encryptionKey.x, y: state.encryptionKey.y },
          originCode(state.census.origin),
        ])
      )
    )
    const out = new Map<Hex, Hex>()
    results.forEach((r, i) => {
      if (r.ok) out.set(processes[i]!.id, lower<Hex>(r.value))
    })
    return out
  }

  /** Receipt, fee and decoded calldata of settlement transactions. */
  async readTxDetails(hashes: Hex[]): Promise<TxDetails[]> {
    const out = await this.parallel(hashes, async (hash) => {
      try {
        this.requests += 2
        const [tx, receipt] = await Promise.all([
          this.client.getTransaction({ hash }),
          this.client.getTransactionReceipt({ hash }),
        ])
        return txDetailsFrom(tx, receipt)
      } catch {
        return null
      }
    })
    return out.filter((d): d is TxDetails => d != null)
  }

  /** Unix seconds of each block. */
  async readBlockTimes(blocks: number[]): Promise<Record<number, number>> {
    const out: Record<number, number> = {}
    await this.parallel(blocks, async (b) => {
      try {
        this.requests += 1
        const block = await this.client.getBlock({ blockNumber: BigInt(b) })
        out[b] = Number(block.timestamp)
      } catch {
        // Retried on a later tick.
      }
    })
    return out
  }
}

interface TxLike {
  hash: Hex
  from: Address
  to: Address | null
  input: Hex
  blockNumber: bigint | null
  blobVersionedHashes?: readonly Hex[] | null
}

interface ReceiptLike {
  status: 'success' | 'reverted'
  gasUsed: bigint
  effectiveGasPrice: bigint
  blobGasUsed?: bigint | null
  blobGasPrice?: bigint | null
  blockNumber: bigint
}

/** Normalises a transaction and its receipt, decoding the registry calldata. */
export function txDetailsFrom(tx: TxLike, receipt: ReceiptLike): TxDetails {
  const blobGasUsed = receipt.blobGasUsed ?? null
  const blobGasPrice = receipt.blobGasPrice ?? null
  const details: TxDetails = {
    hash: tx.hash.toLowerCase() as Hex,
    from: tx.from.toLowerCase() as Address,
    to: tx.to ? (tx.to.toLowerCase() as Address) : null,
    blockNumber: Number(receipt.blockNumber),
    status: receipt.status,
    gasUsed: receipt.gasUsed,
    effectiveGasPrice: receipt.effectiveGasPrice,
    blobGasUsed,
    blobGasPrice,
    fee: receipt.gasUsed * receipt.effectiveGasPrice + (blobGasUsed ?? 0n) * (blobGasPrice ?? 0n),
    blobVersionedHashes: tx.blobVersionedHashes ? tx.blobVersionedHashes.map((h) => h.toLowerCase() as Hex) : null,
    inputSize: (tx.input.length - 2) / 2,
    functionName: null,
    publicValues: null,
    proofBytes: null,
    commitments: [],
    ys: [],
    kzgProofs: [],
    initialCensusRoot: null,
    decodeError: null,
  }
  try {
    const decoded = decodeRegistryCall(tx.input)
    details.functionName = decoded.name
    if ('call' in decoded) {
      const call = decoded.call as unknown as Record<string, unknown>
      details.publicValues = (call.publicValues as Hex | undefined) ?? null
      details.proofBytes = (call.proofBytes as Hex | undefined) ?? null
      details.commitments = (call.commitments as Hex[] | undefined) ?? []
      details.ys = (call.ys as Hex[] | undefined) ?? []
      details.kzgProofs = (call.kzgProofs as Hex[] | undefined) ?? []
    } else if (decoded.name === 'newProcess') {
      const census = decoded.args[5] as { censusRoot?: Hex } | undefined
      details.initialCensusRoot = census?.censusRoot ? (census.censusRoot.toLowerCase() as Hex) : null
    }
  } catch (err) {
    details.decodeError = err instanceof Error ? err.message : String(err)
  }
  return details
}
