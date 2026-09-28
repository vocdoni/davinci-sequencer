// Deployment reads the indexer does not make: the verifier's own setup root
// and the davinci-dkg side of the registry (the manager's immutables and
// verifiers, the newest epoch and its committee, the operator registry and
// the application registration gate). The contracts page is the consumer.
//
// Live mode reads the chain through its own viem client (the reads are a
// handful of eth_calls, folded into one multicall). Demo mode derives the same
// shape from the fixture store, so pages never branch on the mode.

import { useMemo } from 'react'
import { useQuery, type UseQueryResult } from '@tanstack/react-query'
import { parseAbi, type Abi, type Address, type Hex, type PublicClient } from 'viem'
import { useRuntimeConfig } from '~config/config-context'
import type { RuntimeConfig } from '~config/runtime-config'
import { dkgAdapterAbi } from '~contracts/abis'
import type { IndexerStore, RegistryInfo } from '~indexer/types'
import { createChainClient } from './client'
import { useServices } from './context'
import { useStore } from './hooks'

/** `DKGTypes.EpochPhase`, by ordinal (`Completed` is reserved). */
export const DKG_EPOCH_PHASES = ['none', 'committee-selection', 'key-assembly', 'live', 'aborted', 'completed'] as const
export type DkgEpochPhase = (typeof DKG_EPOCH_PHASES)[number]

/** `MAX_K`: pool keys an epoch deals, one per application. */
export const DKG_POOL_KEYS = 16

export type DkgVerifierName = 'contribution' | 'finalize' | 'partialDecrypt' | 'decryptCombine'
export const DKG_VERIFIER_NAMES: DkgVerifierName[] = ['contribution', 'finalize', 'partialDecrypt', 'decryptCombine']

export interface DkgEpochView {
  id: Hex
  nonce: number
  phase: DkgEpochPhase
  /** Whoever called `createEpoch`; it has no special power over the committee. */
  organizer: Address
  threshold: number
  committeeSize: number
  minValidContributions: number
  lotteryAlphaBps: number
  startBlock: number
  seedBlock: number
  committeeSelectionDeadlineBlock: number
  keyAssemblyDeadlineBlock: number
  liveNotBeforeBlock: number
  claimedCount: number
  contributionCount: number
  /** `getPoolStatus`: the index the next registration claims, so the keys claimed so far. */
  poolKeysClaimed: number | null
  /** `selectedParticipants`: the committee, in slot order. */
  committee: Address[]
  /** `getRegisteredAids(epoch).length`. */
  applications: number | null
}

export interface DkgVerifierView {
  name: DkgVerifierName
  address: Address | null
  /** `provingKeyHash()` of the verifier, as the manager reports it. */
  keyHash: Hex | null
}

export type DkgRegistration =
  { kind: 'open'; reason: 'no-gate' | 'unset' } | { kind: 'registrar'; address: Address } | { kind: 'unknown' }

export interface DkgDeployment {
  manager: Address
  appManager: Address
  /** The operator registry (`DKGManager.REGISTRY`). */
  operatorRegistry: Address | null
  verifiers: DkgVerifierView[]
  chainId: number | null
  epochPrefix: number | null
  epochNonce: number | null
  epochDurationBlocks: number | null
  minThreshold: number | null
  minCommitteeSize: number | null
  maxLotteryAlphaBps: number | null
  lastEpochStartBlock: number | null
  /** Earliest block `createEpoch` succeeds by cadence. */
  nextEpochStartBlock: number | null
  newestEpoch: DkgEpochView | null
  /** `adapter.registrationEpoch()`: where a DKG_AUTOMATIC process registers now. */
  registrationEpoch: Hex | null
  /** Set when `registrationEpoch()` reverted (NoLiveEpoch). */
  registrationEpochReverted: boolean
  nodeCount: number | null
  activeCount: number | null
  inactivityWindow: number | null
  registration: DkgRegistration
  /** Wiring, read back: `adapter.registry()`, `appManager.MANAGER()`, `operatorRegistry.manager()`. */
  adapterRegistry: Address | null
  appManagerManager: Address | null
  operatorRegistryManager: Address | null
}

export interface DeploymentDetails {
  /** `ZiskVerifier.getRootCVadcopFinal()`. */
  verifierRootC: Hex | null
  /** Null when the registry has no DKG adapter. */
  dkg: DkgDeployment | null
}

