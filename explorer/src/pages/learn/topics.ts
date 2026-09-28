// The guide's table of contents. Slugs are the `/learn/:topic` URLs; keep
// them stable, other pages link to them.

export type TopicGroup = 'protocol' | 'verify' | 'reference'

export interface TopicMeta {
  slug: string
  title: string
  /** One sentence for the index and the page header. */
  summary: string
  group: TopicGroup
}

export const TOPIC_GROUPS: Array<{ id: TopicGroup; label: string }> = [
  { id: 'protocol', label: 'How it works' },
  { id: 'verify', label: 'Check it yourself' },
  { id: 'reference', label: 'Reference' },
]

export const TOPICS: TopicMeta[] = [
  {
    slug: 'how-it-works',
    title: 'How DAVINCI works',
    summary:
      'From a new process to a proven tally: ballots and their proofs, sequencers and batches, the zkVM proof, blobs, settlement and results.',
    group: 'protocol',
  },
  {
    slug: 'key-modes',
    title: 'Key modes and whom you trust',
    summary:
      'Sequencer key, DKG automatic or DKG locked: who can open the ballots, who publishes the tally and what can go wrong.',
    group: 'protocol',
  },
  {
    slug: 'census',
    title: 'Census origins and ballot slots',
    summary: 'The four ways a process says who may vote, and where each voter’s ballot lives in the state tree.',
    group: 'protocol',
  },
  {
    slug: 'silent-revoting',
    title: 'Revoting, re-encryption and silent refreshes',
    summary:
      'Why an overwrite looks like a routine refresh, what stays public, and what re-encryption does to a stored ballot.',
    group: 'protocol',
  },
  {
    slug: 'blobs',
    title: 'Data availability: the blobs',
    summary:
      'What every transition publishes in its EIP-4844 blobs, how the proof binds them and how anyone rebuilds the state.',
    group: 'protocol',
  },
  {
    slug: 'settlement',
    title: 'What the registry checks per transition',
    summary: 'The checks submitStateTransition runs, in order, and the public values it reads from the proof.',
    group: 'protocol',
  },
  {
    slug: 'results',
    title: 'How results are produced',
    summary:
      'A results proof for a sequencer key, threshold decryption for a DKG key, and what each is checked against.',
    group: 'protocol',
  },
  {
    slug: 'verify-voter',
    title: 'For voters: check your vote',
    summary: 'Find the transition that included your vote, check its tracker proof and see it counted.',
    group: 'verify',
  },
  {
    slug: 'verify-organizer',
    title: 'For organizers: watch your process',
    summary: 'Parameters, census, key, progress and results of your processes, and what to do at the end.',
    group: 'verify',
  },
  {
    slug: 'verify-auditor',
    title: 'For auditors: check the deployment',
    summary: 'The pinned keys, every transition, the data behind it and the results, without trusting any sequencer.',
    group: 'verify',
  },
  {
    slug: 'glossary',
    title: 'Glossary',
    summary: 'Process id, vote id, state root, slot, program vk, blob, epoch and the rest, in a sentence or two each.',
    group: 'reference',
  },
]

export function findTopic(slug: string | undefined): TopicMeta | null {
  return TOPICS.find((t) => t.slug === slug) ?? null
}

/** The topics before and after `slug` in reading order. */
export function neighbours(slug: string): { prev: TopicMeta | null; next: TopicMeta | null } {
  const i = TOPICS.findIndex((t) => t.slug === slug)
  if (i < 0) return { prev: null, next: null }
  return { prev: TOPICS[i - 1] ?? null, next: TOPICS[i + 1] ?? null }
}

/** Anchor id for a heading or glossary term. */
export function anchorId(text: string): string {
  return text
    .toLowerCase()
    .replace(/[`’']/g, '')
    .replace(/[^a-z0-9]+/g, '-')
    .replace(/^-+|-+$/g, '')
}
