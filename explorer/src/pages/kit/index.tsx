import { useMemo, useState } from 'react'
import { CensusOriginBadge, CheckMark, Explain, KeyModeBadge, NativeAmount, ProcessPhaseBadge } from '~components'
import {
  Address,
  Badge,
  BlockCell,
  Button,
  Callout,
  Card,
  CardBody,
  CardHeader,
  DataTable,
  Dialog,
  EmptyState,
  Hash,
  Input,
  KeyValue,
  Pagination,
  Panel,
  ProgressBar,
  SectionHeader,
  Select,
  Skeleton,
  Stack,
  Stat,
  StatCell,
  StatRow,
  Tabs,
  Timeline,
  TimelineRow,
  Toggle,
  TxCell,
  type AnyColumnDef,
} from '~kit'
import { Donut, Sparkline, StackedBars } from '~kit/charts'
import { useTheme } from '~theme/theme-context'

const SAMPLE_ADDRESS = '0x3cde68c39e26ecf94bd029b6ed3b9f945441daf3'
const SAMPLE_PID = '0x42fc20654efd78c6887ff0bd1cc50c9ec1dab58980c5bb9300000000000004'
const SAMPLE_TX = `0x${'9f'.repeat(32)}`

interface Row {
  index: number
  block: number
  votes: number
  blobs: number
}

/**
 * Every primitive and chart on one page, in the active theme. The design
 * review surface: switch the theme in the top bar and check both.
 */