// ── pure helpers ─────────────────────────────────────────────────────────────

/** `DKGIdLib.computeEpochId`: `bytes12(prefix << 64 | nonce)`. */
export function dkgEpochId(prefix: number, nonce: number | bigint): Hex {
  return `0x${(prefix >>> 0).toString(16).padStart(8, '0')}${BigInt(nonce).toString(16).padStart(16, '0')}` as Hex
}

/** Nonce of an epoch id (its low 8 bytes). */
export function dkgEpochNonce(epochId: Hex): number {
  return Number(BigInt(`0x${epochId.slice(-16)}`))
}

export function dkgEpochPhase(ordinal: number | bigint): DkgEpochPhase {
  return DKG_EPOCH_PHASES[Number(ordinal)] ?? 'none'
}

const ZERO = '0x0000000000000000000000000000000000000000'
const lower = <T extends string>(v: unknown): T => String(v ?? '').toLowerCase() as T
const num = (v: unknown): number => (typeof v === 'bigint' ? Number(v) : Number(v ?? 0))

/** Normalises a decoded `getEpoch` struct. */
export function normalizeEpoch(
  id: Hex,
  raw: Record<string, unknown>,
  extra: { poolKeysClaimed: number | null; committee: Address[]; applications: number | null }
): DkgEpochView {
  const policy = (raw.policy ?? {}) as Record<string, unknown>
  return {
    id: lower<Hex>(id),
    nonce: num(raw.nonce),
    phase: dkgEpochPhase(num(raw.status)),
    organizer: lower<Address>(raw.organizer),
    threshold: num(policy.threshold),
    committeeSize: num(policy.committeeSize),
    minValidContributions: num(policy.minValidContributions),
    lotteryAlphaBps: num(policy.lotteryAlphaBps),
    startBlock: num(raw.startBlock),
    seedBlock: num(raw.seedBlock),
    committeeSelectionDeadlineBlock: num(policy.committeeSelectionDeadlineBlock),
    keyAssemblyDeadlineBlock: num(policy.keyAssemblyDeadlineBlock),
    liveNotBeforeBlock: num(policy.liveNotBeforeBlock),
    claimedCount: num(raw.claimedCount),
    contributionCount: num(raw.contributionCount),
    ...extra,
  }
}

/** What a `registrar()` read says about who may register applications. */
export function registrationFrom(read: { ok: true; value: unknown } | { ok: false }): DkgRegistration {
  // A manager built without the gate has no `registrar()` at all: the call reverts.
  if (!read.ok) return { kind: 'open', reason: 'no-gate' }
  const address = lower<Address>(read.value)
  if (!address || address === ZERO) return { kind: 'open', reason: 'unset' }
  return { kind: 'registrar', address }
}

// ── live reads ───────────────────────────────────────────────────────────────

const managerAbi = parseAbi([
  'function REGISTRY() view returns (address)',
  'function CONTRIBUTION_VERIFIER() view returns (address)',
  'function PARTIAL_DECRYPT_VERIFIER() view returns (address)',
  'function FINALIZE_VERIFIER() view returns (address)',
  'function DECRYPT_COMBINE_VERIFIER() view returns (address)',
  'function CHAIN_ID() view returns (uint32)',
  'function EPOCH_PREFIX() view returns (uint32)',
  'function epochNonce() view returns (uint64)',
  'function EPOCH_DURATION_BLOCKS() view returns (uint256)',
  'function MIN_THRESHOLD() view returns (uint16)',
  'function MIN_COMMITTEE_SIZE() view returns (uint16)',
  'function MAX_LOTTERY_ALPHA_BPS() view returns (uint16)',
  'function lastEpochStartBlock() view returns (uint64)',
  'function nextEpochStartBlock() view returns (uint64)',
  'function getContributionVerifierVKeyHash() view returns (bytes32)',
  'function getFinalizeVerifierVKeyHash() view returns (bytes32)',
  'function getPartialDecryptVerifierVKeyHash() view returns (bytes32)',
  'function getDecryptCombineVerifierVKeyHash() view returns (bytes32)',
  'function getPoolStatus(bytes12 epochId) view returns (uint8)',
  'function selectedParticipants(bytes12 epochId) view returns (address[])',
  'struct EpochPolicy { uint16 threshold; uint16 committeeSize; uint16 minValidContributions; uint16 lotteryAlphaBps; uint64 committeeSelectionDeadlineBlock; uint64 keyAssemblyDeadlineBlock; uint64 liveNotBeforeBlock; }',
  'struct Epoch { address organizer; EpochPolicy policy; uint8 status; uint64 nonce; uint64 startBlock; uint64 seedBlock; bytes32 seed; uint256 lotteryThreshold; uint16 claimedCount; uint16 contributionCount; uint16 partialDecryptionCount; uint16 ciphertextCount; }',
  'function getEpoch(bytes12 epochId) view returns (Epoch)',
])

