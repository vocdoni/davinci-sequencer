import { expect, test, type Page } from '@playwright/test'

// Transition, transaction and vote-lookup pages on the deterministic demo
// network (src/fixtures/synthetic.ts): the open process has 40 transitions,
// #37 spans four blobs, and demo sequencer 0 holds a few unsettled votes.
const OPEN_PID = '0x42fc20654efd78c6887ff0bd1cc50c9ec1dab589b12878d500000000000001'
const MULTI_BLOB = 37
const PENDING_VOTE = '0xcb65a1d07ab43d18'
const ERROR_VOTE = '0xdc810dc19fd931a9'

async function demo(page: Page, path: string) {
  const sep = path.includes('?') ? '&' : '?'
  await page.goto(`${path}${sep}demo=1`)
}

function watchErrors(page: Page): string[] {
  const errors: string[] = []
  page.on('pageerror', (e) => errors.push(e.message))
  return errors
}

test.describe('transition page', () => {
  test('shows the summary, the publics, the proof and every check', async ({ page }) => {
    const errors = watchErrors(page)
    await demo(page, `/processes/${OPEN_PID}/transitions/${MULTI_BLOB}`)
    const root = page.getByTestId('page-transition')
    await expect(root.getByRole('heading', { name: `Transition #${MULTI_BLOB}` })).toBeVisible()
    await expect(page.getByTestId('transition-summary')).toContainText('checks passed')
    await expect(page.getByTestId('transition-summary')).toContainText('4 blobs')

    const publics = page.getByTestId('publics')
    for (const name of ['overall_ok', 'fail_mask', 'RootHashBefore', 'CensusRoot', 'BlobsDigest', 'OccupiedBefore']) {
      await expect(publics.getByText(name, { exact: true })).toBeVisible()
    }
    await expect(publics).toContainText('every bit clear')
    await publics.getByText('What the guest checked: 19 fail bits').click()
    await expect(publics).toContainText('FAIL_REFRESH')

    await expect(page.getByTestId('proof')).toContainText('768 B')
    await expect(page.getByTestId('proof')).toContainText('verifySnarkProof')

    const verify = page.getByTestId('verify')
    await expect(verify.locator('[data-testid^="check-"]')).toHaveCount(11)
    const plonk = verify.getByTestId('check-plonk')
    await plonk.getByText('Recheck it').click()
    await expect(plonk.locator('pre')).toContainText('cast call')
    await expect(plonk.locator('pre')).toContainText('verifySnarkProof(bytes32,bytes32,bytes,bytes)')
    expect(errors).toEqual([])
  })

  test('decodes the blobs and opens their content on demand', async ({ page }) => {
    const errors = watchErrors(page)
    await demo(page, `/processes/${OPEN_PID}/transitions/${MULTI_BLOB}`)
    await expect(page.getByTestId('blob-list').locator('tbody tr')).toHaveCount(4)
    const content = page.getByTestId('blob-content')
    await expect(content).toBeVisible()
    await expect(content.getByTestId('vote-id-list').locator('a').first()).toBeVisible()

    await content.getByRole('tab', { name: /Slot updates/ }).click()
    const updates = content.getByTestId('slot-update-list')
    await updates.getByRole('button', { name: 'Show ciphertexts' }).first().click()
    await expect(updates.getByRole('columnheader', { name: 'c1' })).toBeVisible()

    await content.getByRole('tab', { name: /Cells/ }).click()
    const cells = content.getByTestId('blob-cell-view')
    await expect(cells).toContainText('Vote id count')
    await cells.getByRole('button', { name: /Accumulator/ }).click()
    await expect(cells).toContainText('Accumulator field 0 c1')
    expect(errors).toEqual([])
  })

  test('steps to the previous and next transition', async ({ page }) => {
    await demo(page, `/processes/${OPEN_PID}/transitions/${MULTI_BLOB}`)
    await page.getByRole('link', { name: `#${MULTI_BLOB + 1}` }).click()
    await expect(page.getByRole('heading', { name: `Transition #${MULTI_BLOB + 1}` })).toBeVisible()
    await page.getByRole('link', { name: `#${MULTI_BLOB}` }).click()
    await expect(page.getByRole('heading', { name: `Transition #${MULTI_BLOB}` })).toBeVisible()
  })

  test('a vote id links to its lookup', async ({ page }) => {
    await demo(page, `/processes/${OPEN_PID}/transitions/${MULTI_BLOB}`)
    const link = page.getByTestId('vote-id-list').locator('a').first()
    const id = (await link.textContent())!.trim()
    await link.click()
    await expect(page).toHaveURL(new RegExp(`/votes/${OPEN_PID}/${id}`))
    await expect(page.getByTestId('vote-summary')).toContainText(`found in transition #${MULTI_BLOB}`)
  })

  test('an unknown transition says so', async ({ page }) => {
    await demo(page, `/processes/${OPEN_PID}/transitions/999`)
    await expect(page.getByText('No transition found')).toBeVisible()
  })
})

