import { isAddress } from 'viem'
import { parseVoteId } from '~protocol/blob'
import { isProcessId } from '~protocol/process-id'
import { paths } from '~routes/paths'

/**
 * Where a search query sends the user: an in-app route, the block explorer,
 * or nowhere, with the reason to show.
 */
export type SearchTarget =
  | { kind: 'route'; path: string; label: string }
  | { kind: 'external'; url: string; label: string }
  | { kind: 'unknown'; label: string }

export interface SearchContext {
  blockExplorerUrl?: string
  dkgExplorerUrl?: string
}

/** A resolver contributed by the data layer; runs before the shape rules. */
export type SearchResolver = (query: string, ctx: SearchContext) => SearchTarget | null

const BYTES32 = /^0x[0-9a-fA-F]{64}$/
const BYTES12 = /^0x[0-9a-fA-F]{24}$/
const DIGITS = /^\d+$/

const trim = (url: string) => url.replace(/\/+$/, '')

/**
 * Routes a query by its shape: a process id (31 bytes), a transaction hash,
 * an address, a vote id (0x + 16 hex digits, or its decimal form, ≥ 2^63),
 * a DKG epoch id or a block number. What exists is the store's business: the
 * resolvers registered by the data layer run first.
 */
export function resolveSearch(raw: string, ctx: SearchContext = {}, extra: SearchResolver[] = []): SearchTarget {
  const query = raw.trim()
  if (query === '') return { kind: 'unknown', label: 'Type a process id, vote id, transaction, address or block' }

  for (const resolver of extra) {
    const hit = resolver(query, ctx)
    if (hit) return hit
  }

  if (isProcessId(query)) return { kind: 'route', path: paths.process(query.toLowerCase()), label: `Process ${query}` }

  if (BYTES32.test(query)) return { kind: 'route', path: paths.tx(query.toLowerCase()), label: `Transaction ${query}` }

  if (isAddress(query, { strict: false })) {
    return {
      kind: 'route',
      path: paths.processes({ organizer: query.toLowerCase() }),
      label: `Processes organized by ${query}`,
    }
  }

  const voteId = parseVoteId(query)
  if (voteId != null) return { kind: 'route', path: paths.votes({ voteId: query }), label: `Vote ${query}` }

  if (BYTES12.test(query)) {
    if (ctx.dkgExplorerUrl) {
      return { kind: 'external', url: `${trim(ctx.dkgExplorerUrl)}/epochs/${query.toLowerCase()}`, label: 'DKG epoch' }
    }
    return { kind: 'unknown', label: 'Looks like a DKG epoch id, but no DKG explorer is configured' }
  }

  if (DIGITS.test(query)) {
    if (ctx.blockExplorerUrl) {
      return { kind: 'external', url: `${trim(ctx.blockExplorerUrl)}/block/${query}`, label: `Block ${query}` }
    }
    return { kind: 'unknown', label: 'Looks like a block number, but no block explorer is configured' }
  }

  return { kind: 'unknown', label: `No process, vote, transaction or address matches “${query}”` }
}
