import { Link } from 'react-router-dom'
import { useIndexer, useNetworkStats } from '~data/hooks'
import type { ProcessPhase } from '~indexer/selectors'
import { EmptyState, Panel } from '~kit'
import { CHART_COLORS, Donut, type DonutSlice } from '~kit/charts'
import { formatNumber } from '~lib/format'
import { CENSUS_ORIGIN_INFO, KEY_MODE_INFO, type CensusOriginName, type KeyModeName } from '~protocol/types'
import { paths } from '~routes/paths'

const PHASES: Array<{ phase: ProcessPhase; label: string; color: string }> = [
  { phase: 'open', label: 'open', color: CHART_COLORS.emerald },
  { phase: 'upcoming', label: 'upcoming', color: CHART_COLORS.teal },
  { phase: 'paused', label: 'paused', color: CHART_COLORS.amber },
  { phase: 'closed', label: 'voting closed', color: CHART_COLORS.warmGray },
  { phase: 'ended', label: 'ended', color: CHART_COLORS.pewter },
  { phase: 'results', label: 'results', color: CHART_COLORS.slate },
  { phase: 'canceled', label: 'canceled', color: CHART_COLORS.red },
]

const KEY_MODES: KeyModeName[] = ['sequencer', 'dkg-automatic', 'dkg-locked']
const CENSUS: CensusOriginName[] = ['merkle-static', 'merkle-dynamic', 'onchain-dynamic', 'csp']

function CountLink({ to, label, count }: { to: string; label: string; count: number }) {
  return (
    <li>
      <Link
        to={to}
        className='flex items-center justify-between gap-3 rounded-sm px-1 py-1 text-[13px] text-silver hover:bg-onyx hover:text-emerald'
      >
        <span className='truncate'>{label}</span>
        <span className='font-mono text-ash tnum'>{formatNumber(count)}</span>
      </Link>
    </li>
  )
}

/** Processes by phase, key mode and census origin, each linking to the filtered list. */
export function ProcessesPanel() {
  const stats = useNetworkStats()
  const { loading } = useIndexer()
  const slices: DonutSlice[] = PHASES.filter((p) => stats.byPhase[p.phase] > 0).map((p) => ({
    label: p.label,
    value: stats.byPhase[p.phase],
    color: p.color,
  }))

  return (
    <Panel
      title='Processes'
      label='By phase, key and census'
      actions={
        <Link to={paths.processes()} className='text-[13px] text-emerald hover:underline'>
          All processes
        </Link>
      }
    >
      {!loading && stats.processes === 0 ? (
        <EmptyState
          compact
          title='No processes yet'
          description='Each process an organizer creates will be counted here by phase, key mode and census.'
        />
      ) : (
        <div className='flex flex-col gap-5'>
          <Donut
            slices={slices}
            loading={loading}
            centerValue={formatNumber(stats.processes)}
            centerLabel='processes'
          />
          <div className='grid gap-4 sm:grid-cols-2 lg:grid-cols-1 xl:grid-cols-2'>
            <div>
              <div className='label-caps mb-1 text-[11px] text-pewter'>Key mode</div>
              <ul>
                {KEY_MODES.map((m) => (
                  <CountLink
                    key={m}
                    to={paths.processes({ keyMode: m })}
                    label={KEY_MODE_INFO[m].label}
                    count={stats.byKeyMode[m]}
                  />
                ))}
              </ul>
            </div>
            <div>
              <div className='label-caps mb-1 text-[11px] text-pewter'>Census</div>
              <ul>
                {CENSUS.map((c) => (
                  <CountLink
                    key={c}
                    to={paths.processes({ census: c })}
                    label={CENSUS_ORIGIN_INFO[c].label}
                    count={stats.byCensusOrigin[c]}
                  />
                ))}
              </ul>
            </div>
          </div>
        </div>
      )}
    </Panel>
  )
}
