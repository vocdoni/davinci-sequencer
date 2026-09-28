import { CheckMark } from '~components'
import { Badge, Callout, Hash, Panel } from '~kit'
import { KNOWN_RELEASES, PIN_LABELS, type ReleaseMatch } from '~protocol/releases'
import { PIN_DETAILS, releaseVerdict } from './model'
import { Code } from './parts'

const lowerFirst = (s: string) => s.charAt(0).toLowerCase() + s.slice(1)

export function ReleasePanel({ match }: { match: ReleaseMatch }) {
  const verdict = releaseVerdict(match)
  const r = match.closest
  return (
    <Panel
      title='Release check'
      label='Pins against known releases'
      description='The registry’s pins compared with the davinci-zkvm releases this explorer was built with: the program keys and setup root its Rust SDK pins, the verifier code hash and the ballot key hash.'
    >
      <div className='flex flex-wrap items-center gap-3' data-testid='release-verdict'>
        <Badge tone={verdict.tone === 'ok' ? 'ok' : verdict.tone === 'danger' ? 'danger' : 'neutral'} dot>
          {match.release ? 'Matches a release' : verdict.tone === 'danger' ? 'Differs' : 'Checking'}
        </Badge>
        <span className='text-[13px] text-silver'>{verdict.text}</span>
      </div>
      {r ? (
        <p className='mt-2 text-[12px] text-ash'>
          {r.label}: pins frozen on {r.date} at commit <Code>{r.commit}</Code>, proofs wrapped with ZisK {r.zisk}.{' '}
          {KNOWN_RELEASES.length > 1 ? `${KNOWN_RELEASES.length} releases known.` : null}
        </p>
      ) : null}

      <ul className='mt-4 flex flex-col divide-y divide-charcoal rounded-md border border-charcoal'>
        {match.checks.map((c) => {
          const state = c.ok == null ? 'unknown' : c.ok ? 'pass' : 'fail'
          return (
            <li key={c.pin} data-testid={`pin-${c.pin}`} data-state={state} className='flex gap-3 p-3.5'>
              <CheckMark state={state} className='mt-0.5' />
              <div className='min-w-0 flex-1'>
                <div className='flex flex-wrap items-baseline justify-between gap-x-4 gap-y-1'>
                  <span className='text-[13px] font-medium text-ghost'>{PIN_LABELS[c.pin]}</span>
                  <span className='text-[12px] text-ash'>
                    {state === 'pass' ? 'matches' : state === 'fail' ? 'differs' : 'not read yet'}
                  </span>
                </div>
                <dl className='mt-1.5 grid gap-x-4 gap-y-1 text-[12px] sm:grid-cols-[auto_minmax(0,1fr)]'>
                  <dt className='text-ash'>On chain</dt>
                  <dd className='min-w-0'>{c.actual ? <Hash value={c.actual} chars={10} /> : '…'}</dd>
                  <dt className='text-ash'>Release</dt>
                  <dd className='min-w-0'>
                    <Hash value={c.expected} chars={10} />
                  </dd>
                </dl>
                <p className={state === 'fail' ? 'mt-2 text-[12px] text-red' : 'mt-2 text-[12px] text-ash'}>
                  {state === 'fail' ? 'It differs, so ' : 'A mismatch would mean '}
                  {lowerFirst(PIN_DETAILS[c.pin].mismatch)}
                </p>
              </div>
            </li>
          )
        })}
      </ul>

      <Callout className='mt-4' title='What a difference means'>
        The explorer only knows the releases it was built with, so a difference can also mean a newer release it has not
        heard of. Either way the registry verifies something other than those releases, and a sequencer built on them
        refuses to start against it: its boot check compares the same values, names each one that differs and has no
        override. Check the pins against the release notes of davinci-zkvm, or rebuild them from source below.
      </Callout>
    </Panel>
  )
}
