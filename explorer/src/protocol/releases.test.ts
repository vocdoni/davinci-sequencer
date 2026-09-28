import { describe, expect, it } from 'vitest'
import { KNOWN_RELEASES, matchRelease, PIN_NAMES, type DeploymentPins } from './releases'

const release = KNOWN_RELEASES[0]!
const allPins: DeploymentPins = Object.fromEntries(PIN_NAMES.map((p) => [p, release[p]]))

describe('matchRelease', () => {
  it('matches a deployment with every pin equal', () => {
    const m = matchRelease(allPins)
    expect(m.release?.id).toBe(release.id)
    expect(m.complete).toBe(true)
    expect(m.checks.every((c) => c.ok)).toBe(true)
  })

  it('is case-insensitive', () => {
    const upper = Object.fromEntries(PIN_NAMES.map((p) => [p, release[p].toUpperCase().replace('0X', '0x')]))
    expect(matchRelease(upper as DeploymentPins).release).not.toBeNull()
  })

  it('names the pin that differs', () => {
    const m = matchRelease({ ...allPins, resultsProgramVK: `0x${'00'.repeat(32)}` })
    expect(m.release).toBeNull()
    expect(m.closest?.id).toBe(release.id)
    expect(m.checks.find((c) => c.pin === 'resultsProgramVK')?.ok).toBe(false)
  })

  it('waits for pins that have not been read', () => {
    const m = matchRelease({ batchProgramVK: release.batchProgramVK })
    expect(m.release).toBeNull()
    expect(m.complete).toBe(false)
    expect(m.checks.find((c) => c.pin === 'ballotVKHash')?.ok).toBeNull()
  })

  it('pins are well-formed 32-byte values', () => {
    for (const r of KNOWN_RELEASES) for (const p of PIN_NAMES) expect(r[p]).toMatch(/^0x[0-9a-f]{64}$/)
  })
})
