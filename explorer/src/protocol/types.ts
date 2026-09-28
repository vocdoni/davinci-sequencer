// On-chain enums of davinci-contracts `DAVINCITypes`, with the words the
// explorer uses for them.

export type ProcessStatusName = 'ready' | 'ended' | 'canceled' | 'paused' | 'results'
export type CensusOriginName = 'unknown' | 'merkle-static' | 'merkle-dynamic' | 'onchain-dynamic' | 'csp'
export type KeyModeName = 'sequencer' | 'dkg-automatic' | 'dkg-locked'

/** `DAVINCITypes.ProcessStatus`, by ordinal. */
export const PROCESS_STATUSES: ProcessStatusName[] = ['ready', 'ended', 'canceled', 'paused', 'results']
/** `DAVINCITypes.CensusOrigin`, by ordinal. */
export const CENSUS_ORIGINS: CensusOriginName[] = [
  'unknown',
  'merkle-static',
  'merkle-dynamic',
  'onchain-dynamic',
  'csp',
]
/** `DAVINCITypes.KeyMode`, by ordinal. */
export const KEY_MODES: KeyModeName[] = ['sequencer', 'dkg-automatic', 'dkg-locked']

export function processStatusName(v: number | bigint): ProcessStatusName {
  return PROCESS_STATUSES[Number(v)] ?? 'ready'
}

export function censusOriginName(v: number | bigint): CensusOriginName {
  return CENSUS_ORIGINS[Number(v)] ?? 'unknown'
}

export function keyModeName(v: number | bigint): KeyModeName {
  return KEY_MODES[Number(v)] ?? 'sequencer'
}

export interface EnumInfo {
  label: string
  description: string
}

export const PROCESS_STATUS_INFO: Record<ProcessStatusName, EnumInfo> = {
  ready: {
    label: 'Ready',
    description: 'Open: sequencers accept votes and settle batches until the end time.',
  },
  paused: {
    label: 'Paused',
    description: 'Paused by the organizer. Votes may queue at a sequencer but no batch settles until it resumes.',
  },
  ended: {
    label: 'Ended',
    description: 'Closed to votes. The final state root is fixed; the results are pending.',
  },
  canceled: {
    label: 'Canceled',
    description: 'Canceled by the organizer. No results will be published.',
  },
  results: {
    label: 'Results',
    description: 'The tally is on-chain, proven against the final state root.',
  },
}

export const CENSUS_ORIGIN_INFO: Record<CensusOriginName, EnumInfo> = {
  unknown: { label: 'Unknown', description: 'Not a census origin the registry accepts.' },
  'merkle-static': {
    label: 'Merkle tree, fixed',
    description: 'A lean-IMT census root published at creation and never changed. Voters prove membership against it.',
  },
  'merkle-dynamic': {
    label: 'Merkle tree, updatable',
    description: 'A lean-IMT census root the organizer can replace while the process is open (CensusUpdated).',
  },
  'onchain-dynamic': {
    label: 'On-chain census contract',
    description:
      'The census lives in a contract; every batch must use a root the contract held since the process was created.',
  },
  csp: {
    label: 'Credential service provider',
    description: 'Voters present a signature from the CSP signer whose address is the census root.',
  },
}

export const KEY_MODE_INFO: Record<KeyModeName, EnumInfo> = {
  sequencer: {
    label: 'Sequencer key',
    description:
      'One sequencer node holds the election key: it could open the ballots and alone publishes the tally, with a zkVM results proof.',
  },
  'dkg-automatic': {
    label: 'DKG, automatic',
    description:
      'A davinci-dkg committee key. No sequencer and no organizer holds the secret; each committee member holds one share, and a threshold of them decrypts the final tally once asked.',
  },
  'dkg-locked': {
    label: 'DKG, organizer-locked',
    description:
      'A davinci-dkg committee key combined with an organizer key. The tally is decrypted only after the organizer reveals its secret.',
  },
}
