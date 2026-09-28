// Typography for the guide: sections with anchors, paragraphs, lists, inline
// code, links into the explorer and the "see it" pointers.

import { Fragment, type ReactNode } from 'react'
import { Link } from 'react-router-dom'
import { HashLink } from '~components/HashLink'
import { ChevronRightIcon, ExternalIcon } from '~kit'
import { cn } from '~lib/cn'
import { paths } from '~routes/paths'
import { anchorId } from './topics'

const LINK =
  'text-silver underline decoration-charcoal underline-offset-3 transition-colors hover:text-emerald hover:decoration-emerald'

export function Section({ title, children, id }: { title: string; children: ReactNode; id?: string }) {
  const anchor = id ?? anchorId(title)
  return (
    <section id={anchor} className='mt-10 scroll-mt-20 first:mt-0'>
      <h2 className='mb-3 text-[18px] font-semibold tracking-tight text-ghost'>
        <HashLink id={anchor} className='hover:text-emerald'>
          {title}
        </HashLink>
      </h2>
      {children}
    </section>
  )
}

export function H3({ children }: { children: ReactNode }) {
  return <h3 className='mt-6 mb-2 text-[14px] font-semibold text-ghost'>{children}</h3>
}

export function P({ children, className }: { children: ReactNode; className?: string }) {
  return <p className={cn('my-3 text-[14px] leading-[1.7] text-pewter', className)}>{children}</p>
}

export function UL({ children }: { children: ReactNode }) {
  return <ul className='my-3 list-disc space-y-1.5 pl-5 text-[14px] leading-[1.7] text-pewter'>{children}</ul>
}

export function OL({ children }: { children: ReactNode }) {
  return <ol className='my-3 list-decimal space-y-1.5 pl-5 text-[14px] leading-[1.7] text-pewter'>{children}</ol>
}

/** Inline code. */
export function C({ children }: { children: ReactNode }) {
  return <code className='rounded-sm bg-onyx px-1 py-px text-[0.88em] break-words text-silver'>{children}</code>
}

/** Text with `backtick` spans rendered as inline code. */
export function Rich({ text }: { text: string }) {
  const parts = text.split('`')
  return <>{parts.map((part, i) => (i % 2 === 1 ? <C key={i}>{part}</C> : <Fragment key={i}>{part}</Fragment>))}</>
}

/** A link inside the explorer. */
export function A({ to, children }: { to: string; children: ReactNode }) {
  return (
    <Link to={to} className={LINK}>
      {children}
    </Link>
  )
}

/** A link to a glossary entry. */
export function Term({ id, children }: { id: string; children: ReactNode }) {
  return (
    <Link to={{ pathname: paths.learn('glossary'), hash: `term-${id}` }} className={cn(LINK, 'decoration-dotted')}>
      {children}
    </Link>
  )
}

export function Ext({ href, children }: { href: string; children: ReactNode }) {
  return (
    <a href={href} target='_blank' rel='noreferrer noopener' className={cn(LINK, 'inline-flex items-center gap-0.5')}>
      {children}
      <ExternalIcon size={12} />
    </a>
  )
}

/** A pointer to the explorer page that shows what the text describes. */
export function SeeIt({ to, children, hint }: { to: string; children: ReactNode; hint?: ReactNode }) {
  return (
    <Link
      to={to}
      className='group my-4 flex items-center justify-between gap-4 rounded-md border border-charcoal bg-onyx/40 px-4 py-3 transition-colors hover:border-emerald/50'
    >
      <span className='min-w-0'>
        <span className='label-caps block text-[11px] text-emerald'>See it in the explorer</span>
        <span className='mt-1 block text-[13px] text-silver'>{children}</span>
        {hint ? <span className='mt-0.5 block text-[12px] text-ash'>{hint}</span> : null}
      </span>
      <ChevronRightIcon className='shrink-0 text-ash transition-colors group-hover:text-emerald' />
    </Link>
  )
}

/** Numbered steps of a guide. */
export function Steps({ children }: { children: ReactNode }) {
  return <ol className='my-4 flex flex-col gap-5'>{children}</ol>
}

export function Step({ n, title, children }: { n: number; title: string; children: ReactNode }) {
  return (
    <li className='flex gap-4'>
      <span
        aria-hidden='true'
        className='mt-0.5 flex h-6 w-6 shrink-0 items-center justify-center rounded-full border border-emerald/40 font-mono text-[12px] text-emerald'
      >
        {n}
      </span>
      <div className='min-w-0 flex-1'>
        <h3 className='text-[14px] font-semibold text-ghost'>{title}</h3>
        <div className='[&>p:first-child]:mt-1'>{children}</div>
      </div>
    </li>
  )
}

/** A small table that scrolls sideways on a phone. */
export function SimpleTable({ head, rows }: { head: ReactNode[]; rows: ReactNode[][] }) {
  return (
    <div className='scroll-slim my-4 overflow-x-auto rounded-md border border-charcoal'>
      <table className='w-full min-w-[520px] text-left text-[13px]'>
        <thead>
          <tr className='border-b border-charcoal'>
            {head.map((h, i) => (
              <th key={i} className='label-caps px-3 py-2 text-[11px] font-semibold text-pewter'>
                {h}
              </th>
            ))}
          </tr>
        </thead>
        <tbody>
          {rows.map((row, i) => (
            <tr key={i} className='border-b border-charcoal align-top last:border-b-0'>
              {row.map((cell, j) => (
                <td key={j} className={cn('px-3 py-2 leading-relaxed', j === 0 ? 'text-silver' : 'text-pewter')}>
                  {cell}
                </td>
              ))}
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  )
}
