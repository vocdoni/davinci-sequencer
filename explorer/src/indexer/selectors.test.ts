import { describe, expect, it } from 'vitest'
import { readFileSync } from 'node:fs'
import { resolve } from 'node:path'
import { demoFixture } from '~fixtures/demo'
import { formatVoteId } from '~protocol/blob'
import { KNOWN_RELEASES } from '~protocol/releases'
import { applyEvents, applyProcessState, applyTxDetails, createEmptyStore } from './reduce'
import {
  activityFeed,
  blockTimestamp,
  networkStats,
  processPhase,
  processRows,
  releaseCheck,
  rootChain,
  searchStore,
  transitionByTx,
  transitionDetail,
  transitionRows,
  votesPerDay,
} from './selectors'
import { txDetailsFrom } from './state'
import type { Hex, IndexerStore, ProcessState } from './types'

const fixture = demoFixture()
const store = fixture.store
const clone = (s: IndexerStore): IndexerStore => structuredClone(s)

describe('processRows', () => {
  it('lists every process newest first', () => {
    const rows = processRows(store)
    expect(rows).toHaveLength(store.processOrder.length)
    expect(rows[0]!.id).toBe(store.processOrder[store.processOrder.length - 1])
  })

  it('filters by status, phase, key mode, census, organizer and query', () => {
    expect(processRows(store, { status: 'results' }).every((r) => r.status === 'results')).toBe(true)
    expect(processRows(store, { status: 'closed' }).every((r) => r.phase === 'closed')).toBe(true)
    expect(processRows(store, { keyMode: 'dkg-locked' }).every((r) => r.keyMode === 'dkg-locked')).toBe(true)
    expect(processRows(store, { censusOrigin: 'csp' }).every((r) => r.censusOrigin === 'csp')).toBe(true)
    const organizer = store.processes[store.processOrder[0]!]!.organizer
    const mine = processRows(store, { organizer: organizer.toUpperCase().replace('0X', '0x') })
    expect(mine.length).toBeGreaterThan(0)
    expect(mine.every((r) => r.organizer === organizer)).toBe(true)
    const pid = store.processOrder[3]!
    expect(processRows(store, { query: pid.slice(40, 60) }).map((r) => r.id)).toContain(pid)
  })
})

describe('processPhase', () => {
  it('reads the clock for a ready process', () => {
    const p = structuredClone(store.processes[fixture.featured.openProcess]!)
    const s = p.state!
    expect(processPhase(p, s.startTime - 1)).toBe('upcoming')
    expect(processPhase(p, s.startTime + 1)).toBe('open')
    expect(processPhase(p, s.startTime + s.duration)).toBe('closed')
    expect(processPhase({ ...p, state: null }, 0)).toBe('loading')
  })
})

describe('transitions and the root chain', () => {
  it('numbers transitions per process and sums the ballots', () => {
    const pid = fixture.featured.openProcess
    const rows = transitionRows(store, pid)
    expect(rows.map((r) => r.index)).toEqual(rows.map((_, i) => i))
    const p = store.processes[pid]!
    expect(rows.reduce((n, r) => n + r.newVoters, 0)).toBe(p.state!.votersCount)
    expect(rows.every((r) => r.fee != null && r.gasUsed != null)).toBe(true)
  })

  it('flags a broken link in the root chain', () => {
    const s = clone(store)
    const pid = fixture.featured.openProcess
    const key = s.processes[pid]!.transitions[4]!
    s.transitions[key]!.rootBefore = `0x${'ee'.repeat(32)}`
    const chain = rootChain(s, pid)
    expect(chain.gaps).toBe(1)
    expect(chain.links[4]!.continuous).toBe(false)
    expect(transitionDetail(s, pid, 4)!.checks.find((c) => c.id === 'root-continuity')!.state).toBe('fail')
  })

  it('finds a transition by its transaction', () => {
    const t = store.transitions[store.transitionOrder[7]!]!
    expect(transitionByTx(store, t.tx!.toUpperCase().replace('0X', '0x'))?.key).toBe(t.key)
    expect(transitionByTx(store, `0x${'00'.repeat(32)}`)).toBeNull()
  })

  it('fails the digest check when a commitment is swapped', () => {
    const s = clone(store)
    const t = s.transitions[s.transitionOrder[0]!]!
    const tx = s.txDetails[t.tx!]!
    tx.commitments = [`0x${'11'.repeat(48)}`]
    const checks = transitionDetail(s, t.processId, t.index)!.checks
    expect(checks.find((c) => c.id === 'blobs-digest')!.state).toBe('fail')
    expect(checks.find((c) => c.id === 'blob-hashes')!.state).toBe('fail')
  })

  it('waits for the calldata before judging', () => {
    const s = clone(store)
    const t = s.transitions[s.transitionOrder[0]!]!
    delete s.txDetails[t.tx!]
    const checks = transitionDetail(s, t.processId, t.index)!.checks
    expect(checks.find((c) => c.id === 'guest-ok')!.state).toBe('unknown')
    expect(checks.find((c) => c.id === 'root-continuity')!.state).toBe('pass')
  })
})

