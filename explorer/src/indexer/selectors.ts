// Pure selectors over the entity store. Pages read the store only through
// these (via the hooks in `~data/hooks`), so every derived number has one
// definition and one test.

import type { Address, Hex } from 'viem'
import { blobsDigest, parseVoteId, versionedHash } from '~protocol/blob'
import { isProcessId } from '~protocol/process-id'
import { decodeBatchPublicValues, publicsPassed, type BatchPublics } from '~protocol/publics'
import { matchRelease, type DeploymentPins, type ReleaseMatch } from '~protocol/releases'
import type { CensusOriginName, KeyModeName, ProcessStatusName } from '~protocol/types'
import { paths } from '~routes/paths'
import {
  processKey,
  transitionKey,
  txKey,
  type IndexedEvent,
  type IndexerStore,
  type ProcessEntity,
  type TransitionEntity,
  type TxDetails,
} from './types'

// ── time ─────────────────────────────────────────────────────────────────────

/** Unix seconds of a block: known exactly, or estimated from the head and the block time. */
export function blockTimestamp(store: IndexerStore, block: number): number | null {
  const known = store.blockTimes[block]
  if (known != null) return known
  const { headBlock, headTimestamp, blockTimeSeconds } = store.chain
  if (headTimestamp == null) return null
  return headTimestamp - (headBlock - block) * blockTimeSeconds
}

/** "Now" on chain: the head block's time. */
export function chainNow(store: IndexerStore): number | null {
  return store.chain.headTimestamp
}

// ── processes ────────────────────────────────────────────────────────────────

/**
 * Where a process stands, combining the on-chain status with the clock: the
 * registry leaves a process READY after its end time until someone ends it
 * or posts results, so `closed` means "READY but past the end".
 */
export type ProcessPhase = 'loading' | 'upcoming' | 'open' | 'paused' | 'closed' | 'ended' | 'canceled' | 'results'

export function processPhase(p: ProcessEntity, now: number | null): ProcessPhase {
  const s = p.state
  if (!s) return 'loading'
  switch (s.status) {
    case 'results':
      return 'results'
    case 'canceled':
      return 'canceled'
    case 'ended':
      return 'ended'
    case 'paused':
      return 'paused'
    case 'ready':
      if (now != null && now < s.startTime) return 'upcoming'
      if (now != null && now >= s.startTime + s.duration) return 'closed'
      return 'open'
  }
}

export interface ProcessRow {
  id: Hex
  organizer: Address
  status: ProcessStatusName | null
  phase: ProcessPhase
  keyMode: KeyModeName | null
  censusOrigin: CensusOriginName | null
  numFields: number | null
  /** Distinct voters (slots written). */
  votersCount: number
  overwrittenVotesCount: number
  maxVoters: number | null
  startTime: number | null
  endTime: number | null
  createdBlock: number
  createdAt: number | null
  transitions: number
  hasResults: boolean
  lastActivityBlock: number
}

export function processRow(store: IndexerStore, p: ProcessEntity): ProcessRow {
  const s = p.state
  const last = p.transitions.length ? store.transitions[p.transitions[p.transitions.length - 1]!] : undefined
  return {
    id: p.id,
    organizer: p.organizer,
    status: s?.status ?? null,
    phase: processPhase(p, chainNow(store)),
    keyMode: s?.keyMode ?? null,
    censusOrigin: s?.census.origin ?? null,
    numFields: s?.ballotMode.numFields ?? null,
    votersCount: Math.max(s?.votersCount ?? 0, last?.votersCount ?? 0),
    overwrittenVotesCount: Math.max(s?.overwrittenVotesCount ?? 0, last?.overwrittenVotesCount ?? 0),
    maxVoters: s?.maxVoters ?? null,
    startTime: s?.startTime ?? null,
    endTime: s ? s.startTime + s.duration : null,
    createdBlock: p.createdBlock,
    createdAt: p.createdAt ?? blockTimestamp(store, p.createdBlock),
    transitions: p.transitions.length,
    hasResults: p.results != null || s?.status === 'results',
    lastActivityBlock: p.lastActivityBlock,
  }
}

export interface ProcessFilter {
  status?: ProcessStatusName | ProcessPhase
  keyMode?: KeyModeName
  censusOrigin?: CensusOriginName
  organizer?: string
  /** Substring of the id or the organizer. */
  query?: string
}

