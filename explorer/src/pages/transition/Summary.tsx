import { Link } from 'react-router-dom'
import { Explain, NativeAmount, ProcessIdLink, ProcessPhaseBadge, Timestamp, TxLink } from '~components'
import type { DecodedTransitionBlobs } from '~data/queries'
import type { TransitionDetail } from '~indexer/selectors'
import { processPhase } from '~indexer/selectors'
import { Address, BlockCell, Card, Hash, KeyValue, Skeleton, type KeyValueItem } from '~kit'
import { formatBytes, formatGwei, formatNumber } from '~lib/format'
import { paths } from '~routes/paths'
import type { UseQueryResult } from '@tanstack/react-query'

/** Gnosis base fees sit at a few wei, which "0.00 gwei" would hide. */
const gasPrice = (wei: bigint) => (wei < 10_000_000n ? `${formatNumber(wei)} wei` : `${formatGwei(wei)} gwei`)

const plural = (n: number, one: string, many = `${one}s`) => `${formatNumber(n)} ${n === 1 ? one : many}`

function blobsLine(blobs: UseQueryResult<DecodedTransitionBlobs>): string {
  if (blobs.data?.decoded) {
    const d = blobs.data.decoded
    return `blobs: ${plural(d.voteIds.length, 'vote id')}, ${plural(d.updates.length, 'slot update')}`
  }
  if (blobs.data?.decodeError) return `blobs: ${blobs.data.decodeError}`
  if (blobs.error) return 'blobs: not available'
  if (blobs.isLoading) return 'blobs: loading…'
  return 'blobs: waiting for the transaction'
}

function Label({ children, explain }: { children: string; explain?: string }) {
  return (
    <span className='inline-flex items-center gap-1'>
      {children}
      {explain ? <Explain>{explain}</Explain> : null}
    </span>
  )
}

/** The facts of one transition: where it sits, who settled it, what it changed and what it cost. */
export function TransitionSummary({
  detail,
  blobs,
  now,
  total,
}: {
  detail: TransitionDetail
  blobs: UseQueryResult<DecodedTransitionBlobs>
  now: number | null
  total: number
}) {
  const { transition: t, row, tx, process } = detail
  const passed = detail.checks.filter((c) => c.state === 'pass').length
  const failed = detail.checks.filter((c) => c.state === 'fail').length
  const tone = failed ? 'text-red' : passed === detail.checks.length ? 'text-emerald' : 'text-silver'
  const execFee = tx ? tx.gasUsed * tx.effectiveGasPrice : null
  const blobFee = tx?.blobGasUsed != null && tx.blobGasPrice != null ? tx.blobGasUsed * tx.blobGasPrice : null
  const pending = <Skeleton className='inline-block h-3 w-20' />

  const items: KeyValueItem[] = [
    {
      label: <Label>Process</Label>,
      value: (
        <span className='inline-flex items-center gap-2'>
          <ProcessIdLink id={process.id} chars={10} />
          <ProcessPhaseBadge phase={processPhase(process, now)} size='sm' />
        </span>
      ),
    },
    {
      label: (
        <Label explain='Transitions are numbered from 0 in the order they settled, as the sequencer API numbers them.'>
          Position
        </Label>
      ),
      value: `#${t.index} of ${formatNumber(total)}`,
      mono: true,
    },
    {
      label: <Label>Block</Label>,
      value: (
        <span className='inline-flex items-center gap-2'>
          <BlockCell block={t.block} />
          <Timestamp value={row.timestamp} className='text-ash' />
        </span>
      ),
    },
    {
      label: <Label>Transaction</Label>,
      value: t.tx ? <TxLink hash={t.tx} chars={10} /> : '—',
    },
    {
      label: (
        <Label explain='Settlement is permissionless: any sequencer may send a proven batch, and the registry checks it the same way whoever sends it.'>
          Settled by
        </Label>
      ),
      value: <Address value={t.sender} />,
    },
    {
      label: <Label explain='The status the settlement transaction ended with.'>Status</Label>,
      value: tx ? (tx.status === 'success' ? 'Success' : 'Reverted') : pending,
    },
    {
      label: (
        <Label explain="The state tree's root before this batch: the process root the registry held, which the proof had to start from.">
          Root before
        </Label>
      ),
      value: <Hash value={t.rootBefore} chars={10} />,
    },
    {
      label: (
        <Label explain='The root after this batch. The registry stored it as the process root; the next transition starts here.'>
          Root after
        </Label>
      ),
      value: <Hash value={t.rootAfter} chars={10} />,
    },
    {
      label: (
        <Label explain='New voters wrote a slot for the first time; overwrites replaced an earlier vote of the same voter. A first write is public, since refreshes only touch occupied slots; which occupied slots were overwritten and which were only refreshed is not.'>
          Votes
        </Label>
      ),
      value: `${plural(t.newVoters, 'new voter')} · ${plural(t.overwrites, 'overwrite')}`,
    },
    {
      label: <Label explain='The process counters the registry emitted after this batch.'>Totals after</Label>,
      value: `${plural(t.votersCount, 'voter')} · ${plural(t.overwrittenVotesCount, 'overwrite')}`,
    },
    {
      label: (
        <Label explain='EIP-4844 blobs attached to the transaction. They carry the data to rebuild the state and cost blob gas, not calldata.'>
          Blobs
        </Label>
      ),
      value: `${formatNumber(t.nBlobs)}${tx?.blobGasUsed != null ? ` · ${formatNumber(tx.blobGasUsed)} blob gas` : ''}`,
      mono: true,
    },
    {
      label: (
        <Label explain='Execution gas: the PLONK verification, the checks and the storage writes.'>Gas used</Label>
      ),
      value: tx ? `${formatNumber(tx.gasUsed)} @ ${gasPrice(tx.effectiveGasPrice)}` : pending,
      mono: true,
    },
    {
      label: <Label explain='Execution fee plus blob fee, paid by the sender.'>Fee</Label>,
      value: tx ? <NativeAmount wei={tx.fee} /> : pending,
      hint:
        execFee != null ? (
          <>
            execution <NativeAmount wei={execFee} />
            {blobFee != null ? (
              <>
                {' '}
                · blobs <NativeAmount wei={blobFee} />
              </>
            ) : null}
          </>
        ) : undefined,
    },
    {
      label: (
        <Label explain='submitStateTransition calldata: the process id, 512 bytes of public values, the 768-byte proof and one commitment, evaluation and KZG proof per blob.'>
          Calldata
        </Label>
      ),
      value: tx ? formatBytes(tx.inputSize) : pending,
      mono: true,
    },
  ]

  return (
    <Card data-testid='transition-summary'>
      <p className='text-[13px] text-silver'>
        {plural(row.votes, 'ballot')} · {plural(t.nBlobs, 'blob')} ·{' '}
        <a href='#verify' className={`${tone} hover:underline`}>
          {passed}/{detail.checks.length} checks passed
        </a>{' '}
        · {blobsLine(blobs)}
      </p>
      <KeyValue items={items} columns={2} className='mt-3' />
      <p className='mt-3 text-[12px] text-ash'>
        All transitions of this process are on its{' '}
        <Link to={paths.process(process.id, 'transitions')} className='text-silver hover:text-emerald'>
          transitions tab
        </Link>
        , with the chain of roots from genesis.
      </p>
    </Card>
  )
}