const appManagerAbi = parseAbi([
  'function MANAGER() view returns (address)',
  'function registrar() view returns (address)',
  'function getRegisteredAids(bytes12 epochId) view returns (bytes32[])',
])

const operatorRegistryAbi = parseAbi([
  'function nodeCount() view returns (uint64)',
  'function activeCount() view returns (uint64)',
  'function INACTIVITY_WINDOW() view returns (uint64)',
  'function manager() view returns (address)',
])

const verifierAbi = parseAbi(['function getRootCVadcopFinal() view returns (bytes32)'])

interface Call {
  address: Address
  abi: Abi
  functionName: string
  args?: readonly unknown[]
}

type Read = { ok: true; value: unknown } | { ok: false }

/** One multicall; per-call eth_calls when the chain has no Multicall3. Never throws per call. */
async function readAll(client: PublicClient, calls: Call[]): Promise<Read[]> {
  if (calls.length === 0) return []
  let out: Read[] | null = null
  try {
    const res = await client.multicall({
      // eslint-disable-next-line @typescript-eslint/no-explicit-any
      contracts: calls as any,
      allowFailure: true,
    })
    out = res.map((r) => (r.status === 'success' ? { ok: true as const, value: r.result } : { ok: false as const }))
  } catch {
    out = null
  }
  // A chain without Multicall3 reports every call as failed rather than rejecting.
  if (out && out.some((r) => r.ok)) return out
  const single = await Promise.all(
    calls.map((c) =>
      client
        // eslint-disable-next-line @typescript-eslint/no-explicit-any
        .readContract(c as any)
        .then((value) => ({ ok: true as const, value }))
        .catch(() => ({ ok: false as const }))
    )
  )
  if (single.every((r) => !r.ok)) throw new Error('The RPC answered none of the contract reads')
  return single
}

const val = <T>(r: Read | undefined, map: (v: unknown) => T): T | null => (r && r.ok ? map(r.value) : null)