/** Every process, newest first, filtered. */
export function processRows(store: IndexerStore, filter: ProcessFilter = {}): ProcessRow[] {
  const organizer = filter.organizer?.toLowerCase()
  const query = filter.query?.trim().toLowerCase()
  const out: ProcessRow[] = []
  for (let i = store.processOrder.length - 1; i >= 0; i--) {
    const row = processRow(store, store.processes[store.processOrder[i]!]!)
    if (filter.status && row.status !== filter.status && row.phase !== filter.status) continue
    if (filter.keyMode && row.keyMode !== filter.keyMode) continue
    if (filter.censusOrigin && row.censusOrigin !== filter.censusOrigin) continue
    if (organizer && row.organizer !== organizer) continue
    if (query && !row.id.includes(query) && !row.organizer.includes(query)) continue
    out.push(row)
  }
  return out
}

// ── transitions ──────────────────────────────────────────────────────────────

export interface TransitionRow {
  key: string
  processId: Hex
  index: number
  block: number
  tx: Hex | null
  timestamp: number | null
  sender: Address
  rootBefore: Hex
  rootAfter: Hex
  newVoters: number
  overwrites: number
  /** Ballots in the batch: new voters plus overwrites. */
  votes: number
  nBlobs: number
  gasUsed: bigint | null
  blobGasUsed: bigint | null
  fee: bigint | null
  /** Root continuity with the previous transition (or the genesis root); null when unknown. */
  continuous: boolean | null
}

export function transitionRow(store: IndexerStore, t: TransitionEntity, expectedBefore: Hex | null): TransitionRow {
  const tx = t.tx ? store.txDetails[txKey(t.tx)] : undefined
  return {
    key: t.key,
    processId: t.processId,
    index: t.index,
    block: t.block,
    tx: t.tx,
    timestamp: t.timestamp ?? blockTimestamp(store, t.block),
    sender: t.sender,
    rootBefore: t.rootBefore,
    rootAfter: t.rootAfter,
    newVoters: t.newVoters,
    overwrites: t.overwrites,
    votes: t.newVoters + t.overwrites,
    nBlobs: t.nBlobs,
    gasUsed: tx?.gasUsed ?? null,
    blobGasUsed: tx?.blobGasUsed ?? null,
    fee: tx?.fee ?? null,
    continuous: expectedBefore == null ? null : expectedBefore === t.rootBefore,
  }
}

/** A process's transitions in index order. */
export function transitionRows(store: IndexerStore, pid: string): TransitionRow[] {
  const p = store.processes[processKey(pid)]
  if (!p) return []
  const rows: TransitionRow[] = []
  let expected: Hex | null = p.genesisRoot
  for (const key of p.transitions) {
    const t = store.transitions[key]!
    rows.push(transitionRow(store, t, expected))
    expected = t.rootAfter
  }
  return rows
}

export interface RootLink {
  index: number
  expectedBefore: Hex | null
  rootBefore: Hex
  rootAfter: Hex
  continuous: boolean | null
}

export interface RootChain {
  genesisRoot: Hex | null
  links: RootLink[]
  /** Transitions whose root-before is not the previous root-after. */
  gaps: number
  /** The last root equals the registry's `latestStateRoot`; null when unknown. */
  headMatches: boolean | null
}

/** The state-root chain: genesis → every transition → the latest root. */
export function rootChain(store: IndexerStore, pid: string): RootChain {
  const p = store.processes[processKey(pid)]
  if (!p) return { genesisRoot: null, links: [], gaps: 0, headMatches: null }
  const links = transitionRows(store, pid).map((r) => ({
    index: r.index,
    expectedBefore:
      r.index === 0 ? p.genesisRoot : (store.transitions[transitionKey(pid, r.index - 1)]?.rootAfter ?? null),
    rootBefore: r.rootBefore,
    rootAfter: r.rootAfter,
    continuous: r.continuous,
  }))
  const last = links[links.length - 1]?.rootAfter ?? p.genesisRoot
  return {
    genesisRoot: p.genesisRoot,
    links,
    gaps: links.filter((l) => l.continuous === false).length,
    headMatches: p.state && last ? p.state.latestStateRoot === last : null,
  }
}

