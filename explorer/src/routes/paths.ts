// Every URL the explorer renders, in one place. Build links with these
// helpers, never from string literals, so a rename is a one-file change.

export const PROCESS_TABS = ['overview', 'key', 'transitions', 'votes', 'results', 'raw'] as const
export type ProcessTab = (typeof PROCESS_TABS)[number]

export function isProcessTab(value: string | undefined): value is ProcessTab {
  return value != null && (PROCESS_TABS as readonly string[]).includes(value)
}

export const patterns = {
  home: '/',
  processes: '/processes',
  process: '/processes/:pid',
  processTab: '/processes/:pid/:tab',
  transition: '/processes/:pid/transitions/:index',
  tx: '/tx/:hash',
  votes: '/votes',
  vote: '/votes/:pid/:voteId',
  contracts: '/contracts',
  sequencers: '/sequencers',
  learn: '/learn',
  learnTopic: '/learn/:topic',
  kit: '/kit',
} as const

export interface ProcessListFilter {
  status?: string
  keyMode?: string
  census?: string
  organizer?: string
  q?: string
}

function withQuery(path: string, params: Record<string, string | undefined>): string {
  const search = new URLSearchParams()
  for (const [k, v] of Object.entries(params)) if (v) search.set(k, v)
  const qs = search.toString()
  return qs ? `${path}?${qs}` : path
}

export const paths = {
  home: () => patterns.home,
  processes: (filter: ProcessListFilter = {}) => withQuery(patterns.processes, { ...filter }),
  process: (pid: string, tab?: ProcessTab) =>
    tab && tab !== 'overview' ? `/processes/${pid}/${tab}` : `/processes/${pid}`,
  transition: (pid: string, index: number) => `/processes/${pid}/transitions/${index}`,
  tx: (hash: string) => `/tx/${hash}`,
  votes: (params: { pid?: string; voteId?: string } = {}) => withQuery(patterns.votes, params),
  vote: (pid: string, voteId: string) => `/votes/${pid}/${voteId}`,
  contracts: () => patterns.contracts,
  sequencers: () => patterns.sequencers,
  learn: (topic?: string) => (topic ? `/learn/${topic}` : patterns.learn),
  kit: () => patterns.kit,
} as const

export interface NavItem {
  label: string
  to: string
  /** Marks the item active for any path under this prefix. */
  match: string
  /** Hidden unless the deployment configures sequencers. */
  needsSequencers?: boolean
}

/** Primary navigation, in bar order. */
export const NAV_ITEMS: NavItem[] = [
  { label: 'Overview', to: patterns.home, match: '/' },
  { label: 'Processes', to: patterns.processes, match: '/processes' },
  { label: 'Votes', to: patterns.votes, match: '/votes' },
  { label: 'Contracts', to: patterns.contracts, match: '/contracts' },
  { label: 'Sequencers', to: patterns.sequencers, match: '/sequencers', needsSequencers: true },
  { label: 'Learn', to: patterns.learn, match: '/learn' },
]
