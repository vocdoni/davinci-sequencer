// Runtime configuration: which deployment the explorer looks at. Read from
// `/config.json` at boot, not baked into the bundle, so one image serves any
// chain: the Docker entrypoint renders the file from env vars. The committed
// `public/config.json` holds the defaults (the Gnosis Chain deployment) and
// is the one place those values live.

import type { Address } from 'viem'

export interface SequencerConfig {
  /** URL the browser calls: the node itself, or a same-origin proxy path. */
  url: string
  /** The node's own URL when `url` is a proxy, for display. */
  upstream?: string
}

/** The shape of `config.json`. */
export interface RuntimeConfigFile {
  networkName: string
  /** Expected chain id; the RPC's is checked against it at boot. */
  chainId: number
  /** JSON-RPC endpoints, in failover order. */
  rpcUrls: string[]
  /** Beacon API for blob sidecars (may be a same-origin proxy path). */
  beaconUrl?: string
  /** The beacon's own URL when `beaconUrl` is a proxy, for display. */
  beaconUpstream?: string
  registryAddress: Address
  /** Registry deployment block: the floor of the log scan. */
  startBlock: number
  sequencers: SequencerConfig[]
  blockExplorerUrl?: string
  /** davinci-dkg explorer, for DKG epoch and application links. */
  dkgExplorerUrl?: string
}

/** What the app consumes: the file plus the demo switch. */
export interface RuntimeConfig extends RuntimeConfigFile {
  /** Run off the synthetic fixture with no RPC (`?demo=1` or `VITE_DEMO=1`). */
  demo: boolean
}

export const CONFIG_URL = '/config.json'

/**
 * The demo network's config. Demo mode ignores `/config.json`: its data is
 * synthetic, so it must not borrow a real deployment's identity. The block
 * explorer links lead nowhere useful and the UI says so.
 */
export const DEMO_CONFIG: RuntimeConfig = {
  networkName: 'Demo network',
  chainId: 100,
  rpcUrls: ['http://demo.invalid'],
  beaconUrl: 'demo://beacon',
  registryAddress: '0xde30000000000000000000000000000000000001',
  startBlock: 48_047_040,
  sequencers: [{ url: 'demo://sequencer/0' }, { url: 'demo://sequencer/1' }],
  blockExplorerUrl: 'https://gnosisscan.io',
  dkgExplorerUrl: 'https://dkg.example.org',
  demo: true,
}

const DEMO_SESSION_KEY = 'davinci-explorer:demo'

type SessionLike = Pick<Storage, 'getItem' | 'setItem' | 'removeItem'>

function session(): SessionLike | null {
  try {
    return typeof window === 'undefined' ? null : window.sessionStorage
  } catch {
    return null
  }
}

/**
 * `?demo=1` (or `?demo`) in the URL, or a `VITE_DEMO=1` build. The choice is
 * kept for the tab (sessionStorage), so a reload of a deep link stays in demo
 * mode; `?demo=0` leaves it.
 */
export function isDemoRequested(
  search: string = typeof window === 'undefined' ? '' : window.location.search,
  storage: SessionLike | null = session()
): boolean {
  if (import.meta.env.VITE_DEMO === '1' || import.meta.env.VITE_DEMO === 'true') return true
  const params = new URLSearchParams(search)
  if (params.has('demo')) {
    const value = params.get('demo')
    const on = value === null || value === '' || value === '1' || value === 'true'
    try {
      if (on) storage?.setItem(DEMO_SESSION_KEY, '1')
      else storage?.removeItem(DEMO_SESSION_KEY)
    } catch {
      // Storage disabled: the flag holds until the next full load.
    }
    return on
  }
  try {
    return storage?.getItem(DEMO_SESSION_KEY) === '1'
  } catch {
    return false
  }
}

const trimSlash = (url: string) => url.replace(/\/+$/, '')

function urlList(value: unknown, key: string): string[] {
  const items = Array.isArray(value) ? value : typeof value === 'string' ? value.split(',') : null
  if (!items) throw new Error(`config.json: "${key}" must be a list of URLs`)
  return items
    .map((u) => String(u).trim())
    .filter(Boolean)
    .map(trimSlash)
}

/** Narrows an unknown JSON body into a `RuntimeConfigFile`, or throws naming the bad field. */
export function parseRuntimeConfig(raw: unknown): RuntimeConfigFile {
  if (typeof raw !== 'object' || raw === null || Array.isArray(raw)) throw new Error('config.json is not an object')
  const obj = raw as Record<string, unknown>
  const optionalUrl = (key: string): string | undefined => {
    const v = obj[key]
    if (v == null || v === '') return undefined
    if (typeof v !== 'string') throw new Error(`config.json: "${key}" must be a string`)
    return trimSlash(v.trim())
  }

  const chainId = obj.chainId
  if (typeof chainId !== 'number' || !Number.isInteger(chainId) || chainId <= 0) {
    throw new Error('config.json: "chainId" must be a positive integer')
  }
  const rpcUrls = urlList(obj.rpcUrls ?? obj.rpcUrl, 'rpcUrls')
  if (rpcUrls.length === 0) throw new Error('config.json: "rpcUrls" needs at least one URL')
  const registry = obj.registryAddress
  if (typeof registry !== 'string' || !/^0x[0-9a-fA-F]{40}$/.test(registry)) {
    throw new Error('config.json: "registryAddress" is not an address')
  }
  const startBlock = obj.startBlock ?? 0
  if (typeof startBlock !== 'number' || !Number.isInteger(startBlock) || startBlock < 0) {
    throw new Error('config.json: "startBlock" must be a non-negative integer')
  }
  const rawSequencers = obj.sequencers ?? []
  if (!Array.isArray(rawSequencers)) throw new Error('config.json: "sequencers" must be a list')
  const sequencers: SequencerConfig[] = rawSequencers.map((s, i) => {
    if (typeof s === 'string') return { url: trimSlash(s) }
    if (typeof s === 'object' && s && typeof (s as SequencerConfig).url === 'string') {
      const { url, upstream } = s as SequencerConfig
      return { url: trimSlash(url), ...(upstream ? { upstream: trimSlash(upstream) } : {}) }
    }
    throw new Error(`config.json: sequencers[${i}] needs a "url"`)
  })
  const networkName = typeof obj.networkName === 'string' && obj.networkName ? obj.networkName : `Chain ${chainId}`

  return {
    networkName,
    chainId,
    rpcUrls,
    beaconUrl: optionalUrl('beaconUrl'),
    beaconUpstream: optionalUrl('beaconUpstream'),
    registryAddress: registry as Address,
    startBlock,
    sequencers,
    blockExplorerUrl: optionalUrl('blockExplorerUrl'),
    dkgExplorerUrl: optionalUrl('dkgExplorerUrl'),
  }
}

/** Fetches and validates `/config.json`; demo mode never needs it. */
export async function loadRuntimeConfig(demo = isDemoRequested()): Promise<RuntimeConfig> {
  if (demo) return DEMO_CONFIG
  const res = await fetch(CONFIG_URL, { cache: 'no-store' })
  if (!res.ok) throw new Error(`${CONFIG_URL}: HTTP ${res.status}`)
  return { ...parseRuntimeConfig(await res.json()), demo: false }
}
