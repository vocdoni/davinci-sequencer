import { Badge, Tooltip, type BadgeTone } from '~kit'
import type { ProcessPhase } from '~indexer/selectors'
import {
  CENSUS_ORIGIN_INFO,
  KEY_MODE_INFO,
  PROCESS_STATUS_INFO,
  type CensusOriginName,
  type KeyModeName,
} from '~protocol/types'

const PHASE: Record<ProcessPhase, { label: string; tone: BadgeTone; dot?: boolean; description: string }> = {
  loading: { label: 'Loading', tone: 'neutral', description: 'The process state has not been read yet.' },
  upcoming: { label: 'Upcoming', tone: 'neutral', description: 'Ready, but the voting window has not opened.' },
  open: { label: 'Open', tone: 'ok', dot: true, description: PROCESS_STATUS_INFO.ready.description },
  paused: { label: 'Paused', tone: 'warn', description: PROCESS_STATUS_INFO.paused.description },
  closed: {
    label: 'Voting closed',
    tone: 'warn',
    description: 'The end time has passed. The registry still says Ready until someone ends it or posts results.',
  },
  ended: { label: 'Ended', tone: 'neutral', description: PROCESS_STATUS_INFO.ended.description },
  canceled: { label: 'Canceled', tone: 'danger', description: PROCESS_STATUS_INFO.canceled.description },
  results: { label: 'Results', tone: 'accent', description: PROCESS_STATUS_INFO.results.description },
}

/** A process's phase (on-chain status plus the clock), with its meaning on hover. */
export function ProcessPhaseBadge({ phase, size }: { phase: ProcessPhase; size?: 'sm' | 'md' }) {
  const p = PHASE[phase]
  return (
    <Tooltip content={p.description}>
      <span className='inline-flex'>
        <Badge tone={p.tone} dot={p.dot} size={size}>
          {p.label}
        </Badge>
      </span>
    </Tooltip>
  )
}

export function KeyModeBadge({ mode, size }: { mode: KeyModeName; size?: 'sm' | 'md' }) {
  const info = KEY_MODE_INFO[mode]
  return (
    <Tooltip content={info.description}>
      <span className='inline-flex'>
        <Badge tone={mode === 'sequencer' ? 'neutral' : 'accent'} size={size}>
          {info.label}
        </Badge>
      </span>
    </Tooltip>
  )
}

export function CensusOriginBadge({ origin, size }: { origin: CensusOriginName; size?: 'sm' | 'md' }) {
  const info = CENSUS_ORIGIN_INFO[origin]
  return (
    <Tooltip content={info.description}>
      <span className='inline-flex'>
        <Badge tone='neutral' size={size}>
          {info.label}
        </Badge>
      </span>
    </Tooltip>
  )
}
