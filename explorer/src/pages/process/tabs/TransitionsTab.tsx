import { useEffect, useMemo, useState } from 'react'
import type { SortingState } from '@tanstack/react-table'
import { Link } from 'react-router-dom'
import { CheckMark, Explain, NativeAmount, Timestamp, TxLink } from '~components'
import { useDataSource } from '~data/context'
import type { ProcessView } from '~data/hooks'
import type { TransitionRow } from '~indexer/selectors'
import { Address, BlockCell, Card, CardHeader, DataTable, EmptyState, Hash, Panel, type AnyColumnDef } from '~kit'
import { cn } from '~lib/cn'
import { formatNumber } from '~lib/format'
import { paths } from '~routes/paths'

function columns(pid: string): AnyColumnDef<TransitionRow>[] {
  return [
    {
      id: 'index',
      header: '#',
      accessorKey: 'index',
      cell: ({ row }) => (
        <Link
          to={paths.transition(pid, row.original.index)}
          onClick={(e) => e.stopPropagation()}
          className='font-mono text-emerald hover:underline'
        >
          #{row.original.index}
        </Link>
      ),
      meta: { width: '64px', headerTooltip: 'Position among the process’s transitions, from 0.' },
    },
    {
      id: 'block',
      header: 'Block',
      accessorKey: 'block',
      cell: ({ row }) => <BlockCell block={row.original.block} />,
      meta: { width: '110px' },
    },
    {
      id: 'time',
      header: 'Time',
      accessorFn: (r) => r.timestamp ?? 0,
      cell: ({ row }) => <Timestamp value={row.original.timestamp} className='text-[12px]' />,
      meta: { width: '110px' },
    },
    {
      id: 'tx',
      header: 'Transaction',
      accessorFn: (r) => r.tx ?? '',
      cell: ({ row }) => (row.original.tx ? <TxLink hash={row.original.tx} chars={4} /> : '—'),
      meta: { width: '170px' },
    },
    {
      id: 'sender',
      header: 'Sender',
      accessorKey: 'sender',
      cell: ({ row }) => <Address value={row.original.sender} explorer={false} />,
      meta: {
        headerTooltip:
          'The sequencer that sent the settlement. Settlement is permissionless; the proof authenticates it.',
      },
    },
    {
      id: 'newVoters',
      header: 'New voters',
      accessorKey: 'newVoters',
      meta: { numeric: true, width: '100px', headerTooltip: 'Ballot slots written for the first time.' },
    },
    {
      id: 'overwrites',
      header: 'Overwrites',
      accessorKey: 'overwrites',
      meta: { numeric: true, width: '100px', headerTooltip: 'Votes that replaced an earlier vote of the same voter.' },
    },
    {
      id: 'blobs',
      header: 'Blobs',
      accessorKey: 'nBlobs',
      meta: { numeric: true, width: '70px' },
    },
    {
      id: 'gas',
      header: 'Gas',
      accessorFn: (r) => (r.gasUsed == null ? -1 : Number(r.gasUsed)),
      cell: ({ row }) => (row.original.gasUsed == null ? '…' : formatNumber(row.original.gasUsed)),
      meta: { numeric: true, width: '100px' },
    },
    {
      id: 'fee',
      header: 'Fee',
      accessorFn: (r) => (r.fee == null ? -1 : Number(r.fee)),
      cell: ({ row }) => <NativeAmount wei={row.original.fee} digits={6} className='text-[12px]' />,
      meta: {
        numeric: true,
        width: '130px',
        headerTooltip: 'Execution gas plus blob gas, at the prices the transaction paid.',
      },
    },
  ]
}

