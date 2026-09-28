import { describe, expect, it } from 'vitest'
import { loadJsonBig, vectorBytes } from '../test/vectors'
import {
  BATCH_REGISTERS,
  decodeBatchPublicsRegisters,
  decodeBatchPublicValues,
  decodeResultsPublicValues,
  failBits,
  publicsPassed,
  registersFromWords,
  resultsFailBits,
} from './publics'
import { toHex } from './bytes'
import { KNOWN_RELEASES } from './releases'

const regs = vectorBytes('publics_batch.bin')
const snark = loadJsonBig<{ program_vk: string; root_c_vadcop_final: string; public_values: `0x${string}` }>(
  'snark_batch.json'
)

describe('batch publics (rust-sdk vectors)', () => {
  it('decodes the recorded job', () => {
    const p = decodeBatchPublicsRegisters(regs)
    expect(p.ok && publicsPassed(p)).toBe(true)
    expect(p.failMask).toBe(0)
    expect([p.voters, p.overwrites, p.nBlobs, p.occupiedBefore]).toEqual([3, 3, 1, 24])
    expect([p.nproofs, p.nPublic, p.logN]).toEqual([3, 3, 1])
    expect(p.rootBefore).not.toBe(p.rootAfter)
  })

  it('reads 256-bit values from their register bytes', () => {
    const p = decodeBatchPublicsRegisters(regs)
    expect(p.rootBefore).toBe(toHex(regs.slice(8, 40)))
    expect(p.rootAfter).toBe(toHex(regs.slice(40, 72)))
    expect(p.censusRootLE).toBe(toHex(regs.slice(80, 112)))
    expect(p.blobsDigest).toBe(toHex(regs.slice(112, 144)))
    // The registry compares the census root as a big-endian integer.
    expect(p.censusRoot).toBe(BigInt(toHex(regs.slice(80, 112).reverse())))
  })

  it('reads the same values from the 512-byte publicValues', () => {
    const fromWords = decodeBatchPublicValues(snark.public_values)
    const fromRegs = decodeBatchPublicsRegisters(regs)
    expect({ ...fromWords, registers: [] }).toEqual({ ...fromRegs, registers: [] })
    expect(fromWords.registers.slice(0, 46)).toEqual(fromRegs.registers.slice(0, 46))
  })

  it('matches the pinned release', () => {
    const release = KNOWN_RELEASES[0]!
    expect(snark.program_vk).toBe(release.batchProgramVK)
    expect(snark.root_c_vadcop_final).toBe(release.rootCVadcopFinal)
  })

  it('rejects malformed input', () => {
    const pv = Uint8Array.from(Buffer.from(snark.public_values.slice(2), 'hex'))
    const bad = pv.slice()
    bad[4] = 1 // high half of word 0
    expect(() => decodeBatchPublicValues(bad)).toThrow(/above 32 bits/)
    expect(() => decodeBatchPublicsRegisters(regs.slice(0, 100))).toThrow()
    expect(() => decodeBatchPublicsRegisters(regs.slice(0, 183))).toThrow()
    expect(() => decodeBatchPublicValues(pv.slice(0, 300))).toThrow()
  })

  it('names fail bits', () => {
    expect(failBits(0)).toEqual([])
    expect(failBits((1 << 17) | (1 << 24) | (1 << 31))).toEqual(['reencryption', 'refresh', 'parse_error'])
    expect(failBits(1 << 5)).toEqual(['unknown'])
    expect(resultsFailBits((1 << 4) | (1 << 2))).toEqual(['incl_key', 'cp'])
  })

  it('describes every register the guest commits, in order', () => {
    let next = 0
    for (const r of BATCH_REGISTERS) {
      expect(r.index).toBe(next)
      next = r.index + r.span
    }
    expect(next).toBe(46)
  })
})

describe('results publics', () => {
  it('assembles each tally from its (lo, hi) register pair', () => {
    const words = new Uint8Array(512)
    const put = (reg: number, v: number) => new DataView(words.buffer).setUint32(reg * 8, v, true)
    put(0, 1)
    put(10, 7) // results[0] lo
    put(13, 1) // results[1] hi
    put(42, 0xffffffff)
    const p = decodeResultsPublicValues(words)
    expect(p.ok).toBe(true)
    expect(p.results[0]).toBe(7n)
    expect(p.results[1]).toBe(1n << 32n)
    expect(p.cpFailIndex).toBe(0xffffffff)
    expect(registersFromWords(words).length).toBe(64)
  })
})
