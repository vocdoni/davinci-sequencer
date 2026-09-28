import { useRuntimeConfig } from '~config/config-context'
import { useIndexer } from '~data/hooks'
import { Button, Callout, PageContainer, ProgressBar } from '~kit'
import { formatNumber } from '~lib/format'

/**
 * Deployment-level notices above every page: the RPC is on the wrong chain
 * (indexing stops), demo mode, the first scan, and a failing RPC.
 */
export function StatusBanners() {
  const config = useRuntimeConfig()
  const { status, refresh } = useIndexer()
  const mismatch = status.chainMismatch
  const firstScan = status.scanning && status.progress < 1
  const lastError = status.errors[status.errors.length - 1]
  const failing = status.phase === 'error' && !mismatch

  if (!mismatch && !config.demo && !firstScan && !failing) return null
  return (
    <PageContainer className='mt-4 flex flex-col gap-3'>
      {mismatch ? (
        <div data-testid='chain-mismatch'>
          <Callout tone='danger' title='Wrong network'>
            The RPC endpoint reports chain id <span className='font-mono text-ghost'>{mismatch.actual}</span>, but this
            explorer is configured for <span className='font-mono text-ghost'>{mismatch.expected}</span> (
            {config.networkName}). Nothing is indexed until <code>RPC_URL</code> and <code>CHAIN_ID</code> agree.
          </Callout>
        </div>
      ) : null}
      {config.demo ? (
        <Callout tone='warn' title='Demo network'>
          Everything on these pages is synthetic: a deterministic network generated in your browser, with no chain
          behind it. Links to the block explorer lead nowhere. Remove <code>?demo=1</code> (or open <code>?demo=0</code>
          ) to see the configured deployment.
        </Callout>
      ) : null}
      {firstScan && !mismatch ? (
        <Callout tone='info' title='Indexing the registry'>
          <p>
            Reading every ProcessRegistry event from block {formatNumber(status.fromBlock)}. Pages fill in as blocks
            arrive; the result is cached in this browser.
          </p>
          <ProgressBar
            className='mt-3 max-w-md'
            value={Math.max(0, status.lastBlock - status.fromBlock)}
            total={Math.max(1, status.headBlock - status.fromBlock)}
            label={`block ${formatNumber(status.lastBlock)} of ${formatNumber(status.headBlock)}`}
          />
        </Callout>
      ) : null}
      {failing ? (
        <Callout
          tone='danger'
          title='The RPC is not answering'
          actions={
            <Button size='sm' onClick={() => void refresh()}>
              Retry
            </Button>
          }
        >
          {lastError?.message ?? 'The last poll failed.'} The explorer keeps retrying.
        </Callout>
      ) : null}
    </PageContainer>
  )
}
