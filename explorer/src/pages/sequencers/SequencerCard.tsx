import { useMemo, useState } from 'react'
import { CheckMark, Explain, ProcessIdLink, ProcessPhaseBadge } from '~components'
import type { SequencerState } from '~data/queries'
import type { SequencerEndpoint } from '~data/services'
import { useSequencerProcessViews } from '~data/sequencer-processes'
import type { ProcessRow } from '~indexer/selectors'
import type { ChainMeta, IndexerStore } from '~indexer/types'
import { Address, Badge, Card, CardHeader, EmptyState, Hash, KeyValue, Pagination, Skeleton, SkeletonText } from '~kit'
import { formatNumber } from '~lib/format'
import type { Hex } from '~protocol/bytes'
import { infoChecks, settledBy, syncState, type SettlerRow } from './model'

const PAGE = 10

export function SequencerCard({
  state,
  chain,
  store,
  rows,
  settlerRows,
}: {
  state: SequencerState
  chain: ChainMeta
  store: IndexerStore
  rows: Map<string, ProcessRow>
  settlerRows: SettlerRow[]
}) {
  const { endpoint, info, processes } = state
  const data = info.data
  const status = info.isSuccess ? 'up' : info.isError ? 'down' : 'checking'
  const onChain = settledBy(settlerRows, data?.sequencerAddress)

  return (
    <Card flush className='overflow-hidden' data-testid={`sequencer-${endpoint.index}`}>
      <CardHeader
        label={`Sequencer ${endpoint.index + 1}`}
        title={<span className='font-mono text-[14px]'>{endpoint.upstream}</span>}
        actions={
          <>
            {data ? (
              <Badge tone={data.observer ? 'neutral' : 'accent'}>{data.observer ? 'Observer' : 'Signer'}</Badge>
            ) : null}
            <Badge tone={status === 'up' ? 'ok' : status === 'down' ? 'danger' : 'neutral'} dot={status === 'up'}>
              {status === 'up' ? 'Up' : status === 'down' ? 'Down' : 'Checking'}
            </Badge>
          </>
        }
      />
      <div className='p-5'>
        {info.isError ? (
          <p className='mb-4 text-[13px] text-red' role='alert'>
            /info did not answer: {(info.error as Error).message}
          </p>
        ) : null}
        {info.isLoading ? <SkeletonText lines={4} className='max-w-lg' /> : null}
        {data ? (
          <div className='grid gap-8 lg:grid-cols-2'>
            <div>
              <KeyValue
                items={[
                  {
                    label: 'Settling account',
                    value: data.sequencerAddress ? <Address value={data.sequencerAddress} chars={6} /> : 'none',
                    hint: data.observer
                      ? 'An observer has no key: it follows and serves reads, but never settles.'
                      : onChain
                        ? `sent ${formatNumber(onChain.transitions)} transitions on this registry, for ${formatNumber(onChain.processes)} processes`
                        : 'has not settled a transition on this registry yet',
                  },
                  {
                    label: (
                      <span className='inline-flex items-center gap-1'>
                        Settled by itself
                        <Explain>Batches this node proved and settled.</Explain>
                      </span>
                    ),
                    value: formatNumber(data.settledBySelf),
                    mono: true,
                  },
                  {
                    label: (
                      <span className='inline-flex items-center gap-1'>
                        Synced from others
                        <Explain>
                          Transitions another node settled, which this one rebuilt from their blobs and accepted only
                          because replaying them gave the event’s new root.
                        </Explain>
                      </span>
                    ),
                    value: formatNumber(data.syncedFromOthers),
                    mono: true,
                  },
                  {
                    label: (
                      <span className='inline-flex items-center gap-1'>
                        Lost races
                        <Explain>
                          Batches that another node’s transition beat to the chain. The node rolled back, synced the
                          winner and put the votes back in its queue: the only cost is the reverted transaction’s gas.
                        </Explain>
                      </span>
                    ),
                    value: formatNumber(data.lostRaces),
                    mono: true,
                  },
                ]}
              />
            </div>
            <div>
              <div className='label-caps text-[11px] text-pewter'>Configured for this deployment</div>
              <p className='mt-1 text-[12px] leading-relaxed text-ash'>
                The node’s /info against the registry. A node checks these at boot and will not start on a mismatch, so
                a ✗ means it serves another deployment.
              </p>
              <ul className='mt-3 flex flex-col gap-2' data-testid='sequencer-info-checks'>
                {infoChecks(data, chain).map((c) => (
                  <li key={c.id} className='flex items-center gap-2.5 text-[13px] text-silver'>
                    <CheckMark state={c.state} />
                    {c.label}
                  </li>
                ))}
              </ul>
            </div>
          </div>
        ) : null}

        <ServedProcesses
          endpoint={endpoint}
          pids={processes.data ?? null}
          loading={processes.isLoading}
          error={processes.isError ? (processes.error as Error).message : null}
          store={store}
          rows={rows}
        />
      </div>
    </Card>
  )
}

