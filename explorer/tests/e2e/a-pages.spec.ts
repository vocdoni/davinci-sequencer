import { expect, test, type Page } from '@playwright/test'

// Overview, processes list and process pages on the deterministic demo network
// (src/fixtures/synthetic.ts). Process ids are organizer ‖ registry prefix ‖ nonce.
const PREFIX = 'b12878d5'
const pid = (organizer: string, nonce: number) => `0x${organizer}${PREFIX}${nonce.toString(16).padStart(14, '0')}`
const ORG0 = '42fc20654efd78c6887ff0bd1cc50c9ec1dab589'
const ORG1 = '7e5f4552091a69125d5dfcb7b8c2659029395bdf'
const ORG2 = '2b5ad5c4795c026514f8317c7a215e218dccd6cf'
/** Sequencer key, zkVM results. */
const RESULTS = pid(ORG0, 0)
/** DKG automatic, committee results. */
const DKG_RESULTS = pid(ORG1, 0)
/** DKG locked, tally submitted, organizer secret sealed. */
const AWAITING_REVEAL = pid(ORG2, 0)
/** Open, 40 transitions, on-chain census. */
const OPEN = pid(ORG0, 1)
const CANCELED = pid(ORG1, 2)

async function demo(page: Page, path: string) {
  const sep = path.includes('?') ? '&' : '?'
  await page.goto(`${path}${sep}demo=1`)
}

function noPageErrors(page: Page): string[] {
  const errors: string[] = []
  page.on('pageerror', (e) => errors.push(e.message))
  return errors
}