/** The transition a settlement transaction created. */
export function transitionByTx(store: IndexerStore, hash: string): TransitionEntity | null {
  const h = hash.toLowerCase()
  for (const key of store.transitionOrder) {
    const t = store.transitions[key]!
    if (t.tx === h) return t
  }
  return null
}

// ── transition detail and the checks the contract ran ───────────────────────

export type CheckState = 'pass' | 'fail' | 'unknown'

export interface TransitionCheck {
  id:
    | 'guest-ok'
    | 'root-continuity'
    | 'root-after'
    | 'census-root'
    | 'occupied-before'
    | 'blob-count'
    | 'blob-hashes'
    | 'blobs-digest'
    | 'voters'
  label: string
  state: CheckState
  detail: string
}

export interface TransitionDetail {
  transition: TransitionEntity
  row: TransitionRow
  process: ProcessEntity
  previous: TransitionEntity | null
  next: TransitionEntity | null
  tx: TxDetails | null
  publics: BatchPublics | null
  publicsError: string | null
  checks: TransitionCheck[]
}

function check(id: TransitionCheck['id'], label: string, ok: boolean | null, detail: string): TransitionCheck {
  return { id, label, state: ok == null ? 'unknown' : ok ? 'pass' : 'fail', detail }
}

/**
 * Everything about one transition, with the settlement checks recomputed from
 * public data: the ones `submitStateTransition` enforces on-chain, redone
 * here from the event, the calldata and the previous transition.
 */
export function transitionDetail(store: IndexerStore, pid: string, index: number): TransitionDetail | null {
  const t = store.transitions[transitionKey(pid, index)]
  const p = store.processes[processKey(pid)]
  if (!t || !p) return null
  const previous = index > 0 ? (store.transitions[transitionKey(pid, index - 1)] ?? null) : null
  const next = store.transitions[transitionKey(pid, index + 1)] ?? null
  const expectedBefore = previous ? previous.rootAfter : p.genesisRoot
  const row = transitionRow(store, t, expectedBefore)
  const tx = t.tx ? (store.txDetails[txKey(t.tx)] ?? null) : null

  let publics: BatchPublics | null = null
  let publicsError: string | null = null
  if (tx?.publicValues) {
    try {
      publics = decodeBatchPublicValues(tx.publicValues)
    } catch (err) {
      publicsError = err instanceof Error ? err.message : String(err)
    }
  } else if (tx?.decodeError) {
    publicsError = tx.decodeError
  }

  const checks: TransitionCheck[] = []
  checks.push(
    check(
      'guest-ok',
      'The zkVM guest accepted the batch',
      publics ? publicsPassed(publics) : null,
      publics ? `ok = ${publics.ok ? 1 : 0}, fail mask = ${publics.failMask}` : 'Waiting for the calldata'
    )
  )
  checks.push(
    check(
      'root-continuity',
      'Starts from the previous root',
      expectedBefore == null
        ? null
        : expectedBefore === t.rootBefore && (!publics || publics.rootBefore === t.rootBefore),
      previous ? `transition #${previous.index} ended at this root` : 'the genesis root of the process'
    )
  )
  checks.push(
    check(
      'root-after',
      'The proven root is the new root',
      publics ? publics.rootAfter === t.rootAfter : null,
      'publics register 10..17 against the event'
    )
  )
  const census = p.state?.census
  let censusOk: boolean | null = null
  if (publics && census && census.origin !== 'onchain-dynamic') {
    // An updatable census may have used a root replaced since; the ones the
    // explorer knows are the current root and every CensusUpdated root.
    const initial = p.createdTx ? store.txDetails[txKey(p.createdTx)]?.initialCensusRoot : null
    const known = [census.root, ...p.censusUpdates.map((u) => u.value.root), ...(initial ? [initial] : [])].map((r) =>
      BigInt(r)
    )
    censusOk = known.includes(publics.censusRoot)
      ? true
      : census.origin === 'merkle-dynamic' && p.censusUpdates.length > 0
        ? null
        : false
  }
  checks.push(
    check(
      'census-root',
      'Proven against the process census',
      censusOk,
      census?.origin === 'onchain-dynamic'
        ? 'On-chain census: the registry asked the census contract whether it held this root'
        : 'publics register 20..27 against the census root'
    )
  )
  const occupiedExpected = previous ? previous.votersCount : 0
  checks.push(
    check(
      'occupied-before',
      'Slots written before the batch match the registry',
      publics ? publics.occupiedBefore === occupiedExpected : null,
      `occupied_before = ${publics?.occupiedBefore ?? '…'}, registry votersCount = ${occupiedExpected}`
    )
  )
  checks.push(
    check(
      'voters',
      'Vote counts match the event',
      publics ? publics.voters - publics.overwrites === t.newVoters && publics.overwrites === t.overwrites : null,
      `${t.newVoters} new voters, ${t.overwrites} overwrites`
    )
  )
  // Null when the RPC left the field out: nothing to compare, not a mismatch.
  const hashes = tx?.blobVersionedHashes ?? null
  checks.push(
    check(
      'blob-count',
      'One blob per published chunk',
      hashes ? hashes.length === t.nBlobs && (!publics || publics.nBlobs === t.nBlobs) : null,
      `${t.nBlobs} blob${t.nBlobs === 1 ? '' : 's'}`
    )
  )
  checks.push(
    check(
      'blob-hashes',
      'Each commitment is the blob the transaction carries',
      tx && hashes && tx.commitments.length > 0
        ? tx.commitments.length === hashes.length && tx.commitments.every((c, i) => versionedHash(c) === hashes[i])
        : null,
      'versioned hash = 0x01 ‖ sha256(commitment)[1..]'
    )
  )
  checks.push(
    check(
      'blobs-digest',
      'The proof commits to these blobs',
      tx && publics && tx.commitments.length > 0 ? blobsDigest(tx.commitments, tx.ys) === publics.blobsDigest : null,
      'sha256(commitment ‖ evaluation …) against publics register 28..35'
    )
  )

  return { transition: t, row, process: p, previous, next, tx, publics, publicsError, checks }
}

