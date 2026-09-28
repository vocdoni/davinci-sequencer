import { CopyButton } from '~kit'
import { cn } from '~lib/cn'

/**
 * A shell command or snippet: monospace, scrolls sideways inside its own box,
 * with a copy button. `label` names what is copied for screen readers.
 */
export function CodeBlock({
  code,
  label = 'Copy command',
  className,
}: {
  code: string
  label?: string
  className?: string
}) {
  return (
    <div className={cn('relative rounded-md border border-charcoal bg-obsidian', className)}>
      <pre className='scroll-slim overflow-x-auto py-3 pr-12 pl-4 text-[12px] leading-relaxed text-silver'>
        <code>{code}</code>
      </pre>
      <CopyButton value={code} label={label} className='absolute top-2 right-2 border border-charcoal bg-carbon' />
    </div>
  )
}
