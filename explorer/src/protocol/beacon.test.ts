import { describe, expect, it } from 'vitest'
import { readFileSync } from 'node:fs'
import { resolve } from 'node:path'
import type { Hex } from 'viem'
import { BeaconClient, BlobFetchError, matchSidecars, slotForTimestamp } from './beacon'
import { decodeTransitionBlobs, versionedHash } from './blob'
import { decodeStateTransitionCall } from './calldata'
import { SequencerApiError, SequencerClient } from './sequencer-api'

// A live Gnosis transition recorded with its sidecar (see tests/vectors/README.md).
const live = JSON.parse(readFileSync(resolve(__dirname, '../../tests/vectors/gnosis_transition.json'), 'utf8')) as {
  blockTimestamp: number
  input: Hex
  blobVersionedHashes: Hex[]
  numFields: number
  event: {
    processId: Hex
    oldStateRoot: Hex
    newStateRoot: Hex
    newVotersCount: number
    newOverwrittenVotesCount: number
    nBlobs: number
  }
  sidecars: Array<{ index: string; kzg_commitment: Hex; blob: Hex }>
}

const GNOSIS_GENESIS = 1638993340

function fakeFetch(routes: Record<string, unknown | number>) {
  const calls: string[] = []
  const impl = async (url: string) => {
    calls.push(url)
    const path = url.replace(/^https?:\/\/[^/]+/, '')
    const hit = routes[path]
    if (hit === undefined) return new Response('{"message":"not found"}', { status: 404 })
    if (typeof hit === 'number') return new Response('{}', { status: hit })
    return new Response(JSON.stringify(hit), { status: 200 })
  }
  return { impl, calls }
}

describe('a live Gnosis transition', () => {
  it('ties calldata, event and blob together', () => {
    const call = decodeStateTransitionCall(live.input)
    expect(call.processId).toBe(live.event.processId)
    expect(call.publics.rootBefore).toBe(live.event.oldStateRoot)
    expect(call.publics.rootAfter).toBe(live.event.newStateRoot)
    expect(call.publics.nBlobs).toBe(live.event.nBlobs)
    expect(call.commitments.map((c) => versionedHash(c))).toEqual(live.blobVersionedHashes)
  })

  it('decodes the sidecar blob into the batch the publics describe', () => {
    const call = decodeStateTransitionCall(live.input)
    const [blob] = matchSidecars(live.sidecars, live.blobVersionedHashes, 'fixture')
    expect(blob!.binding).toBe('commitment')
    expect(blob!.commitment).toBe(call.commitments[0])
    const t = decodeTransitionBlobs([blob!.data], live.numFields)
    expect(t.voteIds.length).toBe(call.publics.voters)
    // Every vote writes a slot; silent refreshes write more.
    expect(t.updates.length).toBeGreaterThanOrEqual(call.publics.voters)
  })
})