// ── network ──────────────────────────────────────────────────────────────────

export interface NetworkStats {
  processes: number
  byStatus: Record<ProcessStatusName | 'unknown', number>
  byPhase: Record<ProcessPhase, number>
  byKeyMode: Record<KeyModeName, number>
  byCensusOrigin: Record<CensusOriginName, number>
  organizers: number
  /** Distinct voters across processes. */
  voters: number
  overwrites: number
  /** Ballots settled: voters plus overwrites. */
  ballots: number
  transitions: number
  blobs: number
  withResults: number
  lastActivity: { block: number; timestamp: number | null } | null
}

export function networkStats(store: IndexerStore): NetworkStats {
  const byStatus: NetworkStats['byStatus'] = { ready: 0, ended: 0, canceled: 0, paused: 0, results: 0, unknown: 0 }
  const byPhase: NetworkStats['byPhase'] = {
    loading: 0,
    upcoming: 0,
    open: 0,
    paused: 0,
    closed: 0,
    ended: 0,
    canceled: 0,
    results: 0,
  }
  const byKeyMode: NetworkStats['byKeyMode'] = { sequencer: 0, 'dkg-automatic': 0, 'dkg-locked': 0 }
  const byCensusOrigin: NetworkStats['byCensusOrigin'] = {
    unknown: 0,
    'merkle-static': 0,
    'merkle-dynamic': 0,
    'onchain-dynamic': 0,
    csp: 0,
  }
  const organizers = new Set<string>()
  let voters = 0
  let overwrites = 0
  let withResults = 0
  for (const key of store.processOrder) {
    const row = processRow(store, store.processes[key]!)
    byStatus[row.status ?? 'unknown'] += 1
    byPhase[row.phase] += 1
    if (row.keyMode) byKeyMode[row.keyMode] += 1
    if (row.censusOrigin) byCensusOrigin[row.censusOrigin] += 1
    organizers.add(row.organizer)
    voters += row.votersCount
    overwrites += row.overwrittenVotesCount
    if (row.hasResults) withResults += 1
  }
  let blobs = 0
  for (const key of store.transitionOrder) blobs += store.transitions[key]!.nBlobs
  const last = store.events[store.events.length - 1]
  return {
    processes: store.processOrder.length,
    byStatus,
    byPhase,
    byKeyMode,
    byCensusOrigin,
    organizers: organizers.size,
    voters,
    overwrites,
    ballots: voters + overwrites,
    transitions: store.transitionOrder.length,
    blobs,
    withResults,
    lastActivity: last ? { block: last.block, timestamp: last.timestamp ?? blockTimestamp(store, last.block) } : null,
  }
}

