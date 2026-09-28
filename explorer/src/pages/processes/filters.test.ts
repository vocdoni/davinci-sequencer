import { describe, expect, it } from 'vitest'
import { isFiltered, readProcessFilter } from './filters'

describe('readProcessFilter', () => {
  it('maps the query onto the selector filter', () => {
    const org = '0x42FC20654EFD78C6887FF0BD1CC50C9EC1DAB589'
    const { list, filter } = readProcessFilter(
      new URLSearchParams(`status=open&keyMode=dkg-locked&census=csp&organizer=${org}&q=%200x42%20`)
    )
    expect(list).toEqual({
      status: 'open',
      keyMode: 'dkg-locked',
      census: 'csp',
      organizer: org.toLowerCase(),
      q: '0x42',
    })
    expect(filter).toEqual({
      status: 'open',
      keyMode: 'dkg-locked',
      censusOrigin: 'csp',
      organizer: org.toLowerCase(),
      query: '0x42',
    })
    expect(isFiltered(list)).toBe(true)
  })

  it('drops values it does not know', () => {
    const { list } = readProcessFilter(new URLSearchParams('status=bogus&keyMode=x&census=unknown&organizer=0x12'))
    expect(list).toEqual({
      status: undefined,
      keyMode: undefined,
      census: undefined,
      organizer: undefined,
      q: undefined,
    })
    expect(isFiltered(list)).toBe(false)
  })
})
