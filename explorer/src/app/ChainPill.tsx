import { useRuntimeConfig } from '~config/config-context'
import { useIndexer } from '~data/hooks'
import { Badge, Tooltip } from '~kit'
import { cn } from '~lib/cn'

/** Chain identity in the top bar: network name, live head and indexing state. */
export function ChainPill({ className }: { className?: string }) {
  const config = useRuntimeConfig()
  const { status } = useIndexer()
  const mismatch = status.chainMismatch != null
  const error = status.phase === 'error'
  const syncing = status.scanning || status.phase === 'loading' || status.phase === 'idle'
  const head = status.headBlock > 0 && status.lastPollAt != null ? status.headBlock : null
  const lag = head != null ? Math.max(0, head - status.lastBlock) : 0

  const dot = mismatch || error ? 'bg-red' : syncing ? 'bg-amber' : 'animate-skeleton bg-emerald'
  const title = mismatch
    ? `The RPC is on chain ${status.chainMismatch!.actual}, not ${status.chainMismatch!.expected}`
    : error
      ? (status.errors[status.errors.length - 1]?.message ?? 'The RPC is not answering')
      : syncing
        ? `Indexing: ${Math.round(status.progress * 100)}%`
        : `Indexed to block ${status.lastBlock}${lag > 0 ? ` (${lag} behind the head)` : ''}`

  return (
    <div
      className={cn('flex items-center gap-2.5 rounded-pill border border-charcoal bg-carbon px-3 py-1', className)}
      data-testid='chain-pill'
    >
      <Tooltip content={`Chain id ${config.chainId}`}>
        <span className='flex items-center gap-1.5 text-[11px] font-medium whitespace-nowrap text-silver'>
          <span className={cn('h-1.5 w-1.5 rounded-full', dot)} />
          {config.networkName}
        </span>
      </Tooltip>
      <span aria-hidden='true' className='h-3 w-px bg-charcoal' />
      <Tooltip content={title}>
        <span className='font-mono text-[11px] tnum text-pewter'>
          {head == null ? '—' : `#${head}`}
          {syncing && head != null ? (
            <span className='ml-1 text-amber'>{Math.round(status.progress * 100)}%</span>
          ) : null}
        </span>
      </Tooltip>
      {config.demo ? (
        <Badge tone='warn' size='sm' className='ml-0.5'>
          demo
        </Badge>
      ) : null}
    </div>
  )
}
