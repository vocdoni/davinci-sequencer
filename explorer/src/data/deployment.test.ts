import { describe, expect, it } from 'vitest'
import type { PublicClient } from 'viem'
import { demoFixture } from '~fixtures/demo'
import type { RegistryInfo } from '~indexer/types'
import {
  DKG_CIRCUITS_V6_KEY_HASHES,
  demoDeploymentDetails,
  dkgEpochId,
  dkgEpochNonce,
  dkgEpochPhase,
  readDeploymentDetails,
  registrationFrom,
} from './deployment'

const A = (n: number) => `0x${n.toString(16).padStart(40, '0')}` as const

const registry: RegistryInfo = {
  chainID: 100,
  pidPrefix: 0x80c5bb93,
  processCount: 7,
  batchProgramVK: '0x6cfc89d562d0b22f04478a5c15b390433eb52f1b03147030b183076260da7a10',
  resultsProgramVK: '0x7bc8c5e9235548386a44b1885732a2a7ffb1badddc8c7fba599d07ece47be794',
  rootCVadcopFinal: '0x05006517b6ccde5da4d890587ba62845b5af8a307c00e87d4b9d05099b16dc80',
  ballotVKHash: '0xbf1e6590bb1ba883d601c4d7d1c6fa2722a78590716874019db6d68fc776bb0e',
  ziskVerifier: A(0xaa),
  ziskVerifierCodeHash: null,
  dkgAdapter: A(0xad),
  dkgManager: A(0x3a),
  dkgAppManager: A(0xa3),
  readAtBlock: 1,
}

const EPOCH = '0x379447f10000000000000001'

/** A client answering by function name; `undefined` answers revert. */
function fakeClient(values: Record<string, unknown>): PublicClient {
  const answer = (c: { functionName: string }) => values[c.functionName]
  return {
    multicall: async ({ contracts }: { contracts: Array<{ functionName: string }> }) =>
      contracts.map((c) =>
        answer(c) === undefined
          ? { status: 'failure', error: new Error('reverted') }
          : { status: 'success', result: answer(c) }
      ),
    readContract: async (c: { functionName: string }) => {
      if (answer(c) === undefined) throw new Error('reverted')
      return answer(c)
    },
  } as unknown as PublicClient
}

const LIVE = {
  getRootCVadcopFinal: registry.rootCVadcopFinal,
  REGISTRY: A(0x7e),
  CONTRIBUTION_VERIFIER: A(0xc1),
  FINALIZE_VERIFIER: A(0xf1),
  PARTIAL_DECRYPT_VERIFIER: A(0xd1),
  DECRYPT_COMBINE_VERIFIER: A(0xcb),
  getContributionVerifierVKeyHash: DKG_CIRCUITS_V6_KEY_HASHES.contribution,
  getFinalizeVerifierVKeyHash: DKG_CIRCUITS_V6_KEY_HASHES.finalize,
  getPartialDecryptVerifierVKeyHash: DKG_CIRCUITS_V6_KEY_HASHES.partialDecrypt,
  getDecryptCombineVerifierVKeyHash: DKG_CIRCUITS_V6_KEY_HASHES.decryptCombine,
  CHAIN_ID: 100,
  EPOCH_PREFIX: 932464625,
  epochNonce: 1n,
  EPOCH_DURATION_BLOCKS: 17280n,
  MIN_THRESHOLD: 2,
  MIN_COMMITTEE_SIZE: 3,
  MAX_LOTTERY_ALPHA_BPS: 20000,
  lastEpochStartBlock: 48476763n,
  nextEpochStartBlock: 48494043n,
  registry: A(0x1234),
  registrationEpoch: EPOCH,
  MANAGER: registry.dkgManager,
  registrar: registry.dkgAdapter,
  getEpoch: {
    organizer: A(0x42),
    policy: {
      threshold: 2,
      committeeSize: 3,
      minValidContributions: 2,
      lotteryAlphaBps: 10000,
      committeeSelectionDeadlineBlock: 48476771n,
      keyAssemblyDeadlineBlock: 48476783n,
      liveNotBeforeBlock: 48476784n,
    },
    status: 3,
    nonce: 1n,
    startBlock: 48476763n,
    seedBlock: 48476764n,
    seed: '0x00',
    lotteryThreshold: 1n,
    claimedCount: 3,
    contributionCount: 3,
    partialDecryptionCount: 0,
    ciphertextCount: 0,
  },
  getPoolStatus: 3,
  selectedParticipants: [A(0x51), A(0x52), A(0x53)],
  getRegisteredAids: ['0x01', '0x02', '0x03'],
  nodeCount: 3n,
  activeCount: 3n,
  INACTIVITY_WINDOW: 50400n,
  manager: registry.dkgManager,
}