export function KitPage() {
  const { resolved } = useTheme()
  const [dialogOpen, setDialogOpen] = useState(false)
  const [toggle, setToggle] = useState(true)
  const [page, setPage] = useState(1)
  const rows = useMemo<Row[]>(
    () =>
      Array.from({ length: 300 }, (_, i) => ({
        index: i,
        block: 48_476_748 + i * 37,
        votes: (i * 7) % 41,
        blobs: 1 + (i % 3),
      })),
    []
  )
  const columns = useMemo<AnyColumnDef<Row>[]>(
    () => [
      { id: 'index', header: '#', accessorKey: 'index', meta: { numeric: true, width: '70px' } },
      {
        id: 'block',
        header: 'Block',
        accessorKey: 'block',
        cell: ({ row }) => <BlockCell block={row.original.block} />,
      },
      { id: 'votes', header: 'Votes', accessorKey: 'votes', meta: { numeric: true } },
      { id: 'blobs', header: 'Blobs', accessorKey: 'blobs', meta: { numeric: true } },
    ],
    []
  )
  const activity = useMemo(
    () =>
      Array.from({ length: 30 }, (_, i) => ({
        label: String(i + 1),
        values: { votes: (i * 13) % 50, overwrites: (i * 5) % 9 },
      })),
    []
  )

  return (
    <Stack data-testid='page-kit'>
      <SectionHeader
        size='page'
        label='Design kit'
        title='Primitives and charts'
        description={`Everything the pages are built from, in the ${resolved} theme.`}
      />

      <Panel title='Buttons and badges'>
        <div className='flex flex-wrap items-center gap-3'>
          <Button variant='primary'>Primary</Button>
          <Button variant='ghost'>Ghost</Button>
          <Button>Secondary</Button>
          <Button variant='subtle'>Subtle</Button>
          <Button variant='danger'>Danger</Button>
          <Button loading>Loading</Button>
        </div>
        <div className='mt-4 flex flex-wrap items-center gap-2'>
          <Badge tone='ok' dot>
            live
          </Badge>
          <Badge tone='accent'>accent</Badge>
          <Badge tone='warn'>warning</Badge>
          <Badge tone='danger'>danger</Badge>
          <Badge>neutral</Badge>
          <ProcessPhaseBadge phase='open' />
          <ProcessPhaseBadge phase='closed' />
          <ProcessPhaseBadge phase='results' />
          <KeyModeBadge mode='dkg-locked' />
          <CensusOriginBadge origin='csp' />
          <CheckMark state='pass' />
          <CheckMark state='fail' />
          <CheckMark state='unknown' />
          <Explain>Every value can carry a plain-words explanation.</Explain>
        </div>
      </Panel>

      <StatRow>
        <StatCell label='Processes' value='12' mono hint='3 open' />
        <StatCell label='Ballots' value='4,210' mono tone='accent' />
        <StatCell label='Blobs' value='96' mono />
        <StatCell label='Loading' value='' loading />
      </StatRow>

      <div className='grid gap-6 lg:grid-cols-2'>
        <Card flush>
          <CardHeader label='Record' title='KeyValue' description='The raw-record view on every detail page.' />
          <CardBody>
            <KeyValue
              items={[
                { label: 'Registry', value: <Address value={SAMPLE_ADDRESS} /> },
                { label: 'Process', value: <Hash value={SAMPLE_PID} /> },
                { label: 'Transaction', value: <TxCell hash={SAMPLE_TX} copy /> },
                { label: 'Fee', value: <NativeAmount wei={1_234_567_000_000_000n} /> },
                { label: 'Voters', value: '1,024', mono: true, hint: 'distinct slots written' },
              ]}
            />
          </CardBody>
        </Card>
        <Stack>
          <Callout tone='info' title='Info'>
            A neutral note with an explanation.
          </Callout>
          <Callout tone='ok' title='Verified'>
            Every pin matches a known release.
          </Callout>
          <Callout tone='warn' title='Warning'>
            The beacon pruned this blob; trying the sequencer.
          </Callout>
          <Callout tone='danger' title='Mismatch'>
            The RPC reports another chain.
          </Callout>
        </Stack>
      </div>

      <Panel title='Charts' description='Colours follow the theme through CSS variables.'>
        <StackedBars
          data={activity}
          series={[
            { key: 'votes', label: 'votes' },
            { key: 'overwrites', label: 'overwrites' },
          ]}
          height={180}
        />
        <div className='mt-6 grid gap-6 sm:grid-cols-2'>
          <Donut
            slices={[
              { label: 'open', value: 3 },
              { label: 'results', value: 5 },
              { label: 'ended', value: 2 },
            ]}
            centerValue={10}
            centerLabel='processes'
          />
          <div className='flex items-center'>
            <Sparkline values={[3, 8, 5, 12, 9, 15, 11, 18]} area />
          </div>
        </div>
      </Panel>

      <Card flush>
        <CardHeader title='DataTable' description='300 rows, virtualised.' />
        <DataTable data={rows} columns={columns} virtualized maxHeight={280} />
        <div className='border-t border-charcoal px-5 py-3'>
          <Pagination page={page} pageCount={12} onPageChange={setPage} pageSize={25} total={300} />
        </div>
      </Card>

      <Panel title='Inputs, progress, timeline, tabs'>
        <div className='grid gap-4 md:grid-cols-3'>
          <Input label='Process id' mono placeholder='0x…' />
          <Select
            label='Status'
            options={[
              { value: 'all', label: 'All' },
              { value: 'open', label: 'Open' },
            ]}
          />
          <Toggle checked={toggle} onChange={setToggle} label='Only mine' hint='Toggle' />
        </div>
        <ProgressBar className='mt-6 max-w-md' value={620} total={1000} label='voters' />
        <Timeline className='mt-6'>
          <TimelineRow title='Created' meta='#48476748' tone='ok' />
          <TimelineRow title='Transition #0' meta='#48477140' tone='ok' />
          <TimelineRow title='Results' meta='—' tone='muted' last />
        </Timeline>
        <div className='mt-6'>
          <Tabs
            items={[
              { value: 'a', label: 'Overview', content: <p className='text-[13px] text-ash'>Tab one.</p> },
              { value: 'b', label: 'Transitions', meta: 12, content: <p className='text-[13px] text-ash'>Tab two.</p> },
            ]}
          />
        </div>
        <div className='mt-6 flex items-center gap-3'>
          <Skeleton className='h-4 w-40' />
          <Button onClick={() => setDialogOpen(true)}>Open dialog</Button>
          <Dialog open={dialogOpen} onOpenChange={setDialogOpen} title='Dialog' description='A modal panel.'>
            <p className='text-[13px] text-ash'>Content.</p>
          </Dialog>
        </div>
      </Panel>

      <Card>
        <Stat label='Empty state' value='' />
        <EmptyState
          compact
          title='Nothing here yet'
          description='What a panel shows when the chain has nothing to show.'
        />
      </Card>
    </Stack>
  )
}
