import { describe, expect, it } from 'vitest'
import { screen, within } from '@testing-library/react'
import { Route, Routes } from 'react-router-dom'
import { renderWithProviders } from '../../test-utils'
import { GLOSSARY, filterGlossary } from './glossary'
import { pickExamples } from './examples'
import { LearnPage } from './index'
import { TOPICS, anchorId, findTopic, neighbours } from './topics'

// The content files, raw, to check every glossary link has a target.
const sources = import.meta.glob('./content/*.tsx', { query: '?raw', import: 'default', eager: true }) as Record<
  string,
  string
>

function renderLearn(route: string) {
  return renderWithProviders(
    <Routes>
      <Route path='/learn' element={<LearnPage />} />
      <Route path='/learn/:topic' element={<LearnPage />} />
    </Routes>,
    { route }
  )
}

describe('topics', () => {
  it('have unique slugs and walk in order', () => {
    expect(new Set(TOPICS.map((t) => t.slug)).size).toBe(TOPICS.length)
    expect(findTopic('glossary')?.title).toBe('Glossary')
    expect(findTopic('nope')).toBeNull()
    expect(neighbours(TOPICS[0]!.slug).prev).toBeNull()
    expect(neighbours(TOPICS[1]!.slug).prev?.slug).toBe(TOPICS[0]!.slug)
    expect(neighbours('glossary').next).toBeNull()
  })

  it('make anchors from headings', () => {
    expect(anchorId('1. A process is created')).toBe('1-a-process-is-created')
    expect(anchorId('What the guest proves, and what is left')).toBe('what-the-guest-proves-and-what-is-left')
  })
})

describe('glossary', () => {
  it('has unique ids and a definition for each term', () => {
    expect(new Set(GLOSSARY.map((e) => e.id)).size).toBe(GLOSSARY.length)
    for (const e of GLOSSARY) {
      expect(e.text.length).toBeGreaterThan(30)
      expect(e.text.split('`').length % 2).toBe(1)
    }
  })

  it('filters by every word and sorts by term', () => {
    const all = filterGlossary('')
    expect(all).toHaveLength(GLOSSARY.length)
    expect(all[0]!.term.localeCompare(all[1]!.term)).toBeLessThan(0)
    expect(filterGlossary('vote id').map((e) => e.id)).toContain('vote-id')
    expect(filterGlossary('zzzz')).toEqual([])
  })

  it('has an entry for every term the guide links to', () => {
    const ids = new Set(GLOSSARY.map((e) => e.id))
    const used = Object.values(sources).flatMap((src) =>
      [...src.matchAll(/<Term id='([a-z0-9-]+)'/g)].map((m) => m[1]!)
    )
    expect(used.length).toBeGreaterThan(20)
    expect(used.filter((id) => !ids.has(id))).toEqual([])
  })
})

describe('pickExamples', () => {
  it('picks nothing on an empty registry', () => {
    expect(pickExamples([])).toEqual({ active: null, withResults: null, dkg: null, newest: null })
  })
})

describe('LearnPage', () => {
  it('renders the index with every topic', () => {
    renderLearn('/learn')
    const page = screen.getByTestId('page-learn')
    for (const t of TOPICS.filter((x) => x.group !== 'verify')) {
      expect(within(page).getByTestId(`topic-card-${t.slug}`)).toBeInTheDocument()
    }
    expect(within(page).getByText('I voted')).toBeInTheDocument()
  })

  for (const t of TOPICS) {
    it(`renders ${t.slug}`, () => {
      renderLearn(`/learn/${t.slug}`)
      const article = screen.getByTestId('learn-topic')
      expect(article).toHaveAttribute('data-topic', t.slug)
      expect(within(article).getByRole('heading', { level: 1, name: t.title })).toBeInTheDocument()
    })
  }

  it('explains an unknown topic', () => {
    renderLearn('/learn/nope')
    expect(screen.getByText('No such topic')).toBeInTheDocument()
    expect(screen.getByRole('link', { name: 'Glossary' })).toBeInTheDocument()
  })
})
