// Entity store of the in-browser indexer.
//
// Everything a page renders comes from here: the indexer fills it from
// ProcessRegistry events plus the contract state events do not carry, the
// demo fixture builds the same shape by pushing a generated event stream
// through the same reducers, and `selectors.ts` is the only thing that reads
// it. Blocks, counters and times are plain numbers; field elements, tallies
// and wei amounts stay `bigint`.
//
// The store is persisted verbatim (see `persist.ts`): keep it free of class
// instances, functions and cycles.

import type { Address, Hex } from 'viem'
import type { RegistryEventName } from '~contracts/abis'
import type { CensusOriginName, KeyModeName, ProcessStatusName } from '~protocol/types'

export type { Address, Hex, RegistryEventName }

/** Bumped whenever the shape below changes; a mismatch drops the cache. */
export const STORE_VERSION = 2

/** `bytes31` process id, lowercase. */
export type ProcessId = Hex

// ── Normalised events ────────────────────────────────────────────────────────

export interface EventDataMap {
  ProcessCreated: { creator: Address }
  ProcessStatusChanged: { oldStatus: ProcessStatusName; newStatus: ProcessStatusName }
  ProcessStateTransitioned: {
    sender: Address
    oldStateRoot: Hex
    newStateRoot: Hex
    /** Process totals after the transition. */
    newVotersCount: number
    newOverwrittenVotesCount: number
    nBlobs: number
  }
  ProcessResultsSet: { sender: Address; result: bigint[] }
  ProcessDurationChanged: { duration: number }
  ProcessMaxVotersChanged: { maxVoters: number }
  CensusUpdated: { censusRoot: Hex; censusURI: string }
  ResultsDecryptionRequested: { epochId: Hex; aid: Hex; firstIndex: number; count: number }
}

interface EventEnvelope {
  block: number
  tx: Hex | null
  logIndex: number
  /** Block time (unix seconds) when the log or a block read carried it. */
  timestamp: number | null
  processId: ProcessId
}

export type IndexedEvent = {
  [K in RegistryEventName]: EventEnvelope & { name: K; data: EventDataMap[K] }
}[RegistryEventName]

export type IndexedEventOf<K extends RegistryEventName> = Extract<IndexedEvent, { name: K }>

// ── Entities ─────────────────────────────────────────────────────────────────

export interface BallotMode {
  uniqueValues: boolean
  /** Fields per ballot (1..16). */
  numFields: number
  groupSize: number
  costExponent: number
  maxValue: bigint
  minValue: bigint
  /** 0 means "each voter's weight". */
  maxValueSum: bigint
  /** 0 means "no lower bound". */
  minValueSum: bigint
}

export interface CensusInfo {
  origin: CensusOriginName
  /** Big-endian integer as bytes32: the lean-IMT root, or the CSP signer address. */
  root: Hex
  /** Census contract of an on-chain census; the zero address otherwise. */
  contractAddress: Address
  uri: string
}

export interface DkgProcessInfo {
  epochId: Hex
  aid: Hex
  /** Index of the first ciphertext submitted for decryption. */
  firstIndex: number
  /** Ciphertexts submitted; identity fields are skipped. */
  count: number
  /**
   * Bit i set: field i was all-identity and recorded as 0 without decryption.
   * Null when only the `ResultsDecryptionRequested` event has been seen and
   * its count does not settle which fields were skipped.
   */
  zeroSkipped: number | null
  resultsRequested: boolean
}

/** A `getProcess` read, normalised. */
export interface ProcessState {
  status: ProcessStatusName
  organizer: Address
  encryptionKey: { x: bigint; y: bigint }
  /** Raw arbo digest of the latest settled root. */
  latestStateRoot: Hex
  result: bigint[]
  /** Unix seconds. */
  startTime: number
  /** Seconds. */
  duration: number
  maxVoters: number
  /** Distinct ballot slots written (votes minus overwrites). */
  votersCount: number
  overwrittenVotesCount: number
  creationBlock: number
  /** Transitions settled so far. */
  batchNumber: number
  metadataURI: string
  ballotMode: BallotMode
  census: CensusInfo
  keyMode: KeyModeName
  /** Null in sequencer key mode. */
  dkg: DkgProcessInfo | null
}

export interface StatusChange {
  block: number
  tx: Hex | null
  timestamp: number | null
  from: ProcessStatusName
  to: ProcessStatusName
}

export interface ValueChange<T> {
  block: number
  tx: Hex | null
  timestamp: number | null
  value: T
}

export interface ResultsEntity {
  block: number
  tx: Hex | null
  timestamp: number | null
  sender: Address
  values: bigint[]
}

export interface DecryptionRequestEntity {
  block: number
  tx: Hex | null
  timestamp: number | null
  epochId: Hex
  aid: Hex
  firstIndex: number
  count: number
}

export interface ProcessEntity {
  id: ProcessId
  organizer: Address
  createdBlock: number
  createdTx: Hex | null
  createdAt: number | null
  /** The contract state, read with `getProcess`; null until the first read. */
  state: ProcessState | null
  /** Block the state was read at; 0 when never read. */
  stateBlock: number
  /** `genesisRoot(...)`: the root before the first transition. */
  genesisRoot: Hex | null
  /** Keys into `transitions`, index order. */
  transitions: string[]
  statusChanges: StatusChange[]
  durationChanges: ValueChange<number>[]
  maxVotersChanges: ValueChange<number>[]
  censusUpdates: ValueChange<{ root: Hex; uri: string }>[]
  results: ResultsEntity | null
  decryptionRequest: DecryptionRequestEntity | null
  /** Indices into `IndexerStore.events`. */
  events: number[]
  lastActivityBlock: number
}

