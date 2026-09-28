import { useMemo } from 'react'
import { useIndexer, useVotesPerDay } from '~data/hooks'
import { EmptyState, Panel } from '~kit'
import { CHART_COLORS, StackedBars, type BarDatum } from '~kit/charts'
import { formatNumber } from '~lib/format'

const DAYS = 30

/** Ballots settled per UTC day: new voters and overwrites, stacked. */
export function VotesPerDayPanel() {
  const days = useVotesPerDay(DAYS)
  const { loading } = useIndexer()
  const data = useMemo<BarDatum[]>(
    () =>
      days.map((d) => ({
        label: d.day.slice(5),
        values: { newVoters: d.newVoters, overwrites: d.overwrites },
        note: `${d.day} · ${formatNumber(d.transitions)} transition${d.transitions === 1 ? '' : 's'}`,
      })),
    [days]
  )
  const total = days.reduce((n, d) => n + d.ballots, 0)

  return (
    <Panel
      title='Ballots settled per day'
      label={`Last ${DAYS} days, UTC`}
      description='A ballot is a new voter or an overwrite of an earlier vote. The silent refreshes each batch adds are not votes and are not counted.'
      actions={<span className='font-mono text-[12px] text-ash tnum'>{formatNumber(total)} in total</span>}
    >
      {!loading && total === 0 ? (
        <EmptyState
          compact
          title={`No ballots settled in the last ${DAYS} days`}
          description='Each batch a sequencer settles adds its votes to the day it landed on.'
        />
      ) : (
        <StackedBars
          data={data}
          loading={loading}
          height={200}
          series={[
            { key: 'newVoters', label: 'new voters', color: CHART_COLORS.emerald },
            { key: 'overwrites', label: 'overwrites', color: CHART_COLORS.slate },
          ]}
        />
      )}
    </Panel>
  )
}
