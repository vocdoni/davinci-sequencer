import { Link } from 'react-router-dom'
import { ProcessIdLink, Timestamp, TxLink } from '~components'
import { useActivityFeed, useIndexer } from '~data/hooks'
import type { FeedKind } from '~indexer/selectors'
import { Badge, EmptyState, Panel, SkeletonText, type BadgeTone } from '~kit'

const KIND: Record<FeedKind, { label: string; tone: BadgeTone }> = {
  created: { label: 'created', tone: 'accent' },
  transition: { label: 'transition', tone: 'ok' },
  results: { label: 'results', tone: 'accent' },
  status: { label: 'status', tone: 'neutral' },
  decryption: { label: 'decryption', tone: 'warn' },
  census: { label: 'census', tone: 'neutral' },
  duration: { label: 'duration', tone: 'neutral' },
  'max-voters': { label: 'limit', tone: 'neutral' },
}

/** The newest registry events, network-wide. */
export function ActivityPanel({ limit = 12 }: { limit?: number }) {
  const feed = useActivityFeed(limit)
  const { loading } = useIndexer()

  return (
    <Panel
      title='Recent activity'
      label='Registry events'
      description='Every process creation, settled batch, status change and result, newest first.'
      bodyClassName='p-0'
    >
      {loading ? (
        <SkeletonText lines={8} className='p-5' />
      ) : feed.length === 0 ? (
        <EmptyState
          compact
          title='No activity yet'
          description='Process creations, the batches sequencers settle, status changes and results will be listed here as the registry emits them.'
        />
      ) : (
        <ul className='divide-y divide-charcoal/60'>
          {feed.map((e) => (
            <li key={e.key} className='flex flex-wrap items-center gap-x-3 gap-y-1 px-5 py-2.5 text-[13px]'>
              <Badge tone={KIND[e.kind].tone} size='sm' className='w-[74px] justify-center'>
                {KIND[e.kind].label}
              </Badge>
              <Link to={e.href} className='min-w-0 flex-1 truncate text-silver hover:text-emerald'>
                {e.label}
              </Link>
              <span className='flex items-center gap-3 max-sm:w-full max-sm:justify-between max-sm:pl-[86px]'>
                <ProcessIdLink id={e.processId} chars={6} className='text-ash' />
                {e.tx ? <TxLink hash={e.tx} chars={4} className='max-md:hidden' /> : null}
                <Timestamp value={e.timestamp} className='text-right text-xs text-ash sm:w-[88px]' />
              </span>
            </li>
          ))}
        </ul>
      )}
    </Panel>
  )
}
