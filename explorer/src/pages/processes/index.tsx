import { useEffect, useMemo, useState } from 'react'
import type { SortingState } from '@tanstack/react-table'
import { useNavigate, useSearchParams } from 'react-router-dom'
import { CensusOriginBadge, Explain, KeyModeBadge, ProcessIdLink, ProcessPhaseBadge, Timestamp } from '~components'
import { useIndexer, useNetworkStats, useProcesses } from '~data/hooks'
import type { ProcessRow } from '~indexer/selectors'
import {
  Address,
  Badge,
  Button,
  Card,
  DataTable,
  EmptyState,
  Input,
  SearchIcon,
  SectionHeader,
  Select,
  Stack,
  Tooltip,
  type AnyColumnDef,
} from '~kit'
import { formatNumber } from '~lib/format'
import { paths, type ProcessListFilter } from '~routes/paths'
import { CENSUS_OPTIONS, isFiltered, KEY_MODE_OPTIONS, PHASE_OPTIONS, readProcessFilter } from './filters'

const ADDRESS = /^0x[0-9a-fA-F]{40}$/

const columns: AnyColumnDef<ProcessRow>[] = [
  {
    id: 'id',
    header: 'Process',
    accessorKey: 'createdBlock',
    cell: ({ row }) => <ProcessIdLink id={row.original.id} chars={8} />,
    meta: {
      width: '190px',
      headerTooltip: 'Process id: organizer address, registry prefix and nonce. Sorts by creation.',
    },
  },
  {
    id: 'organizer',
    header: 'Organizer',
    accessorKey: 'organizer',
    cell: ({ row }) => (
      <Address
        value={row.original.organizer}
        to={paths.processes({ organizer: row.original.organizer })}
        explorer={false}
      />
    ),
    meta: { headerTooltip: 'The account that created the process; click to list only its processes.' },
  },
  {
    id: 'phase',
    header: 'Phase',
    accessorKey: 'phase',
    cell: ({ row }) => <ProcessPhaseBadge phase={row.original.phase} size='sm' />,
  },
  {
    id: 'keyMode',
    header: 'Key',
    accessorFn: (r) => r.keyMode ?? '',
    cell: ({ row }) => (row.original.keyMode ? <KeyModeBadge mode={row.original.keyMode} size='sm' /> : '—'),
    meta: { headerTooltip: 'Who holds the election key and who can decrypt the tally.' },
  },
  {
    id: 'census',
    header: 'Census',
    accessorFn: (r) => r.censusOrigin ?? '',
    cell: ({ row }) =>
      row.original.censusOrigin ? <CensusOriginBadge origin={row.original.censusOrigin} size='sm' /> : '—',
    meta: { headerTooltip: 'Where the list of eligible voters comes from.' },
  },
  {
    id: 'voters',
    header: 'Voters / overwrites',
    accessorKey: 'votersCount',
    cell: ({ row }) => {
      const r = row.original
      return (
        <Tooltip
          content={r.maxVoters != null ? `at most ${formatNumber(r.maxVoters)} voters` : 'max voters not read yet'}
        >
          <span>
            {formatNumber(r.votersCount)}
            <span className='text-ash'> / {formatNumber(r.overwrittenVotesCount)}</span>
          </span>
        </Tooltip>
      )
    },
    meta: {
      numeric: true,
      headerTooltip: 'Distinct voters, and votes that replaced an earlier vote of the same voter.',
    },
  },
  {
    id: 'start',
    header: 'Start',
    accessorFn: (r) => r.startTime ?? 0,
    cell: ({ row }) => <Timestamp value={row.original.startTime} className='text-[12px]' />,
    meta: { align: 'right' },
  },
  {
    id: 'end',
    header: 'End',
    accessorFn: (r) => r.endTime ?? 0,
    cell: ({ row }) => <Timestamp value={row.original.endTime} className='text-[12px]' />,
    meta: { align: 'right', headerTooltip: 'Start time plus duration; ending early shortens the duration.' },
  },
  {
    id: 'results',
    header: 'Results',
    accessorFn: (r) => (r.hasResults ? 1 : 0),
    cell: ({ row }) =>
      row.original.hasResults ? (
        <Badge tone='accent' size='sm'>
          yes
        </Badge>
      ) : (
        <span className='text-ash'>—</span>
      ),
    meta: { align: 'center', width: '90px' },
  },
]