export function TransitionsTab({ view }: { view: ProcessView }) {
  const source = useDataSource()
  const { process: p, transitions, rootChain } = view
  const cols = useMemo(() => columns(p.id), [p.id])
  const [sorting, setSorting] = useState<SortingState>([])

  // Gas and fees need each settlement's receipt: ask for this process's first.
  const missing = transitions.filter((t) => t.tx && t.gasUsed == null).map((t) => t.tx)
  const missingKey = missing.join(',')
  useEffect(() => {
    if (missingKey) source.ensureTxDetails(missingKey.split(',') as `0x${string}`[])
  }, [source, missingKey])

  const totals = transitions.reduce(
    (acc, t) => ({
      gas: acc.gas + (t.gasUsed ?? 0n),
      fee: acc.fee + (t.fee ?? 0n),
      blobs: acc.blobs + t.nBlobs,
      ballots: acc.ballots + t.votes,
      known: acc.known && t.fee != null,
    }),
    { gas: 0n, fee: 0n, blobs: 0, ballots: 0, known: true }
  )

  const headText =
    rootChain.headMatches == null
      ? 'the registry’s current root is not read yet'
      : rootChain.headMatches
        ? 'the last root is the registry’s current root'
        : 'the last root is not the registry’s current root'

  return (
    <div data-testid='tab-transitions' className='flex flex-col gap-6'>
      <Panel
        title='State-root chain'
        label='Root continuity'
        description='Every transition must start from the root the previous one ended at, the first from the genesis root the registry computed at creation. The registry enforces it on-chain; here it is recomputed from the events.'
      >
        <p data-testid='transition-summary' className='mb-4 flex flex-wrap items-center gap-2 text-[13px] text-silver'>
          <CheckMark
            state={
              rootChain.gaps > 0 || rootChain.headMatches === false
                ? 'fail'
                : rootChain.headMatches == null || rootChain.genesisRoot == null
                  ? 'unknown'
                  : 'pass'
            }
          />
          <span>
            {formatNumber(transitions.length)} transition{transitions.length === 1 ? '' : 's'} ·{' '}
            {rootChain.gaps === 0
              ? 'the chain is continuous'
              : `${rootChain.gaps} gap${rootChain.gaps === 1 ? '' : 's'} in the chain`}{' '}
            · {headText}
          </span>
        </p>
        <RootChainList view={view} />
      </Panel>

      <Card flush className='overflow-hidden'>
        <CardHeader
          title='Transitions'
          label='Settled batches'
          description='Each row is one batch a sequencer proved and settled with submitStateTransition. Open one to see its decoded public values, its blobs and every check the contract ran.'
          actions={
            transitions.length > 0 ? (
              <span className='flex flex-wrap gap-x-4 gap-y-1 font-mono text-[12px] text-ash tnum'>
                <span>{formatNumber(totals.ballots)} ballots</span>
                <span>{formatNumber(totals.blobs)} blobs</span>
                <span>{formatNumber(totals.gas)} gas</span>
                <span>
                  {totals.known ? '' : '≥ '}
                  <NativeAmount wei={totals.fee} />
                </span>
              </span>
            ) : null
          }
        />
        <DataTable
          data={transitions}
          columns={cols}
          getRowId={(r) => r.key}
          sorting={sorting}
          onSortingChange={setSorting}
          virtualized={transitions.length > 50}
          maxHeight={transitions.length > 15 ? 600 : 100_000}
          empty={
            <EmptyState
              title='No transitions yet'
              description='When a sequencer settles the first batch of votes, it appears here with its block, blobs and fee.'
            />
          }
        />
      </Card>
    </div>
  )
}

function RootChainList({ view }: { view: ProcessView }) {
  const { process: p, rootChain } = view
  const latest = p.state?.latestStateRoot ?? null
  return (
    <ol
      className='max-h-[420px] overflow-y-auto rounded-sm border border-charcoal scroll-slim'
      aria-label='State roots'
    >
      <li className='flex flex-wrap items-center gap-x-3 gap-y-1 border-b border-charcoal/60 px-3 py-2 text-[13px]'>
        <span className='w-24 shrink-0 text-pewter'>
          Genesis
          <Explain className='ml-1'>
            The root of the process’s state tree before any vote: the registry derives it from the process id, ballot
            mode, encryption key, census origin and ballot VK hash.
          </Explain>
        </span>
        {rootChain.genesisRoot ? (
          <Hash value={rootChain.genesisRoot} chars={10} />
        ) : (
          <span className='text-ash'>not read yet</span>
        )}
      </li>
      {rootChain.links.map((l) => (
        <li
          key={l.index}
          className={cn(
            'flex flex-wrap items-center gap-x-3 gap-y-1 border-b border-charcoal/60 px-3 py-2 text-[13px]',
            l.continuous === false && 'bg-red/5'
          )}
        >
          <Link to={paths.transition(p.id, l.index)} className='w-24 shrink-0 font-mono text-emerald hover:underline'>
            #{l.index}
          </Link>
          <CheckMark state={l.continuous == null ? 'unknown' : l.continuous ? 'pass' : 'fail'} />
          {l.continuous === false ? (
            <span className='inline-flex flex-wrap items-center gap-1 text-red'>
              starts from <Hash value={l.rootBefore} chars={6} />, expected{' '}
              {l.expectedBefore ? <Hash value={l.expectedBefore} chars={6} /> : '…'}
            </span>
          ) : null}
          <span className='text-ash'>→</span>
          <Hash value={l.rootAfter} chars={10} />
        </li>
      ))}
      <li className='flex flex-wrap items-center gap-x-3 gap-y-1 px-3 py-2 text-[13px]'>
        <span className='w-24 shrink-0 text-pewter'>
          Registry now
          <Explain className='ml-1'>
            latestStateRoot in getProcess: the root the next transition must start from.
          </Explain>
        </span>
        <CheckMark state={rootChain.headMatches == null ? 'unknown' : rootChain.headMatches ? 'pass' : 'fail'} />
        {latest ? <Hash value={latest} chars={10} /> : <span className='text-ash'>not read yet</span>}
      </li>
    </ol>
  )
}
