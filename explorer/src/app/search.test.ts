import { describe, expect, it } from 'vitest'
import { resolveSearch, type SearchResolver } from './search'

const ctx = { blockExplorerUrl: 'https://gnosisscan.io/', dkgExplorerUrl: 'https://dkg.example' }
const PID = `0x42fc20654efd78c6887ff0bd1cc50c9ec1dab58980c5bb9300000000000004`

describe('resolveSearch', () => {
  it('routes a process id to its page', () => {
    expect(resolveSearch(PID, ctx)).toMatchObject({ kind: 'route', path: `/processes/${PID}` })
    expect(resolveSearch(PID.toUpperCase().replace('0X', '0x'), ctx)).toMatchObject({ path: `/processes/${PID}` })
  })

  it('routes a transaction hash to the tx resolver page', () => {
    const tx = `0x${'ab'.repeat(32)}`
    expect(resolveSearch(tx, ctx)).toMatchObject({ kind: 'route', path: `/tx/${tx}` })
  })

  it('routes an address to the processes it organizes', () => {
    const target = resolveSearch('0x42fC20654eFD78C6887fF0bd1CC50c9eC1dAb589', ctx)
    expect(target).toMatchObject({
      kind: 'route',
      path: '/processes?organizer=0x42fc20654efd78c6887ff0bd1cc50c9ec1dab589',
    })
  })

  it('routes vote ids, hex or decimal, to the vote lookup', () => {
    expect(resolveSearch('0x80000000000000ff', ctx)).toMatchObject({
      kind: 'route',
      path: '/votes?voteId=0x80000000000000ff',
    })
    expect(resolveSearch('9223372036854775809', ctx)).toMatchObject({ path: '/votes?voteId=9223372036854775809' })
  })

  it('sends a block number and a DKG epoch to their explorers', () => {
    expect(resolveSearch('48476748', ctx)).toEqual({
      kind: 'external',
      url: 'https://gnosisscan.io/block/48476748',
      label: 'Block 48476748',
    })
    expect(resolveSearch('0x2f1105e90000000000000029', ctx)).toMatchObject({
      kind: 'external',
      url: 'https://dkg.example/epochs/0x2f1105e90000000000000029',
    })
    expect(resolveSearch('48476748', {}).kind).toBe('unknown')
  })

  it('gives registered resolvers the first look', () => {
    const resolver: SearchResolver = (q) => (q === '48476748' ? { kind: 'route', path: '/x', label: 'x' } : null)
    expect(resolveSearch('48476748', ctx, [resolver])).toMatchObject({ path: '/x' })
  })

  it('explains an empty or nonsense query', () => {
    expect(resolveSearch('   ', ctx).kind).toBe('unknown')
    expect(resolveSearch('hello', ctx).kind).toBe('unknown')
  })
})
