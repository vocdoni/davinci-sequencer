// Contract ABIs. `abi/*.json` are copies of davinci-sequencer
// `sequencer/abi/` (forge output of davinci-contracts, branch `zkvm`); refresh
// them together. The DKG fragments are the parts of davinci-dkg's
// DKGManager / DKGAppManager the explorer reads, from davinci-contracts
// `src/interfaces/dkg/`.

import { parseAbi, type Abi, type AbiEvent } from 'viem'
import processRegistryJson from './abi/ProcessRegistry.json'
import dkgAdapterJson from './abi/DavinciDKGAdapter.json'

export const processRegistryAbi = processRegistryJson as unknown as Abi
export const dkgAdapterAbi = dkgAdapterJson as unknown as Abi

/** Every ProcessRegistry event the indexer scans. */
export const REGISTRY_EVENT_NAMES = [
  'ProcessCreated',
  'ProcessStatusChanged',
  'ProcessStateTransitioned',
  'ProcessResultsSet',
  'ProcessDurationChanged',
  'ProcessMaxVotersChanged',
  'CensusUpdated',
  'ResultsDecryptionRequested',
] as const

export type RegistryEventName = (typeof REGISTRY_EVENT_NAMES)[number]

export const REGISTRY_EVENT_ABIS: AbiEvent[] = processRegistryAbi.filter(
  (item): item is AbiEvent => item.type === 'event' && (REGISTRY_EVENT_NAMES as readonly string[]).includes(item.name)
)

export const dkgManagerAbi = parseAbi([
  'function appManager() view returns (address)',
  'function epochNonce() view returns (uint64)',
  'function EPOCH_PREFIX() view returns (uint32)',
  'function getPoolKey(bytes12 epochId, uint8 keyIndex) view returns (uint256 x, uint256 y)',
  'function getPoolStatus(bytes12 epochId) view returns (uint8 nextIndex)',
  'function getAppPoolIndex(bytes12 epochId, bytes32 aid) view returns (uint8)',
  'struct CombinedDecryptionRecord { uint16 ciphertextIndex; bool completed; uint256 plaintext; }',
  'function getCombinedDecryption(bytes12 epochId, bytes32 aid, uint16 ciphertextIndex) view returns (CombinedDecryptionRecord)',
])

export const dkgAppManagerAbi = parseAbi([
  'struct Point { uint256 x; uint256 y; }',
  'struct AppPolicy { uint8 mode; bool openSubmission; address[] submitters; uint16 maxCiphertexts; uint64 notBeforeBlock; uint64 notAfterBlock; uint64 decryptNotBefore; uint64 decryptNotAfter; }',
  'struct Application { address creator; Point organizerPK; uint256 organizerSecret; uint8 poolIndex; AppPolicy policy; uint64 createdAtBlock; bool exists; }',
  'function getApplication(bytes12 epochId, bytes32 aid) view returns (Application)',
  'function getApplicationKey(bytes12 epochId, bytes32 aid) view returns (uint256 x, uint256 y)',
  'function getOrganizerPK(bytes12 epochId, bytes32 aid) view returns (uint256, uint256)',
])
