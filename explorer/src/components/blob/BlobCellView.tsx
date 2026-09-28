import { useState } from 'react'
import { Button, Pagination } from '~kit'
import { beToBigInt } from '~protocol/bytes'
import { formatVoteId } from '~protocol/blob'
import { CELLS_PER_BLOB } from '~protocol/limits'
import { formatNumber } from '~lib/format'
import { cn } from '~lib/cn'
import { SECTION_STYLE } from './sections'
import { cellBytes, describeCell, formatSlotKey, sectionSpans, type CellCounts, type CellInfo } from './cells'

const PAGE = 32

function reading(info: CellInfo | null, bytes: Uint8Array): string {
  if (!info) return ''
  const v = beToBigInt(bytes)
  switch (info.kind) {
    case 'vote-id-count':
    case 'update-count':
      return formatNumber(v)
    case 'vote-id':
      return formatVoteId(v)
    case 'slot-key':
      return formatSlotKey(v)
    case 'padding':
      return v === 0n ? '0' : 'not zero'
    default:
      return (bytes[0]! & 0x40) !== 0 ? 'point, x odd' : 'point, x even'
  }
}

/**
 * The raw cells, 32 bytes each, with what the guest put in every one.
 * `counts` comes from the decoded data; without it the cells are shown
 * unlabelled.
 */
export function BlobCellView({ blobs, counts }: { blobs: Uint8Array[]; counts: CellCounts | null }) {
  const total = blobs.length * CELLS_PER_BLOB
  const pageCount = Math.max(1, Math.ceil(total / PAGE))
  const [page, setPage] = useState(0)
  const current = Math.min(page, pageCount - 1)
  const spans = counts ? sectionSpans(counts, total) : []

  const rows: Array<{ index: number; info: CellInfo | null; bytes: Uint8Array }> = []
  for (let i = current * PAGE; i < Math.min(total, (current + 1) * PAGE); i++) {
    const bytes = cellBytes(blobs, i)
    if (bytes) rows.push({ index: i, info: counts ? describeCell(counts, i) : null, bytes })
  }

  return (
    <div className='flex flex-col gap-3' data-testid='blob-cell-view'>
      {spans.length ? (
        <div className='flex flex-wrap items-center gap-2'>
          <span className='text-[11px] text-ash'>Jump to</span>
          {spans.map((s) => (
            <Button key={s.section} size='sm' variant='secondary' onClick={() => setPage(Math.floor(s.start / PAGE))}>
              <span className={cn('h-2 w-2 rounded-full', SECTION_STYLE[s.section].swatch)} />
              {SECTION_STYLE[s.section].label}
              <span className='font-mono text-ash tnum'>@{formatNumber(s.start)}</span>
            </Button>
          ))}
        </div>
      ) : null}
      <div className='scroll-slim overflow-x-auto'>
        <table className='w-full min-w-[720px] border-collapse text-[12px]'>
          <thead>
            <tr className='label-caps text-[10px] text-pewter'>
              <th scope='col' className='w-20 border-b border-charcoal px-2 py-1.5 text-right'>
                Cell
              </th>
              <th scope='col' className='w-24 border-b border-charcoal px-2 py-1.5 text-left'>
                Blob · pos
              </th>
              <th scope='col' className='border-b border-charcoal px-2 py-1.5 text-left'>
                Holds
              </th>
              <th scope='col' className='border-b border-charcoal px-2 py-1.5 text-left'>
                32 bytes, big-endian
              </th>
            </tr>
          </thead>
          <tbody>
            {rows.map(({ index, info, bytes }) => (
              <tr key={index} className='border-b border-charcoal/60 align-top last:border-b-0'>
                <td className='px-2 py-1.5 text-right font-mono text-ash tnum'>{index}</td>
                <td className='px-2 py-1.5 font-mono text-ash tnum'>
                  {Math.floor(index / CELLS_PER_BLOB)} · {index % CELLS_PER_BLOB}
                </td>
                <td className='px-2 py-1.5'>
                  <span className='inline-flex items-center gap-1.5 text-silver'>
                    {info ? (
                      <span className={cn('h-2 w-2 shrink-0 rounded-full', SECTION_STYLE[info.section].swatch)} />
                    ) : null}
                    {info?.label ?? 'Cell'}
                  </span>
                  <span className='block font-mono text-[11px] text-ash'>{reading(info, bytes)}</span>
                </td>
                <td className='px-2 py-1.5 font-mono text-[11px] break-all text-silver'>
                  {Array.from(bytes, (b) => b.toString(16).padStart(2, '0')).join('')}
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
      <Pagination page={current} pageCount={pageCount} onPageChange={setPage} pageSize={PAGE} total={total} />
    </div>
  )
}
