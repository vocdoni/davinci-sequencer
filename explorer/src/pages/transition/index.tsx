import { Link, useParams } from 'react-router-dom'
import { HashLink, MissingEntity, ProcessIdLink } from '~components'
import { useChainNow, useTransition, useTransitions } from '~data/hooks'
import { useTransitionBlobs } from '~data/queries'
import { buttonClasses, ChevronLeftIcon, ChevronRightIcon, SectionHeader, Stack } from '~kit'
import { cn } from '~lib/cn'
import { paths } from '~routes/paths'
import { BlobsPanel } from './BlobsPanel'
import { ProofPanel } from './ProofPanel'
import { PublicsPanel } from './PublicsPanel'
import { TransitionSummary } from './Summary'
import { VerifyPanel } from './VerifyPanel'

const SECTIONS = [
  ['publics', 'Public values'],
  ['blobs', 'Blobs'],
  ['proof', 'Proof'],
  ['verify', 'Verify it yourself'],
] as const

/**
 * One settled batch: its facts, the proof's public values, the blobs that
 * carry its data, the proof, and every check the registry ran with the
 * commands to rerun it.
 */
export function TransitionPage() {
  const { pid, index } = useParams()
  const i = index != null && /^\d+$/.test(index) ? Number(index) : undefined
  const detail = useTransition(pid, i)
  const all = useTransitions(pid)
  const blobs = useTransitionBlobs(pid, i)
  const now = useChainNow()

  if (!pid || i == null || !detail) return <MissingEntity what='transition' id={`#${index ?? ''} of ${pid ?? ''}`} />
  const { previous, next, process } = detail

  const nav = (to: number | null, label: string, dir: 'prev' | 'next') =>
    to == null ? (
      <span className={cn(buttonClasses('secondary', 'sm'), 'pointer-events-none opacity-40')} aria-disabled='true'>
        {dir === 'prev' ? <ChevronLeftIcon size={13} /> : null}
        {label}
        {dir === 'next' ? <ChevronRightIcon size={13} /> : null}
      </span>
    ) : (
      <Link to={paths.transition(process.id, to)} className={buttonClasses('secondary', 'sm')} rel={dir}>
        {dir === 'prev' ? <ChevronLeftIcon size={13} /> : null}
        {label}
        {dir === 'next' ? <ChevronRightIcon size={13} /> : null}
      </Link>
    )

  return (
    <Stack data-testid='page-transition'>
      <SectionHeader
        size='page'
        label='State transition'
        title={`Transition #${detail.transition.index}`}
        description={
          <>
            One batch of ballots for process <ProcessIdLink id={process.id} chars={8} />, proven by the zkVM and settled
            on the registry: its public values, the blobs carrying its data and the checks the registry ran.
          </>
        }
        actions={
          <>
            {nav(previous?.index ?? null, previous ? `#${previous.index}` : 'First', 'prev')}
            {nav(next?.index ?? null, next ? `#${next.index}` : 'Latest', 'next')}
          </>
        }
      />
      <TransitionSummary detail={detail} blobs={blobs} now={now} total={all.length} />
      <nav aria-label='Sections' className='-mt-2 flex flex-wrap gap-x-4 gap-y-1 text-[12px]'>
        {SECTIONS.map(([id, label]) => (
          <HashLink key={id} id={id} className='text-ash transition-colors hover:text-emerald'>
            {label}
          </HashLink>
        ))}
      </nav>
      <section id='publics' className='scroll-mt-20'>
        <PublicsPanel detail={detail} />
      </section>
      <section id='blobs' className='scroll-mt-20'>
        <BlobsPanel detail={detail} blobs={blobs} />
      </section>
      <section id='proof' className='scroll-mt-20'>
        <ProofPanel detail={detail} />
      </section>
      <section id='verify' className='scroll-mt-20'>
        <VerifyPanel detail={detail} />
      </section>
    </Stack>
  )
}