describe('DKG ids', () => {
  it('builds and splits epoch ids like DKGIdLib', () => {
    expect(dkgEpochId(932464625, 1)).toBe(EPOCH)
    expect(dkgEpochId(0xaab5fe7d, 1n)).toBe('0xaab5fe7d0000000000000001')
    expect(dkgEpochNonce(EPOCH)).toBe(1)
  })

  it('names the epoch phases', () => {
    expect(dkgEpochPhase(1)).toBe('committee-selection')
    expect(dkgEpochPhase(3)).toBe('live')
    expect(dkgEpochPhase(4)).toBe('aborted')
    expect(dkgEpochPhase(9)).toBe('none')
  })

  it('reads the registration gate', () => {
    expect(registrationFrom({ ok: false })).toEqual({ kind: 'open', reason: 'no-gate' })
    expect(registrationFrom({ ok: true, value: A(0) })).toEqual({ kind: 'open', reason: 'unset' })
    expect(registrationFrom({ ok: true, value: '0xABC0000000000000000000000000000000000001' })).toEqual({
      kind: 'registrar',
      address: '0xabc0000000000000000000000000000000000001',
    })
  })
})

describe('readDeploymentDetails', () => {
  it('maps every read to its field', async () => {
    const d = await readDeploymentDetails(fakeClient(LIVE), registry)
    expect(d.verifierRootC).toBe(registry.rootCVadcopFinal)
    const dkg = d.dkg!
    expect(dkg.operatorRegistry).toBe(A(0x7e))
    expect(dkg.verifiers.map((v) => [v.name, v.address])).toEqual([
      ['contribution', A(0xc1)],
      ['finalize', A(0xf1)],
      ['partialDecrypt', A(0xd1)],
      ['decryptCombine', A(0xcb)],
    ])
    expect(dkg.verifiers.every((v) => v.keyHash === DKG_CIRCUITS_V6_KEY_HASHES[v.name])).toBe(true)
    expect(dkg).toMatchObject({
      chainId: 100,
      epochPrefix: 932464625,
      epochNonce: 1,
      epochDurationBlocks: 17280,
      minThreshold: 2,
      minCommitteeSize: 3,
      maxLotteryAlphaBps: 20000,
      lastEpochStartBlock: 48476763,
      nextEpochStartBlock: 48494043,
      registrationEpoch: EPOCH,
      registrationEpochReverted: false,
      adapterRegistry: A(0x1234),
      appManagerManager: registry.dkgManager,
      registration: { kind: 'registrar', address: registry.dkgAdapter },
      nodeCount: 3,
      activeCount: 3,
      inactivityWindow: 50400,
      operatorRegistryManager: registry.dkgManager,
    })
    expect(dkg.newestEpoch).toMatchObject({
      id: EPOCH,
      nonce: 1,
      phase: 'live',
      threshold: 2,
      committeeSize: 3,
      startBlock: 48476763,
      liveNotBeforeBlock: 48476784,
      poolKeysClaimed: 3,
      committee: [A(0x51), A(0x52), A(0x53)],
      applications: 3,
    })
  })

  it('treats a missing registrar() as an open registration and a reverted registrationEpoch() as no live epoch', async () => {
    const d = await readDeploymentDetails(
      fakeClient({ ...LIVE, registrar: undefined, registrationEpoch: undefined }),
      registry
    )
    expect(d.dkg!.registration).toEqual({ kind: 'open', reason: 'no-gate' })
    expect(d.dkg!.registrationEpoch).toBeNull()
    expect(d.dkg!.registrationEpochReverted).toBe(true)
  })

  it('reads only the verifier without a DKG adapter', async () => {
    const d = await readDeploymentDetails(fakeClient(LIVE), {
      ...registry,
      dkgAdapter: null,
      dkgManager: null,
      dkgAppManager: null,
    })
    expect(d).toEqual({ verifierRootC: registry.rootCVadcopFinal, dkg: null })
  })

  it('fails when the RPC answers nothing', async () => {
    await expect(readDeploymentDetails(fakeClient({}), registry)).rejects.toThrow(/none of the contract reads/)
  })
})

describe('demoDeploymentDetails', () => {
  it('derives a live committee from the demo DKG processes', () => {
    const store = demoFixture().store
    const d = demoDeploymentDetails(store)
    const epochs = store.processOrder.map((k) => store.processes[k]!.state?.dkg?.epochId).filter(Boolean)
    expect(epochs.length).toBeGreaterThan(0)
    expect(d.verifierRootC).toBe(store.chain.registry!.rootCVadcopFinal)
    expect(d.dkg!.newestEpoch!.phase).toBe('live')
    expect(epochs).toContain(d.dkg!.newestEpoch!.id)
    expect(d.dkg!.adapterRegistry).toBe(store.chain.registryAddress.toLowerCase())
    expect(d.dkg!.appManagerManager).toBe(store.chain.registry!.dkgManager)
  })
})
