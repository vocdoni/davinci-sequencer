import { useEffect, useMemo, useState, type FormEvent } from 'react'
import { Link, useNavigate, useSearchParams } from 'react-router-dom'
import { Explain, Timestamp } from '~components'
import { useDataSource } from '~data/context'
import type { ProcessView } from '~data/hooks'
import { useTransitionBlobs } from '~data/queries'
import {
  Button,
  buttonClasses,
  Callout,
  ChevronLeftIcon,
  ChevronRightIcon,
  EmptyState,
  Input,
  Pagination,
  Panel,
  Select,
  SkeletonText,
} from '~kit'
import { cn } from '~lib/cn'
import { formatNumber } from '~lib/format'
import { formatVoteId, parseVoteId } from '~protocol/blob'
import { paths } from '~routes/paths'

const PAGE = 60

export function VotesTab({ view }: { view: ProcessView }) {
  const { process: p, transitions } = view
  const [params, setParams] = useSearchParams()
  const navigate = useNavigate()
  const newest = transitions.length - 1
  const requested = Number.parseInt(params.get('t') ?? '', 10)
  const index = Number.isInteger(requested) && requested >= 0 && requested <= newest ? requested : newest
  const [lookup, setLookup] = useState('')
  const lookupId = parseVoteId(lookup)
  const lookupInvalid = lookup.trim() !== '' && lookupId == null

  const select = (i: number) => {
    const next = new URLSearchParams(params)
    next.set('t', String(i))
    setParams(next, { replace: true })
  }

  const submit = (e: FormEvent) => {
    e.preventDefault()
    if (lookupId != null) navigate(paths.vote(p.id, formatVoteId(lookupId)))
  }

  return (
    <div data-testid='tab-votes' className='flex flex-col gap-6'>
      <Panel
        title='Find a vote'
        label='Vote lookup'
        description='Your voting app gives you a vote id when you cast. The vote page finds the batch whose blob lists it and checks the sequencer’s tracker proof against an on-chain root.'
      >
        <form onSubmit={submit} className='flex flex-col gap-2 sm:flex-row sm:items-start' role='search'>
          <Input
            aria-label='Vote id'
            placeholder='Vote id: 0x followed by 16 hex digits, or decimal'
            mono
            value={lookup}
            onChange={(e) => setLookup(e.target.value)}
            error={lookupInvalid ? 'A vote id is a number from 2^63 to 2^64 − 1, in hex (0x…) or decimal.' : undefined}
            wrapperClassName='flex-1'
          />
          <Button type='submit' variant='primary' disabled={lookupId == null}>
            Look up this vote
          </Button>
        </form>
      </Panel>

      <Panel
        title='Vote ids per transition'
        label='From the blobs'
        description='Each transition publishes its data in EIP-4844 blobs: the vote ids of the batch, then every ballot slot it wrote with the new ciphertexts, then the new encrypted tally. An overwrite and a silent refresh look the same, so nobody can tell a revote from a routine re-randomization. A slot’s first write is public, and with a Merkle census the slot follows from the address, so who voted and when is public.'
      >
        {transitions.length === 0 ? (
          <EmptyState
            compact
            title='No transitions yet'
            description='Vote ids appear here once a sequencer settles the first batch of this process.'
          />
        ) : (
          <div className='flex flex-col gap-4'>
            <div className='flex flex-wrap items-end gap-2'>
              <Button
                size='icon'
                variant='subtle'
                aria-label='Older transition'
                disabled={index <= 0}
                onClick={() => select(index - 1)}
              >
                <ChevronLeftIcon />
              </Button>
              <Select
                aria-label='Transition'
                wrapperClassName='min-w-0 flex-1 sm:max-w-md'
                value={String(index)}
                onChange={(e) => select(Number(e.target.value))}
                options={[...transitions].reverse().map((t) => ({
                  value: String(t.index),
                  label: `#${t.index} · ${formatNumber(t.votes)} vote${t.votes === 1 ? '' : 's'} · ${t.nBlobs} blob${t.nBlobs === 1 ? '' : 's'}`,
                }))}
              />
              <Button
                size='icon'
                variant='subtle'
                aria-label='Newer transition'
                disabled={index >= newest}
                onClick={() => select(index + 1)}
              >
                <ChevronRightIcon />
              </Button>
              <Link to={paths.transition(p.id, index)} className={buttonClasses('ghost', 'md', 'ml-auto')}>
                Open transition #{index}
              </Link>
            </div>
            <TransitionVotes key={index} view={view} index={index} highlight={lookupId} />
          </div>
        )}
      </Panel>
    </div>
  )
}

