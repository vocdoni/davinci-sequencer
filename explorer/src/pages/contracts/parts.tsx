import type { ReactNode } from 'react'
import { useRuntimeConfig } from '~config/config-context'
import { ExternalIcon, Tooltip } from '~kit'
import { cn } from '~lib/cn'
import { explorerCodeUrl } from '~lib/explorer'

/** "Source" link to a contract's verified code on the block explorer. */
export function SourceLink({ address, className }: { address: string; className?: string }) {
  const { blockExplorerUrl } = useRuntimeConfig()
  const href = explorerCodeUrl(blockExplorerUrl, address)
  if (!href) return null
  return (
    <Tooltip content='Verified source code on the block explorer'>
      <a
        href={href}
        target='_blank'
        rel='noreferrer noopener'
        className={cn(
          'inline-flex shrink-0 items-center gap-1 rounded-sm px-1.5 py-0.5 text-[12px] text-pewter transition-colors hover:bg-onyx hover:text-emerald',
          className
        )}
      >
        Source
        <ExternalIcon size={12} />
      </a>
    </Tooltip>
  )
}

/** Inline code: a Solidity name, a flag, a file. */
export function Code({ children }: { children: ReactNode }) {
  return <code className='rounded-sm bg-onyx px-1 py-px text-[0.92em] text-silver'>{children}</code>
}

/** A small heading inside a panel. */
export function SubHeading({ children, className }: { children: ReactNode; className?: string }) {
  return <h3 className={cn('label-caps text-[11px] text-pewter', className)}>{children}</h3>
}

/** Two-state segmented switch, keyboard reachable, for panel-level view options. */
export function Segmented<T extends string>({
  value,
  options,
  onChange,
  label,
}: {
  value: T
  options: Array<{ value: T; label: string }>
  onChange: (value: T) => void
  label: string
}) {
  return (
    <div role='radiogroup' aria-label={label} className='inline-flex rounded-sm border border-charcoal p-0.5'>
      {options.map((o) => (
        <button
          key={o.value}
          type='button'
          role='radio'
          aria-checked={value === o.value}
          onClick={() => onChange(o.value)}
          className={cn(
            'rounded-[3px] px-2.5 py-1 text-[12px] font-medium transition-colors',
            value === o.value ? 'bg-emerald/15 text-emerald' : 'text-pewter hover:text-ghost'
          )}
        >
          {o.label}
        </button>
      ))}
    </div>
  )
}
