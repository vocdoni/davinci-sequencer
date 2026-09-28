import { expect, test, type Page } from '@playwright/test'

// Contracts, sequencers and the guide, against the demo network.

async function demo(page: Page, path: string) {
  const [base, hash] = path.split('#')
  const sep = base!.includes('?') ? '&' : '?'
  await page.goto(`${base}${sep}demo=1${hash ? `#${hash}` : ''}`)
}

function collectErrors(page: Page): string[] {
  const errors: string[] = []
  page.on('pageerror', (e) => errors.push(e.message))
  return errors
}

test.describe('contracts', () => {
  test('shows every contract, the pins and their check', async ({ page }) => {
    const errors = collectErrors(page)
    await demo(page, '/contracts')
    const root = page.getByTestId('page-contracts')
    await expect(root).toBeVisible()
    await expect(page.getByTestId('release-summary')).toContainText('All five pins match')
    for (const pin of [
      'batchProgramVK',
      'resultsProgramVK',
      'rootCVadcopFinal',
      'ziskVerifierCodeHash',
      'ballotVKHash',
    ]) {
      await expect(page.getByTestId(`pin-${pin}`)).toHaveAttribute('data-state', 'pass')
    }
    for (const id of ['registry', 'verifier', 'adapter', 'dkg-manager', 'dkg-app-manager', 'dkg-registry']) {
      await expect(page.getByTestId(`contract-row-${id}`)).toContainText('0x')
    }
    await expect(page.getByTestId('contract-row-registry').getByRole('link', { name: 'Source' })).toHaveAttribute(
      'href',
      /\/address\/0x[0-9a-fA-F]{40}#code$/
    )
    await expect(page.getByTestId('wiring-checks')).toContainText('7 of 7 consistent')
    await expect(page.getByTestId('param-pidPrefix')).toContainText('0x')
    expect(errors).toEqual([])
  })

  test('the verification command follows the chosen pins', async ({ page }) => {
    await demo(page, '/contracts')
    const script = page.getByTestId('verify-script')
    await expect(script).toContainText('python3 script/verify_deployment.py')
    await expect(script).toContainText('--registry 0x')
    await expect(script).toContainText('With the pins the registry holds')
    await script.getByRole('radio', { name: /^Pins of / }).click()
    await expect(script.getByRole('radio', { name: /^Pins of / })).toHaveAttribute('aria-checked', 'true')
    await expect(script).toContainText('also checks that the registry holds exactly the released keys')
  })

  test('the DKG committee and its registration gate', async ({ page }) => {
    await demo(page, '/contracts#dkg')
    await expect(page.getByTestId('dkg-epoch')).toContainText('Live')
    await expect(page.getByTestId('registration-epoch')).toContainText('registrationEpoch()')
    await expect(page.getByText('Anyone can register an application')).toBeVisible()
    for (const v of ['contribution', 'finalize', 'partialDecrypt', 'decryptCombine']) {
      await expect(page.getByTestId(`dkg-verifier-${v}`).getByRole('img', { name: 'passed' })).toBeVisible()
    }
  })

  test('the section links scroll to their section', async ({ page }) => {
    await demo(page, '/contracts')
    await page
      .getByRole('navigation', { name: 'On this page' })
      .getByRole('link', { name: 'Verify it yourself' })
      .click()
    await expect(page).toHaveURL(/#verify$/)
    await expect(page.getByTestId('verify-script')).toBeInViewport()
  })

  test('the footer’s "Verify the deployment" lands here', async ({ page }) => {
    await demo(page, '/')
    await page.getByRole('navigation', { name: 'Footer' }).getByRole('link', { name: 'Verify the deployment' }).click()
    await expect(page.getByTestId('page-contracts')).toBeVisible()
  })
})

test.describe('sequencers', () => {
  test('shows each node, its checks and the processes it serves', async ({ page }) => {
    const errors = collectErrors(page)
    await demo(page, '/sequencers')
    const first = page.getByTestId('sequencer-0')
    await expect(first.getByText('Signer', { exact: true })).toBeVisible()
    await expect(first.getByText('Up', { exact: true })).toBeVisible()
    await expect(first.getByTestId('sequencer-info-checks').getByRole('img', { name: 'passed' })).toHaveCount(5)
    await expect(first.getByText('at the on-chain root').first()).toBeVisible()
    await expect(page.getByTestId('sequencer-1').getByText('Observer', { exact: true })).toBeVisible()
    await expect(page.getByTestId('settlers').getByText('configured')).toBeVisible()
    expect(errors).toEqual([])
  })

  test('a served process opens its page', async ({ page }) => {
    await demo(page, '/sequencers')
    const link = page.getByTestId('sequencer-0').locator('a[href^="/processes/0x"]').first()
    const href = await link.getAttribute('href')
    await link.click()
    await expect(page).toHaveURL(new RegExp(`${href}$`))
    await expect(page.getByTestId('page-process')).toBeVisible()
  })
})

test.describe('learn', () => {
  const topics = [
    'how-it-works',
    'key-modes',
    'census',
    'silent-revoting',
    'blobs',
    'settlement',
    'results',
    'verify-voter',
    'verify-organizer',
    'verify-auditor',
    'glossary',
  ]

  test('the index links every topic and every role', async ({ page }) => {
    await demo(page, '/learn')
    await expect(page.getByTestId('learn-summary')).toBeVisible()
    await page.getByRole('link', { name: /I audit the deployment/ }).click()
    await expect(page).toHaveURL(/\/learn\/verify-auditor$/)
    await expect(page.getByTestId('learn-topic')).toHaveAttribute('data-topic', 'verify-auditor')
  })

  for (const slug of topics) {
    test(`/learn/${slug} renders`, async ({ page }) => {
      const errors = collectErrors(page)
      await demo(page, `/learn/${slug}`)
      await expect(page.getByTestId('learn-topic')).toHaveAttribute('data-topic', slug)
      await expect(page.getByRole('heading', { level: 1 })).toBeVisible()
      expect(errors).toEqual([])
    })
  }

  test('next and previous walk the guide', async ({ page }) => {
    await demo(page, '/learn/how-it-works')
    const nav = page.getByRole('navigation', { name: 'Next and previous topics' })
    await nav.getByRole('link', { name: /Key modes/ }).click()
    await expect(page.getByTestId('learn-topic')).toHaveAttribute('data-topic', 'key-modes')
    await page
      .getByRole('navigation', { name: 'Next and previous topics' })
      .getByRole('link', { name: /How DAVINCI works/ })
      .click()
    await expect(page.getByTestId('learn-topic')).toHaveAttribute('data-topic', 'how-it-works')
  })

  test('a term links to its glossary entry', async ({ page }) => {
    await demo(page, '/learn/how-it-works')
    await page.getByTestId('learn-topic').getByRole('link', { name: 'vote id', exact: true }).first().click()
    await expect(page).toHaveURL(/\/learn\/glossary#term-vote-id$/)
    const entry = page.locator('#term-vote-id')
    await expect(entry).toHaveAttribute('aria-current', 'true')
    await expect(entry).toBeInViewport()
  })

  test('the glossary filters', async ({ page }) => {
    await demo(page, '/learn/glossary')
    const list = page.getByTestId('glossary')
    await expect(list.locator('dt').first()).toBeVisible()
    const before = await list.locator('dt').count()
    await page.getByRole('searchbox', { name: 'Filter the glossary' }).fill('epoch')
    await expect(list.locator('dt').first()).toBeVisible()
    expect(await list.locator('dt').count()).toBeLessThan(before)
    await expect(list.getByText('Epoch', { exact: true })).toBeVisible()
    await page.getByRole('searchbox', { name: 'Filter the glossary' }).fill('no such thing at all')
    await expect(page.getByText('No term matches')).toBeVisible()
  })

  test('an unknown topic explains itself', async ({ page }) => {
    await demo(page, '/learn/nope')
    await expect(page.getByTestId('page-learn')).toBeVisible()
    await expect(page.getByText('No such topic')).toBeVisible()
  })

  test('the footer’s "How it works" lands on the guide', async ({ page }) => {
    await demo(page, '/contracts')
    await page.getByRole('navigation', { name: 'Footer' }).getByRole('link', { name: 'How it works' }).click()
    await expect(page.getByTestId('learn-summary')).toBeVisible()
  })

  test('on a phone the topic picker navigates', async ({ page }) => {
    await page.setViewportSize({ width: 390, height: 844 })
    await demo(page, '/learn/blobs')
    await page.getByRole('combobox', { name: 'Guide topic' }).selectOption('census')
    await expect(page.getByTestId('learn-topic')).toHaveAttribute('data-topic', 'census')
    const overflow = await page.evaluate(() => document.documentElement.scrollWidth > window.innerWidth)
    expect(overflow).toBe(false)
  })
})
