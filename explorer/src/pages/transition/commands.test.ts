import { readFileSync } from 'node:fs'
import { resolve } from 'node:path'
import { describe, expect, it } from 'vitest'
import { blobEvaluationPoint, blobsDigest, versionedHash } from '~protocol/blob'
import type { Hex } from '~protocol/bytes'
import { decodeRegistryCall, type StateTransitionCall } from '~protocol/calldata'
import {
  GET_PROCESS,
  observerCommand,
  pointEvaluationInput,
  recheckCommands,
  sha256Command,
  TRANSITION_EVENT,
  type RecheckInput,
} from './commands'

// A settled Gnosis transition (tests/vectors/README.md). Its point-evaluation
// input below was sent to the precompile with `cast call` and accepted.
const live = JSON.parse(readFileSync(resolve(__dirname, '../../../tests/vectors/gnosis_transition.json'), 'utf8')) as {
  registry: string
  txHash: Hex
  blockNumber: number
  input: Hex
  blobVersionedHashes: Hex[]
  event: { processId: Hex; oldStateRoot: Hex }
}
const decoded = decodeRegistryCall(live.input)
if (decoded.name !== 'submitStateTransition') throw new Error('vector is not a state transition')
const call = (decoded as { call: StateTransitionCall }).call

const ACCEPTED_INPUT =
  '0x0156cf08f8fbb521c2364e3d157543c8f04195aa3b0844504d03c09d9662e2d8' +
  '122a1aa8bb2d62067e9f4211c7e300cc8845a28ecdd002a3cd7ca5c1db528461' +
  '668b53d71569c2bec4fc91d7cffba3829fef193791aba1ffac885403e7585fd0' +
  'b1ae31e9774e9ca1fca0a27cdd366515bc99342d56b0859f6a413f1afe6215d4d462dc859491983ace7bed461bee9a1f' +
  '8afb25020c5a6b4fc864c853ecfc9c1ff0ed9e36f9d961e6445988db6f8146412245d8e15c7739d359364e304c311304'

function input(overrides: Partial<RecheckInput> = {}): RecheckInput {
  const pid = live.event.processId.toLowerCase() as Hex
  return {
    registry: live.registry,
    verifier: '0xae7b632a72cf474039e4128354576770bea33f34',
    processId: pid,
    createdBlock: 48477085,
    block: live.blockNumber,
    tx: live.txHash,
    census: { origin: 'merkle-static', contract: null, root: call.publics.censusRoot },
    batchProgramVK: '0x6cfc89d562d0b22f04478a5c15b390433eb52f1b03147030b183076260da7a10',
    rootCVadcopFinal: '0x05006517b6ccde5da4d890587ba62845b5af8a307c00e87d4b9d05099b16dc80',
    publicValues: call.publicValues,
    proofBytes: call.proofBytes,
    versionedHashes: live.blobVersionedHashes,
    commitments: call.commitments,
    ys: call.ys,
    kzgProofs: call.kzgProofs,
    evaluationPoints: call.commitments.map((c) => blobEvaluationPoint(pid, live.event.oldStateRoot, c)),
    ...overrides,
  }
}

describe('recheckCommands', () => {
  it('builds the point-evaluation input the precompile accepted', () => {
    const i = input()
    const data = pointEvaluationInput(
      i.versionedHashes[0]!,
      i.evaluationPoints[0]!,
      i.ys[0]!,
      i.commitments[0]!,
      i.kzgProofs[0]!
    )
    expect(data).toBe(ACCEPTED_INPUT)
    expect((data.length - 2) / 2).toBe(192)
    expect(recheckCommands(i)['kzg-openings']).toContain(`--data \\\n  ${ACCEPTED_INPUT}`)
  })

  it('hashes the commitment to the versioned hash and the pairs to the digest', () => {
    const cmds = recheckCommands(input())
    const com = call.commitments[0]!
    expect(versionedHash(com)).toBe(live.blobVersionedHashes[0])
    expect(cmds['blob-hashes']).toContain(sha256Command(com))
    expect(cmds['blobs-digest']).toBe(sha256Command(`${com}${call.ys[0]!.slice(2)}`))
    expect(blobsDigest(call.commitments, call.ys)).toBe(call.publics.blobsDigest)
  })

  it('calls the verifier with the pinned vk, the setup root and the calldata', () => {
    const plonk = recheckCommands(input()).plonk!
    expect(plonk).toContain('verifySnarkProof(bytes32,bytes32,bytes,bytes)')
    expect(plonk).toContain(call.publicValues)
    expect(plonk).toContain(call.proofBytes)
    expect(recheckCommands(input({ verifier: null })).plonk).toBeUndefined()
  })

  it('reads the registry one block before the settlement', () => {
    const cmds = recheckCommands(input())
    expect(cmds['root-continuity']).toContain(GET_PROCESS)
    expect(cmds['root-continuity']).toContain(`--block ${live.blockNumber - 1}`)
    expect(cmds['occupied-before']).toBe(cmds['root-continuity'])
    expect(cmds.voters).toContain(TRANSITION_EVENT)
    expect(cmds.voters).toContain(`--from-block 48477085 --to-block ${live.blockNumber}`)
  })

  it('asks an on-chain census contract about the root', () => {
    const cmds = recheckCommands(
      input({
        census: { origin: 'onchain-dynamic', contract: '0x00000000000000000000000000000000000000c1', root: 42n },
      })
    )
    expect(cmds['census-root']).toContain('getRootBlockNumber(uint256)(uint256)')
    expect(cmds['census-root']).toContain(`42 --block ${live.blockNumber}`)
  })

  it('leaves out what it cannot build yet', () => {
    const cmds = recheckCommands(
      input({ tx: null, commitments: [], ys: [], kzgProofs: [], evaluationPoints: [], publicValues: null })
    )
    expect(cmds['guest-ok']).toBeUndefined()
    expect(cmds['blob-hashes']).toBeUndefined()
    expect(cmds['kzg-openings']).toBeUndefined()
    expect(cmds.plonk).toBeUndefined()
    expect(cmds['root-continuity']).toBeDefined()
  })
})

describe('observerCommand', () => {
  it('pins the registry on Gnosis', () => {
    expect(observerCommand({ chainId: 100, registry: '0xabc', startBlock: 7, beaconUrl: null })).toBe(
      'cargo build --release -p davinci-sequencer\n./target/release/davinci-sequencer --registry 0xabc --start-block 7'
    )
  })

  it('spells out a custom network elsewhere', () => {
    const cmd = observerCommand({ chainId: 31337, registry: '0xabc', startBlock: 0, beaconUrl: 'http://b' })
    expect(cmd).toContain(
      '--network custom --registry 0xabc --start-block 0 --rpc-url $RPC --blob-source beacon:http://b'
    )
  })
})
