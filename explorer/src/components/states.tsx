import { EmptyState, SkeletonText } from '~kit'
import { useIndexer } from '~data/hooks'

/**
 * What a detail page shows when its entity is not in the store: a skeleton
 * until the indexer has finished its first poll, "not found" after.
 */
export function MissingEntity({ what, id }: { what: string; id?: string }) {
  const { status } = useIndexer()
  // Until the first poll completes the entity may still arrive.
  if (status.phase === 'idle' || status.phase === 'loading' || status.scanning) {
    return <SkeletonText lines={6} className='max-w-2xl' />
  }
  return (
    <EmptyState
      title={`No ${what} found`}
      description={id ? `The registry has no ${what} ${id}.` : `The registry has no such ${what}.`}
    />
  )
}
