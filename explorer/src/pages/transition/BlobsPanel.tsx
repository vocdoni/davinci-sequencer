import { useMemo } from 'react'
import type { UseQueryResult } from '@tanstack/react-query'
import { Explain } from '~components'
import { BlobCellView, BlobLayoutBar, CiphertextTable, countsOf, SlotUpdateList, VoteIdList } from '~components/blob'
import { Disclosure } from '~components/code'
import { useServices } from '~data/context'
import type { DecodedTransitionBlobs } from '~data/queries'
import type { TransitionDetail } from '~indexer/selectors'
import { Badge, Callout, EmptyState, Hash, Panel, Skeleton, SkeletonText, StatCell, StatRow, Tabs, Tooltip } from '~kit'
import { formatNumber } from '~lib/format'
import type { FetchedBlob } from '~protocol/beacon'
import { blobEvaluationPoint, usedCells } from '~protocol/blob'
import type { Hex } from '~protocol/bytes'
import { CELLS_PER_BLOB, requiredRefresh } from '~protocol/limits'

const BINDING: Record<FetchedBlob['binding'], { label: string; tone: 'ok' | 'warn'; hint: string }> = {
  commitment: {
    label: 'by commitment',
    tone: 'ok',
    hint: "The beacon returned this blob with a KZG commitment that hashes to the transaction's versioned hash. The explorer does not recompute the commitment from the bytes.",
  },
  'beacon-filter': {
    label: 'by versioned hash',
    tone: 'ok',
    hint: 'The beacon selected this blob by the versioned hash. The explorer does not recompute the commitment from the bytes.',
  },
  sequencer: {
    label: 'by position',
    tone: 'warn',
    hint: 'A sequencer node served this blob for this transition. Nothing ties the bytes to the versioned hash but the node’s word; the on-chain checks below still hold for the real blob.',
  },
}

const COLUMNS: Array<[string, string | null]> = [
  ['Blob', null],
  [
    'Versioned hash',
    'What the transaction commits to and what BLOBHASH returns: 0x01 followed by the last 31 bytes of sha256(commitment).',
  ],
  [
    'KZG commitment',
    '48 bytes that commit to the blob as a polynomial. It comes from the calldata; the guest used it to derive the evaluation point.',
  ],
  [
    'Point z',
    'sha256(process id ‖ root before ‖ commitment) mod the BLS12-381 scalar field, computed here. It ties the blob to this process and this starting root, so a commitment from another transition is useless.',
  ],
  [
    'Evaluation y',
    'The blob polynomial at z, as the guest computed it from the data it proved. The registry checks the KZG opening of the real blob against it.',
  ],
  ['KZG proof', '48-byte opening proof that the blob evaluates to y at z.'],
  [
    'Bytes',
    'How the explorer tied the bytes it shows to the transaction, and where it got them. Hover a badge for details.',
  ],
]

/** Pruned by the beacon, and nothing else to ask. */
function isPruned(message: string): boolean {
  return /not found|pruned|404/i.test(message)
}

/**
 * The transition's EIP-4844 blobs: what binds each one to the proof, and the
 * data they carry once fetched from the beacon (or a sequencer) and decoded.
 */
