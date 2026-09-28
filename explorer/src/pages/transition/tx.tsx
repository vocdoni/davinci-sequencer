import { Link, Navigate, useParams } from 'react-router-dom'
import { useRuntimeConfig } from '~config/config-context'
import { useIndexer, useStore } from '~data/hooks'
import { ButtonLink, Callout, EmptyState, SectionHeader, SkeletonText, Stack } from '~kit'
import { explorerTxUrl } from '~lib/explorer'
import { paths } from '~routes/paths'
import { isTxHash, txTarget } from './tx-target'

/**
 * Resolves /tx/:hash to the transition it settled, the process it created,
 * the results it published or the process it changed. Anything else is not
 * a transaction of this registry, and the page says so.
 */
export function TxPage() {
  const { hash = '' } = useParams()
  const store = useStore()
  const { status } = useIndexer()
  const { blockExplorerUrl } = useRuntimeConfig()
  const h = hash.trim().toLowerCase()
  const valid = isTxHash(h)
  const target = valid ? txTarget(store, h) : null

  if (target) return <Navigate replace to={target} />
  if (valid && (status.phase === 'idle' || status.phase === 'loading' || status.scanning)) {
    return (
      <Stack data-testid='page-tx'>
        <SectionHeader size='page' label='Transaction' title='Looking for this transaction' />
        <p className='text-[13px] text-ash'>The indexer is still reading the registry's events.</p>
        <SkeletonText lines={4} className='max-w-2xl' />
      </Stack>
    )
  }
  const external = valid ? explorerTxUrl(blockExplorerUrl, h) : null
  return (
    <Stack data-testid='page-tx'>
      <SectionHeader
        size='page'
        label='Transaction'
        title={valid ? 'Not a registry transaction' : 'Not a transaction hash'}
        description={<span className='font-mono text-[12px] break-all'>{hash}</span>}
      />
      {valid ? (
        <>
          <EmptyState
            title='No event of this registry came from this transaction'
            description='The explorer knows the transactions that emitted a ProcessRegistry event here: a process creation, a settled transition, results, a decryption request, or a status, census, duration or max-voters change. This one did none of them, or it happened on another network or registry.'
            action={
              external ? (
                <ButtonLink href={external} external variant='ghost' size='sm'>
                  Open in the block explorer
                </ButtonLink>
              ) : undefined
            }
          />
          <Callout title='Looking for something else?'>
            A vote id goes to the{' '}
            <Link to={paths.votes()} className='text-silver hover:text-emerald'>
              vote lookup
            </Link>
            ; the registry and verifier are on the{' '}
            <Link to={paths.contracts()} className='text-silver hover:text-emerald'>
              contracts page
            </Link>
            .
          </Callout>
        </>
      ) : (
        <EmptyState
          title='A transaction hash is 0x followed by 64 hex digits'
          description='Paste the hash of a settlement, a process creation or a results transaction.'
        />
      )}
    </Stack>
  )
}