// ── activity ─────────────────────────────────────────────────────────────────

export type FeedKind =
  'created' | 'transition' | 'results' | 'status' | 'decryption' | 'census' | 'duration' | 'max-voters'

export interface FeedEntry {
  key: string
  kind: FeedKind
  processId: Hex
  block: number
  tx: Hex | null
  timestamp: number | null
  /** One plain line: "Transition #3: 12 votes in 1 blob". */
  label: string
  href: string
}

function feedEntry(store: IndexerStore, ev: IndexedEvent): FeedEntry {
  const base = {
    key: `${ev.block}:${ev.logIndex}`,
    processId: ev.processId,
    block: ev.block,
    tx: ev.tx,
    timestamp: ev.timestamp ?? blockTimestamp(store, ev.block),
    href: paths.process(ev.processId),
  }
  switch (ev.name) {
    case 'ProcessCreated':
      return { ...base, kind: 'created', label: 'Process created' }
    case 'ProcessStateTransitioned': {
      const t = transitionByTx(store, ev.tx ?? '') ?? null
      const votes = t ? t.newVoters + t.overwrites : null
      return {
        ...base,
        kind: 'transition',
        label: `Transition #${t?.index ?? '?'}${votes != null ? `: ${votes} vote${votes === 1 ? '' : 's'}` : ''} in ${ev.data.nBlobs} blob${ev.data.nBlobs === 1 ? '' : 's'}`,
        href: t ? paths.transition(ev.processId, t.index) : base.href,
      }
    }
    case 'ProcessResultsSet':
      return { ...base, kind: 'results', label: 'Results published', href: paths.process(ev.processId, 'results') }
    case 'ProcessStatusChanged':
      return { ...base, kind: 'status', label: `Status ${ev.data.oldStatus} → ${ev.data.newStatus}` }
    case 'ResultsDecryptionRequested':
      return {
        ...base,
        kind: 'decryption',
        label: `Tally sent to the DKG committee (${ev.data.count} ciphertext${ev.data.count === 1 ? '' : 's'})`,
        href: paths.process(ev.processId, 'results'),
      }
    case 'CensusUpdated':
      return { ...base, kind: 'census', label: 'Census root replaced' }
    case 'ProcessDurationChanged':
      return { ...base, kind: 'duration', label: 'Duration changed' }
    case 'ProcessMaxVotersChanged':
      return { ...base, kind: 'max-voters', label: `Max voters set to ${ev.data.maxVoters}` }
  }
}

/** The newest `limit` events as feed entries, newest first. Pass a pid for one process. */
export function activityFeed(store: IndexerStore, limit = 20, pid?: string): FeedEntry[] {
  const out: FeedEntry[] = []
  const events = pid ? (store.processes[processKey(pid)]?.events ?? []).map((i) => store.events[i]!) : store.events
  for (let i = events.length - 1; i >= 0 && out.length < limit; i--) out.push(feedEntry(store, events[i]!))
  return out
}

export interface DayBucket {
  /** YYYY-MM-DD (UTC). */
  day: string
  ballots: number
  newVoters: number
  overwrites: number
  transitions: number
}

/** Settled votes per UTC day over the last `days` days (oldest first), up to the chain's now. */
export function votesPerDay(store: IndexerStore, days = 30): DayBucket[] {
  const now = chainNow(store)
  if (now == null) return []
  const dayOf = (ts: number) => new Date(ts * 1000).toISOString().slice(0, 10)
  const buckets = new Map<string, DayBucket>()
  const end = Math.floor(now / 86400)
  for (let d = end - days + 1; d <= end; d++) {
    const day = dayOf(d * 86400)
    buckets.set(day, { day, ballots: 0, newVoters: 0, overwrites: 0, transitions: 0 })
  }
  for (const key of store.transitionOrder) {
    const t = store.transitions[key]!
    const ts = t.timestamp ?? blockTimestamp(store, t.block)
    if (ts == null) continue
    const b = buckets.get(dayOf(ts))
    if (!b) continue
    b.newVoters += t.newVoters
    b.overwrites += t.overwrites
    b.ballots += t.newVoters + t.overwrites
    b.transitions += 1
  }
  return [...buckets.values()]
}

