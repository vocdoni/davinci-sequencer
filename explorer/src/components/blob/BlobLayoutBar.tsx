import { CELLS_PER_BLOB } from '~protocol/limits'
import { formatNumber } from '~lib/format'
import { cn } from '~lib/cn'
import { sectionSpans, type CellCounts, type CellSection } from './cells'
import { SECTION_STYLE } from './sections'

/**
 * One bar per blob, split by what its 4096 cells carry. Shows at a glance
 * how full the blobs are and where the data runs across blob boundaries.
 */
export function BlobLayoutBar({ counts, nBlobs }: { counts: CellCounts; nBlobs: number }) {
  const total = nBlobs * CELLS_PER_BLOB
  const spans = sectionSpans(counts, total)
  const cellsOf = (section: CellSection) =>
    spans.filter((s) => s.section === section).reduce((n, s) => n + s.end - s.start, 0)

  return (
    <div className='flex flex-col gap-2'>
      {Array.from({ length: nBlobs }, (_, b) => {
        const lo = b * CELLS_PER_BLOB
        const hi = lo + CELLS_PER_BLOB
        const parts = spans
          .map((s) => ({ section: s.section, n: Math.max(0, Math.min(hi, s.end) - Math.max(lo, s.start)) }))
          .filter((p) => p.n > 0)
        return (
          <div key={b} className='flex items-center gap-3'>
            <span className='w-14 shrink-0 font-mono text-[11px] text-ash tnum'>blob {b}</span>
            <div
              className='flex h-2.5 min-w-0 flex-1 overflow-hidden rounded-pill border border-charcoal'
              role='img'
              aria-label={`Blob ${b}: ${parts.map((p) => `${formatNumber(p.n)} ${SECTION_STYLE[p.section].label.toLowerCase()} cells`).join(', ')}`}
            >
              {parts.map((p) => (
                <span
                  key={p.section}
                  className={cn('h-full', SECTION_STYLE[p.section].swatch)}
                  style={{ width: `${(p.n / CELLS_PER_BLOB) * 100}%` }}
                />
              ))}
            </div>
          </div>
        )
      })}
      <ul className='mt-1 flex flex-wrap gap-x-4 gap-y-1 text-[11px] text-ash'>
        <li>Cells:</li>
        {(Object.keys(SECTION_STYLE) as CellSection[]).map((section) => (
          <li key={section} className='inline-flex items-center gap-1.5'>
            <span className={cn('h-2 w-2 rounded-full border border-charcoal', SECTION_STYLE[section].swatch)} />
            {SECTION_STYLE[section].label}{' '}
            <span className='font-mono tnum text-pewter'>{formatNumber(cellsOf(section))}</span>
          </li>
        ))}
      </ul>
    </div>
  )
}
