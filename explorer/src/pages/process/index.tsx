import { useNavigate, useParams } from 'react-router-dom'
import { CensusOriginBadge, KeyModeBadge, MissingEntity, ProcessPhaseBadge } from '~components'
import { useProcess } from '~data/hooks'
import { useJsonDocument } from '~data/queries'
import { Address, Hash, SectionHeader, Stack, StatCell, StatRow, Tabs } from '~kit'
import { formatNumber, shortHash } from '~lib/format'
import { isProcessTab, paths, type ProcessTab } from '~routes/paths'
import { Lifecycle } from './Lifecycle'
import { fetchableUri, metadataTitle } from './metadata'
import { KeyTab } from './tabs/KeyTab'
import { OverviewTab } from './tabs/OverviewTab'
import { RawTab } from './tabs/RawTab'
import { ResultsTab } from './tabs/ResultsTab'
import { TransitionsTab } from './tabs/TransitionsTab'
import { VotesTab } from './tabs/VotesTab'

// Header (phase, lifecycle, counters) plus one tab per aspect; the active tab
// is the last path segment (/processes/:pid/:tab), so every tab is linkable.
export function ProcessPage() {
  const { pid, tab } = useParams()
  const navigate = useNavigate()
  const view = useProcess(pid)
  const active: ProcessTab = isProcessTab(tab) ? tab : 'overview'
  const metadata = useJsonDocument(fetchableUri(view?.process.state?.metadataURI))

  if (!pid || !view) return <MissingEntity what='process' id={pid} />
  const { process, row, transitions } = view
  const title = metadataTitle(metadata.data)
  const blobs = transitions.reduce((n, t) => n + t.nBlobs, 0)

  return (
    <Stack data-testid='page-process'>
      <SectionHeader
        size='page'
        label='Process'
        title={title ?? `Process ${shortHash(process.id, 8, 6)}`}
        description={
          <span className='flex flex-col gap-1'>
            <span className='inline-flex min-w-0 flex-wrap items-center gap-x-2'>
              <span className='text-ash'>id</span>
              <Hash value={process.id} chars={14} />
            </span>
            <span className='inline-flex min-w-0 flex-wrap items-center gap-x-2'>
              <span className='text-ash'>organizer</span>
              <Address value={process.organizer} to={paths.processes({ organizer: process.organizer })} />
            </span>
          </span>
        }
        actions={
          <>
            <ProcessPhaseBadge phase={row.phase} />
            {row.keyMode ? <KeyModeBadge mode={row.keyMode} /> : null}
            {row.censusOrigin ? <CensusOriginBadge origin={row.censusOrigin} /> : null}
          </>
        }
      />

      <Lifecycle view={view} />

      <StatRow>
        <StatCell
          label='Voters'
          value={formatNumber(row.votersCount)}
          mono
          hint={row.maxVoters != null ? `of at most ${formatNumber(row.maxVoters)}` : undefined}
        />
        <StatCell
          label='Overwrites'
          value={formatNumber(row.overwrittenVotesCount)}
          mono
          hint='votes that replaced an earlier one'
        />
        <StatCell
          label='Transitions'
          value={formatNumber(transitions.length)}
          mono
          hint={`${formatNumber(transitions.reduce((n, t) => n + t.votes, 0))} ballots settled`}
        />
        <StatCell label='Blobs' value={formatNumber(blobs)} mono hint='EIP-4844 data blobs published' />
      </StatRow>

      <Tabs
        value={active}
        onValueChange={(value) => navigate(paths.process(process.id, value as ProcessTab))}
        items={[
          { value: 'overview', label: 'Overview', content: <OverviewTab view={view} /> },
          { value: 'key', label: 'Encryption key', content: <KeyTab view={view} /> },
          {
            value: 'transitions',
            label: 'Transitions',
            meta: transitions.length,
            content: <TransitionsTab view={view} />,
          },
          { value: 'votes', label: 'Votes', content: <VotesTab view={view} /> },
          { value: 'results', label: 'Results', content: <ResultsTab view={view} /> },
          { value: 'raw', label: 'Raw', content: <RawTab view={view} /> },
        ]}
      />
    </Stack>
  )
}
