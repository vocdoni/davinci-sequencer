// Input checks and status words of the vote lookup. Pure, unit-tested.

import { parseVoteId } from '~protocol/blob'
import type { Hex } from '~protocol/bytes'
import { U64_MAX, VOTE_ID_MIN } from '~protocol/limits'
import { isProcessId, normalizeProcessId } from '~protocol/process-id'
import type { VoteStatus } from '~protocol/sequencer-api'

export interface LookupQuery {
  pid: Hex | null
  voteId: bigint | null
  pidError: string | null
  voteError: string | null
}

/** Validates the two fields; each error is a sentence for the form. */
export function validateLookup(pidInput: string, voteInput: string): LookupQuery {
  const p = pidInput.trim()
  const v = voteInput.trim()
  let pidError: string | null = null
  if (!p) pidError = 'Enter the process id.'
  else if (!isProcessId(p)) pidError = 'A process id is 0x followed by 62 hex digits (31 bytes).'

  const voteId = v ? parseVoteId(v) : null
  let voteError: string | null = null
  if (!v) voteError = 'Enter the vote id.'
  else if (voteId == null) {
    const n = /^0x[0-9a-fA-F]{1,16}$/.test(v) || /^\d{1,20}$/.test(v) ? BigInt(v) : null
    voteError =
      n != null && n < VOTE_ID_MIN
        ? 'Vote ids start at 0x8000000000000000 (2^63).'
        : n != null && n > U64_MAX
          ? 'A vote id fits in 64 bits.'
          : 'A vote id is 0x followed by 16 hex digits, or its decimal value.'
  }
  return { pid: pidError ? null : normalizeProcessId(p), voteId, pidError, voteError }
}

/** The happy path a vote walks at a sequencer. */
export const STATUS_STEPS: Array<Exclude<VoteStatus, 'error'>> = ['pending', 'aggregated', 'processed', 'settled']

/** Position on the happy path; -1 for an error. */
export function statusStep(status: VoteStatus): number {
  return status === 'error' ? -1 : STATUS_STEPS.indexOf(status)
}

/** Sequencer README "HTTP API": what each status means. */
export const STATUS_INFO: Record<VoteStatus, { label: string; description: string }> = {
  pending: {
    label: 'Pending',
    description:
      'Queued at the sequencer, waiting for a batch. A batch that loses a settlement race puts its votes back here.',
  },
  aggregated: { label: 'Aggregated', description: 'In a batch that is being proved.' },
  processed: { label: 'Processed', description: 'The batch proof is checked; the settlement is on its way.' },
  settled: { label: 'Settled', description: 'On-chain: the batch carrying the vote settled on the registry.' },
  error: {
    label: 'Error',
    description:
      'The vote will not settle: a guest check failed, the process closed, the settlement reverted or the prover refused the batch.',
  },
}
