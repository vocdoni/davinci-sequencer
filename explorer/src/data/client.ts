// The viem client of the live explorer: every configured RPC behind a
// fallback transport, each batching concurrent requests into JSON-RPC batch
// calls so a page of reads costs a handful of HTTP requests.

import { createPublicClient, defineChain, fallback, http, type PublicClient } from 'viem'
import type { RuntimeConfig } from '~config/runtime-config'
import { MULTICALL3_ADDRESS } from '~indexer/state'

export function createChainClient(config: RuntimeConfig): PublicClient {
  const chain = defineChain({
    id: config.chainId,
    name: config.networkName,
    nativeCurrency:
      config.chainId === 100
        ? { name: 'xDAI', symbol: 'xDAI', decimals: 18 }
        : { name: 'Ether', symbol: 'ETH', decimals: 18 },
    rpcUrls: { default: { http: config.rpcUrls } },
    blockExplorers: config.blockExplorerUrl
      ? { default: { name: 'Explorer', url: config.blockExplorerUrl } }
      : undefined,
    contracts: { multicall3: { address: MULTICALL3_ADDRESS } },
  })
  const transports = config.rpcUrls.map((url) =>
    http(url, { batch: { batchSize: 50, wait: 20 }, retryCount: 1, timeout: 20_000 })
  )
  return createPublicClient({
    chain,
    transport: transports.length === 1 ? transports[0]! : fallback(transports, { retryCount: 1 }),
  }) as PublicClient
}

/** Native currency symbol of the configured chain. */
export function nativeSymbol(chainId: number): string {
  return chainId === 100 || chainId === 10200 ? 'xDAI' : 'ETH'
}
