// What the contracts page says and checks, kept out of the components so it
// can be tested: the explanation of every pinned value, the rows of the
// address table, the wiring checks and the verification commands.

import type { Address, Hex } from 'viem'
import type { DeploymentDetails, DkgVerifierName } from '~data/deployment'
import type { CheckState } from '~indexer/selectors'
import type { ChainMeta } from '~indexer/types'
import { checksum } from '~lib/address'
import { processIdPrefix } from '~protocol/process-id'
import type { KnownRelease, PinName, ReleaseMatch } from '~protocol/releases'

export interface PinDetail {
  /** The Solidity name, or how the value is read. */
  source: string
  what: string
  why: string
  /** What a value different from the release would mean. */
  mismatch: string
}

export const PIN_DETAILS: Record<PinName, PinDetail> = {
  batchProgramVK: {
    source: 'registry batchProgramVK()',
    what: 'The program verification key of the vote-batch guest (davinci-zkvm circuit/elf/circuit.elf): what cargo-zisk setup prints as its root hash.',
    why: 'submitStateTransition verifies every batch proof against it. The verifier hashes it into the proof’s public input, so only a proof of this exact program settles a transition.',
    mismatch:
      'Transitions would be proven by a program other than the released vote-batch guest, so none of the checks that guest makes could be taken for granted.',
  },
  resultsProgramVK: {
    source: 'registry resultsProgramVK()',
    what: 'The program verification key of the results guest (circuit-results/elf/results.elf).',
    why: 'setProcessResults verifies the tally proof of every sequencer-key process against it: the key and accumulator leaves under the final root and the 16 Chaum–Pedersen decryptions.',
    mismatch:
      'Sequencer-key tallies would be proven by another program. DKG-key tallies do not use this key: the committee’s proofs replace it.',
  },
  rootCVadcopFinal: {
    source: 'registry rootCVadcopFinal()',
    what: 'The root of the ZisK vadcop-final setup the proofs are wrapped with. It moves with the ZisK snark setup, not with the guests.',
    why: 'The verifier’s public input is sha256(programVK ‖ publicValues ‖ rootCVadcopFinal), so a proof made under another setup does not verify.',
    mismatch:
      'The registry would expect proofs from another ZisK setup: proofs from a prover on the released setup would not verify here.',
  },
  ziskVerifierCodeHash: {
    source: 'keccak256(eth_getCode(ziskVerifier))',
    what: 'keccak256 of the ZiskVerifier’s runtime code. The contract has no immutables, so this equals the hash of its deployedBytecode in a local forge build.',
    why: 'The registry believes whatever this contract says about a proof. The hash shows it is the vendored ZisK verifier and nothing else.',
    mismatch:
      'The contract that checks every proof would not be the released verifier code, and could accept proofs the real verifier rejects.',
  },
  ballotVKHash: {
    source: 'registry ballotVKHash()',
    what: 'sha256 of the ballot proof verification key: the davinci-circom Groth16 key voters’ clients prove their ballots against.',
    why: 'The registry writes it into every process’s genesis state as leaf 0x07. The batch guest hashes the key it is given and requires it to equal that leaf, so only ballots proven for this circuit settle.',
    mismatch:
      'Ballots would be checked against another circuit’s key: ballots from the released client would fail, and ballots from the other circuit would pass.',
  },
}

export interface ContractRow {
  id: string
  name: string
  role: string
  address: Address | null
  /** Shown instead of an address when there is none. */
  note?: string
  group: 'davinci' | 'dkg'
}

export const DKG_VERIFIER_LABELS: Record<DkgVerifierName, { name: string; role: string }> = {
  contribution: {
    name: 'ContributionVerifier',
    role: 'Groth16 verifier of each committee member’s contribution, which deals its shares of all 16 pool keys.',
  },
  finalize: {
    name: 'FinalizeVerifier',
    role: 'Groth16 verifier of finalizeEpoch, which stores the 16 pool keys and their share roots and makes the epoch Live.',
  },
  partialDecrypt: {
    name: 'PartialDecryptVerifier',
    role: 'Groth16 verifier of each member’s partial decryption, checked against its committed share.',
  },
  decryptCombine: {
    name: 'DecryptCombineVerifier',
    role: 'Groth16 verifier of the combine that interpolates the partials and recovers a plaintext.',
  },
}