export interface TransitionEntity {
  key: string
  processId: ProcessId
  /** 0-based position among the process's transitions (the sequencer API's index). */
  index: number
  block: number
  tx: Hex | null
  logIndex: number
  timestamp: number | null
  sender: Address
  rootBefore: Hex
  rootAfter: Hex
  /** Process totals after the transition. */
  votersCount: number
  overwrittenVotesCount: number
  /** Slots written for the first time in this batch. */
  newVoters: number
  /** Votes that replaced an earlier vote. */
  overwrites: number
  nBlobs: number
}

/** A settlement transaction, read lazily: receipt, fee and decoded calldata. */
export interface TxDetails {
  hash: Hex
  from: Address
  to: Address | null
  blockNumber: number
  status: 'success' | 'reverted'
  gasUsed: bigint
  effectiveGasPrice: bigint
  blobGasUsed: bigint | null
  blobGasPrice: bigint | null
  /** gasUsed·effectiveGasPrice + blobGasUsed·blobGasPrice. */
  fee: bigint
  /** Null when the RPC returned the transaction without the field. */
  blobVersionedHashes: Hex[] | null
  /** Calldata size in bytes. */
  inputSize: number
  functionName: string | null
  /** `submitStateTransition` / `setProcessResults` arguments. */
  publicValues: Hex | null
  proofBytes: Hex | null
  commitments: Hex[]
  ys: Hex[]
  kzgProofs: Hex[]
  /** `newProcess` only: the census root the process was created with. */
  initialCensusRoot: Hex | null
  /** Why the calldata could not be decoded, if it could not. */
  decodeError: string | null
}

/** Registry immutables and counters, plus what they point at. */
export interface RegistryInfo {
  chainID: number
  pidPrefix: number
  processCount: number
  batchProgramVK: Hex
  resultsProgramVK: Hex
  rootCVadcopFinal: Hex
  ballotVKHash: Hex
  ziskVerifier: Address
  /** keccak256 of the verifier's runtime code (of empty code when it has none); null while the read fails. */
  ziskVerifierCodeHash: Hex | null
  /** Null when the DKG key modes are disabled (`dkgAdapter()` is the zero address). */
  dkgAdapter: Address | null
  /** The adapter's `manager()` / `appManager()`; null while the read fails. */
  dkgManager: Address | null
  dkgAppManager: Address | null
  readAtBlock: number
}

/** Chain-level facts, partly from config and partly read on chain. */
export interface ChainMeta {
  chainId: number
  networkName: string
  registryAddress: Address
  startBlock: number
  headBlock: number
  /** Unix seconds of `headBlock`. */
  headTimestamp: number | null
  /** Seconds per block, for estimates between known timestamps. */
  blockTimeSeconds: number
  registry: RegistryInfo | null
}

export interface IndexerStore {
  version: number
  chain: ChainMeta
  /** Highest block whose logs are in the store. */
  lastIndexedBlock: number
  /** Hash of `lastIndexedBlock`, to notice a reorg below the lag; null when unknown. */
  lastIndexedHash: Hex | null
  processes: Record<string, ProcessEntity>
  /** Process ids in creation order. */
  processOrder: string[]
  transitions: Record<string, TransitionEntity>
  /** Transition keys in chain order. */
  transitionOrder: string[]
  txDetails: Record<string, TxDetails>
  /** Block number → unix seconds, for blocks whose logs carried no timestamp. */
  blockTimes: Record<string, number>
  /** Every event ever seen, ascending by (block, logIndex). */
  events: IndexedEvent[]
}

// ── Status ───────────────────────────────────────────────────────────────────

export interface IndexerError {
  at: number
  scope: 'scan' | 'state' | 'tx' | 'persist' | 'poll' | 'chain'
  message: string
}

export interface IndexerStatus {
  phase: 'idle' | 'loading' | 'scanning' | 'live' | 'error'
  /** True while a backfill (not an incremental poll) is running. */
  scanning: boolean
  /** Where the scan started: the start block, or the cached cursor. */
  fromBlock: number
  /** Last block whose logs are indexed. */
  lastBlock: number
  /** Chain head as of the last poll. */
  headBlock: number
  /** 0…1 over the backfill range; 1 when caught up. */
  progress: number
  eventCount: number
  /** RPC requests issued since start. */
  requests: number
  lastPollAt: number | null
  errors: IndexerError[]
  /** Settlement transactions whose lookup failed 3 times this session (tx keys); retried with backoff. */
  skippedTx: string[]
  /** Set when the RPC reports another chain than the config expects; indexing stops. */
  chainMismatch: { expected: number; actual: number } | null
}

export interface IndexerSnapshot {
  store: IndexerStore
  status: IndexerStatus
}

// ── Keys ─────────────────────────────────────────────────────────────────────

export function processKey(id: string): string {
  return id.toLowerCase()
}

export function transitionKey(processId: string, index: number): string {
  return `${processId.toLowerCase()}:${index}`
}

export function txKey(hash: string): string {
  return hash.toLowerCase()
}