test.describe('overview', () => {
  test('network, stats, activity and entry points', async ({ page }) => {
    const errors = noPageErrors(page)
    await demo(page, '/')
    const root = page.getByTestId('page-overview')
    await expect(root.getByText('Ballots settled', { exact: true })).toBeVisible()
    await expect(root.getByText(/matches davinci-zkvm/)).toBeVisible()
    await expect(
      root.getByRole('list', { name: 'Release pin checks' }).getByRole('img', { name: 'passed' })
    ).toHaveCount(5)
    await expect(root.getByRole('link', { name: /^Transition #\d+/ }).first()).toBeVisible()
    await expect(root.getByRole('img', { name: 'Stacked activity chart' })).toBeVisible()
    expect(errors).toEqual([])
  })

  test('the organizer box lists that organizer’s processes', async ({ page }) => {
    await demo(page, '/')
    const box = page.getByRole('textbox', { name: 'Organizer address' })
    await box.fill('0x1234')
    await expect(page.getByText('An address is 0x followed by 40 hex digits.')).toBeVisible()
    await box.fill(`0x${ORG0.toUpperCase()}`)
    await page.getByRole('button', { name: 'Show my processes' }).click()
    await expect(page).toHaveURL(new RegExp(`/processes\\?organizer=0x${ORG0}`))
    await expect(page.getByTestId('process-count')).toHaveText('4 processes')
  })

  test('role cards link to the vote lookup and the contracts', async ({ page }) => {
    await demo(page, '/')
    await page.getByRole('link', { name: 'Check my vote' }).click()
    await expect(page).toHaveURL(/\/votes$/)
    await demo(page, '/')
    await page.getByRole('link', { name: 'Contracts and pins' }).click()
    await expect(page).toHaveURL(/\/contracts$/)
  })
})

test.describe('processes list', () => {
  test('filters live in the URL', async ({ page }) => {
    await demo(page, '/processes')
    await expect(page.getByTestId('process-count')).toHaveText('10 processes')
    await page.getByLabel('Phase').selectOption('results')
    await expect(page).toHaveURL(/status=results/)
    await expect(page.getByTestId('process-count')).toHaveText('2 processes')
    await page.getByLabel('Key mode').selectOption('dkg-automatic')
    await expect(page).toHaveURL(/keyMode=dkg-automatic/)
    await expect(page.getByTestId('process-count')).toHaveText('1 process')
    await page.getByRole('button', { name: 'Clear filters' }).first().click()
    await expect(page.getByTestId('process-count')).toHaveText('10 processes')

    await demo(page, '/processes?census=csp')
    await expect(page.getByLabel('Census')).toHaveValue('csp')
    await expect(page.getByTestId('process-count')).toHaveText('2 processes')
  })

  test('search and an impossible filter', async ({ page }) => {
    await demo(page, '/processes')
    await page.getByRole('textbox', { name: 'Search processes by id or organizer' }).fill(ORG2.slice(0, 10))
    await expect(page).toHaveURL(/q=2b5ad5c479/)
    await expect(page.getByTestId('process-count')).toHaveText('3 processes')
    await demo(page, '/processes?status=upcoming&keyMode=sequencer')
    await expect(page.getByText('No process matches these filters')).toBeVisible()
  })

  test('sorting and opening a row', async ({ page }) => {
    await demo(page, '/processes')
    const table = page.getByTestId('page-processes').locator('table')
    // Numeric columns sort descending first.
    await table.getByRole('columnheader', { name: /Voters/ }).click()
    const first = table.locator('tbody tr').first()
    await expect(first).toContainText('1,054')
    await first.getByRole('cell').nth(2).click()
    await expect(page).toHaveURL(new RegExp(`/processes/${OPEN}$`))
    await expect(page.getByTestId('page-process')).toBeVisible()
  })
})

test.describe('process page', () => {
  test('header, lifecycle and tab navigation', async ({ page }) => {
    const errors = noPageErrors(page)
    await demo(page, `/processes/${OPEN}`)
    await expect(page.getByRole('heading', { name: 'Community fund round' })).toBeVisible()
    await expect(page.getByTestId('process-lifecycle')).toContainText('40 batches')
    for (const [name, tab] of [
      ['Encryption key', 'key'],
      ['Transitions', 'transitions'],
      ['Votes', 'votes'],
      ['Results', 'results'],
      ['Raw', 'raw'],
      ['Overview', 'overview'],
    ]) {
      await page.getByRole('tab', { name: new RegExp(`^${name}`) }).click()
      await expect(page.getByTestId(`tab-${tab}`)).toBeVisible()
      await expect(page).toHaveURL(
        new RegExp(tab === 'overview' ? `/processes/${OPEN}$` : `/processes/${OPEN}/${tab}$`)
      )
    }
    expect(errors).toEqual([])
  })

  test('overview tab explains ballot, census and id', async ({ page }) => {
    await demo(page, `/processes/${OPEN}`)
    const tab = page.getByTestId('tab-overview')
    await expect(tab.getByTestId('ballot-rules')).toContainText('The ballot has 16 fields')
    await expect(tab).toContainText('Reads as:')
    await expect(tab).toContainText('any root the census contract recorded at or after the creation block')
    await expect(tab).toContainText('matches this registry')
    await expect(tab.getByTestId('process-metadata')).toContainText('Community fund round')
    await expect(tab.getByText('field 16: Option 16')).toBeVisible()
  })

  test('key tab: sequencer and a sealed DKG-locked key', async ({ page }) => {
    await demo(page, `/processes/${OPEN}/key`)
    await expect(page.getByTestId('tab-key')).toContainText('This mode trusts one party with ballot secrecy.')
    await demo(page, `/processes/${AWAITING_REVEAL}/key`)
    const tab = page.getByTestId('tab-key')
    await expect(tab.getByText('sealed', { exact: true })).toBeVisible()
    await expect(tab).toContainText('waiting for partials')
    await expect(tab.getByRole('link', { name: /Open in the DKG explorer/ })).toHaveAttribute(
      'href',
      /^https:\/\/dkg\.example\.org\/applications\/0x[0-9a-f]{24}\/0x[0-9a-f]{64}$/
    )
  })

  test('transitions tab: root chain and table', async ({ page }) => {
    await demo(page, `/processes/${OPEN}/transitions`)
    await expect(page.getByTestId('transition-summary')).toContainText('40 transitions')
    await expect(page.getByTestId('transition-summary')).toContainText('the chain is continuous')
    await expect(page.getByTestId('transition-summary')).toContainText('the last root is the registry’s current root')
    await page.getByRole('list', { name: 'State roots' }).getByRole('link', { name: '#3', exact: true }).click()
    await expect(page).toHaveURL(new RegExp(`/processes/${OPEN}/transitions/3$`))
  })

  test('votes tab: vote ids from the blobs and the lookup', async ({ page }) => {
    await demo(page, `/processes/${OPEN}/votes`)
    const ids = page.getByTestId('vote-ids')
    await expect(ids).toContainText('slot updates')
    const first = ids.getByRole('link').first()
    const voteId = (await first.textContent())!.trim()
    expect(voteId).toMatch(/^0x[0-9a-f]{16}$/)

    await page.getByLabel('Transition', { exact: true }).selectOption('0')
    await expect(page).toHaveURL(/t=0/)
    await expect(page.getByTestId('vote-ids')).toBeVisible()

    const box = page.getByRole('textbox', { name: 'Vote id' })
    await box.fill('12')
    await expect(page.getByText(/A vote id is a number from 2\^63/)).toBeVisible()
    await box.fill(voteId)
    await page.getByRole('button', { name: 'Look up this vote' }).click()
    await expect(page).toHaveURL(new RegExp(`/votes/${OPEN}/${voteId}$`))
  })

  test('results tab: zkVM tally, DKG tally and no results yet', async ({ page }) => {
    await demo(page, `/processes/${RESULTS}/results`)
    const tab = page.getByTestId('tab-results')
    await expect(tab.getByTestId('tally').getByRole('listitem')).toHaveCount(4)
    await expect(tab.getByText('The results program passed every check')).toBeVisible()
    await expect(tab.getByRole('img', { name: 'passed' })).toHaveCount(4)

    await demo(page, `/processes/${DKG_RESULTS}/results`)
    await expect(page.getByTestId('tab-results')).toContainText('Every submitted ciphertext is combined')

    await demo(page, `/processes/${OPEN}/results`)
    await expect(page.getByTestId('no-results')).toContainText('No results yet')
    await demo(page, `/processes/${AWAITING_REVEAL}/results`)
    await expect(page.getByTestId('no-results')).toContainText('Decryption requested')
    await demo(page, `/processes/${CANCELED}/results`)
    await expect(page.getByTestId('no-results')).toContainText('Canceled: no results')
  })

  test('raw tab: the contract state as JSON', async ({ page }) => {
    await demo(page, `/processes/${RESULTS}/raw`)
    const state = page.getByTestId('raw-state')
    await expect(state).toContainText('"ballotMode"')
    await expect(state).toContainText('"keyMode": "sequencer"')
    const json = JSON.parse((await state.textContent())!)
    expect(typeof json.encryptionKey.x).toBe('string')
    await expect(page.getByRole('button', { name: 'Copy getProcess' })).toBeVisible()
  })

  test('an unknown process id', async ({ page }) => {
    await demo(page, `/processes/0x${'ab'.repeat(31)}`)
    await expect(page.getByText('No process found')).toBeVisible()
  })
})