/** Every contract of the deployment, in reading order; null addresses are still being read. */
export function contractRows(chain: ChainMeta, details: DeploymentDetails | undefined): ContractRow[] {
  const r = chain.registry
  const rows: ContractRow[] = [
    {
      id: 'registry',
      name: 'ProcessRegistry',
      role: 'Stores the processes, settles every state transition (proof, root continuity, census root, blob openings) and records the results.',
      address: chain.registryAddress.toLowerCase() as Address,
      group: 'davinci',
    },
    {
      id: 'verifier',
      name: 'ZiskVerifier',
      role: 'The vendored ZisK PLONK verifier. The registry calls verifySnarkProof on it for every transition and every results proof.',
      address: r?.ziskVerifier ?? null,
      group: 'davinci',
    },
  ]
  if (r && !r.dkgAdapter) {
    rows.push({
      id: 'adapter',
      name: 'DavinciDKGAdapter',
      role: 'The registry’s link to davinci-dkg.',
      address: null,
      note: 'None: this registry was deployed without a DKG manager, so the DKG key modes are disabled.',
      group: 'davinci',
    })
    return rows
  }
  rows.push(
    {
      id: 'adapter',
      name: 'DavinciDKGAdapter',
      role: 'Created by the registry. Registers one DKG application per DKG-mode process, is its only ciphertext submitter, converts keys between the two BabyJubJub forms and reads the plaintexts back.',
      address: r?.dkgAdapter ?? null,
      group: 'davinci',
    },
    {
      id: 'dkg-manager',
      name: 'DKGManager',
      role: 'Epochs, pool keys, ciphertexts, partial and combined decryptions.',
      address: r?.dkgManager ?? null,
      group: 'dkg',
    },
    {
      id: 'dkg-app-manager',
      name: 'DKGAppManager',
      role: 'Application registration, submission policy and the organizer-secret reveal.',
      address: r?.dkgAppManager ?? null,
      group: 'dkg',
    },
    {
      id: 'dkg-registry',
      name: 'DKGRegistry',
      role: 'The committee operators, their encryption keys and liveness.',
      address: details?.dkg?.operatorRegistry ?? null,
      group: 'dkg',
    }
  )
  for (const v of details?.dkg?.verifiers ?? defaultVerifiers()) {
    rows.push({
      id: `dkg-${v.name}`,
      name: DKG_VERIFIER_LABELS[v.name].name,
      role: DKG_VERIFIER_LABELS[v.name].role,
      address: v.address,
      group: 'dkg',
    })
  }
  return rows
}

function defaultVerifiers(): Array<{ name: DkgVerifierName; address: null }> {
  return (['contribution', 'finalize', 'partialDecrypt', 'decryptCombine'] as const).map((name) => ({
    name,
    address: null,
  }))
}

export interface WiringCheck {
  id: string
  label: string
  detail: string
  state: CheckState
}

const same = (a: string | null | undefined, b: string | null | undefined): CheckState =>
  a == null || b == null ? 'unknown' : a.toLowerCase() === b.toLowerCase() ? 'pass' : 'fail'

/** Consistency checks between the contracts, all from values read on chain. */
export function wiringChecks(
  chain: ChainMeta,
  details: DeploymentDetails | undefined,
  expectedChainId: number
): WiringCheck[] {
  const r = chain.registry
  const checks: WiringCheck[] = [
    {
      id: 'chain-id',
      label: 'The registry’s chainID is the chain the explorer reads',
      detail: `registry chainID() = ${r?.chainID ?? '…'}, configured chain ${expectedChainId}`,
      state: r ? (r.chainID === expectedChainId ? 'pass' : 'fail') : 'unknown',
    },
    {
      id: 'pid-prefix',
      label: 'pidPrefix is the low 4 bytes of keccak256(chainID ‖ registry)',
      detail: r
        ? `on chain 0x${r.pidPrefix.toString(16).padStart(8, '0')}, recomputed 0x${processIdPrefix(r.chainID, chain.registryAddress).toString(16).padStart(8, '0')}`
        : 'not read yet',
      state: r ? (processIdPrefix(r.chainID, chain.registryAddress) === r.pidPrefix ? 'pass' : 'fail') : 'unknown',
    },
    {
      id: 'verifier-root',
      label: 'The verifier’s getRootCVadcopFinal() equals the registry’s rootCVadcopFinal',
      detail: 'Sequencers check this at boot; a verifier built for another setup would fail it.',
      state: same(details?.verifierRootC, r?.rootCVadcopFinal),
    },
  ]
  if (r && !r.dkgAdapter) return checks
  const dkg = details?.dkg
  checks.push(
    {
      id: 'adapter-registry',
      label: 'The adapter’s registry() is this registry',
      detail: 'The registry’s constructor creates the adapter, so it must point back at it.',
      state: same(dkg?.adapterRegistry, chain.registryAddress),
    },
    {
      id: 'app-manager',
      label: 'The DKGAppManager’s MANAGER() is the DKGManager',
      detail: 'The two DKG contracts share one logical storage and must name each other.',
      state: same(dkg?.appManagerManager, r?.dkgManager),
    },
    {
      id: 'operator-registry',
      label: 'The DKGRegistry’s manager() is the DKGManager',
      detail: 'The committee is drawn from this operator registry.',
      state: same(dkg?.operatorRegistryManager, r?.dkgManager),
    },
    {
      id: 'dkg-chain',
      label: 'The DKGManager’s CHAIN_ID is the chain the explorer reads',
      detail: `CHAIN_ID() = ${dkg?.chainId ?? '…'}`,
      state: dkg?.chainId == null ? 'unknown' : dkg.chainId === expectedChainId ? 'pass' : 'fail',
    }
  )
  return checks
}

