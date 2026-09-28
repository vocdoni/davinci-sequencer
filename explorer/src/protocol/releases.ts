// Known davinci-zkvm release pins, from `rust-sdk/src/release.rs` (and go-sdk
// `chain/release.go`). A deployment's registry immutables are compared with
// these so a page can say which release it runs. Add a row per release; the
// newest first.

import type { Hex } from './bytes'

export interface KnownRelease {
  id: string
  /** Human label, e.g. "davinci-zkvm v0.1.0". */
  label: string
  /** Commit of davinci-zkvm that froze the pins. */
  commit: string
  /** Date the pins were frozen (YYYY-MM-DD). */
  date: string
  /** Vote-batch guest program vk (`batchProgramVK`). */
  batchProgramVK: Hex
  /** Results guest program vk (`resultsProgramVK`). */
  resultsProgramVK: Hex
  /** ZisK vadcop-final setup root (`rootCVadcopFinal`). */
  rootCVadcopFinal: Hex
  /** keccak256 of the ZiskVerifier runtime code. */
  ziskVerifierCodeHash: Hex
  /** sha256 of the ballot Groth16 VK wire bytes (`ballotVKHash`, state leaf 0x07). */
  ballotVKHash: Hex
  /** ZisK release the proofs are wrapped with. */
  zisk: string
}

export const KNOWN_RELEASES: KnownRelease[] = [
  {
    id: 'davinci-zkvm@v0.1.0',
    label: 'davinci-zkvm v0.1.0',
    commit: '67c62d1',
    date: '2026-09-28',
    batchProgramVK: '0x6cfc89d562d0b22f04478a5c15b390433eb52f1b03147030b183076260da7a10',
    resultsProgramVK: '0x7bc8c5e9235548386a44b1885732a2a7ffb1badddc8c7fba599d07ece47be794',
    rootCVadcopFinal: '0x05006517b6ccde5da4d890587ba62845b5af8a307c00e87d4b9d05099b16dc80',
    ziskVerifierCodeHash: '0x82385a405b7301345d7e246017846ca3228aaea349cb68b116d76e0e77056566',
    ballotVKHash: '0xbf1e6590bb1ba883d601c4d7d1c6fa2722a78590716874019db6d68fc776bb0e',
    zisk: '1.3.0-alpha',
  },
]

export type PinName =
  'batchProgramVK' | 'resultsProgramVK' | 'rootCVadcopFinal' | 'ziskVerifierCodeHash' | 'ballotVKHash'

export const PIN_NAMES: PinName[] = [
  'batchProgramVK',
  'resultsProgramVK',
  'rootCVadcopFinal',
  'ziskVerifierCodeHash',
  'ballotVKHash',
]

export const PIN_LABELS: Record<PinName, string> = {
  batchProgramVK: 'Vote-batch program vk',
  resultsProgramVK: 'Results program vk',
  rootCVadcopFinal: 'ZisK setup root (rootCVadcopFinal)',
  ziskVerifierCodeHash: 'Verifier code hash',
  ballotVKHash: 'Ballot VK hash',
}

export type DeploymentPins = Partial<Record<PinName, Hex | null>>

export interface PinCheck {
  pin: PinName
  expected: Hex
  /** Null while the value has not been read. */
  actual: Hex | null
  ok: boolean | null
}

export interface ReleaseMatch {
  /** The release every known pin matches, or null. */
  release: KnownRelease | null
  /** The release with the most matching pins, checked field by field. */
  closest: KnownRelease | null
  checks: PinCheck[]
  /** True when every pin has been read. */
  complete: boolean
}

const same = (a: Hex | null | undefined, b: Hex) => a != null && a.toLowerCase() === b.toLowerCase()

/** Compares a deployment's pins with every known release. */
export function matchRelease(pins: DeploymentPins, releases: KnownRelease[] = KNOWN_RELEASES): ReleaseMatch {
  const complete = PIN_NAMES.every((p) => pins[p] != null)
  let best: KnownRelease | null = null
  let bestScore = -1
  for (const r of releases) {
    const score = PIN_NAMES.filter((p) => same(pins[p], r[p])).length
    if (score > bestScore) {
      best = r
      bestScore = score
    }
  }
  const checks: PinCheck[] = best
    ? PIN_NAMES.map((pin) => {
        const actual = pins[pin] ?? null
        return { pin, expected: best![pin], actual, ok: actual == null ? null : same(actual, best![pin]) }
      })
    : []
  const release = best && complete && checks.every((c) => c.ok) ? best : null
  return { release, closest: best, checks, complete }
}