export async function readDeploymentDetails(client: PublicClient, registry: RegistryInfo): Promise<DeploymentDetails> {
  const adapter = registry.dkgAdapter
  const manager = registry.dkgManager
  const appManager = registry.dkgAppManager
  const verifierCall: Call = { address: registry.ziskVerifier, abi: verifierAbi, functionName: 'getRootCVadcopFinal' }
  if (!adapter || !manager || !appManager) {
    const [root] = await readAll(client, [verifierCall])
    return { verifierRootC: val(root, lower<Hex>), dkg: null }
  }

  const m = (functionName: string, args?: readonly unknown[]): Call => ({
    address: manager,
    abi: managerAbi,
    functionName,
    args,
  })
  const first: Call[] = [
    verifierCall,
    m('REGISTRY'),
    m('CONTRIBUTION_VERIFIER'),
    m('FINALIZE_VERIFIER'),
    m('PARTIAL_DECRYPT_VERIFIER'),
    m('DECRYPT_COMBINE_VERIFIER'),
    m('getContributionVerifierVKeyHash'),
    m('getFinalizeVerifierVKeyHash'),
    m('getPartialDecryptVerifierVKeyHash'),
    m('getDecryptCombineVerifierVKeyHash'),
    m('CHAIN_ID'),
    m('EPOCH_PREFIX'),
    m('epochNonce'),
    m('EPOCH_DURATION_BLOCKS'),
    m('MIN_THRESHOLD'),
    m('MIN_COMMITTEE_SIZE'),
    m('MAX_LOTTERY_ALPHA_BPS'),
    m('lastEpochStartBlock'),
    m('nextEpochStartBlock'),
    { address: adapter, abi: dkgAdapterAbi, functionName: 'registry' },
    { address: adapter, abi: dkgAdapterAbi, functionName: 'registrationEpoch' },
    { address: appManager, abi: appManagerAbi, functionName: 'MANAGER' },
    { address: appManager, abi: appManagerAbi, functionName: 'registrar' },
  ]
  const r = await readAll(client, first)
  const operatorRegistry = val(r[1], lower<Address>)
  const epochPrefix = val(r[11], num)
  const epochNonce = val(r[12], num)
  const newestId = epochPrefix != null && epochNonce ? dkgEpochId(epochPrefix, epochNonce) : null

  const second: Call[] = []
  if (newestId) {
    second.push(m('getEpoch', [newestId]), m('getPoolStatus', [newestId]), m('selectedParticipants', [newestId]), {
      address: appManager,
      abi: appManagerAbi,
      functionName: 'getRegisteredAids',
      args: [newestId],
    })
  }
  const opBase = second.length
  if (operatorRegistry) {
    const o = (functionName: string): Call => ({ address: operatorRegistry, abi: operatorRegistryAbi, functionName })
    second.push(o('nodeCount'), o('activeCount'), o('INACTIVITY_WINDOW'), o('manager'))
  }
  const s = second.length ? await readAll(client, second) : []

  let newestEpoch: DkgEpochView | null = null
  if (newestId && s[0]?.ok) {
    newestEpoch = normalizeEpoch(newestId, s[0].value as Record<string, unknown>, {
      poolKeysClaimed: val(s[1], num),
      committee: val(s[2], (v) => (v as string[]).map((a) => lower<Address>(a))) ?? [],
      applications: val(s[3], (v) => (v as unknown[]).length),
    })
  }

  const verifierAddresses = [r[2], r[3], r[4], r[5]]
  const keyHashes = [r[6], r[7], r[8], r[9]]
  return {
    verifierRootC: val(r[0], lower<Hex>),
    dkg: {
      manager,
      appManager,
      operatorRegistry,
      verifiers: DKG_VERIFIER_NAMES.map((name, i) => ({
        name,
        address: val(verifierAddresses[i], lower<Address>),
        keyHash: val(keyHashes[i], lower<Hex>),
      })),
      chainId: val(r[10], num),
      epochPrefix,
      epochNonce,
      epochDurationBlocks: val(r[13], num),
      minThreshold: val(r[14], num),
      minCommitteeSize: val(r[15], num),
      maxLotteryAlphaBps: val(r[16], num),
      lastEpochStartBlock: val(r[17], num),
      nextEpochStartBlock: val(r[18], num),
      newestEpoch,
      adapterRegistry: val(r[19], lower<Address>),
      registrationEpoch: val(r[20], lower<Hex>),
      registrationEpochReverted: !r[20]?.ok,
      appManagerManager: val(r[21], lower<Address>),
      registration: registrationFrom(r[22] ?? { ok: false }),
      nodeCount: operatorRegistry ? val(s[opBase], num) : null,
      activeCount: operatorRegistry ? val(s[opBase + 1], num) : null,
      inactivityWindow: operatorRegistry ? val(s[opBase + 2], num) : null,
      operatorRegistryManager: operatorRegistry ? val(s[opBase + 3], lower<Address>) : null,
    },
  }
}

// ── demo ─────────────────────────────────────────────────────────────────────

/** The davinci-dkg `circuits-v6` verifier key hashes, which the demo verifiers report. */
export const DKG_CIRCUITS_V6_KEY_HASHES: Record<DkgVerifierName, Hex> = {
  contribution: '0xc5970bb317278755591e5ef36ffe9f104ba6fb53d6531465584378c4528d33ba',
  finalize: '0xb359f97f8fef92093004dd4bf279d126e86b7c8d54b4b34fb6c714357abce759',
  partialDecrypt: '0xd80bfa3d4d43e86204180d8884a3b1bc5c60b5f5832974f3867d11eafb22f865',
  decryptCombine: '0x0a82f57b9a605ea4fb7f72d305887e7b441e405ef42893eb16c0dfa462d93785',
}

const DEMO_EPOCH_BLOCKS = 17_280