function ServedProcesses({
  endpoint,
  pids,
  loading,
  error,
  store,
  rows,
}: {
  endpoint: SequencerEndpoint
  pids: Hex[] | null
  loading: boolean
  error: string | null
  store: IndexerStore
  rows: Map<string, ProcessRow>
}) {
  const [page, setPage] = useState(0)
  const all = useMemo(() => pids ?? [], [pids])
  const pageCount = Math.max(1, Math.ceil(all.length / PAGE))
  const current = Math.min(page, pageCount - 1)
  const slice = useMemo(() => all.slice(current * PAGE, current * PAGE + PAGE), [all, current])
  const views = useSequencerProcessViews(endpoint, slice)

  return (
    <div className='mt-8'>
      <div className='flex flex-wrap items-baseline justify-between gap-2'>
        <div className='label-caps text-[11px] text-pewter'>Processes it serves</div>
        {pids ? <span className='text-[12px] text-ash'>{formatNumber(pids.length)} known to the node</span> : null}
      </div>
      <p className='mt-1 text-[12px] leading-relaxed text-ash'>
        Each process with its phase on chain and the node’s own view: whether it takes votes and whether its committed
        tree is at the registry’s latest root.
      </p>
      {error ? (
        <p className='mt-3 text-[13px] text-red' role='alert'>
          /processes did not answer: {error}
        </p>
      ) : loading ? (
        <Skeleton className='mt-3 h-24 w-full' />
      ) : all.length === 0 ? (
        <EmptyState compact title='No processes yet' description='The node knows no process of this registry.' />
      ) : (
        <>
          <div className='scroll-slim mt-3 overflow-x-auto rounded-md border border-charcoal'>
            <table className='w-full min-w-[640px] text-left text-[13px]'>
              <thead>
                <tr className='border-b border-charcoal text-pewter'>
                  <th className='label-caps px-3 py-2 text-[11px] font-semibold'>Process</th>
                  <th className='label-caps px-3 py-2 text-[11px] font-semibold'>On chain</th>
                  <th className='label-caps px-3 py-2 text-[11px] font-semibold'>Votes</th>
                  <th className='label-caps px-3 py-2 text-[11px] font-semibold'>Node’s root</th>
                </tr>
              </thead>
              <tbody>
                {slice.map((pid, i) => {
                  const row = rows.get(pid.toLowerCase())
                  const view = views[i]
                  const onchainRoot = store.processes[pid.toLowerCase()]?.state?.latestStateRoot
                  const sync = syncState(view?.data, onchainRoot)
                  return (
                    <tr key={pid} className='border-b border-charcoal last:border-b-0'>
                      <td className='px-3 py-2'>
                        <ProcessIdLink id={pid} />
                      </td>
                      <td className='px-3 py-2'>
                        {row ? (
                          <ProcessPhaseBadge phase={row.phase} size='sm' />
                        ) : (
                          <span className='text-[12px] text-ash'>not indexed</span>
                        )}
                      </td>
                      <td className='px-3 py-2 text-[12px]'>
                        {view?.isLoading ? (
                          <Skeleton className='h-3 w-20' />
                        ) : view?.data?.ignored ? (
                          <span className='text-amber'>ignored{view.data.note ? `: ${view.data.note}` : ''}</span>
                        ) : view?.data ? (
                          <span className={view.data.isAcceptingVotes ? 'text-emerald' : 'text-ash'}>
                            {view.data.isAcceptingVotes ? 'accepting' : 'not accepting'}
                          </span>
                        ) : view?.isError ? (
                          <span className='text-ash'>no answer</span>
                        ) : (
                          '—'
                        )}
                      </td>
                      <td className='px-3 py-2 text-[12px]'>
                        {view?.data?.localStateRoot ? (
                          <span className='inline-flex items-center gap-2'>
                            <CheckMark state={sync === 'in-sync' ? 'pass' : 'unknown'} />
                            <Hash value={view.data.localStateRoot} chars={6} copy={false} />
                            <span className='text-ash'>
                              {sync === 'in-sync' ? 'at the on-chain root' : sync === 'differs' ? 'not at it yet' : ''}
                            </span>
                          </span>
                        ) : (
                          <span className='text-ash'>—</span>
                        )}
                      </td>
                    </tr>
                  )
                })}
              </tbody>
            </table>
          </div>
          {pageCount > 1 ? (
            <Pagination
              className='mt-3'
              page={current}
              pageCount={pageCount}
              onPageChange={setPage}
              pageSize={PAGE}
              total={all.length}
            />
          ) : null}
        </>
      )}
    </div>
  )
}
