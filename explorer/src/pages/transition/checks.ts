// What `submitStateTransition` enforced for each check, and how to read the
// recheck command's output. From davinci-contracts `ProcessRegistry.sol`
// and its README "State transitions".

import type { CensusOriginName } from '~protocol/types'
import type { RecheckId } from './commands'

export interface CheckCopy {
  enforced: string
  recheck: string
}

/** Contract order: the order `submitStateTransition` runs them in. */
export const CHECK_ORDER: RecheckId[] = [
  'guest-ok',
  'root-continuity',
  'census-root',
  'occupied-before',
  'voters',
  'blob-count',
  'blobs-digest',
  'plonk',
  'blob-hashes',
  'kzg-openings',
  'root-after',
]

export const ONCHAIN_LABELS: Record<'plonk' | 'kzg-openings', string> = {
  plonk: 'The PLONK proof verifies under the pinned keys',
  'kzg-openings': 'Every blob opens to its evaluation at the bound point',
}

export function checkCopy(id: RecheckId, censusOrigin: CensusOriginName | null): CheckCopy {
  switch (id) {
    case 'guest-ok':
      return {
        enforced:
          'publicValues must be exactly 512 bytes with ok = 1 and fail_mask = 0, or the call reverts with CircuitFailed. A batch the guest rejected can still be proven, but it can never settle.',
        recheck:
          'Decode the calldata. The second value is publicValues: register i is the 8-byte little-endian word at byte 8·i, so it opens with 0100000000000000 (ok = 1) and then 0000000000000000 (an empty fail mask).',
      }
    case 'root-continuity':
      return {
        enforced:
          "The root before (registers 2..9) must equal the process's latestStateRoot, or InvalidStateRoot. Each transition starts where the previous one ended and the first starts at the genesis root the registry computed at creation, so a batch cannot be skipped, replayed or forked.",
        recheck:
          'Read the registry one block before the settlement: latestStateRoot, the fourth value, is the root this batch had to start from. Reading a past block needs an RPC that keeps old state.',
      }
    case 'census-root':
      return censusOrigin === 'onchain-dynamic'
        ? {
            enforced:
              'For an on-chain census the registry asks the census contract getRootBlockNumber(root) for the root in registers 20..27. The answer must be non-zero, at most the settlement block and at least the block the process was created in, or InvalidCensusRoot.',
            recheck:
              "Ask the census contract yourself at the settlement block. The explorer can't recompute this one from events.",
          }
        : {
            enforced:
              'Registers 20..27, read as a big-endian integer, must equal the census root stored for the process, or InvalidCensusRoot. For a CSP census the root is the signer address. An updatable census accepts only its current root, so a batch proven against a replaced root stops settling.',
            recheck:
              'In the same registry read the census is the fifteenth value; its second field is the root this batch had to prove against.',
          }
    case 'occupied-before':
      return {
        enforced:
          'Register 42 must equal votersCount, the distinct ballot slots written before this batch, or InvalidOccupiedBefore. The guest cannot see the whole tree, so it takes this number as input and uses it to size the silent refreshes; the registry pins it to its own count.',
        recheck: 'votersCount is the ninth value of the registry read one block before the settlement.',
      }
    case 'voters':
      return {
        enforced:
          'The registry adds votes minus overwrites (register 18 − register 19) to votersCount, refusing a batch that would pass maxVoters (MaxVotersReached), adds the overwrites to overwrittenVotesCount and emits the new totals in ProcessStateTransitioned.',
        recheck:
          "List the process's transition events. Each log's data is oldStateRoot, newStateRoot, newVotersCount, newOverwrittenVotesCount and nBlobs, 32 bytes each: newVotersCount grows by votes − overwrites from one log to the next, and the same list shows the root chain.",
      }
    case 'blob-count':
      return {
        enforced:
          'n_blobs (register 36) must be non-zero, the commitment, evaluation and proof arrays must each hold n_blobs entries, and the transaction may carry no blob past them (NoBlobs, BlobCountMismatch).',
        recheck: "The transaction's blobVersionedHashes lists one hash per blob.",
      }
    case 'blobs-digest':
      return {
        enforced:
          'sha256(commitment_0 ‖ y_0 ‖ commitment_1 ‖ y_1 …) over the calldata must equal registers 28..35, or InvalidBlobsDigest. That ties the commitments and evaluations the transaction carries to the ones the guest computed from the data it proved.',
        recheck: 'Hash the commitments and evaluations in order: the result is the blobs digest in the public values.',
      }
    case 'plonk':
      return {
        enforced:
          'ZiskVerifier.verifySnarkProof(batchProgramVK, rootCVadcopFinal, publicValues, proofBytes) must accept, or InvalidProof. Both keys are registry immutables and the verifier hashes them together with the public values, so a proof of another program or another setup does not verify.',
        recheck: 'Make the same call: it returns 0x when the proof verifies and reverts otherwise.',
      }
    case 'blob-hashes':
      return {
        enforced:
          "A blob's versioned hash, what BLOBHASH returns, is 0x01 ‖ sha256(commitment)[1..]. The point-evaluation precompile rejects a commitment that does not match the transaction's hash.",
        recheck: "Hash each commitment and replace the first byte with 01: that is the transaction's versioned hash.",
      }
    case 'kzg-openings':
      return {
        enforced:
          'For every blob the registry calls the point-evaluation precompile with the versioned hash, z = sha256(process id ‖ root before ‖ commitment) mod r_BLS, the evaluation y and the KZG proof, or InvalidBlobOpening. The guest evaluated the blob it laid out itself at the same z, so the blobs on-chain are the ones the proof covers.',
        recheck:
          'Send the same 192 bytes to the precompile: it answers 4096 and the BLS12-381 modulus when the opening holds, and fails otherwise.',
      }
    case 'root-after':
      return {
        enforced:
          'Once everything above passed, the registry stores registers 10..17 as the new latestStateRoot and emits it as newStateRoot.',
        recheck:
          'The ProcessStateTransitioned log in the receipt carries newStateRoot as the second 32-byte word of its data.',
      }
  }
}
