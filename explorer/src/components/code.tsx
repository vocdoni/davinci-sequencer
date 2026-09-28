import { useState, type ReactNode } from 'react'
import { ChevronDownIcon, CopyButton } from '~kit'
import { cn } from '~lib/cn'

/**
 * A command or raw value to copy: monospace, scrolls inside itself, with a
 * copy button. Long hex wraps instead of stretching the page.
 */
export function CodeBlock({
  code,
  label,
  wrap = false,
  maxHeight,
  className,
}: {
  code: string
  /** Accessible name of the copy button. */
  label?: string
  /** Break long tokens (hex) instead of scrolling sideways. */
  wrap?: boolean
  maxHeight?: number
  className?: string
}) {
  return (
    <div className={cn('relative min-w-0 rounded-sm border border-charcoal bg-obsidian', className)}>
      <pre
        className={cn(
          'scroll-slim m-0 overflow-auto p-3 pr-10 text-[12px] leading-relaxed text-silver',
          wrap ? 'break-all whitespace-pre-wrap' : 'whitespace-pre'
        )}
        style={maxHeight ? { maxHeight } : undefined}
      >
        <code>{code}</code>
      </pre>
      <CopyButton value={code} label={label ?? 'Copy'} className='absolute top-1.5 right-1.5 bg-obsidian' />
    </div>
  )
}

/**
 * Native `<details>`, closed by default: keyboard and screen-reader friendly.
 * The content renders only while open, so a heavy view costs nothing until
 * someone asks for it.
 */
export function Disclosure({
  summary,
  children,
  defaultOpen = false,
  className,
  bodyClassName,
  testId,
  variant = 'boxed',
}: {
  summary: ReactNode
  children: ReactNode
  defaultOpen?: boolean
  className?: string
  bodyClassName?: string
  testId?: string
  /** `boxed` draws a bordered row; `plain` is a small text toggle. */
  variant?: 'boxed' | 'plain'
}) {
  const [open, setOpen] = useState(defaultOpen)
  const boxed = variant === 'boxed'
  return (
    <details
      open={open}
      onToggle={(e) => setOpen((e.currentTarget as HTMLDetailsElement).open)}
      className={cn('group/disclosure min-w-0', boxed && 'rounded-sm border border-charcoal', className)}
      data-testid={testId}
    >
      <summary
        className={cn(
          'flex cursor-pointer list-none items-center gap-2 select-none [&::-webkit-details-marker]:hidden',
          boxed
            ? 'rounded-sm px-3 py-2 text-[13px] text-silver hover:bg-onyx'
            : 'w-fit rounded-sm text-[12px] text-emerald hover:underline'
        )}
      >
        <ChevronDownIcon
          size={13}
          className={cn(
            'shrink-0 -rotate-90 transition-transform group-open/disclosure:rotate-0',
            boxed ? 'text-ash' : 'text-emerald'
          )}
        />
        <span className='min-w-0 flex-1'>{summary}</span>
      </summary>
      {open ? (
        <div className={cn(boxed ? 'border-t border-charcoal p-3' : 'pt-2', bodyClassName)}>{children}</div>
      ) : null}
    </details>
  )
}
