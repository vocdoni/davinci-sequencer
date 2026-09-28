import { Link } from 'react-router-dom'
import { Explain, Timestamp } from '~components'
import { useRuntimeConfig } from '~config/config-context'
import { useIndexer, useNetworkStats } from '~data/hooks'
import { buttonClasses, Callout, SectionHeader, Stack, StatCell, StatRow } from '~kit'
import { formatNumber } from '~lib/format'
import { paths } from '~routes/paths'
import { ActivityPanel } from './ActivityPanel'
import { NetworkCard } from './NetworkCard'
import { ProcessesPanel } from './ProcessesPanel'
import { RoleCards } from './RoleCards'
import { VotesPerDayPanel } from './VotesPerDayPanel'

/** The front page: what the deployment is doing, and where to start checking it. */
export function OverviewPage() {
  const config = useRuntimeConfig()
  const stats = useNetworkStats()
  const { loading } = useIndexer()
  const empty = !loading && stats.processes === 0

  return (
    <Stack data-testid='page-overview'>
      <SectionHeader
        size='page'
        label='Overview'
        title={config.networkName}
        description='What this DAVINCI deployment is doing, and how to check it yourself. Everything here is read from the chain by your browser.'
        actions={
          <Link to={paths.processes()} className={buttonClasses('secondary', 'md')}>
            Browse processes
          </Link>
        }
      />

      {empty ? (
        <div data-testid='empty-registry'>
          <Callout tone='info' title='No processes on this registry yet'>
            The registry is deployed and the explorer is watching it. When an organizer creates a process it shows up
            here with its ballot rules, census and key. Each batch a sequencer settles then appears as a state
            transition with its blobs, and the tally appears once the process ends. Meanwhile you can already check the
            deployment itself.
          </Callout>
        </div>
      ) : null}

      <StatRow>
        <StatCell
          label='Processes'
          value={formatNumber(stats.processes)}
          loading={loading}
          mono
          hint={`${formatNumber(stats.byPhase.open)} open · ${formatNumber(stats.withResults)} with results`}
        />
        <StatCell
          label='Ballots settled'
          value={formatNumber(stats.ballots)}
          loading={loading}
          mono
          tone={stats.ballots > 0 ? 'accent' : 'default'}
          hint={`${formatNumber(stats.voters)} voters · ${formatNumber(stats.overwrites)} overwrites`}
          aside={
            <Explain>
              Votes in settled batches. A voter counts once however often they vote again; each later vote is an
              overwrite that replaces the previous one.
            </Explain>
          }
        />
        <StatCell
          label='Transitions'
          value={formatNumber(stats.transitions)}
          loading={loading}
          mono
          hint={`${formatNumber(stats.blobs)} blob${stats.blobs === 1 ? '' : 's'} published`}
          aside={
            <Explain>
              A transition is one batch of votes a sequencer proved and settled on the registry. Its data travels in
              EIP-4844 blobs, so anyone can rebuild the state.
            </Explain>
          }
        />
        <StatCell
          label='Last activity'
          value={stats.lastActivity ? <Timestamp value={stats.lastActivity.timestamp} /> : '—'}
          loading={loading}
          hint={stats.lastActivity ? `block ${formatNumber(stats.lastActivity.block)}` : 'no registry events yet'}
        />
      </StatRow>

      <RoleCards />

      <div className='grid items-start gap-6 lg:grid-cols-5'>
        <Stack className='min-w-0 lg:col-span-3'>
          <VotesPerDayPanel />
          <ActivityPanel />
        </Stack>
        <Stack className='min-w-0 lg:col-span-2'>
          <NetworkCard />
          <ProcessesPanel />
        </Stack>
      </div>
    </Stack>
  )
}
