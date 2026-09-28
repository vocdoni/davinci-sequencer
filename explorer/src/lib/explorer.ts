// Block-explorer URL helpers. The base URL comes from the runtime config
// (BLOCK_EXPLORER_URL). Etherscan-compatible paths ("/address/0x…",
// "/tx/0x…", "/block/123") cover Etherscan, Blockscout and their clones.

function trimTrailingSlash(url: string): string {
  return url.replace(/\/+$/, '')
}

export function explorerAddressUrl(explorerUrl: string | undefined, address: string): string | null {
  if (!explorerUrl) return null
  return `${trimTrailingSlash(explorerUrl)}/address/${address}`
}

export function explorerTxUrl(explorerUrl: string | undefined, hash: string): string | null {
  if (!explorerUrl) return null
  return `${trimTrailingSlash(explorerUrl)}/tx/${hash}`
}

export function explorerBlockUrl(explorerUrl: string | undefined, block: bigint | number): string | null {
  if (!explorerUrl) return null
  return `${trimTrailingSlash(explorerUrl)}/block/${block.toString()}`
}

/** Verified-source tab of a contract (Etherscan and Blockscout both answer `#code`). */
export function explorerCodeUrl(explorerUrl: string | undefined, address: string): string | null {
  const base = explorerAddressUrl(explorerUrl, address)
  return base ? `${base}#code` : null
}
