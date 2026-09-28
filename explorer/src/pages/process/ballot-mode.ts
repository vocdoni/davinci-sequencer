// A process's BallotMode in words. The rules are the ones the voter's ballot
// proof enforces (davinci-circom `CheckBallotMode`): every field in
// [minValue, maxValue], no repeated value when uniqueValues is set, and the
// sum of value^costExponent in [minValueSum, maxValueSum], where a zero
// maxValueSum means "up to the voter's census weight". groupSize is only
// checked to be at most numFields; it describes multi-question layouts.

import type { BallotMode } from '~indexer/types'
import { formatNumber } from '~lib/format'

export type BallotKind =
  | 'single-choice'
  | 'multiple-choice'
  | 'approval'
  | 'ranking'
  | 'quadratic'
  | 'rating'
  | 'points'
  | 'unsatisfiable'
  | 'custom'

export interface BallotModeDescription {
  kind: BallotKind
  /**
   * Short name of the pattern the rules read as, e.g. "Single choice". The
   * protocol has no ballot-type field, so this is a reading, not a fact.
   */
  label: string
  /** One sentence: what a voter can put on the ballot. */
  summary: string
  /** Each rule the ballot proof enforces, in words. */
  rules: string[]
}

const n = (v: bigint | number) => formatNumber(v)
const plural = (count: bigint | number, one: string, many = `${one}s`) => (BigInt(count) === 1n ? one : many)

/** "up to 20 points", or "as many points as their census weight" when maxValueSum is 0. */
function budget(bm: BallotMode, unit: string): string {
  return bm.maxValueSum > 0n ? `up to ${n(bm.maxValueSum)} ${unit}` : `as many ${unit} as their census weight`
}

function costText(e: number): string {
  if (e === 1) return 'the values'
  if (e === 2) return 'the squares of the values'
  return `each value raised to the power ${e}`
}

export function describeBallotMode(bm: BallotMode): BallotModeDescription {
  const k = bm.numFields
  const options = `${n(k)} ${plural(k, 'option')}`
  const binary = bm.minValue === 0n && bm.maxValue === 1n
  const e = bm.costExponent
  const rules: string[] = []

  rules.push(
    `The ballot has ${n(k)} ${plural(k, 'field')}, usually one per option; each holds a number between ${n(bm.minValue)} and ${n(bm.maxValue)}.`
  )
  if (bm.uniqueValues) rules.push('No two fields may carry the same value.')
  const sumOf = e === 1 ? 'The fields' : `The sum of ${costText(e)}`
  const upper =
    bm.maxValueSum > 0n ? `at most ${n(bm.maxValueSum)}` : "at most the voter's census weight (maxValueSum is 0)"
  const lower = bm.minValueSum > 0n ? ` and at least ${n(bm.minValueSum)}` : ''
  rules.push(e === 1 ? `${sumOf} add up to ${upper}${lower}.` : `${sumOf} is ${upper}${lower}.`)
  if (bm.groupSize > 1) {
    rules.push(
      `Fields come in groups of ${n(bm.groupSize)} (a multi-question layout); the ballot proof only checks the group size does not exceed the field count.`
    )
  }
  rules.push(
    "The tally adds each field over every voter's latest ballot: an option's result is the sum of the values voters gave it."
  )

  // Unique values need at least as many distinct values as fields.
  const distinct = bm.maxValue - bm.minValue + 1n
  if (bm.uniqueValues && distinct < BigInt(k)) {
    return {
      kind: 'unsatisfiable',
      label: 'Cannot be satisfied',
      summary: `No ballot can meet these rules: the ${n(k)} fields must all differ, but only ${n(distinct)} ${plural(distinct, 'value is', 'values are')} allowed.`,
      rules,
    }
  }

  // Common patterns first; anything else is described by its rules.
  if (bm.uniqueValues && k > 1) {
    return {
      kind: 'ranking',
      label: 'Ranking',
      summary: `Each voter ranks ${options}, giving each a different value from ${n(bm.minValue)} to ${n(bm.maxValue)}.`,
      rules,
    }
  }
  // A budget that every ballot within the per-field bounds already meets never binds.
  if (
    e >= 1 &&
    bm.maxValue > 1n &&
    k > 1 &&
    bm.maxValueSum > 0n &&
    BigInt(k) * bm.maxValue ** BigInt(e) <= bm.maxValueSum
  ) {
    return {
      kind: 'rating',
      label: 'Rating',
      summary: `Each voter rates each of ${options} from ${n(bm.minValue)} to ${n(bm.maxValue)}.`,
      rules,
    }
  }
  if (e >= 2 && bm.maxValue > 1n) {
    return {
      kind: 'quadratic',
      label: e === 2 ? 'Quadratic voting' : `Cost exponent ${e}`,
      summary: `Each voter spends ${budget(bm, 'credits')} across ${options}; putting v votes on one option costs v${e === 2 ? '²' : `^${e}`} credits, at most ${n(bm.maxValue)} votes per option.`,
      rules,
    }
  }
  if (binary && k > 1 && e >= 1) {
    const atLeast = bm.minValueSum > 0n ? `at least ${n(bm.minValueSum)} and ` : ''
    if (bm.maxValueSum === 1n) {
      return {
        kind: 'single-choice',
        label: 'Single choice',
        summary:
          bm.minValueSum > 0n
            ? `Each voter picks exactly one of ${options}.`
            : `Each voter picks one of ${options}, or none (a blank ballot).`,
        rules,
      }
    }
    if (bm.maxValueSum > 1n && bm.maxValueSum < BigInt(k)) {
      return {
        kind: 'multiple-choice',
        label: 'Multiple choice',
        summary: `Each voter picks ${atLeast}up to ${n(bm.maxValueSum)} of ${options}.`,
        rules,
      }
    }
    return {
      kind: 'approval',
      label: 'Approval',
      summary:
        bm.maxValueSum === 0n
          ? `Each voter approves ${atLeast}up to W of ${options}, W being their census weight.`
          : bm.minValueSum > 0n
            ? `Each voter approves at least ${n(bm.minValueSum)} of ${options}.`
            : `Each voter approves any number of ${options}.`,
      rules,
    }
  }
  if (e === 1 && bm.maxValue > 1n && k > 1) {
    return {
      kind: 'points',
      label: 'Points',
      summary: `Each voter distributes ${budget(bm, 'points')} among ${options}, at most ${n(bm.maxValue)} per option.`,
      rules,
    }
  }
  return {
    kind: 'custom',
    label: k === 1 ? 'Single field' : 'Custom',
    summary:
      k === 1
        ? `Each voter enters one value between ${n(bm.minValue)} and ${n(bm.maxValue)}.`
        : `Each voter fills ${n(k)} fields with values between ${n(bm.minValue)} and ${n(bm.maxValue)}.`,
    rules,
  }
}
