import { useState } from 'react'
import { Link, NavLink, useLocation } from 'react-router-dom'
import { useRuntimeConfig } from '~config/config-context'
import { NAV_ITEMS, paths } from '~routes/paths'
import { Button, CloseIcon, MenuIcon, PageContainer } from '~kit'
import { cn } from '~lib/cn'
import { ChainPill } from './ChainPill'
import { GlobalSearch } from './GlobalSearch'
import { ThemeToggle } from './ThemeToggle'

function isActive(pathname: string, match: string): boolean {
  return match === '/' ? pathname === '/' : pathname === match || pathname.startsWith(`${match}/`)
}

/**
 * Sticky top bar: brand, primary nav, global search, chain identity, theme.
 * Below 1024 px the nav folds into a disclosure and the search takes its own
 * row.
 */
export function TopBar() {
  const { pathname } = useLocation()
  const { sequencers } = useRuntimeConfig()
  const [open, setOpen] = useState(false)
  const items = NAV_ITEMS.filter((item) => !item.needsSequencers || sequencers.length > 0)

  const navLink = (item: (typeof items)[number], mobile = false) => (
    <NavLink
      key={item.to}
      to={item.to}
      onClick={mobile ? () => setOpen(false) : undefined}
      aria-current={isActive(pathname, item.match) ? 'page' : undefined}
      className={cn(
        'rounded-sm text-[13px] font-medium transition-colors',
        mobile ? 'px-2 py-2' : 'px-3 py-1.5',
        isActive(pathname, item.match) ? 'text-emerald' : 'text-pewter hover:text-ghost'
      )}
    >
      {item.label}
    </NavLink>
  )

  return (
    <header className='sticky top-0 z-40 border-b border-charcoal bg-obsidian/90 backdrop-blur-md'>
      <PageContainer className='flex h-14 items-center gap-4'>
        <Link to={paths.home()} className='flex shrink-0 items-center gap-2' aria-label='DAVINCI explorer home'>
          <span className='text-[15px] font-bold tracking-tight text-emerald'>DAVINCI</span>
          <span className='label-caps hidden rounded-pill border border-emerald/20 bg-emerald/8 px-2 py-[2px] text-[9px] text-emerald sm:inline'>
            explorer
          </span>
        </Link>

        <nav aria-label='Primary' className='hidden shrink-0 items-center gap-0.5 lg:flex'>
          {items.map((item) => navLink(item))}
        </nav>

        <div className='ml-auto flex min-w-0 items-center gap-3'>
          <GlobalSearch id='global-search' className='hidden w-72 min-w-32 shrink xl:block' />
          <ChainPill className='hidden md:flex' />
          <ThemeToggle />
          <Button
            size='icon'
            variant='subtle'
            className='lg:hidden'
            aria-label={open ? 'Close menu' : 'Open menu'}
            aria-expanded={open}
            onClick={() => setOpen((v) => !v)}
          >
            {open ? <CloseIcon /> : <MenuIcon />}
          </Button>
        </div>
      </PageContainer>

      <PageContainer className='pb-3 xl:hidden'>
        <GlobalSearch id='global-search-compact' />
      </PageContainer>

      {open ? (
        <PageContainer className='border-t border-charcoal py-2 lg:hidden'>
          <nav aria-label='Primary' className='flex flex-col'>
            {items.map((item) => navLink(item, true))}
          </nav>
          <div className='mt-2 border-t border-charcoal pt-3 md:hidden'>
            <ChainPill className='w-fit' />
          </div>
        </PageContainer>
      ) : null}
    </header>
  )
}
