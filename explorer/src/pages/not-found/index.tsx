import { Link, useLocation } from 'react-router-dom'
import { buttonClasses, EmptyState, SectionHeader, Stack } from '~kit'
import { paths } from '~routes/paths'

export function NotFoundPage() {
  const { pathname } = useLocation()
  return (
    <Stack data-testid='page-not-found'>
      <SectionHeader size='page' label='404' title='No such page' description={`Nothing is routed at ${pathname}.`} />
      <EmptyState
        title='Try the overview'
        description='Or search for a process id, vote id, transaction, address or block in the bar above.'
        action={
          <Link to={paths.home()} className={buttonClasses('ghost', 'sm')}>
            Go to the overview
          </Link>
        }
      />
    </Stack>
  )
}
