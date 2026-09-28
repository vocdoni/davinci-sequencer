import { expect, test, type Page } from '@playwright/test'

// Deterministic demo network: the first process of organizer 0 (nonce 1) is
// the busy open one, see src/fixtures/synthetic.ts.
const OPEN_PID = '0x42fc20654efd78c6887ff0bd1cc50c9ec1dab589'

async function demo(page: Page, path: string) {
  const sep = path.includes('?') ? '&' : '?'
  await page.goto(`${path}${sep}demo=1`)
}

/** Opens transition #1 of the first process with results (it has several). */
async function openTransition(page: Page): Promise<string> {
  await demo(page, '/processes?status=results')
  const href = await page.getByTestId('page-processes').locator('a[href^="/processes/0x"]').first().getAttribute('href')
  await demo(page, `${href}/transitions/1`)
  await expect(page.getByTestId('page-transition')).toBeVisible()
  return page.url()
}

async function firstProcessId(page: Page): Promise<string> {
  await demo(page, '/processes')
  const link = page.getByTestId('page-processes').locator('a[href^="/processes/0x"]').first()
  const href = await link.getAttribute('href')
  return href!.split('/').pop()!
}

test.describe('every route renders', () => {
  const routes: Array<[string, string]> = [
    ['/', 'page-overview'],
    ['/processes', 'page-processes'],
    ['/votes', 'page-votes'],
    ['/contracts', 'page-contracts'],
    ['/sequencers', 'page-sequencers'],
    ['/learn', 'page-learn'],
    ['/learn/glossary', 'page-learn'],
    ['/kit', 'page-kit'],
    ['/no/such/page', 'page-not-found'],
  ]
  for (const [path, testId] of routes) {
    test(path, async ({ page }) => {
      const errors: string[] = []
      page.on('pageerror', (e) => errors.push(e.message))
      await demo(page, path)
      await expect(page.getByTestId(testId)).toBeVisible()
      expect(errors).toEqual([])
    })
  }

  test('process page and every tab', async ({ page }) => {
    const pid = await firstProcessId(page)
    for (const tab of ['', '/key', '/transitions', '/votes', '/results', '/raw']) {
      await demo(page, `/processes/${pid}${tab}`)
      await expect(page.getByTestId('page-process')).toBeVisible()
      await expect(page.getByTestId(`tab-${tab ? tab.slice(1) : 'overview'}`)).toBeVisible()
    }
  })

  test('transition page', async ({ page }) => {
    await openTransition(page)
    await expect(page.getByTestId('transition-summary')).toContainText('9/9 checks passed')
    await expect(page.getByTestId('transition-summary')).toContainText('vote ids')
  })

  test('vote lookup', async ({ page }) => {
    await demo(page, '/votes')
    await expect(page.getByRole('button', { name: 'Look up' })).toBeVisible()
  })
})

test.describe('theme', () => {
  test('the toggle applies and persists across reloads', async ({ page }) => {
    await demo(page, '/')
    const html = page.locator('html')
    await page.getByRole('radio', { name: 'Light theme' }).click()
    await expect(html).toHaveAttribute('data-theme', 'light')
    await page.reload()
    await expect(html).toHaveAttribute('data-theme', 'light')
    await expect(page.getByRole('radio', { name: 'Light theme' })).toHaveAttribute('aria-checked', 'true')
    await page.getByRole('radio', { name: 'Dark theme' }).click()
    await expect(html).toHaveAttribute('data-theme', 'dark')
    await page.reload()
    await expect(html).toHaveAttribute('data-theme', 'dark')
  })

  test('system follows the OS preference, before the first paint', async ({ browser }) => {
    const context = await browser.newContext({ colorScheme: 'light' })
    const page = await context.newPage()
    await page.goto('/?demo=1')
    // Set by the inline script in index.html, before React mounts.
    expect(await page.evaluate(() => document.documentElement.getAttribute('data-theme'))).toBe('light')
    await expect(page.getByTestId('page-overview')).toBeVisible()
    await page.emulateMedia({ colorScheme: 'dark' })
    await expect(page.locator('html')).toHaveAttribute('data-theme', 'dark')
    await context.close()
  })
})

test.describe('search', () => {
  const search = (page: Page) => page.getByRole('textbox', { name: /Search processes/ }).first()

  test('a process id opens the process', async ({ page }) => {
    const pid = await firstProcessId(page)
    await search(page).fill(pid)
    await search(page).press('Enter')
    await expect(page).toHaveURL(new RegExp(`/processes/${pid}$`))
    await expect(page.getByTestId('page-process')).toBeVisible()
  })

  test('an organizer address lists its processes', async ({ page }) => {
    await demo(page, '/')
    await search(page).fill(OPEN_PID)
    await search(page).press('Enter')
    await expect(page).toHaveURL(/\/processes\?organizer=/)
    await expect(page.getByTestId('process-count')).not.toHaveText('0 processes')
  })

  test('a vote id opens the vote lookup', async ({ page }) => {
    await demo(page, '/')
    await search(page).fill('0x8000000000000001')
    await search(page).press('Enter')
    await expect(page).toHaveURL(/\/votes\?voteId=0x8000000000000001/)
  })

  test('a transaction hash resolves to its transition', async ({ page }) => {
    const url = await openTransition(page)
    const txHref = await page.getByTestId('page-transition').locator('a[href^="/tx/0x"]').first().getAttribute('href')
    const hash = txHref!.split('/').pop()!
    await demo(page, '/')
    await search(page).fill(hash)
    await search(page).press('Enter')
    await expect(page.getByTestId('page-transition')).toBeVisible()
    expect(new URL(page.url()).pathname).toBe(new URL(url).pathname)
  })

  test('nonsense explains itself', async ({ page }) => {
    await demo(page, '/')
    await search(page).fill('hello')
    await search(page).press('Enter')
    await expect(page.getByRole('alert')).toContainText('No process')
  })
})
