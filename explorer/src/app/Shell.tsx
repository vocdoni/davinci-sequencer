import { Outlet, ScrollRestoration } from 'react-router-dom'
import { useIndexerSearchResolver } from '~data/hooks'
import { PageContainer, TooltipProvider } from '~kit'
import { useRegisterSearchResolver } from './search-context'
import { SearchProvider } from './SearchProvider'
import { StatusBanners } from './StatusBanners'
import { TopBar } from './TopBar'
import { Footer } from './Footer'

/**
 * App frame every route renders inside. It owns the Radix tooltip provider,
 * so any kit component with a tooltip works on any page without wiring.
 * Pages get a 1400 px container and own only their content.
 */
export function Shell() {
  return (
    <SearchProvider>
      <IndexerSearchBridge />
      <TooltipProvider>
        <div className='flex min-h-screen flex-col bg-obsidian'>
          <a
            href='#main'
            className='sr-only focus:not-sr-only focus:fixed focus:top-2 focus:left-2 focus:z-50 focus:rounded-sm focus:bg-carbon focus:px-3 focus:py-2 focus:text-ghost'
          >
            Skip to content
          </a>
          <TopBar />
          <StatusBanners />
          <main id='main' className='flex-1 py-8'>
            <PageContainer>
              <Outlet />
            </PageContainer>
          </main>
          <Footer />
        </div>
      </TooltipProvider>
      <ScrollRestoration />
    </SearchProvider>
  )
}

// Routes ids the store knows (processes, transitions, organizers, blocks with
// a transition) before the search box's shape rules.
function IndexerSearchBridge() {
  const resolver = useIndexerSearchResolver()
  useRegisterSearchResolver(resolver)
  return null
}