describe('network views', () => {
  it('counts processes, ballots, transitions and blobs', () => {
    const stats = networkStats(store)
    expect(stats.processes).toBe(store.processOrder.length)
    expect(stats.transitions).toBe(store.transitionOrder.length)
    const blobs = store.transitionOrder.reduce((n, k) => n + store.transitions[k]!.nBlobs, 0)
    expect(stats.blobs).toBe(blobs)
    expect(stats.ballots).toBe(stats.voters + stats.overwrites)
    expect(stats.lastActivity?.block).toBe(store.events[store.events.length - 1]!.block)
  })

  it('feeds the newest events first, per network or per process', () => {
    const feed = activityFeed(store, 15)
    expect(feed).toHaveLength(15)
    expect(feed[0]!.block).toBeGreaterThanOrEqual(feed[14]!.block)
    const pid = fixture.featured.resultsProcess
    const mine = activityFeed(store, 100, pid)
    expect(mine.every((e) => e.processId === pid)).toBe(true)
    expect(mine.some((e) => e.kind === 'results')).toBe(true)
    expect(mine[mine.length - 1]!.kind).toBe('created')
  })

  it('buckets settled ballots per day', () => {
    const days = votesPerDay(store, 40)
    expect(days).toHaveLength(40)
    const total = store.transitionOrder.reduce(
      (n, k) => n + store.transitions[k]!.newVoters + store.transitions[k]!.overwrites,
      0
    )
    expect(days.reduce((n, d) => n + d.ballots, 0)).toBe(total)
    expect(days[0]!.day < days[39]!.day).toBe(true)
  })

  it('estimates block times from the head', () => {
    const { headBlock, headTimestamp, blockTimeSeconds } = store.chain
    expect(blockTimestamp(store, headBlock - 10)).toBe(headTimestamp! - 10 * blockTimeSeconds)
  })

  it('matches the demo deployment to the known release', () => {
    expect(releaseCheck(store).release?.id).toBe(KNOWN_RELEASES[0]!.id)
  })
})

describe('searchStore', () => {
  it('knows processes, transactions, organizers, blocks and vote ids', () => {
    const pid = fixture.featured.openProcess
    expect(searchStore(store, pid)[0]).toMatchObject({ kind: 'process', href: `/processes/${pid}` })
    const t = store.transitions[store.transitionOrder[2]!]!
    expect(searchStore(store, t.tx!)[0]).toMatchObject({
      kind: 'transition',
      href: `/processes/${t.processId}/transitions/${t.index}`,
    })
    expect(searchStore(store, String(t.block))[0]).toMatchObject({ kind: 'block' })
    const p = store.processes[pid]!
    expect(searchStore(store, p.createdTx!)[0]).toMatchObject({ kind: 'process' })
    expect(searchStore(store, p.organizer)[0]).toMatchObject({ kind: 'organizer' })
    expect(searchStore(store, store.chain.registryAddress)[0]).toMatchObject({ kind: 'contract', href: '/contracts' })
    const vote = formatVoteId(fixture.featured.settledVote.voteId)
    expect(searchStore(store, vote)[0]).toMatchObject({ kind: 'vote' })
    expect(searchStore(store, pid.slice(0, 20)).some((h) => h.href === `/processes/${pid}`)).toBe(true)
    expect(searchStore(store, 'nothing')).toEqual([])
  })
})

