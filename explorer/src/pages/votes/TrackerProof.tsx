import type { UseQueryResult } from '@tanstack/react-query'
import { Link } from 'react-router-dom'
import { CheckMark } from '~components'
import { CodeBlock, Disclosure } from '~components/code'
import type { TrackerCheck } from '~data/queries'
import type { TransitionRow } from '~indexer/selectors'
import { Callout, Hash, Panel, SkeletonText } from '~kit'
import { formatVoteId } from '~protocol/blob'
import { SMT_LEVELS } from '~protocol/limits'
import { paths } from '~routes/paths'

/**
 * A sequencer's tracker proof, recomputed in the browser: the vote-id leaf
 * hashes up to a root, and that root is one the registry held.
 */
export function TrackerProof({
  pid,
  voteId,
  tracker,
  sequencers,
  transitions,
  genesisRoot,
}: {
  pid: string
  voteId: bigint
  tracker: UseQueryResult<TrackerCheck | null>
  sequencers: number
  transitions: TransitionRow[]
  genesisRoot: string | null
}) {
  const data = tracker.data
  const root = data?.proof.root.toLowerCase()
  const at = root ? transitions.find((t) => t.rootAfter === root) : undefined

  return (
    <Panel
      label='Recorded as cast'
      title='Tracker proof'
      description="A sequencer's proof that the vote id is a leaf of the state tree under a root the registry holds. The explorer recomputes the path here: the leaf is sha256(vote id as 8 little-endian bytes ‖ 32 zero bytes ‖ 0x01), each level hashes sha256(left ‖ right), and the vote id's bits say which side, the lowest bit at the root."
    >
      <div className='flex flex-col gap-3' data-testid='tracker-proof'>
        {sequencers === 0 ? (
          <Callout title='No sequencer is configured'>
            Tracker proofs come from a sequencer node's copy of the state tree, and this explorer has none to ask. The
            inclusion check does the same job from the blobs.
          </Callout>
        ) : tracker.isLoading ? (
          <SkeletonText lines={3} />
        ) : tracker.error ? (
          <Callout tone='warn' title='No tracker proof'>
            {tracker.error.message}
          </Callout>
        ) : data == null ? (
          <Callout title='No configured sequencer knows this vote'>
            A node serves a tracker proof once the vote id is in its tree, which happens when the batch carrying it
            settles.
          </Callout>
        ) : (
          <>
            <ul className='flex flex-col gap-2 text-[13px] text-silver'>
              <li className='flex items-start gap-2'>
                <CheckMark state={data.valid ? 'pass' : 'fail'} className='mt-0.5' />
                <span>
                  {data.otherVote
                    ? 'The sequencer answered with a proof for another vote'
                    : data.valid
                      ? 'The path reaches the root the proof names'
                      : 'The path does not reach its root'}
                  <span className='block text-[12px] text-ash'>
                    {data.otherVote ? (
                      <>
                        it names vote {formatVoteId(data.proof.voteId)}
                        {data.proof.processId?.toLowerCase() !== pid.toLowerCase()
                          ? ` of process ${data.proof.processId}`
                          : ''}
                        , not the one asked for, from {data.sequencer.upstream}
                      </>
                    ) : (
                      <>
                        {data.proof.siblings.length} sibling{data.proof.siblings.length === 1 ? '' : 's'} of at most{' '}
                        {SMT_LEVELS} levels, from {data.sequencer.upstream}
                      </>
                    )}
                  </span>
                </span>
              </li>
              <li className='flex items-start gap-2'>
                <CheckMark state={data.rootOnChain ? 'pass' : 'fail'} className='mt-0.5' />
                <span>
                  {data.rootOnChain
                    ? 'That root is one the registry held for this process'
                    : 'That root is not one the registry held for this process'}
                  <span className='block text-[12px] text-ash'>
                    {at ? (
                      <>
                        the root after{' '}
                        <Link to={paths.transition(pid, at.index)} className='text-silver hover:text-emerald'>
                          transition #{at.index}
                        </Link>
                      </>
                    ) : root && root === genesisRoot ? (
                      'the genesis root'
                    ) : data.rootOnChain ? (
                      'the latest root'
                    ) : (
                      'not the genesis root nor any transition root'
                    )}
                  </span>
                </span>
              </li>
            </ul>
            <div className='flex items-center gap-2 text-[12px] text-ash'>
              Root <Hash value={data.proof.root} chars={10} />
            </div>
            <Disclosure summary='The proof as served'>
              <CodeBlock
                code={JSON.stringify(
                  {
                    processId: data.proof.processId,
                    voteId: formatVoteId(data.proof.voteId),
                    root: data.proof.root,
                    siblings: data.proof.siblings,
                  },
                  null,
                  2
                )}
                label='Copy the tracker proof'
                maxHeight={280}
              />
            </Disclosure>
            <Disclosure summary='Fetch it yourself'>
              <p className='mb-2 text-[12px] text-ash'>
                The node route is in the sequencer README; davinci_client::api::verify_tracker checks the answer against
                the registry.
              </p>
              <CodeBlock
                code={`curl ${data.sequencer.upstream.replace(/\/+$/, '')}/votes/${pid}/voteId/${formatVoteId(voteId)}/proof`}
                label='Copy the command'
              />
            </Disclosure>
          </>
        )}
      </div>
    </Panel>
  )
}