// ── contracts ────────────────────────────────────────────────────────────────

/** The deployment's pins as far as they are known, for `matchRelease`. */
export function deploymentPins(store: IndexerStore): DeploymentPins {
  const r = store.chain.registry
  return {
    batchProgramVK: r?.batchProgramVK ?? null,
    resultsProgramVK: r?.resultsProgramVK ?? null,
    rootCVadcopFinal: r?.rootCVadcopFinal ?? null,
    ballotVKHash: r?.ballotVKHash ?? null,
    ziskVerifierCodeHash: r?.ziskVerifierCodeHash ?? null,
  }
}

export function releaseCheck(store: IndexerStore): ReleaseMatch {
  return matchRelease(deploymentPins(store))
}

/** Every root the registry has held for a process: genesis and each root after. */
export function onchainRoots(store: IndexerStore, pid: string): Set<Hex> {
  const p = store.processes[processKey(pid)]
  const out = new Set<Hex>()
  if (!p) return out
  if (p.genesisRoot) out.add(p.genesisRoot)
  for (const key of p.transitions) out.add(store.transitions[key]!.rootAfter)
  if (p.state) out.add(p.state.latestStateRoot)
  return out
}

// ── search ───────────────────────────────────────────────────────────────────

export interface SearchHit {
  kind: 'process' | 'transition' | 'organizer' | 'contract' | 'vote' | 'block'
  label: string
  href: string
}

/**
 * What the store knows about a query: a process id, a settlement or creation
 * transaction, an organizer or contract address, a vote id, or a block with a
 * transition. Shape-only routing (and the block-explorer fallback) is the
 * shell's `resolveSearch`; this runs first.
 */
export function searchStore(store: IndexerStore, raw: string, limit = 8): SearchHit[] {
  const q = raw.trim().toLowerCase()
  if (!q) return []
  const hits: SearchHit[] = []
  const push = (h: SearchHit) => {
    if (hits.length < limit && !hits.some((x) => x.href === h.href)) hits.push(h)
  }

  if (isProcessId(q) && store.processes[q]) push({ kind: 'process', label: `Process ${q}`, href: paths.process(q) })

  if (/^0x[0-9a-f]{64}$/.test(q)) {
    const t = transitionByTx(store, q)
    if (t) push({ kind: 'transition', label: `Transition #${t.index}`, href: paths.transition(t.processId, t.index) })
    for (const key of store.processOrder) {
      const p = store.processes[key]!
      if (p.createdTx === q) push({ kind: 'process', label: 'Process creation', href: paths.process(p.id) })
      if (p.results?.tx === q) push({ kind: 'process', label: 'Results', href: paths.process(p.id, 'results') })
    }
  }

  if (/^0x[0-9a-f]{40}$/.test(q)) {
    const chain = store.chain
    const contracts = [chain.registryAddress, chain.registry?.ziskVerifier, chain.registry?.dkgAdapter].filter(Boolean)
    if (contracts.includes(q as Address)) push({ kind: 'contract', label: 'Contract', href: paths.contracts() })
    const owned = store.processOrder.filter((k) => store.processes[k]!.organizer === q).length
    if (owned > 0) {
      push({
        kind: 'organizer',
        label: `Organizer of ${owned} process${owned === 1 ? '' : 'es'}`,
        href: paths.processes({ organizer: q }),
      })
    }
  }

  const voteId = parseVoteId(q)
  if (voteId != null && (q.startsWith('0x') || q.length >= 19)) {
    push({ kind: 'vote', label: 'Vote id', href: paths.votes({ voteId: q }) })
  }

  if (/^\d+$/.test(q) && q.length < 12) {
    const block = Number(q)
    for (const key of store.transitionOrder) {
      const t = store.transitions[key]!
      if (t.block === block)
        push({
          kind: 'block',
          label: `Transition #${t.index} in block ${block}`,
          href: paths.transition(t.processId, t.index),
        })
    }
  }

  // A prefix of a process id.
  if (/^0x[0-9a-f]{6,61}$/.test(q)) {
    for (const key of store.processOrder) {
      if (key.startsWith(q)) push({ kind: 'process', label: `Process ${key}`, href: paths.process(key) })
    }
  }
  return hits
}
