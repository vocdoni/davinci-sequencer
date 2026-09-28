import { beforeEach, describe, expect, it } from 'vitest'
import { render, screen, within } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { QueryClient, QueryClientProvider } from '@tanstack/react-query'
import { createMemoryRouter, RouterProvider } from 'react-router-dom'
import { ConfigContext } from '~config/config-context'
import { DEMO_CONFIG } from '~config/runtime-config'
import { DataProvider } from '~data/DataProvider'
import { createExplorerData } from '~data/create'
import { demoFixture } from '~fixtures/demo'
import { routes, ROUTER_FUTURE } from '~routes/router'
import { ThemeProvider } from '~theme/ThemeProvider'
import { THEME_STORAGE_KEY } from '~theme/theme'

const fixture = demoFixture()

function renderApp(path = '/') {
  const data = createExplorerData({ config: DEMO_CONFIG, demoOptions: { blockIntervalMs: 0 } })
  const router = createMemoryRouter(routes, { initialEntries: [path], future: ROUTER_FUTURE })
  return render(
    <ThemeProvider>
      <ConfigContext.Provider value={DEMO_CONFIG}>
        <QueryClientProvider client={new QueryClient()}>
          <DataProvider source={data.source} services={data.services}>
            <RouterProvider router={router} future={{ v7_startTransition: true }} />
          </DataProvider>
        </QueryClientProvider>
      </ConfigContext.Provider>
    </ThemeProvider>
  )
}

beforeEach(() => localStorage.clear())

describe('Shell', () => {
  it('renders the brand, the navigation and the overview', async () => {
    renderApp()
    expect(screen.getByLabelText('DAVINCI explorer home')).toBeInTheDocument()
    for (const label of ['Overview', 'Processes', 'Votes', 'Contracts', 'Sequencers', 'Learn']) {
      expect(screen.getAllByRole('link', { name: label }).length).toBeGreaterThan(0)
    }
    expect(await screen.findByTestId('page-overview')).toBeInTheDocument()
    expect(screen.getByText('Demo network', { selector: 'span' })).toBeInTheDocument()
  })

  it('persists the theme choice', async () => {
    const user = userEvent.setup()
    renderApp()
    const group = screen.getByRole('radiogroup', { name: 'Theme' })
    await user.click(within(group).getByRole('radio', { name: 'Light theme' }))
    expect(localStorage.getItem(THEME_STORAGE_KEY)).toBe('light')
    expect(document.documentElement.getAttribute('data-theme')).toBe('light')
    await user.click(within(group).getByRole('radio', { name: 'Dark theme' }))
    expect(document.documentElement.getAttribute('data-theme')).toBe('dark')
  })

  it('routes a searched process id to its page', async () => {
    const user = userEvent.setup()
    renderApp()
    const [box] = screen.getAllByRole('textbox', { name: /Search processes/ })
    await user.type(box!, `${fixture.featured.openProcess}{Enter}`)
    expect(await screen.findByTestId('page-process')).toBeInTheDocument()
  })

  it('resolves a transaction hash to its transition', async () => {
    const t = fixture.store.transitions[fixture.store.transitionOrder[3]!]!
    renderApp(`/tx/${t.tx}`)
    expect(await screen.findByTestId('page-transition')).toBeInTheDocument()
  })

  it('shows 404 for unknown routes', async () => {
    renderApp('/nowhere')
    expect(await screen.findByTestId('page-not-found')).toBeInTheDocument()
  })
})
