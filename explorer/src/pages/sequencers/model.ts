// Pure derivations for the sequencers page: who settled what according to the
// registry events, and whether a node's /info describes this deployment.

import type { Address, Hex } from 'viem'
import type { CheckState } from '~indexer/selectors'
import type { ChainMeta, IndexerStore } from '~indexer/types'
import type { SequencerInfo, SequencerProcess } from '~protocol/sequencer-api'

export interface SettlerRow {
  address: Address
  /** Transitions this address sent. */
  transitions: number
  /** Distinct processes among them. */
  processes: number
  lastBlock: number
  lastTimestamp: number | null
}

/** Every sender of a `ProcessStateTransitioned`, busiest first. */
export function settlers(store: IndexerStore): SettlerRow[] {
  const by = new Map<string, SettlerRow & { pids: Set<string> }>()
  for (const key of store.transitionOrder) {
    const t = store.transitions[key]
    if (!t) continue
    const address = t.sender.toLowerCase() as Address
    let row = by.get(address)
    if (!row) {
      row = { address, transitions: 0, processes: 0, lastBlock: 0, lastTimestamp: null, pids: new Set() }
      by.set(address, row)
    }
    row.transitions += 1
    row.pids.add(t.processId.toLowerCase())
    if (t.block >= row.lastBlock) {
      row.lastBlock = t.block
      row.lastTimestamp = t.timestamp ?? store.blockTimes[t.block] ?? row.lastTimestamp
    }
  }
  return [...by.values()]
    .map(({ pids, ...row }) => ({ ...row, processes: pids.size }))
    .sort((a, b) => b.transitions - a.transitions || b.lastBlock - a.lastBlock)
}

export interface InfoCheck {
  id: string
  label: string
  state: CheckState
}

const same = (a: string | null | undefined, b: string | null | undefined): CheckState =>
  a == null || b == null ? 'unknown' : a.toLowerCase() === b.toLowerCase() ? 'pass' : 'fail'

/** Whether the node's /info names this chain, this registry and the registry's pins. */
export function infoChecks(info: SequencerInfo, chain: ChainMeta): InfoCheck[] {
  const r = chain.registry
  return [
    { id: 'chain', label: 'Chain id', state: info.chainId === chain.chainId ? 'pass' : 'fail' },
    { id: 'registry', label: 'Registry', state: same(info.processRegistry, chain.registryAddress) },
    { id: 'ballot-vk', label: 'Ballot VK hash', state: same(info.ballotVkHash, r?.ballotVKHash) },
    { id: 'batch-vk', label: 'Vote-batch program vk', state: same(info.batchProgramVk, r?.batchProgramVK) },
    { id: 'results-vk', label: 'Results program vk', state: same(info.resultsProgramVk, r?.resultsProgramVK) },
  ]
}

/** Transitions an address sent, from the store. */
export function settledBy(rows: SettlerRow[], address: string | null | undefined): SettlerRow | null {
  if (!address) return null
  return rows.find((r) => r.address === address.toLowerCase()) ?? null
}

export type SyncState = 'in-sync' | 'differs' | 'unknown'

/** The node's committed root against the registry's latest root for the process. */
export function syncState(view: SequencerProcess | undefined, onchainRoot: Hex | null | undefined): SyncState {
  if (!view?.localStateRoot || !onchainRoot) return 'unknown'
  return view.localStateRoot.toLowerCase() === onchainRoot.toLowerCase() ? 'in-sync' : 'differs'
}
