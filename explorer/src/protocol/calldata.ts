// Decoding of ProcessRegistry transactions. A state transition carries its
// proof and publics in the calldata and its data in the blobs:
//   submitStateTransition(processId, publicValues, proofBytes,
//                         commitments[], ys[], kzgProofs[])

import { decodeFunctionData, type Hex } from 'viem'
import { processRegistryAbi } from '~contracts/abis'
import { decodeBatchPublicValues, decodeResultsPublicValues, type BatchPublics, type ResultsPublics } from './publics'

export class CalldataError extends Error {}

export interface StateTransitionCall {
  processId: Hex
  publicValues: Hex
  proofBytes: Hex
  /** 48-byte KZG commitments, one per blob. */
  commitments: Hex[]
  /** Blob evaluations at the guest's points, one per blob. */
  ys: Hex[]
  /** 48-byte KZG openings, one per blob. */
  kzgProofs: Hex[]
  publics: BatchPublics
}

export interface ResultsCall {
  processId: Hex
  publicValues: Hex
  proofBytes: Hex
  publics: ResultsPublics
}

export interface DecryptionRequestCall {
  processId: Hex
  /** 64 coordinates: `(c1x, c1y, c2x, c2y)` per field. */
  accumulator: bigint[]
  siblings: Hex[]
}

export type RegistryCall =
  | { name: 'submitStateTransition'; call: StateTransitionCall }
  | { name: 'setProcessResults'; call: ResultsCall }
  | { name: 'requestResultsDecryption'; call: DecryptionRequestCall }
  | { name: string; args: readonly unknown[] }

const lower = (h: unknown) => String(h).toLowerCase() as Hex

/** Any ProcessRegistry call, with the three settlement calls decoded fully. */
export function decodeRegistryCall(input: Hex): RegistryCall {
  let decoded: { functionName: string; args?: readonly unknown[] }
  try {
    decoded = decodeFunctionData({ abi: processRegistryAbi, data: input }) as typeof decoded
  } catch (err) {
    throw new CalldataError(`not a ProcessRegistry call: ${err instanceof Error ? err.message.split('\n')[0] : err}`)
  }
  const args = decoded.args ?? []
  switch (decoded.functionName) {
    case 'submitStateTransition': {
      const [processId, publicValues, proofBytes, commitments, ys, kzgProofs] = args as [
        Hex,
        Hex,
        Hex,
        Hex[],
        Hex[],
        Hex[],
      ]
      return {
        name: 'submitStateTransition',
        call: {
          processId: lower(processId),
          publicValues,
          proofBytes,
          commitments: commitments.map(lower),
          ys: ys.map(lower),
          kzgProofs: kzgProofs.map(lower),
          publics: decodeBatchPublicValues(publicValues),
        },
      }
    }
    case 'setProcessResults': {
      const [processId, publicValues, proofBytes] = args as [Hex, Hex, Hex]
      return {
        name: 'setProcessResults',
        call: {
          processId: lower(processId),
          publicValues,
          proofBytes,
          publics: decodeResultsPublicValues(publicValues),
        },
      }
    }
    case 'requestResultsDecryption': {
      const [processId, accumulator, siblings] = args as [Hex, readonly bigint[], readonly Hex[]]
      return {
        name: 'requestResultsDecryption',
        call: { processId: lower(processId), accumulator: [...accumulator], siblings: siblings.map(lower) },
      }
    }
    default:
      return { name: decoded.functionName, args }
  }
}

/** Decodes `submitStateTransition` calldata or throws. */
export function decodeStateTransitionCall(input: Hex): StateTransitionCall {
  const decoded = decodeRegistryCall(input)
  if (decoded.name !== 'submitStateTransition' || !('call' in decoded)) {
    throw new CalldataError(`expected submitStateTransition, got ${decoded.name}`)
  }
  return decoded.call as StateTransitionCall
}
