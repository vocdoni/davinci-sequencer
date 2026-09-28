import { describe, expect, it } from 'vitest'
import { statusStep, validateLookup } from './lookup'

const PID = '0x42fc20654efd78c6887ff0bd1cc50c9ec1dab589b12878d500000000000001'

describe('validateLookup', () => {
  it('accepts a process id and a hex or decimal vote id', () => {
    expect(validateLookup(` ${PID.toUpperCase().replace('0X', '0x')} `, '0x8000000000000001')).toEqual({
      pid: PID,
      voteId: 0x8000000000000001n,
      pidError: null,
      voteError: null,
    })
    expect(validateLookup(PID, '9223372036854775809').voteId).toBe(0x8000000000000001n)
  })

  it('asks for missing fields', () => {
    const q = validateLookup('', '')
    expect(q.pidError).toMatch(/Enter the process id/)
    expect(q.voteError).toMatch(/Enter the vote id/)
    expect(q.pid).toBeNull()
    expect(q.voteId).toBeNull()
  })

  it('explains what is wrong with a vote id', () => {
    expect(validateLookup(PID, '0x1').voteError).toMatch(/start at 0x8000000000000000/)
    expect(validateLookup(PID, '99999999999999999999').voteError).toMatch(/64 bits/)
    expect(validateLookup(PID, 'hello').voteError).toMatch(/16 hex digits/)
  })

  it('rejects a short process id', () => {
    expect(validateLookup('0x42fc', '0x8000000000000001').pidError).toMatch(/62 hex digits/)
  })
})

describe('statusStep', () => {
  it('orders the happy path and flags errors', () => {
    expect(['pending', 'aggregated', 'processed', 'settled'].map((s) => statusStep(s as never))).toEqual([0, 1, 2, 3])
    expect(statusStep('error')).toBe(-1)
  })
})