/** The same shape as the live read, derived from the demo store's DKG processes. */
export function demoDeploymentDetails(store: IndexerStore): DeploymentDetails {
  const registry = store.chain.registry
  if (!registry) return { verifierRootC: null, dkg: null }
  if (!registry.dkgAdapter || !registry.dkgManager || !registry.dkgAppManager) {
    return { verifierRootC: registry.rootCVadcopFinal, dkg: null }
  }
  const epochs = new Map<Hex, number>()
  for (const key of store.processOrder) {
    const dkg = store.processes[key]!.state?.dkg
    if (dkg) epochs.set(dkg.epochId, (epochs.get(dkg.epochId) ?? 0) + 1)
  }
  const ids = [...epochs.keys()].sort((a, b) => dkgEpochNonce(a) - dkgEpochNonce(b))
  const newestId = ids[ids.length - 1] ?? null
  const base = registry.dkgManager.slice(0, -2)
  const demoAddress = (n: number) => `${base}${n.toString(16).padStart(2, '0')}` as Address
  const startBlock = store.chain.headBlock - 4_000
  const newestEpoch: DkgEpochView | null = newestId
    ? {
        id: newestId,
        nonce: dkgEpochNonce(newestId),
        phase: 'live',
        organizer: demoAddress(0x10),
        threshold: 2,
        committeeSize: 3,
        minValidContributions: 2,
        lotteryAlphaBps: 10_000,
        startBlock,
        seedBlock: startBlock + 1,
        committeeSelectionDeadlineBlock: startBlock + 8,
        keyAssemblyDeadlineBlock: startBlock + 20,
        liveNotBeforeBlock: startBlock + 21,
        claimedCount: 3,
        contributionCount: 3,
        poolKeysClaimed: epochs.get(newestId) ?? 0,
        committee: [demoAddress(0x11), demoAddress(0x12), demoAddress(0x13)],
        applications: epochs.get(newestId) ?? 0,
      }
    : null
  return {
    verifierRootC: registry.rootCVadcopFinal,
    dkg: {
      manager: registry.dkgManager,
      appManager: registry.dkgAppManager,
      operatorRegistry: demoAddress(0x0a),
      verifiers: DKG_VERIFIER_NAMES.map((name, i) => ({
        name,
        address: demoAddress(0x06 + i),
        keyHash: DKG_CIRCUITS_V6_KEY_HASHES[name],
      })),
      chainId: store.chain.chainId,
      epochPrefix: newestId ? Number.parseInt(newestId.slice(2, 10), 16) : null,
      epochNonce: newestEpoch?.nonce ?? 0,
      epochDurationBlocks: DEMO_EPOCH_BLOCKS,
      minThreshold: 2,
      minCommitteeSize: 3,
      maxLotteryAlphaBps: 20_000,
      lastEpochStartBlock: newestEpoch ? startBlock : null,
      nextEpochStartBlock: newestEpoch ? startBlock + DEMO_EPOCH_BLOCKS : null,
      newestEpoch,
      registrationEpoch: newestId,
      registrationEpochReverted: newestId == null,
      nodeCount: 3,
      activeCount: 3,
      inactivityWindow: 50_400,
      registration: { kind: 'open', reason: 'no-gate' },
      adapterRegistry: store.chain.registryAddress.toLowerCase() as Address,
      appManagerManager: registry.dkgManager,
      operatorRegistryManager: registry.dkgManager,
    },
  }
}

// ── hook ─────────────────────────────────────────────────────────────────────

const clients = new WeakMap<RuntimeConfig, PublicClient>()

function clientFor(config: RuntimeConfig): PublicClient {
  let client = clients.get(config)
  if (!client) {
    client = createChainClient(config)
    clients.set(config, client)
  }
  return client
}

/**
 * The verifier's setup root and the DKG deployment behind the registry,
 * re-read every minute (epochs move). Waits for the registry immutables.
 */
export function useDeploymentDetails(): UseQueryResult<DeploymentDetails> {
  const config = useRuntimeConfig()
  const services = useServices()
  const store = useStore()
  const registry = store.chain.registry
  const demo = services.kind === 'demo'
  // The demo answer depends only on which DKG epochs the processes use.
  const demoKey = useMemo(
    () =>
      demo
        ? store.processOrder
            .map((k) => store.processes[k]!.state?.dkg?.epochId)
            .filter(Boolean)
            .join(',')
        : '',
    [demo, store]
  )
  return useQuery({
    queryKey: [
      'deployment-details',
      demo ? 'demo' : config.chainId,
      config.registryAddress.toLowerCase(),
      registry?.ziskVerifier,
      registry?.dkgAdapter,
      demoKey,
    ],
    enabled: registry != null,
    queryFn: () => (demo ? demoDeploymentDetails(store) : readDeploymentDetails(clientFor(config), registry!)),
    staleTime: 30_000,
    refetchInterval: demo ? false : 60_000,
    retry: 1,
  })
}