describe('BeaconClient', () => {
  const slot = slotForTimestamp(live.blockTimestamp, GNOSIS_GENESIS, 5)

  it('derives the slot from the block time', () => {
    expect(slot).toBe(30315748)
    expect(() => slotForTimestamp(10, 20, 5)).toThrow(BlobFetchError)
  })

  it('fetches and matches sidecars by versioned hash', async () => {
    const { impl, calls } = fakeFetch({
      '/eth/v1/beacon/genesis': { data: { genesis_time: String(GNOSIS_GENESIS) } },
      '/eth/v1/config/spec': { data: { SECONDS_PER_SLOT: '5' } },
      [`/eth/v1/beacon/blob_sidecars/${slot}`]: { data: live.sidecars },
    })
    const beacon = new BeaconClient('https://beacon.example/', impl)
    const blobs = await beacon.fetchBlobs({ timestamp: live.blockTimestamp, versionedHashes: live.blobVersionedHashes })
    expect(blobs).toHaveLength(1)
    expect(blobs[0]!.versionedHash).toBe(live.blobVersionedHashes[0])
    // Genesis and spec are read once.
    await beacon.slotFor(live.blockTimestamp)
    expect(calls.filter((c) => c.endsWith('/genesis'))).toHaveLength(1)
  })

  it('falls back to blobs/{slot}, every versioned hash in one request, when the sidecars fail', async () => {
    const other = `0x01${'ab'.repeat(31)}` as Hex
    const hashes = [live.blobVersionedHashes[0]!, other]
    const zero = `0x${'00'.repeat(131072)}` as Hex
    const { impl, calls } = fakeFetch({
      '/eth/v1/beacon/genesis': { data: { genesis_time: String(GNOSIS_GENESIS) } },
      '/eth/v1/config/spec': { data: { SECONDS_PER_SLOT: '5' } },
      [`/eth/v1/beacon/blob_sidecars/${slot}`]: 404,
      [`/eth/v1/beacon/blobs/${slot}?versioned_hashes=${hashes[0]}&versioned_hashes=${hashes[1]}`]: {
        data: [live.sidecars[0]!.blob, zero],
      },
    })
    const beacon = new BeaconClient('https://beacon.example', impl)
    const blobs = await beacon.fetchBlobs({ timestamp: live.blockTimestamp, versionedHashes: hashes })
    expect(blobs.map((b) => [b.versionedHash, b.binding])).toEqual([
      [hashes[0], 'beacon-filter'],
      [other, 'beacon-filter'],
    ])
    expect(blobs[1]!.data.every((b) => b === 0)).toBe(true)
    const fetched = calls.filter((c) => c.includes('/blobs/') || c.includes('/blob_sidecars/'))
    expect(fetched.map((c) => c.includes('/blob_sidecars/'))).toEqual([true, false])
  })

  it('refuses a filtered answer that is not one blob per hash', async () => {
    const { impl } = fakeFetch({
      '/eth/v1/beacon/genesis': { data: { genesis_time: String(GNOSIS_GENESIS) } },
      '/eth/v1/config/spec': { data: { SECONDS_PER_SLOT: '5' } },
      [`/eth/v1/beacon/blob_sidecars/${slot}`]: 500,
      [`/eth/v1/beacon/blobs/${slot}?versioned_hashes=${live.blobVersionedHashes[0]}`]: {
        data: [live.sidecars[0]!.blob, live.sidecars[0]!.blob],
      },
    })
    await expect(
      new BeaconClient('https://beacon.example', impl).fetchBlobs({
        timestamp: live.blockTimestamp,
        versionedHashes: live.blobVersionedHashes,
      })
    ).rejects.toThrow(/2 blobs for 1 versioned hashes/)
  })

  it('says so when the slot was pruned', async () => {
    const { impl } = fakeFetch({
      '/eth/v1/beacon/genesis': { data: { genesis_time: String(GNOSIS_GENESIS) } },
      '/eth/v1/config/spec': { data: { SECONDS_PER_SLOT: '5' } },
    })
    const beacon = new BeaconClient('https://beacon.example', impl)
    await expect(
      beacon.fetchBlobs({ timestamp: live.blockTimestamp, versionedHashes: live.blobVersionedHashes })
    ).rejects.toThrow(/pruned/)
  })

  it('refuses a sidecar set without the wanted blob', () => {
    expect(() => matchSidecars(live.sidecars, [`0x01${'00'.repeat(31)}`], 'x')).toThrow(/no sidecar/)
  })
})

describe('SequencerClient', () => {
  it('formats vote ids and surfaces API errors', async () => {
    const pid = live.event.processId
    const { impl, calls } = fakeFetch({
      [`/votes/${pid}/voteId/0x8000000000000001`]: { status: 'settled' },
    })
    const client = new SequencerClient('http://seq.example/', impl)
    expect(await client.voteStatus(pid, (1n << 63n) + 1n)).toEqual({ status: 'settled' })
    expect(calls[0]).toBe(`http://seq.example/votes/${pid}/voteId/0x8000000000000001`)
    await expect(client.info()).rejects.toBeInstanceOf(SequencerApiError)
  })

  it('returns raw blobs as bytes', async () => {
    const pid = live.event.processId
    const { impl } = fakeFetch({ [`/processes/${pid}/transitions/0/blobs`]: { blobs: [live.sidecars[0]!.blob] } })
    const blobs = await new SequencerClient('http://seq.example', impl).transitionBlobs(pid, 0)
    expect(blobs[0]!.length).toBe(131072)
  })
})
