import { Timestamp, TxLink } from '~components'
import { useChainNow, type ProcessView } from '~data/hooks'
import { Card } from '~kit'
import { cn } from '~lib/cn'
import { processLifecycle, type StepState } from './timeline'

const DOT: Record<StepState, string> = {
  done: 'bg-emerald border-emerald',
  current: 'bg-transparent border-emerald ring-4 ring-emerald/15',
  upcoming: 'bg-transparent border-warm-gray',
  skipped: 'bg-transparent border-charcoal',
}

const STATE_LABEL: Record<StepState, string> = {
  done: 'done',
  current: 'in progress',
  upcoming: 'not yet',
  skipped: 'skipped',
}

/** created → start → transitions → end → results, as a strip (a list on a phone). */
export function Lifecycle({ view }: { view: ProcessView }) {
  const now = useChainNow()
  const steps = processLifecycle(view, now)
  return (
    <Card data-testid='process-lifecycle' className='px-5 py-4'>
      <ol className='grid gap-4 sm:grid-cols-5 sm:gap-2' aria-label='Process lifecycle'>
        {steps.map((step, i) => (
          <li key={step.id} className='relative flex items-start gap-3 sm:flex-col sm:gap-2'>
            <div className='relative mt-1 flex items-center sm:mt-0 sm:w-full'>
              <span
                aria-label={STATE_LABEL[step.state]}
                role='img'
                className={cn('z-10 h-3 w-3 shrink-0 rounded-full border-2', DOT[step.state])}
              />
              {i < steps.length - 1 ? (
                <span
                  aria-hidden='true'
                  className={cn(
                    'absolute top-1.5 left-3 hidden h-px w-[calc(100%-4px)] sm:block',
                    step.state === 'done' ? 'bg-emerald/50' : 'bg-charcoal'
                  )}
                />
              ) : null}
            </div>
            <div className='min-w-0 sm:pr-3'>
              <div
                className={cn(
                  'text-[13px] font-medium',
                  step.state === 'skipped' ? 'text-ash' : step.state === 'upcoming' ? 'text-pewter' : 'text-ghost'
                )}
              >
                {step.label}
              </div>
              <div className='text-xs text-ash'>{step.detail}</div>
              {step.time != null ? (
                <div>
                  <Timestamp value={step.time} className='text-xs text-silver' />
                </div>
              ) : null}
              {step.tx ? (
                <div className='-ml-0.5'>
                  <TxLink hash={step.tx} chars={4} />
                </div>
              ) : null}
            </div>
          </li>
        ))}
      </ol>
    </Card>
  )
}
