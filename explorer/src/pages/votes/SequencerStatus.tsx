import { Explain } from '~components'
import type { VoteStatusBySequencer } from '~data/queries'
import { Badge, Callout, Panel, Skeleton, type BadgeTone } from '~kit'
import { cn } from '~lib/cn'
import { SequencerApiError } from '~protocol/sequencer-api'
import { STATUS_INFO, STATUS_STEPS, statusStep } from './lookup'

function errorText(err: Error): { tone: BadgeTone; label: string; text: string } {
  if (err instanceof SequencerApiError && err.status === 404) {
    return err.code === 40402
      ? { tone: 'neutral', label: 'Unknown process', text: 'This node does not serve this process.' }
      : {
          tone: 'neutral',
          label: 'Unknown vote',
          text: 'This node has no such vote: it never received it and has not seen its vote id settle.',
        }
  }
  return { tone: 'warn', label: 'Unreachable', text: err.message }
}

/** A vote's status at each configured sequencer: pending → aggregated → processed → settled. */
export function SequencerStatus({ statuses }: { statuses: VoteStatusBySequencer[] }) {
  return (
    <Panel
      label='At the sequencers'
      title='Sequencer status'
      description='What each configured sequencer node says about the vote. A node that never received the vote still answers settled once the vote id is in its copy of the state.'
    >
      {statuses.length === 0 ? (
        <Callout title='No sequencer is configured'>
          This explorer talks to the chain and the beacon only, so it cannot ask a sequencer where a vote waits before
          it settles. The on-chain inclusion check does not need one: once the vote is in a settled batch, its vote id
          is in that batch's blob.
        </Callout>
      ) : (
        <ul className='flex flex-col gap-4' data-testid='sequencer-status'>
          {statuses.map(({ endpoint, status }) => {
            const data = status.data
            const step = data ? statusStep(data.status) : null
            const err = status.error ? errorText(status.error) : null
            return (
              <li key={endpoint.index} className='flex flex-col gap-2'>
                <div className='flex flex-wrap items-center justify-between gap-2'>
                  <span className='truncate font-mono text-[12px] text-silver'>{endpoint.upstream}</span>
                  {data ? (
                    <Badge
                      tone={data.status === 'error' ? 'danger' : data.status === 'settled' ? 'ok' : 'accent'}
                      dot={data.status !== 'settled' && data.status !== 'error'}
                    >
                      {STATUS_INFO[data.status].label}
                    </Badge>
                  ) : err ? (
                    <Badge tone={err.tone}>{err.label}</Badge>
                  ) : null}
                </div>
                {status.isLoading ? <Skeleton className='h-6 w-full' /> : null}
                {step != null ? (
                  <ol className='grid grid-cols-4 gap-1' aria-label='Progress'>
                    {STATUS_STEPS.map((s, i) => {
                      const done = step >= 0 && i <= step
                      return (
                        <li key={s} className='flex min-w-0 flex-col gap-1'>
                          <span
                            className={cn(
                              'h-1.5 rounded-pill',
                              done ? 'bg-emerald' : step < 0 ? 'bg-red/30' : 'bg-onyx'
                            )}
                          />
                          <span
                            className={cn(
                              'inline-flex items-center gap-0.5 truncate text-[11px]',
                              done ? 'text-silver' : 'text-ash'
                            )}
                            aria-current={i === step ? 'step' : undefined}
                          >
                            {STATUS_INFO[s].label}
                            <Explain>{STATUS_INFO[s].description}</Explain>
                          </span>
                        </li>
                      )
                    })}
                  </ol>
                ) : null}
                {data?.status === 'error' ? (
                  <p className='text-[12px] text-red'>
                    {data.error ?? 'No reason given.'} <span className='text-ash'>{STATUS_INFO.error.description}</span>
                  </p>
                ) : data ? (
                  <p className='text-[12px] text-ash'>{STATUS_INFO[data.status].description}</p>
                ) : err ? (
                  <p className='text-[12px] break-words text-ash'>{err.text}</p>
                ) : null}
              </li>
            )
          })}
        </ul>
      )}
    </Panel>
  )
}
