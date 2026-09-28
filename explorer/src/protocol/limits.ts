// Protocol limits, mirrored from davinci-zkvm `rust-sdk/src/limits.rs`
// (itself mirrored from `circuit-primitives/src/types.rs`).

/** Ballot capacity: every ballot carries 16 ElGamal ciphertexts. */
export const NUM_FIELDS = 16
export const MAX_BATCH_SIZE = 1024
export const MAX_REFRESH = 2048
export const REFRESH_MIN = 16
export const REFRESH_TAU = 2
export const REFRESH_KAPPA = 1
export const MAX_BLOBS = 32
/** EIP-7594 cap on blobs per transaction. */
export const TX_BLOB_CAP = 6
export const SMT_LEVELS = 64

/** Ballot slots live in [0x10, 2^63); vote ids in [2^63, 2^64). */
export const VOTE_ID_MIN = 1n << 63n
export const BALLOT_MIN = 0x10n
export const BALLOT_MAX = VOTE_ID_MIN - 1n
export const U64_MAX = (1n << 64n) - 1n

/** Config keys of the state tree. */
export const STATE_KEYS = {
  processId: 0x00,
  ballotMode: 0x02,
  encryptionKey: 0x03,
  results: 0x04,
  censusOrigin: 0x06,
  ballotVK: 0x07,
} as const

/** EIP-4844 blob geometry. */
export const CELLS_PER_BLOB = 4096
export const BYTES_PER_CELL = 32
export const BLOB_SIZE = CELLS_PER_BLOB * BYTES_PER_CELL

/** BN254 scalar field: the BabyJubJub base field. */
export const BN254_FR = 21888242871839275222246405745257275088548364400416034343698204186575808495617n
/** BLS12-381 scalar field: the blob field. */
export const BLS_MODULUS = 52435875175126190479447740508185965837690552500527637822603658699938581184513n

/** Silent refreshes the guest asks for: min(MAX_REFRESH, max(REFRESH_MIN, TAU*w, KAPPA*n)). */
export function refreshTarget(n: number, w: number): number {
  return Math.min(MAX_REFRESH, Math.max(REFRESH_MIN, REFRESH_TAU * w, REFRESH_KAPPA * n))
}

/** Minimum refresh count the guest accepts: min(target, occupied_before - w). */
export function requiredRefresh(n: number, w: number, occupiedBefore: number): number {
  return Math.min(refreshTarget(n, w), Math.max(0, occupiedBefore - w))
}
