import { useEffect, type ComponentType } from 'react'
import { Link, useLocation, useNavigate, useParams } from 'react-router-dom'
import { Card, ChevronLeftIcon, ChevronRightIcon, EmptyState, SectionHeader, Select, Stack } from '~kit'
import { cn } from '~lib/cn'
import { paths } from '~routes/paths'
import { Blobs } from './content/Blobs'
import { Census } from './content/Census'
import { Glossary } from './content/Glossary'
import { HowItWorks } from './content/HowItWorks'
import { KeyModes } from './content/KeyModes'
import { Results } from './content/Results'
import { Settlement } from './content/Settlement'
import { SilentRevoting } from './content/SilentRevoting'
import { VerifyAuditor } from './content/VerifyAuditor'
import { VerifyOrganizer } from './content/VerifyOrganizer'
import { VerifyVoter } from './content/VerifyVoter'
import { useLearnExamples, type LearnExamples } from './examples'
import { TOPIC_GROUPS, TOPICS, findTopic, neighbours, type TopicMeta } from './topics'

const CONTENT: Record<string, ComponentType<{ ex: LearnExamples }>> = {
  'how-it-works': HowItWorks,
  'key-modes': KeyModes,
  census: Census,
  'silent-revoting': SilentRevoting,
  blobs: Blobs,
  settlement: Settlement,
  results: Results,
  'verify-voter': VerifyVoter,
  'verify-organizer': VerifyOrganizer,
  'verify-auditor': VerifyAuditor,
  glossary: Glossary,
}

/** `/learn` (the index) and `/learn/:topic`. */
export function LearnPage() {
  const { topic } = useParams()
  const { hash } = useLocation()
  const meta = findTopic(topic)

  // A deep link (#term-vote-id, #ballot-slots) lands once the topic has rendered.
  useEffect(() => {
    if (hash) document.getElementById(decodeURIComponent(hash.slice(1)))?.scrollIntoView()
  }, [topic, hash])

  return (
    <Stack data-testid='page-learn'>
      {!topic ? <LearnIndex /> : meta ? <TopicPage meta={meta} /> : <UnknownTopic slug={topic} />}
    </Stack>
  )
}

const ROLES = [
  {
    slug: 'verify-voter',
    who: 'I voted',
    text: 'Find the transition that included your vote, check its tracker proof and see it counted.',
  },
  {
    slug: 'verify-organizer',
    who: 'I run a process',
    text: 'Check what the registry stored, the key, the batches as they settle and the results.',
  },
  {
    slug: 'verify-auditor',
    who: 'I audit the deployment',
    text: 'The pinned keys, every transition, the data behind it and the results, without trusting a sequencer.',
  },
]

function LearnIndex() {
  return (
    <>
      <SectionHeader
        size='page'
        label='Learn'
        title='How DAVINCI works'
        description='A guide to what this explorer shows: how votes become a proven tally, whom each part trusts, and how to check every step yourself.'
      />

      <Card data-testid='learn-summary'>
        <h2 className='text-[15px] font-semibold text-ghost'>In short</h2>
        <div className='mt-2 grid gap-x-10 gap-y-3 text-[14px] leading-[1.7] text-pewter lg:grid-cols-2'>
          <p>
            Voters encrypt their ballots and prove in zero knowledge that each one is valid. Sequencers group the
            ballots into batches, and a single zkVM program proves everything about a batch at once: every ballot proof,
            every signature, census membership, the updated state and the encrypted tally.
          </p>
          <p>
            Each batch settles on the ProcessRegistry contract with its proof and the EIP-4844 blobs that publish what
            changed, so anyone can rebuild the state. At the end the protocol decrypts only the final encrypted sum, by
            the sequencer that holds the key or by a DKG committee, and that step is proven too.
          </p>
        </div>
        <Link
          to={paths.learn('how-it-works')}
          className='mt-4 inline-flex items-center gap-1 text-[13px] font-medium text-emerald hover:underline'
        >
          The full walk-through <ChevronRightIcon size={14} />
        </Link>
      </Card>

      <section aria-labelledby='learn-roles'>
        <h2 id='learn-roles' className='label-caps mb-3 text-[11px] text-pewter'>
          Check it yourself
        </h2>
        <div className='grid gap-4 md:grid-cols-3'>
          {ROLES.map((r) => (
            <Link
              key={r.slug}
              to={paths.learn(r.slug)}
              className='group rounded-md border border-charcoal bg-carbon p-5 transition-colors hover:border-emerald/50'
            >
              <div className='text-[15px] font-semibold text-ghost group-hover:text-emerald'>{r.who}</div>
              <p className='mt-1.5 text-[13px] leading-relaxed text-ash'>{r.text}</p>
            </Link>
          ))}
        </div>
      </section>

      {TOPIC_GROUPS.filter((g) => g.id !== 'verify').map((g) => (
        <section key={g.id} aria-labelledby={`learn-${g.id}`}>
          <h2 id={`learn-${g.id}`} className='label-caps mb-3 text-[11px] text-pewter'>
            {g.label}
          </h2>
          <div className='grid gap-4 sm:grid-cols-2 lg:grid-cols-3'>
            {TOPICS.filter((t) => t.group === g.id).map((t) => (
              <Link
                key={t.slug}
                to={paths.learn(t.slug)}
                data-testid={`topic-card-${t.slug}`}
                className='group flex flex-col rounded-md border border-charcoal bg-carbon p-5 transition-colors hover:border-emerald/50'
              >
                <span className='text-[14px] font-semibold text-ghost group-hover:text-emerald'>{t.title}</span>
                <span className='mt-1.5 text-[13px] leading-relaxed text-ash'>{t.summary}</span>
              </Link>
            ))}
          </div>
        </section>
      ))}
    </>
  )
}

