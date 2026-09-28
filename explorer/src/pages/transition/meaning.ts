// Page copy for the publics: the guest's fail bits and the registers, in the
// words of davinci-zkvm `circuit/CIRCUIT.md` §3 and §11.

import type { TransitionCheck } from '~indexer/selectors'
import { BATCH_FAIL_BITS } from '~protocol/publics'

export interface FailBitMeaning {
  bit: number
  /** Constant in the guest source. */
  constant: string
  meaning: string
}

/** CIRCUIT.md §11, one row per bit. */
export const FAIL_BIT_MEANINGS: FailBitMeaning[] = [
  {
    bit: 1,
    constant: 'FAIL_CURVE',
    meaning: 'A ballot proof or VK point is off the curve or the identity, or β/γ/δ is outside the G2 subgroup.',
  },
  { bit: 2, constant: 'FAIL_PAIRING', meaning: 'The batched pairing check of the ballot proofs failed.' },
  {
    bit: 3,
    constant: 'FAIL_ECDSA',
    meaning:
      'A vote signature is missing, a vote id is not a u64, key recovery failed or the key does not match the voter.',
  },
  { bit: 10, constant: 'FAIL_SMT_VOTEID', meaning: 'The vote-id insertion chain is invalid.' },
  { bit: 11, constant: 'FAIL_SMT_BALLOT', meaning: 'The ballot insertion or update chain is invalid.' },
  {
    bit: 12,
    constant: 'FAIL_SMT_RESULTS',
    meaning: 'The results transition is invalid or is not an update of key 0x04.',
  },
  { bit: 13, constant: 'FAIL_SMT_PROCESS', meaning: 'A process config read proof is invalid or missing.' },
  {
    bit: 14,
    constant: 'FAIL_CONSISTENCY',
    meaning: 'A vote id is outside its namespace, has non-zero upper limbs or is not bound to its ballot proof.',
  },
  {
    bit: 15,
    constant: 'FAIL_BALLOT_NS',
    meaning:
      'A ballot slot is outside its namespace, has non-zero upper limbs, is not the slot bound to the voter, or repeats in the batch.',
  },
  {
    bit: 16,
    constant: 'FAIL_CENSUS',
    meaning:
      'A census proof has an invalid path or a non-canonical shape, a leaf repeats, or the proofs disagree on the census root.',
  },
  {
    bit: 17,
    constant: 'FAIL_REENC',
    meaning:
      'The election key is invalid, a re-encryption does not match, a padded field is not the identity, or num_fields is out of range.',
  },
  {
    bit: 18,
    constant: 'FAIL_KZG',
    meaning: 'The blob count is zero or is not the number of blobs the data needs.',
  },
  {
    bit: 19,
    constant: 'FAIL_MISSING_BLOCK',
    meaning: 'A required input block (the state transition, the re-encryptions or the census) is absent or empty.',
  },
  {
    bit: 20,
    constant: 'FAIL_RESULT_ACCUM',
    meaning:
      'The new accumulator leaf does not match the ballots, the results transition is missing or unexpected, or a padded field is not the identity.',
  },
  { bit: 21, constant: 'FAIL_LEAF_HASH', meaning: 'A ballot leaf hash or the ballot count does not match.' },
  { bit: 22, constant: 'FAIL_BINDING', meaning: 'The input blocks do not agree with each other.' },
  {
    bit: 23,
    constant: 'FAIL_CSP',
    meaning: 'A CSP census entry is malformed or repeated, or its signature does not recover to the CSP.',
  },
  {
    bit: 24,
    constant: 'FAIL_REFRESH',
    meaning:
      'The silent refreshes break a rule: too many or too few, keys not increasing or overlapping the batch, broken root chaining, a leaf hash or re-encryption that does not match, a padded field that is not the identity, or occupied_before too large for its register.',
  },
  { bit: 31, constant: 'FAIL_PARSE', meaning: 'The prover input could not be parsed.' },
]

/** The shared table's short name for a bit (go-sdk `FailString`). */
export function failBitName(bit: number): string {
  return BATCH_FAIL_BITS.find((b) => b.bit === bit)?.name ?? `bit ${bit}`
}

/** Register → the settlement check that reads it. */
export const REGISTER_CHECK: Partial<Record<number, TransitionCheck['id']>> = {
  0: 'guest-ok',
  1: 'guest-ok',
  2: 'root-continuity',
  10: 'root-after',
  18: 'voters',
  19: 'voters',
  20: 'census-root',
  28: 'blobs-digest',
  36: 'blob-count',
  42: 'occupied-before',
}

/** Extra notes on how a register is encoded or read, beyond the shared description. */
export const REGISTER_NOTES: Partial<Record<number, string>> = {
  2: 'Eight registers whose little-endian bytes, concatenated, are the raw root digest the registry stores.',
  10: 'Same encoding as the root before.',
  20: 'Read byte-reversed, as a big-endian integer: that is how the registry stores a census root.',
  28: 'Zero when the guest failed its blob-count check.',
  37: 'Registers 46 to 63 are zero too: a ZisK proof carries 64 registers and this guest writes 46.',
  42: 'The guest cannot see the whole tree, so the registry pins this one to its own count.',
}

export const READ_BY_LABEL: Record<'contract' | 'fold' | 'diagnostic', { label: string; hint: string }> = {
  contract: {
    label: 'Registry',
    hint: 'submitStateTransition reads it and refuses the batch if it is wrong.',
  },
  fold: {
    label: 'Fold guest',
    hint: 'In chained mode the aggregator guest checks it while folding batches into one proof.',
  },
  diagnostic: {
    label: 'Nobody',
    hint: 'A diagnostic: nothing on-chain reads it.',
  },
}