test.describe('transaction route', () => {
  test('a settlement resolves to its transition', async ({ page }) => {
    await demo(page, `/processes/${OPEN_PID}/transitions/${MULTI_BLOB}`)
    const href = await page.getByTestId('transition-summary').locator('a[href^="/tx/0x"]').first().getAttribute('href')
    await demo(page, href!)
    await expect(page).toHaveURL(new RegExp(`/processes/${OPEN_PID}/transitions/${MULTI_BLOB}`))
    await expect(page.getByTestId('page-transition')).toBeVisible()
  })

  test('an unknown hash and a malformed one explain themselves', async ({ page }) => {
    await demo(page, `/tx/0x${'ab'.repeat(32)}`)
    await expect(page.getByTestId('page-tx')).toContainText('Not a registry transaction')
    await demo(page, '/tx/0x1234')
    await expect(page.getByTestId('page-tx')).toContainText('Not a transaction hash')
  })
})

test.describe('vote lookup', () => {
  test('validates the form', async ({ page }) => {
    await demo(page, '/votes')
    await page.getByLabel('Process id').fill('0x42')
    await page.getByLabel('Vote id').fill('0x1')
    await page.getByRole('button', { name: 'Look up' }).click()
    await expect(page.getByText('A process id is 0x followed by 62 hex digits (31 bytes).')).toBeVisible()
    await expect(page.getByText('Vote ids start at 0x8000000000000000 (2^63).')).toBeVisible()
    await expect(page).toHaveURL(/\/votes\?demo=1$/)
  })

  test('fills in an example and finds it on-chain', async ({ page }) => {
    const errors = watchErrors(page)
    await demo(page, '/votes')
    await page.getByRole('button', { name: 'Fill in an example' }).click()
    await expect(page).toHaveURL(/\/votes\/0x[0-9a-f]{62}\/0x[0-9a-f]{16}/)
    await expect(page.getByTestId('vote-summary')).toContainText('found in transition #', { timeout: 15_000 })
    await expect(page.getByTestId('vote-inclusion')).toContainText('Listed in the blob of')
    await expect(page.getByTestId('vote-inclusion-source')).toContainText('The blob came from the beacon API')
    await expect(page.getByTestId('tracker-proof')).toContainText('The path reaches the root the proof names')
    await expect(page.getByTestId('tracker-proof')).toContainText('That root is one the registry held for this process')
    await expect(page.getByTestId('sequencer-status').getByText('Settled', { exact: true }).first()).toBeVisible()
    expect(errors).toEqual([])
  })

  test('a queued vote is pending at the sequencer and not on-chain yet', async ({ page }) => {
    await demo(page, `/votes/${OPEN_PID}/${PENDING_VOTE}`)
    const status = page.getByTestId('sequencer-status')
    await expect(status.getByText('Pending', { exact: true }).first()).toBeVisible()
    await expect(page.getByTestId('vote-summary')).toContainText('not in any settled transition', { timeout: 15_000 })
  })

  test('a refused vote shows the reason', async ({ page }) => {
    await demo(page, `/votes/${OPEN_PID}/${ERROR_VOTE}`)
    await expect(page.getByTestId('sequencer-status')).toContainText('census proof: not a member')
  })

  test('a vote id alone asks for the process, a full query becomes a path', async ({ page }) => {
    await demo(page, '/votes?voteId=0x8000000000000001')
    await expect(page.getByText('Which process?')).toBeVisible()
    await expect(page.getByLabel('Vote id')).toHaveValue('0x8000000000000001')
    await demo(page, `/votes?pid=${OPEN_PID}&voteId=${PENDING_VOTE}`)
    await expect(page).toHaveURL(new RegExp(`/votes/${OPEN_PID}/${PENDING_VOTE}`))
  })

  test('explains what a check proves', async ({ page }) => {
    await demo(page, '/votes')
    const text = page.getByTestId('vote-explainers')
    await expect(text).toContainText('Silent revoting')
    await expect(text).toContainText('Why the ciphertext on-chain is not the one you sent')
  })
})
