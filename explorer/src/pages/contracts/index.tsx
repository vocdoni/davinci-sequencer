import { useEffect, useMemo } from 'react'
import { Link, useLocation } from 'react-router-dom'
import { HashLink } from '~components/HashLink'
import { useRuntimeConfig } from '~config/config-context'
import { useDeploymentDetails } from '~data/deployment'
import { useChain, useReleaseCheck } from '~data/hooks'
import { Callout, SectionHeader, Stack } from '~kit'
import { paths } from '~routes/paths'
import { AddressesPanel } from './AddressesPanel'
import { DkgPanel } from './DkgPanel'
import { ParametersPanel } from './ParametersPanel'
import { ReleasePanel } from './ReleasePanel'
import { VerifyPanel } from './VerifyPanel'
import { contractRows, publicRpc, releaseVerdict, wiringChecks } from './model'

const SECTIONS = [
  { id: 'addresses', label: 'Addresses' },
  { id: 'parameters', label: 'Pinned values' },
  { id: 'release', label: 'Release check' },
  { id: 'verify', label: 'Verify it yourself' },
  { id: 'dkg', label: 'DKG committee' },
] as const

/** The auditor's page: what the deployment is, what it is pinned to, and how to check both. */
export function ContractsPage() {
  const config = useRuntimeConfig()
  const chain = useChain()
  const match = useReleaseCheck()
  const details = useDeploymentDetails()
  const { hash } = useLocation()

  const rows = useMemo(() => contractRows(chain, details.data), [chain, details.data])
  const checks = useMemo(() => wiringChecks(chain, details.data, config.chainId), [chain, details.data, config.chainId])
  const verdict = releaseVerdict(match)
  const ready = chain.registry != null

  // Deep links (#verify, #dkg) land after the lazy page and its data have rendered.
  useEffect(() => {
    if (!hash) return
    document.getElementById(decodeURIComponent(hash.slice(1)))?.scrollIntoView()
  }, [hash, ready])

  return (
    <Stack data-testid='page-contracts'>
      <SectionHeader
        size='page'
        label='Contracts'
        title='Contracts and parameters'
        description='What this deployment is made of and what it is pinned to: the contract addresses, the verification keys every proof is checked against, and the commands to check both without trusting this page.'
      />

      <Callout
        tone={verdict.tone}
        title={
          match.release
            ? `Pinned to ${match.release.label}`
            : verdict.tone === 'danger'
              ? 'The pins differ from the known releases'
              : 'Checking the pins'
        }
        actions={
          <HashLink id='release' className='text-[12px] whitespace-nowrap text-pewter hover:text-emerald'>
            Details
          </HashLink>
        }
      >
        <span data-testid='release-summary'>{verdict.text}</span>{' '}
        {match.release
          ? 'Every transition and every sequencer-key tally on this registry is verified against the released guests, the released ZisK setup and the released verifier code.'
          : null}
      </Callout>

      <nav aria-label='On this page' className='flex flex-wrap gap-x-4 gap-y-1 text-[12px]'>
        {SECTIONS.map((s) => (
          <HashLink key={s.id} id={s.id} className='text-pewter transition-colors hover:text-emerald'>
            {s.label}
          </HashLink>
        ))}
        <Link to={paths.learn('verify-auditor')} className='text-pewter transition-colors hover:text-emerald'>
          The auditor’s guide
        </Link>
      </nav>

      <section id='addresses' className='scroll-mt-20'>
        <AddressesPanel rows={rows} checks={checks} />
      </section>
      <section id='parameters' className='scroll-mt-20'>
        <ParametersPanel chain={chain} details={details.data} />
      </section>
      <section id='release' className='scroll-mt-20'>
        <ReleasePanel match={match} />
      </section>
      <section id='verify' className='scroll-mt-20'>
        <VerifyPanel chain={chain} rpc={publicRpc(config.rpcUrls)} match={match} />
      </section>
      <section id='dkg' className='scroll-mt-20'>
        <DkgPanel
          chain={chain}
          dkg={details.data?.dkg}
          loading={details.isLoading || !ready}
          error={details.error ? (details.error as Error).message : null}
        />
      </section>
    </Stack>
  )
}