describe('a live Gnosis transition', () => {
  const live = JSON.parse(readFileSync(resolve(__dirname, '../../tests/vectors/gnosis_transition.json'), 'utf8'))

  it('passes the checks the registry ran on-chain', () => {
    const s = createEmptyStore({ chainId: 100, registryAddress: live.registry, startBlock: live.blockNumber - 10 })
    applyEvents(s, [
      {
        name: 'ProcessStateTransitioned',
        block: live.blockNumber,
        tx: live.txHash,
        logIndex: 0,
        timestamp: live.blockTimestamp,
        processId: live.event.processId,
        data: { ...live.event, sender: live.event.sender.toLowerCase() },
      },
    ])
    const details = txDetailsFrom(
      {
        hash: live.txHash,
        from: live.from,
        to: live.registry,
        input: live.input,
        blockNumber: BigInt(live.blockNumber),
        blobVersionedHashes: live.blobVersionedHashes,
      },
      {
        status: 'success',
        gasUsed: BigInt(live.gasUsed),
        effectiveGasPrice: 1n,
        blobGasUsed: BigInt(live.blobGasUsed),
        blobGasPrice: 1n,
        blockNumber: BigInt(live.blockNumber),
      }
    )
    applyTxDetails(s, [details])
    const d = transitionDetail(s, live.event.processId as Hex, 0)!
    const byId = Object.fromEntries(d.checks.map((c) => [c.id, c.state]))
    expect(byId['guest-ok']).toBe('pass')
    expect(byId['root-after']).toBe('pass')
    expect(byId['blob-count']).toBe('pass')
    expect(byId['blob-hashes']).toBe('pass')
    expect(byId['blobs-digest']).toBe('pass')
    expect(details.functionName).toBe('submitStateTransition')
    expect(details.fee).toBe(BigInt(live.gasUsed) + BigInt(live.blobGasUsed))

    // With the process's census root known, the census check runs too.
    const census = d.publics!.censusRoot
    applyProcessState(
      s,
      live.event.processId,
      {
        organizer: '0x0000000000000000000000000000000000000000',
        census: {
          origin: 'merkle-static',
          root: `0x${census.toString(16).padStart(64, '0')}`,
          contractAddress: '0x0000000000000000000000000000000000000000',
          uri: '',
        },
      } as unknown as ProcessState,
      live.blockNumber
    )
    expect(transitionDetail(s, live.event.processId, 0)!.checks.find((c) => c.id === 'census-root')!.state).toBe('pass')
  })

  it('leaves the blob checks unknown when the RPC omits blobVersionedHashes', () => {
    const s = createEmptyStore({ chainId: 100, registryAddress: live.registry, startBlock: live.blockNumber - 10 })
    applyEvents(s, [
      {
        name: 'ProcessStateTransitioned',
        block: live.blockNumber,
        tx: live.txHash,
        logIndex: 0,
        timestamp: live.blockTimestamp,
        processId: live.event.processId,
        data: { ...live.event, sender: live.event.sender.toLowerCase() },
      },
    ])
    const details = txDetailsFrom(
      {
        hash: live.txHash,
        from: live.from,
        to: live.registry,
        input: live.input,
        blockNumber: BigInt(live.blockNumber),
      },
      {
        status: 'success',
        gasUsed: BigInt(live.gasUsed),
        effectiveGasPrice: 1n,
        blockNumber: BigInt(live.blockNumber),
      }
    )
    expect(details.blobVersionedHashes).toBeNull()
    applyTxDetails(s, [details])
    const byId = Object.fromEntries(transitionDetail(s, live.event.processId, 0)!.checks.map((c) => [c.id, c.state]))
    expect(byId['blob-count']).toBe('unknown')
    expect(byId['blob-hashes']).toBe('unknown')
    expect(byId['blobs-digest']).toBe('pass')
  })
})
