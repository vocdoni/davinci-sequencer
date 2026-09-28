import { describe, expect, it } from 'vitest'
import { screen, waitFor, within } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { KNOWN_RELEASES } from '~protocol/releases'
import { renderWithProviders } from '../../test-utils'
import { ContractsPage } from './index'

const release = KNOWN_RELEASES[0]!

describe('ContractsPage', () => {
  it('shows the demo deployment pinned to the known release, with every contract', async () => {
    renderWithProviders(<ContractsPage />, { route: '/contracts' })
    expect(screen.getByTestId('release-summary')).toHaveTextContent(`All five pins match ${release.label}`)
    for (const pin of [
      'batchProgramVK',
      'resultsProgramVK',
      'rootCVadcopFinal',
      'ziskVerifierCodeHash',
      'ballotVKHash',
    ]) {
      expect(screen.getByTestId(`pin-${pin}`)).toHaveAttribute('data-state', 'pass')
    }
    await waitFor(() => expect(screen.getByTestId('contract-row-dkg-registry')).toHaveTextContent('0x'))
    expect(within(screen.getByTestId('wiring-checks')).getByText('7 of 7 consistent')).toBeInTheDocument()
    expect(screen.getByTestId('registration-epoch')).toHaveTextContent('registrationEpoch()')
  })

  it('switches the verification command between the chain’s pins and the release’s', async () => {
    const user = userEvent.setup()
    renderWithProviders(<ContractsPage />, { route: '/contracts' })
    const script = screen.getByTestId('verify-script')
    expect(script).toHaveTextContent('python3 script/verify_deployment.py')
    expect(script).toHaveTextContent(`--batch-vk ${release.batchProgramVK}`)
    await user.click(within(script).getByRole('radio', { name: `Pins of ${release.label}` }))
    expect(within(script).getByRole('radio', { name: `Pins of ${release.label}` })).toHaveAttribute(
      'aria-checked',
      'true'
    )
    expect(script).toHaveTextContent('also checks that the registry holds exactly the released keys')
  })
})
