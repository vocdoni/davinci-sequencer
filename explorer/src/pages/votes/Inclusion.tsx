import { Link } from 'react-router-dom'
import { CheckMark, Timestamp, TxLink } from '~components'
import { Disclosure } from '~components/code'
import { useTransitionBlobs, type DecodedTransitionBlobs, type VoteInclusion } from '~data/queries'
import type { TransitionRow } from '~indexer/selectors'
import { BlockCell, Callout, Hash, KeyValue, Panel, ProgressBar, SkeletonText } from '~kit'
import { formatNumber } from '~lib/format'
import { paths } from '~routes/paths'

/** Where the blob that lists the vote came from, and what ties it to the transaction. */
function blobOrigin(data: DecodedTransitionBlobs): { text: string; warn: boolean } {
  if (data.source === 'sequencer' || data.blobs.some((b) => b.binding === 'sequencer')) {
    return {
      text: `The blob came from ${data.sourceUrl}, from a sequencer's archive, not checked against the transaction's blob hashes.`,
      warn: true,
    }
  }
  if (data.blobs.some((b) => b.binding === 'beacon-filter')) {
    return {
      text: `The blob came from the beacon API ${data.sourceUrl}, which selected it by the transaction's versioned hash.`,
      warn: false,
    }
  }
  return {
    text: `The blob came from the beacon API ${data.sourceUrl}; its KZG commitment hashes to the transaction's versioned hash.`,
    warn: false,
  }
}

/**
 * Where the vote id landed on-chain: the settled transition whose blob lists
 * it. Needs only the beacon (or a sequencer's blob archive), no sequencer API.
 */
export function Inclusion({
  pid,
  inclusion,
  transitions,
  txsKnown,
}: {
  pid: string
  inclusion: VoteInclusion
  transitions: TransitionRow[]
  /** Transitions whose settlement transaction the indexer has read. */
  txsKnown: number
}) {
  const found = inclusion.transitionIndex != null ? transitions[inclusion.transitionIndex] : undefined
  // The search already fetched these blobs; this reads them back from the cache.
  const blobs = useTransitionBlobs(pid, found?.index, { enabled: found != null })
  const origin = found && blobs.data ? blobOrigin(blobs.data) : null

  return (
    <Panel
      label='On-chain'
      title='Inclusion'
      description="Every settled batch lists the vote ids it inserted in its blob. The explorer reads the process's blobs, newest first, until it finds this one."
    >
      <div className='flex flex-col gap-3' data-testid='vote-inclusion'>
        {transitions.length === 0 ? (
          <Callout title='No batch has settled for this process yet'>
            A vote shows up here once the batch carrying it settles on the registry.
          </Callout>
        ) : inclusion.state === 'idle' ? (
          <div className='flex flex-col gap-2'>
            <p className='text-[13px] text-ash'>
              Waiting for the indexer to read the settlement transactions ({formatNumber(txsKnown)} of{' '}
              {formatNumber(transitions.length)}).
            </p>
            <SkeletonText lines={2} />
          </div>
        ) : inclusion.state === 'searching' ? (
          <ProgressBar
            value={inclusion.checked}
            total={inclusion.total}
            label='Reading blobs, newest transition first'
            tone='neutral'
          />
        ) : inclusion.state === 'found' && found ? (
          <>
            <p className='flex items-start gap-2 text-[13px] text-silver'>
              <CheckMark state='pass' className='mt-0.5' />
              <span>
                Listed in the blob of{' '}
                <Link to={paths.transition(pid, found.index)} className='text-emerald hover:underline'>
                  transition #{found.index}
                </Link>
                . That batch inserted the vote id into the state tree, the zkVM proof covers the insertion and the
                registry settled it.
              </span>
            </p>
            {origin ? (
              <p
                className={`text-xs break-words ${origin.warn ? 'text-amber' : 'text-ash'}`}
                data-testid='vote-inclusion-source'
              >
                {origin.text}
              </p>
            ) : null}
            <KeyValue
              columns={2}
              items={[
                {
                  label: 'Block',
                  value: (
                    <span className='inline-flex items-center gap-2'>
                      <BlockCell block={found.block} />
                      <Timestamp value={found.timestamp} className='text-ash' />
                    </span>
                  ),
                },
                { label: 'Transaction', value: found.tx ? <TxLink hash={found.tx} chars={8} /> : '—' },
                { label: 'Root after', value: <Hash value={found.rootAfter} chars={8} /> },
                {
                  label: 'Batch',
                  value: `${formatNumber(found.votes)} ballots · ${formatNumber(found.nBlobs)} blob${found.nBlobs === 1 ? '' : 's'}`,
                },
              ]}
            />
          </>
        ) : inclusion.state === 'not-found' ? (
          <Callout tone='warn' title={`Not in any of the ${formatNumber(inclusion.total)} settled transitions`}>
            The vote may still be waiting at a sequencer, or it belongs to another process, or the id has a typo.
            {inclusion.errors.length > 0
              ? ` ${inclusion.errors.length} transition${inclusion.errors.length === 1 ? "'s blobs" : "s' blobs"} could not be read, so it may be in one of those.`
              : null}
          </Callout>
        ) : (
          <Callout tone='danger' title='The blobs could not be read'>
            None of this process's blobs could be fetched. Beacon nodes prune blobs after about 15 days on Gnosis Chain
            (16384 epochs of 80 s) and about 18 on Ethereum mainnet; without a sequencer that archived them the vote ids
            are no longer available here. The tracker proof, when a sequencer serves one, does not need the blobs.
          </Callout>
        )}
        {inclusion.errors.length > 0 ? (
          <Disclosure
            summary={`${inclusion.errors.length} transition${inclusion.errors.length === 1 ? '' : 's'} not read`}
          >
            <ul className='flex flex-col gap-1 font-mono text-[11px] break-all text-ash'>
              {inclusion.errors.map((e, i) => (
                <li key={i}>{e}</li>
              ))}
            </ul>
          </Disclosure>
        ) : null}
      </div>
    </Panel>
  )
}