/** The first plain http(s) RPC, for commands run outside the browser. */
export function publicRpc(rpcUrls: string[]): string {
  return rpcUrls.find((u) => /^https?:\/\//.test(u)) ?? '$RPC_URL'
}

export interface VerifyCommandInput {
  rpc: string
  chainId: number
  registry: string
  pins: Partial<Record<'batchProgramVK' | 'resultsProgramVK' | 'rootCVadcopFinal' | 'ballotVKHash', Hex | null>>
}

const orPlaceholder = (v: Hex | null | undefined, name: string) => v ?? `<${name}>`

/** davinci-contracts `script/verify_deployment.py`, filled in. */
export function verifyDeploymentCommand({ rpc, chainId, registry, pins }: VerifyCommandInput): string {
  return [
    `python3 script/verify_deployment.py --rpc ${rpc} --chain-id ${chainId} \\`,
    `    --registry ${checksum(registry)} \\`,
    `    --batch-vk ${orPlaceholder(pins.batchProgramVK, 'batchProgramVK')} \\`,
    `    --results-vk ${orPlaceholder(pins.resultsProgramVK, 'resultsProgramVK')} \\`,
    `    --root-c ${orPlaceholder(pins.rootCVadcopFinal, 'rootCVadcopFinal')} \\`,
    `    --ballot-vk-hash ${orPlaceholder(pins.ballotVKHash, 'ballotVKHash')}`,
  ].join('\n')
}

/** The same pins from a known release, for the command. */
export function releasePins(release: KnownRelease): VerifyCommandInput['pins'] {
  return {
    batchProgramVK: release.batchProgramVK,
    resultsProgramVK: release.resultsProgramVK,
    rootCVadcopFinal: release.rootCVadcopFinal,
    ballotVKHash: release.ballotVKHash,
  }
}

/** Foundry `cast` reads of every pinned value. */
export function castCommands(rpc: string, registry: string, verifier: string | null): string {
  const lines = [
    `RPC=${rpc}`,
    `REGISTRY=${checksum(registry)}`,
    `cast call $REGISTRY "batchProgramVK()(bytes32)" --rpc-url $RPC`,
    `cast call $REGISTRY "resultsProgramVK()(bytes32)" --rpc-url $RPC`,
    `cast call $REGISTRY "rootCVadcopFinal()(bytes32)" --rpc-url $RPC`,
    `cast call $REGISTRY "ballotVKHash()(bytes32)" --rpc-url $RPC`,
    `cast call $REGISTRY "chainID()(uint32)" --rpc-url $RPC`,
    `VERIFIER=$(cast call $REGISTRY "ziskVerifier()(address)" --rpc-url $RPC)`,
    `cast call $VERIFIER "getRootCVadcopFinal()(bytes32)" --rpc-url $RPC`,
    `cast keccak $(cast code $VERIFIER --rpc-url $RPC)`,
  ]
  if (verifier) lines.splice(7, 0, `# ziskVerifier() should print ${checksum(verifier)}`)
  return lines.join('\n')
}

/** DKG explorer links, when one is configured. */
export function dkgExplorerLink(base: string | undefined, kind: 'epoch' | 'operator', id: string): string | null {
  if (!base) return null
  const root = base.replace(/\/+$/, '')
  return kind === 'epoch' ? `${root}/epochs/${id.toLowerCase()}` : `${root}/operators/${id.toLowerCase()}`
}

/** One-line verdict, shared by the page header and the panel. */
export function releaseVerdict(match: ReleaseMatch): { tone: 'ok' | 'danger' | 'info'; text: string } {
  const failed = match.checks.filter((c) => c.ok === false).length
  if (match.release) return { tone: 'ok', text: `All five pins match ${match.release.label}.` }
  if (!match.closest) return { tone: 'info', text: 'This explorer carries no known release to compare with.' }
  if (failed > 0) {
    return {
      tone: 'danger',
      text: `${failed} of ${match.checks.length} pins differ from ${match.closest.label}, the closest release this explorer knows.`,
    }
  }
  return { tone: 'info', text: 'Reading the pins from the registry…' }
}