function TopicNav({ current }: { current: string }) {
  return (
    <nav aria-label='Guide topics' className='flex flex-col gap-5'>
      {TOPIC_GROUPS.map((g) => (
        <div key={g.id}>
          <div className='label-caps mb-1.5 text-[11px] text-pewter'>{g.label}</div>
          <ul className='flex flex-col'>
            {TOPICS.filter((t) => t.group === g.id).map((t) => (
              <li key={t.slug}>
                <Link
                  to={paths.learn(t.slug)}
                  aria-current={t.slug === current ? 'page' : undefined}
                  className={cn(
                    '-ml-3 block border-l-2 py-1 pl-3 text-[13px] transition-colors',
                    t.slug === current
                      ? 'border-emerald text-emerald'
                      : 'border-transparent text-ash hover:border-charcoal hover:text-ghost'
                  )}
                >
                  {t.title}
                </Link>
              </li>
            ))}
          </ul>
        </div>
      ))}
    </nav>
  )
}

function TopicPage({ meta }: { meta: TopicMeta }) {
  const ex = useLearnExamples()
  const navigate = useNavigate()
  const Content = CONTENT[meta.slug]!
  const { prev, next } = neighbours(meta.slug)
  const group = TOPIC_GROUPS.find((g) => g.id === meta.group)!
  return (
    <div className='grid gap-10 lg:grid-cols-[220px_minmax(0,1fr)]'>
      <aside className='hidden lg:block'>
        <div className='sticky top-20'>
          <Link to={paths.learn()} className='mb-5 block text-[13px] text-pewter hover:text-emerald'>
            ← All topics
          </Link>
          <TopicNav current={meta.slug} />
        </div>
      </aside>

      <article className='min-w-0 max-w-[780px]' data-testid='learn-topic' data-topic={meta.slug}>
        <div className='mb-6 lg:hidden'>
          <Select
            aria-label='Guide topic'
            size='sm'
            value={meta.slug}
            onChange={(e) => navigate(paths.learn(e.target.value))}
            options={TOPICS.map((t) => ({ value: t.slug, label: t.title }))}
          />
        </div>
        <SectionHeader
          size='page'
          label={
            <>
              <Link to={paths.learn()} className='hover:underline'>
                Learn
              </Link>{' '}
              · {group.label}
            </>
          }
          title={meta.title}
          description={meta.summary}
        />
        <div className='mt-8'>
          <Content ex={ex} />
        </div>
        <nav
          aria-label='Next and previous topics'
          className='mt-12 grid gap-3 border-t border-charcoal pt-6 sm:grid-cols-2'
        >
          {prev ? (
            <Link
              to={paths.learn(prev.slug)}
              className='group rounded-md border border-charcoal p-4 transition-colors hover:border-emerald/50'
            >
              <span className='flex items-center gap-1 text-[12px] text-ash'>
                <ChevronLeftIcon size={13} /> Previous
              </span>
              <span className='mt-1 block text-[13px] font-medium text-silver group-hover:text-emerald'>
                {prev.title}
              </span>
            </Link>
          ) : (
            <span />
          )}
          {next ? (
            <Link
              to={paths.learn(next.slug)}
              className='group rounded-md border border-charcoal p-4 text-right transition-colors hover:border-emerald/50'
            >
              <span className='flex items-center justify-end gap-1 text-[12px] text-ash'>
                Next <ChevronRightIcon size={13} />
              </span>
              <span className='mt-1 block text-[13px] font-medium text-silver group-hover:text-emerald'>
                {next.title}
              </span>
            </Link>
          ) : null}
        </nav>
      </article>
    </div>
  )
}

function UnknownTopic({ slug }: { slug: string }) {
  return (
    <>
      <SectionHeader size='page' label='Learn' title='No such topic' />
      <Card>
        <EmptyState
          title={`The guide has no topic “${slug}”`}
          description='It may have been renamed. These are the topics it has.'
        />
        <ul className='mx-auto grid max-w-2xl gap-2 pb-4 sm:grid-cols-2'>
          {TOPICS.map((t) => (
            <li key={t.slug}>
              <Link to={paths.learn(t.slug)} className='text-[13px] text-pewter hover:text-emerald'>
                {t.title}
              </Link>
            </li>
          ))}
        </ul>
      </Card>
    </>
  )
}
