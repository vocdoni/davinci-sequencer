import { useMemo } from 'react'
import { Navigate, useParams, useSearchParams } from 'react-router-dom'
import { ProcessIdLink } from '~components'
import { useServices } from '~data/context'
import { useIndexer, useStore, useTransitions } from '~data/hooks'
import { useTrackerProof, useVoteInclusion, useVoteStatus, type VoteInclusion } from '~data/queries'
import { processKey, txKey } from '~indexer/types'
import { Callout, Card, SectionHeader, SkeletonText, Stack } from '~kit'
import { formatVoteId } from '~protocol/blob'
import { paths } from '~routes/paths'
import { VoteExplainers } from './Explainers'
import { Inclusion } from './Inclusion'
import { LookupForm } from './LookupForm'
import { validateLookup } from './lookup'
import { SequencerStatus } from './SequencerStatus'
import { TrackerProof } from './TrackerProof'

/**
 * Vote lookup: where a vote stands at the sequencers, which settled
 * transition carries its vote id, the tracker proof checked against an
 * on-chain root, and what all of that does and does not say.
 */
export function VotesPage() {
  const params = useParams()
  const [search] = useSearchParams()
  const pidInput = params.pid ?? search.get('pid') ?? ''
  const voteInput = params.voteId ?? search.get('voteId') ?? ''
  const query = validateLookup(pidInput, voteInput)
  const onRoute = params.pid != null
  const active = onRoute && query.pid != null && query.voteId != null

  // /votes?pid=…&voteId=… with both valid: the shareable path.
  if (!onRoute && query.pid && query.voteId != null) {
    return <Navigate replace to={paths.vote(query.pid, formatVoteId(query.voteId))} />
  }

  return (
    <Stack data-testid='page-votes'>
      <SectionHeader
        size='page'
        label='Votes'
        title='Check a vote'
        description='Enter the process and the vote id your voting app gave you: see where the vote stands, which settled batch carries it on-chain and what that proves.'
      />
      <LookupForm key={`${pidInput}|${voteInput}`} initialPid={pidInput} initialVote={voteInput} />
      {!onRoute && query.voteId != null && !query.pid ? (
        <Callout title='Which process?'>
          Vote ids are unique within a process, so the lookup needs the process id too. Pick it above.
        </Callout>
      ) : null}
      {onRoute && !active ? (
        <Callout tone='warn' title='This lookup address is not valid'>
          {[query.pidError, query.voteError].filter(Boolean).join(' ')}
        </Callout>
      ) : null}
      {active ? <Lookup pid={query.pid!} voteId={query.voteId!} /> : null}
      <VoteExplainers pid={active ? query.pid : null} />
    </Stack>
  )
}

function inclusionWords(i: VoteInclusion): string {
  switch (i.state) {
    case 'found':
      return `found in transition #${i.transitionIndex}`
    case 'searching':
      return `searching, ${i.checked} of ${i.total} transitions read`
    case 'not-found':
      return 'not in any settled transition'
    case 'error':
      return 'blobs unavailable'
    case 'idle':
      return 'waiting for the indexer'
  }
}

function Lookup({ pid, voteId }: { pid: string; voteId: bigint }) {
  const store = useStore()
  const { status } = useIndexer()
  const services = useServices()
  const process = store.processes[processKey(pid)]
  const transitions = useTransitions(pid)
  const statuses = useVoteStatus(process ? pid : undefined, process ? voteId : null)
  const inclusion = useVoteInclusion(process ? pid : undefined, process ? voteId : null)
  const tracker = useTrackerProof(process ? pid : undefined, process ? voteId : null)
  const txsKnown = useMemo(
    () => transitions.filter((t) => t.tx != null && store.txDetails[txKey(t.tx)] != null).length,
    [transitions, store]
  )

  if (!process) {
    if (status.phase === 'idle' || status.phase === 'loading' || status.scanning) return <SkeletonText lines={4} />
    return (
      <Callout tone='warn' title='The registry has no such process'>
        No process {pid} was created on this registry. Check the id, or the network the explorer points at.
      </Callout>
    )
  }

  const answered = statuses.filter((s) => s.status.data).length
  return (
    <div className='flex flex-col gap-6'>
      <Card>
        <p className='text-[13px] text-silver' data-testid='vote-summary'>
          Vote <span className='font-mono text-ghost'>{formatVoteId(voteId)}</span> in process{' '}
          <ProcessIdLink id={process.id} chars={8} /> ·{' '}
          {statuses.length ? `${answered}/${statuses.length} sequencers answered` : 'no sequencer configured'} ·
          inclusion: {inclusionWords(inclusion)}
        </p>
      </Card>
      <Inclusion pid={process.id} inclusion={inclusion} transitions={transitions} txsKnown={txsKnown} />
      <div className='grid gap-6 lg:grid-cols-2'>
        <SequencerStatus statuses={statuses} />
        <TrackerProof
          pid={process.id}
          voteId={voteId}
          tracker={tracker}
          sequencers={services.sequencers.length}
          transitions={transitions}
          genesisRoot={process.genesisRoot}
        />
      </div>
    </div>
  )
}
