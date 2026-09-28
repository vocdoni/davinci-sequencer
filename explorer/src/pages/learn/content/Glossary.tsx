import { useState } from 'react'
import { Link, useLocation } from 'react-router-dom'
import { HashLink } from '~components/HashLink'
import { EmptyState, Input } from '~kit'
import { cn } from '~lib/cn'
import { filterGlossary } from '../glossary'
import { Rich } from '../prose'

export function Glossary() {
  const [query, setQuery] = useState('')
  const entries = filterGlossary(query)
  const { hash } = useLocation()
  const target = hash.startsWith('#term-') ? decodeURIComponent(hash.slice('#term-'.length)) : null
  return (
    <>
      <Input
        type='search'
        value={query}
        onChange={(e) => setQuery(e.target.value)}
        placeholder='Filter the glossary'
        aria-label='Filter the glossary'
        wrapperClassName='max-w-sm'
      />
      {entries.length === 0 ? (
        <EmptyState compact title='No term matches' description='Try a shorter word, or clear the filter.' />
      ) : (
        <dl className='mt-6 flex flex-col divide-y divide-charcoal' data-testid='glossary'>
          {entries.map((e) => (
            <div
              key={e.id}
              id={`term-${e.id}`}
              aria-current={e.id === target ? 'true' : undefined}
              className={cn('scroll-mt-20 py-4', e.id === target && '-mx-3 rounded-md bg-emerald/[0.06] px-3')}
            >
              <dt className='text-[14px] font-semibold text-ghost'>
                <HashLink id={`term-${e.id}`} className='hover:text-emerald'>
                  {e.term}
                </HashLink>
              </dt>
              <dd className='mt-1 text-[14px] leading-[1.7] text-pewter'>
                <Rich text={e.text} />
                {e.see ? (
                  <>
                    {' '}
                    <Link
                      to={e.see.to}
                      className='whitespace-nowrap text-[13px] text-ash underline-offset-3 hover:text-emerald hover:underline'
                    >
                      {e.see.label} →
                    </Link>
                  </>
                ) : null}
              </dd>
            </div>
          ))}
        </dl>
      )}
    </>
  )
}
