import { useMemo, useState } from 'react'
import { Link } from 'react-router-dom'
import { EmptyState, Input, Pagination } from '~kit'
import { formatVoteId } from '~protocol/blob'
import { paths } from '~routes/paths'

const PAGE = 60

/** The vote ids a transition inserted, paged, each linking to its lookup. */
export function VoteIdList({ processId, voteIds }: { processId: string; voteIds: bigint[] }) {
  const [page, setPage] = useState(0)
  const [filter, setFilter] = useState('')
  const q = filter.trim().toLowerCase()
  const ids = useMemo(() => voteIds.map(formatVoteId), [voteIds])
  const shown = useMemo(() => (q ? ids.filter((id) => id.includes(q)) : ids), [ids, q])
  const pageCount = Math.max(1, Math.ceil(shown.length / PAGE))
  const current = Math.min(page, pageCount - 1)

  return (
    <div className='flex flex-col gap-3'>
      <Input
        size='sm'
        mono
        label='Find a vote id in this transition'
        placeholder='0x8…'
        value={filter}
        onChange={(e) => {
          setFilter(e.target.value)
          setPage(0)
        }}
        wrapperClassName='max-w-sm'
      />
      {shown.length === 0 ? (
        <EmptyState
          compact
          title={ids.length === 0 ? 'No vote ids' : 'No match'}
          description={
            ids.length === 0 ? 'This batch inserted no vote id.' : 'No vote id of this transition contains that text.'
          }
        />
      ) : (
        <ul className='grid grid-cols-1 gap-x-4 gap-y-1 sm:grid-cols-2 lg:grid-cols-3' data-testid='vote-id-list'>
          {shown.slice(current * PAGE, (current + 1) * PAGE).map((id) => (
            <li key={id}>
              <Link
                to={paths.vote(processId, id)}
                className='font-mono text-[12px] text-silver tnum transition-colors hover:text-emerald'
              >
                {id}
              </Link>
            </li>
          ))}
        </ul>
      )}
      {pageCount > 1 ? (
        <Pagination page={current} pageCount={pageCount} onPageChange={setPage} pageSize={PAGE} total={shown.length} />
      ) : null}
    </div>
  )
}