export function BlobsPanel({
  detail,
  blobs,
}: {
  detail: TransitionDetail
  blobs: UseQueryResult<DecodedTransitionBlobs>
}) {
  const { tx, transition: t, publics } = detail
  const services = useServices()
  const points = useMemo<Hex[]>(
    () => (tx ? tx.commitments.map((c) => blobEvaluationPoint(t.processId, t.rootBefore, c)) : []),
    [tx, t.processId, t.rootBefore]
  )
  const decoded = blobs.data?.decoded ?? null
  const refreshes = decoded && publics ? decoded.updates.length - publics.voters : null
  const minRefresh = publics ? requiredRefresh(publics.voters, publics.overwrites, publics.occupiedBefore) : null

  return (
    <Panel
      label='Data availability'
      title='Blobs'
      description='Everything needed to rebuild the state after this batch travels in EIP-4844 blobs attached to the settlement transaction. Anyone can replay them onto the previous state and get the new root.'
    >
      <div className='flex flex-col gap-5'>
        <Layout />

        {!tx ? (
          <SkeletonText lines={4} />
        ) : (
          <div className='scroll-slim overflow-x-auto'>
            <table className='w-full min-w-[980px] border-collapse text-[12px]' data-testid='blob-list'>
              <thead>
                <tr className='label-caps text-left text-[10px] text-pewter'>
                  {COLUMNS.map(([label, hint]) => (
                    <th key={label} scope='col' className='border-b border-charcoal px-2 py-2 font-semibold'>
                      <span className='inline-flex items-center gap-1'>
                        {label}
                        {hint ? <Explain>{hint}</Explain> : null}
                      </span>
                    </th>
                  ))}
                </tr>
              </thead>
              <tbody>
                {(tx.blobVersionedHashes ?? []).map((vh, i) => {
                  const fetched = blobs.data?.blobs[i]
                  const cell = (v: Hex | undefined) => (v ? <Hash value={v} chars={6} /> : '—')
                  return (
                    <tr key={vh} className='border-b border-charcoal/60 last:border-b-0'>
                      <td className='px-2 py-2 font-mono text-silver tnum'>{i}</td>
                      <td className='px-2 py-2'>{cell(vh)}</td>
                      <td className='px-2 py-2'>{cell(tx.commitments[i])}</td>
                      <td className='px-2 py-2'>{cell(points[i])}</td>
                      <td className='px-2 py-2'>{cell(tx.ys[i])}</td>
                      <td className='px-2 py-2'>{cell(tx.kzgProofs[i])}</td>
                      <td className='px-2 py-2'>
                        {fetched ? (
                          <Tooltip content={BINDING[fetched.binding].hint}>
                            <span className='inline-flex'>
                              <Badge size='sm' tone={BINDING[fetched.binding].tone}>
                                {BINDING[fetched.binding].label} · {fetched.source}
                              </Badge>
                            </span>
                          </Tooltip>
                        ) : (
                          <span className='text-ash'>{blobs.isLoading ? 'fetching…' : 'not fetched'}</span>
                        )}
                      </td>
                    </tr>
                  )
                })}
              </tbody>
            </table>
          </div>
        )}

        <Content detail={detail} blobs={blobs} sequencers={services.sequencers.length} />

        {decoded ? (
          <div className='flex flex-col gap-4' data-testid='blob-content'>
            <StatRow>
              <StatCell
                label='Vote ids'
                value={formatNumber(decoded.voteIds.length)}
                hint='one per ballot, sorted'
                mono
              />
              <StatCell
                label='Slot updates'
                value={formatNumber(decoded.updates.length)}
                hint={
                  refreshes != null && refreshes >= 0
                    ? `${formatNumber(publics!.voters)} ballots + ${formatNumber(refreshes)} silent refreshes`
                    : 'every slot the batch wrote'
                }
                mono
              />
              <StatCell
                label='Fields per ballot'
                value={decoded.numFields}
                hint='ciphertexts per slot and in the accumulator'
                mono
              />
              <StatCell
                label='Cells used'
                value={formatNumber(usedCells(decoded))}
                hint={`of ${formatNumber(blobs.data!.blobs.length * CELLS_PER_BLOB)}; the rest is zero`}
                mono
              />
            </StatRow>
            {refreshes != null && minRefresh != null ? (
              <p className='text-[12px] text-ash'>
                The guest required at least {formatNumber(minRefresh)} silent refreshes for this batch
                <Explain>
                  min(target, occupied_before − overwrites), with target = min(2048, max(16, 2 × overwrites, votes)).
                  The count is public; which slots were refreshed is not.
                </Explain>{' '}
                and the blob carries {formatNumber(refreshes)}.
              </p>
            ) : null}
            <BlobLayoutBar counts={countsOf(decoded)} nBlobs={blobs.data!.blobs.length} />
            <Tabs
              items={[
                {
                  value: 'vote-ids',
                  label: 'Vote ids',
                  meta: formatNumber(decoded.voteIds.length),
                  content: <VoteIdList processId={t.processId} voteIds={decoded.voteIds} />,
                },
                {
                  value: 'updates',
                  label: 'Slot updates',
                  meta: formatNumber(decoded.updates.length),
                  content: (
                    <div className='flex flex-col gap-3'>
                      <p className='text-[12px] text-ash'>
                        A slot holds one voter's current ballot. For a Merkle census its key comes from the voter's
                        address, for a CSP census from the index the CSP signed. The ciphertexts are the re-encrypted
                        ballot, compressed to one cell per point; open a row to unpack them.
                      </p>
                      <SlotUpdateList updates={decoded.updates} />
                    </div>
                  ),
                },
                {
                  value: 'accumulator',
                  label: 'Accumulator',
                  meta: decoded.accumulator.length,
                  content: (
                    <div className='flex flex-col gap-3'>
                      <p className='text-[12px] text-ash'>
                        The encrypted tally after this batch, one ciphertext per ballot field: the previous accumulator
                        plus every new ballot, minus the ballots they replaced, plus the refresh deltas (encryptions of
                        zero). Only the final one is decrypted by the protocol, when the results are published. The
                        holder of the election key could decrypt this one, or any ballot in the blobs.
                      </p>
                      <CiphertextTable ciphertexts={decoded.accumulator} />
                    </div>
                  ),
                },
                {
                  value: 'cells',
                  label: 'Cells',
                  meta: formatNumber(blobs.data!.blobs.length * CELLS_PER_BLOB),
                  content: <BlobCellView blobs={blobs.data!.blobs.map((b) => b.data)} counts={countsOf(decoded)} />,
                },
              ]}
            />
          </div>
        ) : blobs.data && blobs.data.decodeError ? (
          <Disclosure summary='Raw cells'>
            <BlobCellView blobs={blobs.data.blobs.map((b) => b.data)} counts={null} />
          </Disclosure>
        ) : null}
      </div>
    </Panel>
  )
}

