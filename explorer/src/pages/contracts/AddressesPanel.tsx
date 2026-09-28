import { CheckMark } from '~components'
import { Address, Badge, Panel, Skeleton } from '~kit'
import type { ContractRow, WiringCheck } from './model'
import { SourceLink, SubHeading } from './parts'

export function AddressesPanel({ rows, checks }: { rows: ContractRow[]; checks: WiringCheck[] }) {
  const passed = checks.filter((c) => c.state === 'pass').length
  return (
    <Panel
      title='Addresses'
      label='Contracts'
      description='Every contract this deployment runs on. Each links to the block explorer, and to its verified source where the explorer has it: compare that source with the repositories before trusting the rest of this page.'
    >
      <ul className='-my-3 divide-y divide-charcoal'>
        {rows.map((row) => (
          <li
            key={row.id}
            data-testid={`contract-row-${row.id}`}
            className='flex flex-col gap-2 py-3 md:flex-row md:items-center md:justify-between md:gap-8'
          >
            <div className='min-w-0 md:max-w-[62%]'>
              <div className='flex flex-wrap items-center gap-2'>
                <span className='text-[13px] font-semibold text-ghost'>{row.name}</span>
                {row.group === 'dkg' ? (
                  <Badge size='sm' tone='neutral'>
                    davinci-dkg
                  </Badge>
                ) : null}
              </div>
              <p className='mt-0.5 text-[12px] leading-relaxed text-ash'>{row.role}</p>
            </div>
            <div className='flex min-w-0 shrink-0 items-center gap-2'>
              {row.address ? (
                <>
                  <Address value={row.address} chars={6} />
                  <SourceLink address={row.address} />
                </>
              ) : row.note ? (
                <span className='text-[12px] text-ash md:max-w-xs md:text-right'>{row.note}</span>
              ) : (
                <Skeleton className='h-4 w-44' />
              )}
            </div>
          </li>
        ))}
      </ul>

      <div className='mt-6 border-t border-charcoal pt-5' data-testid='wiring-checks'>
        <div className='flex flex-wrap items-baseline justify-between gap-2'>
          <SubHeading>How the contracts point at each other</SubHeading>
          <span className='text-[12px] text-ash'>
            {passed} of {checks.length} consistent
          </span>
        </div>
        <p className='mt-1 text-[12px] leading-relaxed text-ash'>
          Read back from the contracts themselves, so a copied address or a registry wired to the wrong DKG deployment
          shows up here.
        </p>
        <ul className='mt-3 flex flex-col gap-2.5'>
          {checks.map((c) => (
            <li key={c.id} className='flex items-start gap-2.5'>
              <CheckMark state={c.state} className='mt-0.5' />
              <div className='min-w-0'>
                <div className='text-[13px] text-silver'>{c.label}</div>
                <div className='break-words text-[12px] text-ash'>{c.detail}</div>
              </div>
            </li>
          ))}
        </ul>
      </div>
    </Panel>
  )
}
