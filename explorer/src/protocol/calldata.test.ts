import { describe, expect, it } from 'vitest'
import { encodeFunctionData, type Hex } from 'viem'
import { processRegistryAbi } from '~contracts/abis'
import { loadJsonBig } from '../test/vectors'
import { blobsDigest } from './blob'
import { bigIntToBe, toHex } from './bytes'
import { CalldataError, decodeRegistryCall, decodeStateTransitionCall } from './calldata'

const snark = loadJsonBig<{ public_values: Hex; proof_bytes: Hex }>('snark_batch.json')
const blob =
  loadJsonBig<Array<{ process_id: string; commitments: string[]; ys: string[]; proofs: string[] }>>('blob.json')
const hex = (s: string) => `0x${s}` as Hex

describe('submitStateTransition calldata', () => {
  const c = blob[2]!
  const pid = toHex(bigIntToBe(BigInt(c.process_id), 31))
  const input = encodeFunctionData({
    abi: processRegistryAbi,
    functionName: 'submitStateTransition',
    args: [pid, snark.public_values, snark.proof_bytes, c.commitments.map(hex), c.ys.map(hex), c.proofs.map(hex)],
  })

  it('round-trips the six arguments', () => {
    const call = decodeStateTransitionCall(input)
    expect(call.processId).toBe(pid)
    expect(call.publicValues).toBe(snark.public_values)
    expect(call.proofBytes).toBe(snark.proof_bytes)
    expect(call.commitments).toEqual(c.commitments.map(hex))
    expect(call.ys).toEqual(c.ys.map(hex))
    expect(call.kzgProofs).toEqual(c.proofs.map(hex))
    expect((call.proofBytes.length - 2) / 2).toBe(768)
  })

  it('decodes the publics carried in the call', () => {
    const call = decodeStateTransitionCall(input)
    expect(call.publics.ok).toBe(true)
    expect([call.publics.voters, call.publics.overwrites, call.publics.nBlobs]).toEqual([3, 3, 1])
  })

  it('lets a reader recompute the blob digest from the call', () => {
    const call = decodeStateTransitionCall(input)
    expect(blobsDigest(call.commitments, call.ys)).toMatch(/^0x[0-9a-f]{64}$/)
  })

  it('refuses other calls and garbage', () => {
    const other = encodeFunctionData({ abi: processRegistryAbi, functionName: 'setProcessStatus', args: [pid, 1] })
    expect(decodeRegistryCall(other).name).toBe('setProcessStatus')
    expect(() => decodeStateTransitionCall(other)).toThrow(CalldataError)
    expect(() => decodeRegistryCall('0xdeadbeef')).toThrow(CalldataError)
  })
})

describe('setProcessResults calldata', () => {
  it('decodes the results publics', () => {
    const words = new Uint8Array(512)
    new DataView(words.buffer).setUint32(0, 1, true)
    new DataView(words.buffer).setUint32(10 * 8, 42, true)
    const pid = `0x${'11'.repeat(31)}` as Hex
    const input = encodeFunctionData({
      abi: processRegistryAbi,
      functionName: 'setProcessResults',
      args: [pid, toHex(words), '0x1234'],
    })
    const decoded = decodeRegistryCall(input)
    expect(decoded.name).toBe('setProcessResults')
    if (decoded.name === 'setProcessResults' && 'call' in decoded) {
      const call = decoded.call as { publics: { results: bigint[] } }
      expect(call.publics.results[0]).toBe(42n)
    }
  })
})
