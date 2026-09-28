import { describe, expect, it } from 'vitest'
import { computeProcessId, isProcessId, parseProcessId, processIdPrefix } from './process-id'

// A live Gnosis process: organizer 0x42fC…b589 on registry 0x3CDE…daf3.
const PID = '0x42fc20654efd78c6887ff0bd1cc50c9ec1dab58980c5bb930000000000000100'.slice(0, 64)

describe('process ids', () => {
  it('recognises the bytes31 shape only', () => {
    expect(isProcessId(PID)).toBe(true)
    expect(isProcessId(`${PID}00`)).toBe(false)
    expect(isProcessId('0x1234')).toBe(false)
  })

  it('splits organizer, prefix and nonce', () => {
    const p = parseProcessId(PID)
    expect(p.organizer.toLowerCase()).toBe('0x42fc20654efd78c6887ff0bd1cc50c9ec1dab589')
    expect(p.prefix).toBe(0x80c5bb93)
    expect(p.nonce).toBe(1n)
    expect(computeProcessId(p.prefix, p.organizer, p.nonce)).toBe(PID)
  })

  it('derives the registry prefix from chain id and address', () => {
    expect(processIdPrefix(100, '0x3CDE68c39E26ecf94bD029b6ED3b9F945441daf3')).toBe(0x80c5bb93)
  })
})
