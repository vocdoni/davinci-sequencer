// The lifecycle strip of a process page: created → start → transitions → end
// → results, each step done, in progress, still ahead, or skipped (a canceled
// process never ends normally and never gets results).

import type { ProcessView } from '~data/hooks'
import type { Hex } from '~indexer/types'
import { formatNumber } from '~lib/format'

export type StepState = 'done' | 'current' | 'upcoming' | 'skipped'

export interface LifecycleStep {
  id: 'created' | 'start' | 'transitions' | 'end' | 'results'
  label: string
  state: StepState
  /** Unix seconds, when the step has a time. */
  time: number | null
  detail: string
  tx: Hex | null
}

const s = (count: number, one: string, many = `${one}s`) => `${formatNumber(count)} ${count === 1 ? one : many}`

export function processLifecycle(view: Pick<ProcessView, 'process' | 'row' | 'transitions'>, now: number | null) {
  const { process: p, row, transitions } = view
  const phase = row.phase
  const canceled = phase === 'canceled'
  const cancel = canceled ? [...p.statusChanges].reverse().find((c) => c.to === 'canceled') : undefined
  const started = row.startTime != null && now != null && now >= row.startTime
  const last = transitions[transitions.length - 1]
  const ballots = transitions.reduce((sum, t) => sum + t.votes, 0)
  const ended = phase === 'ended' || phase === 'results' || phase === 'closed'
  const endChange = [...p.statusChanges].reverse().find((c) => c.to === 'ended')

  const steps: LifecycleStep[] = [
    {
      id: 'created',
      label: 'Created',
      state: 'done',
      time: row.createdAt,
      detail: `Block ${formatNumber(p.createdBlock)}`,
      tx: p.createdTx,
    },
    {
      id: 'start',
      label: started ? 'Voting opened' : 'Voting opens',
      state: canceled && !started ? 'skipped' : started ? 'done' : 'upcoming',
      time: row.startTime,
      detail: canceled && !started ? 'Canceled before the start' : started ? 'Start time reached' : 'Start time',
      tx: null,
    },
    {
      id: 'transitions',
      label: 'Transitions',
      state:
        phase === 'open' || phase === 'paused'
          ? 'current'
          : transitions.length > 0
            ? 'done'
            : ended || canceled
              ? 'skipped'
              : 'upcoming',
      time: last?.timestamp ?? null,
      detail:
        transitions.length > 0
          ? `${s(transitions.length, 'batch', 'batches')}, ${s(ballots, 'ballot')}`
          : phase === 'open'
            ? 'No batch settled yet'
            : 'No batch settled',
      tx: last?.tx ?? null,
    },
    {
      id: 'end',
      label: canceled ? 'Canceled' : ended ? 'Voting closed' : 'Voting closes',
      state: canceled ? 'skipped' : ended ? 'done' : 'upcoming',
      time: canceled ? (cancel?.timestamp ?? null) : row.endTime,
      detail: canceled
        ? 'Canceled by the organizer'
        : phase === 'closed'
          ? 'End time passed; the status still reads Ready'
          : phase === 'paused'
            ? 'Paused by the organizer'
            : endChange
              ? 'Ended'
              : ended
                ? 'End time reached'
                : 'End time',
      tx: canceled ? (cancel?.tx ?? null) : (endChange?.tx ?? null),
    },
  ]

  const results = p.results
  steps.push(
    results
      ? {
          id: 'results',
          label: 'Results',
          state: 'done',
          time: results.timestamp,
          detail: 'Tally on-chain',
          tx: results.tx,
        }
      : canceled
        ? { id: 'results', label: 'Results', state: 'skipped', time: null, detail: 'None: canceled', tx: null }
        : p.decryptionRequest
          ? {
              id: 'results',
              label: 'Results',
              state: 'current',
              time: p.decryptionRequest.timestamp,
              detail: 'Decryption requested',
              tx: p.decryptionRequest.tx,
            }
          : {
              id: 'results',
              label: 'Results',
              state: ended ? 'current' : 'upcoming',
              time: null,
              detail: ended ? 'Pending' : 'After the end',
              tx: null,
            }
  )
  return steps
}
