import { describe, expect, it } from 'vitest'
import { demoFixture } from '~fixtures/demo'
import { demoDeploymentDetails } from '~data/deployment'
import type { ChainMeta } from '~indexer/types'
import { KNOWN_RELEASES, matchRelease } from '~protocol/releases'
import {
  castCommands,
  contractRows,
  dkgExplorerLink,
  publicRpc,
  releasePins,
  releaseVerdict,
  verifyDeploymentCommand,
  wiringChecks,
} from './model'

const store = demoFixture().store
const chain = store.chain
const details = demoDeploymentDetails(store)
const release = KNOWN_RELEASES[0]!

describe('verifyDeploymentCommand', () => {
  it('fills in the script the way the davinci-contracts README runs it', () => {
    const cmd = verifyDeploymentCommand({
      rpc: 'https://rpc.gnosischain.com',
      chainId: 100,
      registry: '0x48a5091b64434a6690aea32455712bd2b7ee3e77',
      pins: releasePins(release),
    })
    expect(cmd).toBe(
      [
        'python3 script/verify_deployment.py --rpc https://rpc.gnosischain.com --chain-id 100 \\',
        '    --registry 0x48a5091B64434a6690AeA32455712Bd2b7EE3E77 \\',
        `    --batch-vk ${release.batchProgramVK} \\`,
        `    --results-vk ${release.resultsProgramVK} \\`,
        `    --root-c ${release.rootCVadcopFinal} \\`,
        `    --ballot-vk-hash ${release.ballotVKHash}`,
      ].join('\n')
    )
  })

  it('leaves a placeholder for a pin not read yet', () => {
    const cmd = verifyDeploymentCommand({ rpc: 'x', chainId: 1, registry: '0x01', pins: {} })
    expect(cmd).toContain('--batch-vk <batchProgramVK>')
  })
})

describe('publicRpc', () => {
  it('prefers a plain http(s) endpoint', () => {
    expect(publicRpc(['/proxy/rpc', 'https://a.example'])).toBe('https://a.example')
    expect(publicRpc(['/proxy/rpc'])).toBe('$RPC_URL')
  })
})

describe('castCommands', () => {
  it('reads every pin and hashes the verifier code', () => {
    const out = castCommands('https://rpc.example', chain.registryAddress, chain.registry!.ziskVerifier)
    expect(out).toContain('cast call $REGISTRY "batchProgramVK()(bytes32)" --rpc-url $RPC')
    expect(out).toContain('cast keccak $(cast code $VERIFIER --rpc-url $RPC)')
    expect(out.split('\n')[0]).toBe('RPC=https://rpc.example')
  })
})

describe('contractRows', () => {
  it('lists the DAVINCI and DKG contracts', () => {
    const rows = contractRows(chain, details)
    expect(rows.map((r) => r.name)).toEqual([
      'ProcessRegistry',
      'ZiskVerifier',
      'DavinciDKGAdapter',
      'DKGManager',
      'DKGAppManager',
      'DKGRegistry',
      'ContributionVerifier',
      'FinalizeVerifier',
      'PartialDecryptVerifier',
      'DecryptCombineVerifier',
    ])
    expect(rows.every((r) => r.address != null)).toBe(true)
  })

  it('says the DKG modes are off without an adapter', () => {
    const noDkg: ChainMeta = {
      ...chain,
      registry: { ...chain.registry!, dkgAdapter: null, dkgManager: null, dkgAppManager: null },
    }
    const rows = contractRows(noDkg, undefined)
    expect(rows).toHaveLength(3)
    expect(rows[2]!.address).toBeNull()
    expect(rows[2]!.note).toMatch(/disabled/)
  })

  it('keeps the rows, unread, before the registry is read', () => {
    const rows = contractRows({ ...chain, registry: null }, undefined)
    expect(rows[1]!.address).toBeNull()
    expect(rows.length).toBe(10)
  })
})

describe('wiringChecks', () => {
  it('passes on a consistent deployment', () => {
    const checks = wiringChecks(chain, details, chain.chainId)
    expect(checks.map((c) => c.state)).toEqual(checks.map(() => 'pass'))
    expect(checks).toHaveLength(7)
  })

  it('flags a verifier on another setup and an adapter of another registry', () => {
    const checks = wiringChecks(
      chain,
      {
        verifierRootC: '0x01',
        dkg: { ...details.dkg!, adapterRegistry: '0x0000000000000000000000000000000000000bad' },
      },
      chain.chainId
    )
    const state = Object.fromEntries(checks.map((c) => [c.id, c.state]))
    expect(state['verifier-root']).toBe('fail')
    expect(state['adapter-registry']).toBe('fail')
    expect(state['app-manager']).toBe('pass')
  })

  it('flags a registry deployed for another chain', () => {
    const checks = wiringChecks(chain, details, 1)
    expect(checks.find((c) => c.id === 'chain-id')!.state).toBe('fail')
    expect(checks.find((c) => c.id === 'dkg-chain')!.state).toBe('fail')
  })

  it('is unknown before anything is read', () => {
    const checks = wiringChecks({ ...chain, registry: null }, undefined, chain.chainId)
    expect(checks.every((c) => c.state === 'unknown')).toBe(true)
  })
})

describe('releaseVerdict', () => {
  it('names the release every pin matches', () => {
    const v = releaseVerdict(
      matchRelease({
        batchProgramVK: release.batchProgramVK,
        resultsProgramVK: release.resultsProgramVK,
        rootCVadcopFinal: release.rootCVadcopFinal,
        ziskVerifierCodeHash: release.ziskVerifierCodeHash,
        ballotVKHash: release.ballotVKHash,
      })
    )
    expect(v).toEqual({ tone: 'ok', text: `All five pins match ${release.label}.` })
  })

  it('counts the pins that differ', () => {
    const v = releaseVerdict(
      matchRelease({
        batchProgramVK: '0x00',
        resultsProgramVK: release.resultsProgramVK,
        rootCVadcopFinal: release.rootCVadcopFinal,
        ziskVerifierCodeHash: release.ziskVerifierCodeHash,
        ballotVKHash: release.ballotVKHash,
      })
    )
    expect(v.tone).toBe('danger')
    expect(v.text).toMatch(/^1 of 5 pins differ/)
  })

  it('waits while nothing is read', () => {
    expect(releaseVerdict(matchRelease({})).tone).toBe('info')
  })
})

describe('dkgExplorerLink', () => {
  it('builds epoch and operator links when a DKG explorer is configured', () => {
    expect(dkgExplorerLink('https://dkg.example/', 'epoch', '0xAB')).toBe('https://dkg.example/epochs/0xab')
    expect(dkgExplorerLink('https://dkg.example', 'operator', '0xCD')).toBe('https://dkg.example/operators/0xcd')
    expect(dkgExplorerLink(undefined, 'epoch', '0xab')).toBeNull()
  })
})
