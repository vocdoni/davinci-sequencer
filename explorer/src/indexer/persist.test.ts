import { describe, expect, it } from 'vitest'
import 'fake-indexeddb/auto'
import { demoFixture } from '~fixtures/demo'
import {
  cacheKey,
  clearStore,
  createIdbStore,
  decodeStore,
  encodeStore,
  loadStore,
  memoryStore,
  saveStore,
} from './persist'
import { STORE_VERSION } from './types'

const store = demoFixture({ scale: 0.25 }).store
const { chainId, registryAddress } = store.chain

describe('store encoding', () => {
  it('round-trips bigints and nested entities', () => {
    const back = decodeStore(encodeStore(store))
    expect(back).toEqual(store)
    const tx = Object.values(back.txDetails)[0]!
    expect(typeof tx.gasUsed).toBe('bigint')
  })
})

describe('persistence', () => {
  it('keys the cache by chain and registry', () => {
    expect(cacheKey(100, '0xABC')).toBe(`davinci-explorer:v${STORE_VERSION}:100:0xabc`)
  })

  it('saves, loads and clears through IndexedDB', async () => {
    const kv = createIdbStore('davinci-explorer-test')
    await saveStore(kv, store)
    const loaded = await loadStore(kv, chainId, registryAddress.toUpperCase().replace('0X', '0x'))
    expect(loaded?.transitionOrder).toEqual(store.transitionOrder)
    await clearStore(kv, chainId, registryAddress)
    expect(await loadStore(kv, chainId, registryAddress)).toBeNull()
  })

  it('ignores a cache written for another deployment or version', async () => {
    const kv = memoryStore()
    await saveStore(kv, store)
    expect(await loadStore(kv, 1, registryAddress)).toBeNull()
    expect(await loadStore(kv, chainId, '0x0000000000000000000000000000000000000001')).toBeNull()
    const key = cacheKey(chainId, registryAddress)
    const envelope = (await kv.get(key)) as Record<string, unknown>
    await kv.set(key, { ...envelope, version: STORE_VERSION + 1 })
    expect(await loadStore(kv, chainId, registryAddress)).toBeNull()
    await kv.set(key, { ...envelope, data: '{not json' })
    expect(await loadStore(kv, chainId, registryAddress)).toBeNull()
  })
})