function TransitionVotes({ view, index, highlight }: { view: ProcessView; index: number; highlight: bigint | null }) {
  const { process: p } = view
  const row = view.transitions[index]!
  const source = useDataSource()
  const blobs = useTransitionBlobs(p.id, index)
  const [page, setPage] = useState(0)

  useEffect(() => {
    if (row.tx) source.ensureTxDetails([row.tx])
  }, [source, row.tx])

  const ids = useMemo(() => blobs.data?.decoded?.voteIds ?? [], [blobs.data])
  const hit = highlight != null ? ids.indexOf(highlight) : -1
  useEffect(() => {
    if (hit >= 0) setPage(Math.floor(hit / PAGE))
  }, [hit])

  const waitingForTx = blobs.status === 'pending' && blobs.fetchStatus === 'idle'
  const header = (
    <p className='text-[13px] text-ash'>
      Settled <Timestamp value={row.timestamp} /> in block {formatNumber(row.block)}: {formatNumber(row.newVoters)} new
      voter{row.newVoters === 1 ? '' : 's'} and {formatNumber(row.overwrites)} overwrite
      {row.overwrites === 1 ? '' : 's'}, in {row.nBlobs} blob{row.nBlobs === 1 ? '' : 's'}.
    </p>
  )

  if (waitingForTx || blobs.isLoading) {
    return (
      <div className='flex flex-col gap-3'>
        {header}
        <p className='text-[13px] text-ash'>
          {waitingForTx
            ? 'Reading the settlement transaction for its blob hashes…'
            : `Fetching ${row.nBlobs} blob${row.nBlobs === 1 ? '' : 's'}…`}
        </p>
        <SkeletonText lines={4} />
      </div>
    )
  }
  if (blobs.error) {
    return (
      <div className='flex flex-col gap-3'>
        {header}
        <Callout tone='warn' title='The blobs could not be fetched'>
          <p>{blobs.error instanceof Error ? blobs.error.message : String(blobs.error)}</p>
          <p className='mt-1'>
            Beacon nodes prune blobs after about 15 days on Gnosis Chain (16384 epochs of 80 s) and about 18 on Ethereum
            mainnet. After that only a sequencer that stored them (or an archive) can serve them; the transaction still
            carries their versioned hashes.
          </p>
        </Callout>
      </div>
    )
  }
  const data = blobs.data
  if (!data) return header
  if (!data.decoded) {
    return (
      <div className='flex flex-col gap-3'>
        {header}
        <Callout tone='danger' title='The blobs did not decode'>
          {data.decodeError}
        </Callout>
      </div>
    )
  }

  const updates = data.decoded.updates.length
  const refreshes = Math.max(0, updates - row.votes)
  const pageCount = Math.ceil(ids.length / PAGE)
  const shown = ids.slice(page * PAGE, (page + 1) * PAGE)

  return (
    <div className='flex flex-col gap-3' data-testid='vote-ids'>
      {header}
      <p className='text-[13px] text-silver'>
        {formatNumber(ids.length)} vote id{ids.length === 1 ? '' : 's'} · {formatNumber(updates)} slot update
        {updates === 1 ? '' : 's'}: {formatNumber(row.votes)} vote{row.votes === 1 ? '' : 's'} plus{' '}
        {formatNumber(refreshes)} silent refresh{refreshes === 1 ? '' : 'es'}
        <Explain className='ml-1'>
          The counts are public (the registry event carries them), and so is a slot’s first write, since refreshes only
          touch occupied slots. Which occupied slots were overwritten and which were only refreshed is not: every batch
          re-randomizes a random sample of occupied slots it did not write, so an overwrite hides among the refreshes.
        </Explain>
      </p>
      <p className='text-xs text-ash'>
        {data.source === 'sequencer'
          ? `Served by the sequencer ${data.sourceUrl} from its archive, tied to this transition by position only, not checked against the transaction's blob hashes.`
          : data.source === 'beacon'
            ? data.blobs.some((b) => b.binding === 'beacon-filter')
              ? `From the beacon API ${data.sourceUrl}, which selected each blob by a versioned hash the transaction carries.`
              : `From the beacon API ${data.sourceUrl}; each blob's commitment hashes to a versioned hash the transaction carries.`
            : 'From the demo network.'}
      </p>
      {highlight != null && hit < 0 ? (
        <p className='text-xs text-amber'>This transition does not list {formatVoteId(highlight)}.</p>
      ) : null}
      <ul className='grid grid-cols-1 gap-1 sm:grid-cols-2 lg:grid-cols-4'>
        {shown.map((id) => {
          const hex = formatVoteId(id)
          return (
            <li key={hex}>
              <Link
                to={paths.vote(p.id, hex)}
                className={cn(
                  'block truncate rounded-sm border px-2 py-1 font-mono text-[12px] tnum transition-colors hover:border-emerald hover:text-emerald',
                  id === highlight ? 'border-emerald bg-emerald/10 text-emerald' : 'border-charcoal text-silver'
                )}
              >
                {hex}
              </Link>
            </li>
          )
        })}
      </ul>
      {pageCount > 1 ? (
        <Pagination page={page} pageCount={pageCount} onPageChange={setPage} pageSize={PAGE} total={ids.length} />
      ) : null}
    </div>
  )
}
