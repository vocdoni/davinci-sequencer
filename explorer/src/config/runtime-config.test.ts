import { describe, expect, it } from 'vitest'
import { readFileSync } from 'node:fs'
import { resolve } from 'node:path'
import { isDemoRequested, parseRuntimeConfig } from './runtime-config'

const committed = JSON.parse(readFileSync(resolve(__dirname, '../../public/config.json'), 'utf8'))

describe('parseRuntimeConfig', () => {
  it('accepts the committed defaults: the Gnosis deployment', () => {
    const cfg = parseRuntimeConfig(committed)
    expect(cfg.chainId).toBe(100)
    expect(cfg.networkName).toBe('Gnosis Chain')
    expect(cfg.rpcUrls).toHaveLength(3)
    expect(cfg.registryAddress).toBe('0x48a5091B64434a6690AeA32455712Bd2b7EE3E77')
    expect(cfg.startBlock).toBe(48483867)
    expect(cfg.beaconUrl).toBe('https://rpc-gbc.gnosischain.com')
    expect(cfg.blockExplorerUrl).toBe('https://gnosisscan.io')
    expect(cfg.dkgExplorerUrl).toBeUndefined()
    expect(cfg.sequencers).toEqual([])
  })

  it('takes a comma list of RPCs and trims trailing slashes', () => {
    const cfg = parseRuntimeConfig({
      ...committed,
      rpcUrls: undefined,
      rpcUrl: 'https://a.example/, https://b.example',
    })
    expect(cfg.rpcUrls).toEqual(['https://a.example', 'https://b.example'])
    expect(parseRuntimeConfig({ ...committed, blockExplorerUrl: 'https://x.example/' }).blockExplorerUrl).toBe(
      'https://x.example'
    )
  })

  it('accepts sequencers as URLs or proxy entries', () => {
    const cfg = parseRuntimeConfig({
      ...committed,
      sequencers: ['https://seq.example/', { url: '/proxy/sequencer/1/', upstream: 'https://other.example' }],
    })
    expect(cfg.sequencers).toEqual([
      { url: 'https://seq.example' },
      { url: '/proxy/sequencer/1', upstream: 'https://other.example' },
    ])
  })

  it('names the field that is wrong', () => {
    expect(() => parseRuntimeConfig(null)).toThrow(/not an object/)
    expect(() => parseRuntimeConfig({ ...committed, chainId: '100' })).toThrow(/chainId/)
    expect(() => parseRuntimeConfig({ ...committed, rpcUrls: [] })).toThrow(/rpcUrls/)
    expect(() => parseRuntimeConfig({ ...committed, registryAddress: '0xnope' })).toThrow(/registryAddress/)
    expect(() => parseRuntimeConfig({ ...committed, startBlock: -1 })).toThrow(/startBlock/)
    expect(() => parseRuntimeConfig({ ...committed, sequencers: [{}] })).toThrow(/sequencers\[0\]/)
  })

  it('defaults the start block and the network name', () => {
    const { startBlock: _s, networkName: _n, ...rest } = committed
    const cfg = parseRuntimeConfig(rest)
    expect(cfg.startBlock).toBe(0)
    expect(cfg.networkName).toBe('Chain 100')
  })
})

function memorySession() {
  const map = new Map<string, string>()
  return {
    getItem: (k: string) => map.get(k) ?? null,
    setItem: (k: string, v: string) => void map.set(k, v),
    removeItem: (k: string) => void map.delete(k),
  }
}

describe('isDemoRequested', () => {
  it.each(['?demo=1', '?demo', '?demo=true', '?foo=1&demo=1'])('is true for %s', (search) => {
    expect(isDemoRequested(search, memorySession())).toBe(true)
  })

  it.each(['', '?demo=0', '?demo=false', '?other=1'])('is false for "%s"', (search) => {
    expect(isDemoRequested(search, memorySession())).toBe(false)
  })

  it('stays on for the tab until ?demo=0', () => {
    const s = memorySession()
    expect(isDemoRequested('?demo=1', s)).toBe(true)
    expect(isDemoRequested('', s)).toBe(true)
    expect(isDemoRequested('?demo=0', s)).toBe(false)
    expect(isDemoRequested('', s)).toBe(false)
  })
})