/** Every process on the registry, filtered and searched through the URL query. */
export function ProcessesPage() {
  const [params] = useSearchParams()
  const navigate = useNavigate()
  const { list, filter } = useMemo(() => readProcessFilter(params), [params])
  const rows = useProcesses(filter)
  const stats = useNetworkStats()
  const { loading } = useIndexer()
  const filtered = isFiltered(list)
  // Controlled sorting: uncontrolled, the kit table passes onSortingChange: undefined over
  // TanStack's default updater and header clicks do nothing.
  const [sorting, setSorting] = useState<SortingState>([])

  const apply = (next: Partial<ProcessListFilter>) => navigate(paths.processes({ ...list, ...next }), { replace: true })

  // The organizer box applies once it holds a whole address (or nothing).
  const [organizer, setOrganizer] = useState(list.organizer ?? '')
  useEffect(() => setOrganizer(list.organizer ?? ''), [list.organizer])
  const organizerInvalid = organizer.trim() !== '' && !ADDRESS.test(organizer.trim())

  return (
    <Stack data-testid='page-processes'>
      <SectionHeader
        size='page'
        label='Processes'
        title='Every voting process on the registry'
        description='Each process fixes its ballot rules, census and encryption key at creation. Sequencers then settle batches of votes on it until it ends and its tally is published.'
      />

      <Card className='flex flex-col gap-4'>
        <div className='grid gap-3 sm:grid-cols-2 lg:grid-cols-[minmax(0,2fr)_repeat(3,minmax(0,1fr))_minmax(0,2fr)]'>
          <Input
            label='Search'
            aria-label='Search processes by id or organizer'
            placeholder='Process id or organizer, or part of one'
            mono
            iconLeft={<SearchIcon size={14} />}
            value={params.get('q') ?? ''}
            onChange={(e) => apply({ q: e.target.value || undefined })}
          />
          <Select
            label='Phase'
            value={list.status ?? ''}
            onChange={(e) => apply({ status: e.target.value || undefined })}
            options={[{ value: '', label: 'Any phase' }, ...PHASE_OPTIONS]}
          />
          <Select
            label='Key mode'
            value={list.keyMode ?? ''}
            onChange={(e) => apply({ keyMode: e.target.value || undefined })}
            options={[{ value: '', label: 'Any key mode' }, ...KEY_MODE_OPTIONS]}
          />
          <Select
            label='Census'
            value={list.census ?? ''}
            onChange={(e) => apply({ census: e.target.value || undefined })}
            options={[{ value: '', label: 'Any census' }, ...CENSUS_OPTIONS]}
          />
          <Input
            label='Organizer'
            placeholder='0x… (whole address)'
            mono
            value={organizer}
            error={organizerInvalid ? 'An address is 0x followed by 40 hex digits.' : undefined}
            onChange={(e) => {
              const value = e.target.value
              setOrganizer(value)
              const v = value.trim()
              if (v === '') apply({ organizer: undefined })
              else if (ADDRESS.test(v)) apply({ organizer: v.toLowerCase() })
            }}
          />
        </div>
        <div className='flex flex-wrap items-center justify-between gap-3 text-[13px] text-ash'>
          <p>
            <span data-testid='process-count' className='font-medium text-silver'>
              {formatNumber(rows.length)} process{rows.length === 1 ? '' : 'es'}
            </span>
            {filtered
              ? ` match, of ${formatNumber(stats.processes)} on the registry.`
              : ' on the registry, newest first.'}
            <Explain className='ml-1'>
              Phases combine the on-chain status with the clock: a process stays Ready after its end time until someone
              ends it or publishes results, which is shown as Voting closed.
            </Explain>
          </p>
          {filtered ? (
            <Button size='sm' variant='subtle' onClick={() => navigate(paths.processes(), { replace: true })}>
              Clear filters
            </Button>
          ) : null}
        </div>
      </Card>

      <Card flush className='overflow-hidden'>
        <DataTable
          data={rows}
          columns={columns}
          loading={loading}
          getRowId={(r) => r.id}
          sorting={sorting}
          onSortingChange={setSorting}
          onRowClick={(r) => navigate(paths.process(r.id))}
          virtualized={rows.length > 50}
          maxHeight={rows.length > 50 ? 640 : 100_000}
          empty={
            filtered ? (
              <EmptyState
                title='No process matches these filters'
                description='Widen the phase, key mode or census, or clear the search.'
                action={
                  <Button size='sm' variant='ghost' onClick={() => navigate(paths.processes(), { replace: true })}>
                    Clear filters
                  </Button>
                }
              />
            ) : (
              <EmptyState
                title='No processes yet'
                description='When an organizer calls newProcess on the registry, the process appears here with its phase, key mode, census and progress.'
              />
            )
          }
        />
      </Card>
    </Stack>
  )
}
