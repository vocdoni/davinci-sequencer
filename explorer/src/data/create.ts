// One factory for both modes, so the app shell never branches on `demo`
// beyond passing the config.

import type { PublicClient } from 'viem'
import type { RuntimeConfig } from '~config/runtime-config'
import type { KVStore } from '~indexer/persist'
import { createDemoDataSource, createDemoServices, demoFixture, type DemoOptions } from '~fixtures/demo'
import { createChainClient } from './client'
import { createLiveServices, type ExplorerServices } from './services'
import { createLiveDataSource, type DataSource } from './source'

export interface CreateDataOptions {
  config: RuntimeConfig
  /** Live mode only; built from the config when omitted. */
  client?: PublicClient
  /** Persistence backend; `null` disables the IndexedDB cache. */
  kv?: KVStore | null
  pollIntervalMs?: number
  demoOptions?: DemoOptions
}

export interface ExplorerData {
  source: DataSource
  services: ExplorerServices
  client: PublicClient | null
}

export function createExplorerData(options: CreateDataOptions): ExplorerData {
  const { config } = options
  if (config.demo) {
    const demo = options.demoOptions ?? {}
    const { blockIntervalMs: _interval, ...fixtureOptions } = demo
    return {
      source: createDemoDataSource(demo),
      services: createDemoServices(demoFixture(fixtureOptions)),
      client: null,
    }
  }
  const client = options.client ?? createChainClient(config)
  return {
    source: createLiveDataSource({
      client,
      chainId: config.chainId,
      networkName: config.networkName,
      registryAddress: config.registryAddress,
      startBlock: config.startBlock,
      pollIntervalMs: options.pollIntervalMs,
      kv: options.kv,
    }),
    services: createLiveServices(config, client),
    client,
  }
}