function Layout() {
  return (
    <div className='grid gap-4 text-[13px] leading-relaxed text-ash md:grid-cols-2'>
      <p>
        The zkVM guest lays out the blob cells itself from the state it just proved, so the sequencer cannot publish
        anything else. A cell is a 32-byte number. In order: the vote ids of the batch, sorted; one list of slot updates
        sorted by slot, each with its encrypted ballot compressed to one cell per curve point; then the new encrypted
        tally (the accumulator); zeros to the end of the last blob.
      </p>
      <p>
        New votes, overwrites and silent refreshes all appear as the same kind of slot update. Each batch also
        re-encrypts a random sample of occupied slots it did not write, so the blob does not say which occupied slots
        were overwritten and which were only refreshed. A slot’s first write is public, though, because refreshes only
        touch occupied slots; with a Merkle census the slot follows from the address, so who voted and when is public.
        The vote ids and the slot updates are sorted separately. In a small batch the new vote ids and the new slots can
        still be matched.
      </p>
    </div>
  )
}

function Content({
  detail,
  blobs,
  sequencers,
}: {
  detail: TransitionDetail
  blobs: UseQueryResult<DecodedTransitionBlobs>
  sequencers: number
}) {
  if (blobs.data?.decoded) {
    if (blobs.data.attempts.length === 0) return null
    return <Attempts attempts={blobs.data.attempts} intro='Found after these sources failed:' />
  }
  if (blobs.data?.decodeError) {
    return (
      <Callout tone='danger' title='The blob data does not decode'>
        {blobs.data.decodeError}. The decoder checks the layout the guest produces; a transition that settled can only
        fail here if the bytes are not the blob the transaction carried, or the ballot field count is not known yet.
      </Callout>
    )
  }
  if (blobs.error) {
    const message = blobs.error.message
    if (isPruned(message) && sequencers === 0) {
      return (
        <Callout tone='warn' title='The beacon no longer serves these blobs, and no sequencer is configured'>
          <p>
            Beacon nodes keep blobs for about 15 days on Gnosis Chain (16384 epochs of 80 s) and about 18 on Ethereum
            mainnet, then prune them. This transition's blobs are gone from the configured beacon, and this explorer has
            no sequencer node to ask: nodes archive the blobs they saw and serve them at /processes/{'{pid}'}
            /transitions/{'{index}'}/blobs.
          </p>
          <p className='mt-2'>
            The settlement is not in doubt. The commitments, evaluations and KZG proofs are in the calldata and the
            registry checked them against the real blobs when they were fresh; the checks below still hold. Only the
            content cannot be shown.
          </p>
          <Attempts attempts={[]} raw={message} />
        </Callout>
      )
    }
    return (
      <Callout tone='danger' title='The blobs could not be fetched'>
        <p>{message}</p>
      </Callout>
    )
  }
  if (!detail.tx) return <Skeleton className='h-24 w-full' />
  if (blobs.isLoading) {
    return (
      <div className='flex flex-col gap-2' aria-busy='true'>
        <p className='text-[12px] text-ash'>Fetching the blobs from the beacon…</p>
        <Skeleton className='h-24 w-full' />
      </div>
    )
  }
  return (
    <EmptyState
      compact
      title='Waiting for the transaction'
      description='The blobs are fetched once the settlement transaction and the ballot field count are known.'
    />
  )
}

function Attempts({
  attempts,
  intro,
  raw,
}: {
  attempts: DecodedTransitionBlobs['attempts']
  intro?: string
  raw?: string
}) {
  return (
    <Disclosure summary='Sources tried' className='mt-3'>
      {intro ? <p className='mb-2 text-[12px] text-ash'>{intro}</p> : null}
      <ul className='flex flex-col gap-1 font-mono text-[11px] break-all text-ash'>
        {attempts.map((a, i) => (
          <li key={i}>
            {a.source} {a.url}: {a.error}
          </li>
        ))}
        {raw ? <li>{raw}</li> : null}
      </ul>
    </Disclosure>
  )
}
