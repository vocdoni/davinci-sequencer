// Shell commands that recheck a settled transition with nothing but an RPC:
// Foundry's `cast` and coreutils. Pure, so the page and the tests build the
// same strings. Every command reads `$RPC`.

import type { TransitionCheck } from '~indexer/selectors'
import type { Hex } from '~protocol/bytes'
import type { CensusOriginName } from '~protocol/types'

export const SUBMIT_SIGNATURE = 'submitStateTransition(bytes31,bytes,bytes,bytes[],bytes32[],bytes[])'

export const TRANSITION_EVENT =
  'ProcessStateTransitioned(bytes31 indexed processId, address indexed sender, bytes32 oldStateRoot, ' +
  'bytes32 newStateRoot, uint256 newVotersCount, uint256 newOverwrittenVotesCount, uint256 nBlobs)'

/** `getProcess` with its return type, so `cast` decodes the struct. */
export const GET_PROCESS =
  'getProcess(bytes31)((uint8,address,(uint256,uint256),bytes32,uint256[],uint256,uint256,uint256,uint256,' +
  'uint256,uint256,uint256,string,(bool,uint8,uint8,uint8,uint256,uint256,uint256,uint256),' +
  '(uint8,bytes32,address,string,bool),uint8,bytes12,uint16,uint8,uint16,bool,bytes32))'

export const VERIFY_SIGNATURE = 'verifySnarkProof(bytes32,bytes32,bytes,bytes)'

/** EIP-4844 point-evaluation precompile. */
export const POINT_EVALUATION = '0x000000000000000000000000000000000000000a'

export type RecheckId = TransitionCheck['id'] | 'plonk' | 'kzg-openings'

export interface RecheckInput {
  registry: string
  verifier: string | null
  processId: Hex
  /** Block the process was created in: the lower bound of its logs. */
  createdBlock: number
  /** Settlement block. */
  block: number
  tx: Hex | null
  census: { origin: CensusOriginName | null; contract: string | null; root: bigint | null }
  batchProgramVK: Hex | null
  rootCVadcopFinal: Hex | null
  publicValues: Hex | null
  proofBytes: Hex | null
  versionedHashes: Hex[]
  commitments: Hex[]
  ys: Hex[]
  kzgProofs: Hex[]
  /** `z_i` per blob (`blobEvaluationPoint`). */
  evaluationPoints: Hex[]
}

const strip = (h: string) => (h.startsWith('0x') ? h.slice(2) : h)

/** `sha256sum` over the bytes of a hex string. */
export function sha256Command(hex: string): string {
  return `printf '%s' ${strip(hex)} | xxd -r -p | sha256sum`
}

/** The 192-byte point-evaluation input: versioned hash, z, y, commitment, proof. */
export function pointEvaluationInput(versionedHash: Hex, z: Hex, y: Hex, commitment: Hex, proof: Hex): Hex {
  return `0x${[versionedHash, z, y, commitment, proof].map(strip).join('')}`
}

/** The registry's view of the process just before the settlement block. */
export function registryStateCommand(input: RecheckInput): string {
  return [
    `cast call ${input.registry} \\`,
    `  "${GET_PROCESS}" \\`,
    `  ${input.processId} --block ${input.block - 1} --rpc-url $RPC`,
  ].join('\n')
}

/** Commands per check; a check is missing when a value it needs is not known yet. */
export function recheckCommands(input: RecheckInput): Partial<Record<RecheckId, string>> {
  const out: Partial<Record<RecheckId, string>> = {}
  const { tx } = input

  if (tx) {
    out['guest-ok'] = [`cast decode-calldata "${SUBMIT_SIGNATURE}" \\`, `  $(cast tx ${tx} input --rpc-url $RPC)`].join(
      '\n'
    )
    out['root-after'] = `cast receipt ${tx} --rpc-url $RPC`
    out['blob-count'] = `cast tx ${tx} blobVersionedHashes --rpc-url $RPC`
  }
  out['root-continuity'] = registryStateCommand(input)
  out['occupied-before'] = registryStateCommand(input)
  out['voters'] = [
    `cast logs --from-block ${input.createdBlock} --to-block ${input.block} --address ${input.registry} \\`,
    `  "${TRANSITION_EVENT}" \\`,
    `  ${input.processId} --rpc-url $RPC`,
  ].join('\n')

  const { census } = input
  if (census.origin === 'onchain-dynamic') {
    if (census.contract && census.root != null) {
      out['census-root'] = [
        `cast call ${census.contract} "getRootBlockNumber(uint256)(uint256)" \\`,
        `  ${census.root.toString()} --block ${input.block} --rpc-url $RPC`,
      ].join('\n')
    }
  } else {
    out['census-root'] = registryStateCommand(input)
  }

  if (input.commitments.length > 0) {
    out['blob-hashes'] = input.commitments.map((c, i) => `# blob ${i}\n${sha256Command(c)}`).join('\n')
  }
  if (input.commitments.length > 0 && input.ys.length === input.commitments.length) {
    out['blobs-digest'] = sha256Command(input.commitments.map((c, i) => strip(c) + strip(input.ys[i]!)).join(''))
  }

  if (input.verifier && input.batchProgramVK && input.rootCVadcopFinal && input.publicValues && input.proofBytes) {
    out.plonk = [
      `cast call ${input.verifier} "${VERIFY_SIGNATURE}" \\`,
      `  ${input.batchProgramVK} \\`,
      `  ${input.rootCVadcopFinal} \\`,
      `  ${input.publicValues} \\`,
      `  ${input.proofBytes} \\`,
      `  --rpc-url $RPC`,
    ].join('\n')
  }

  const n = input.commitments.length
  if (
    n > 0 &&
    input.versionedHashes.length === n &&
    input.ys.length === n &&
    input.kzgProofs.length === n &&
    input.evaluationPoints.length === n
  ) {
    out['kzg-openings'] = input.commitments
      .map((c, i) =>
        [
          `# blob ${i}`,
          `cast call ${POINT_EVALUATION} --rpc-url $RPC --data \\`,
          `  ${pointEvaluationInput(input.versionedHashes[i]!, input.evaluationPoints[i]!, input.ys[i]!, c, input.kzgProofs[i]!)}`,
        ].join('\n')
      )
      .join('\n')
  }
  return out
}

/**
 * Builds and starts a sequencer node without a signing key: an observer that
 * replays every transition from its blobs (sequencer README "Observer mode").
 * The `gnosis` network is the node's default; any other chain is `custom`.
 */
export function observerCommand(opts: {
  chainId: number
  registry: string
  startBlock: number
  beaconUrl: string | null
}): string {
  const flags = [`--registry ${opts.registry}`, `--start-block ${opts.startBlock}`]
  if (opts.chainId !== 100) {
    flags.unshift('--network custom')
    flags.push('--rpc-url $RPC', `--blob-source beacon:${opts.beaconUrl ?? '<beacon url>'}`)
  }
  return ['cargo build --release -p davinci-sequencer', `./target/release/davinci-sequencer ${flags.join(' ')}`].join(
    '\n'
  )
}
