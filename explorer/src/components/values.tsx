import type { ReactNode } from 'react'
import { Link } from 'react-router-dom'
import { useRuntimeConfig } from '~config/config-context'
import { nativeSymbol } from '~data/client'
import { useChainNow } from '~data/hooks'
import { CheckIcon, CloseIcon, CopyButton, ExternalIcon, Hash, InfoIcon, Tooltip } from '~kit'
import type { CheckState } from '~indexer/selectors'
import { cn } from '~lib/cn'
import { explorerTxUrl } from '~lib/explorer'
import { formatTimestamp, formatWei, shortHash, timeAgo } from '~lib/format'
import { paths } from '~routes/paths'

/** A process id, shortened, linking to its page. */
export function ProcessIdLink({ id, chars = 8, className }: { id: string; chars?: number; className?: string }) {
  return <Hash value={id} chars={chars} href={paths.process(id)} className={className} />
}

/**
 * A transaction hash: the in-app route (`/tx/:hash` resolves it to its
 * transition or process) plus the block-explorer link.
 */
export function TxLink({ hash, chars = 6, className }: { hash: string; chars?: number; className?: string }) {
  const { blockExplorerUrl } = useRuntimeConfig()
  const external = explorerTxUrl(blockExplorerUrl, hash)
  return (
    <span className={cn('inline-flex min-w-0 items-center gap-1 font-mono text-[12px]', className)}>
      <Tooltip content={hash}>
        <Link to={paths.tx(hash)} className='truncate text-silver transition-colors hover:text-emerald'>
          {shortHash(hash, chars, 4)}
        </Link>
      </Tooltip>
      <CopyButton value={hash} label='Copy transaction hash' />
      {external ? (
        <a
          href={external}
          target='_blank'
          rel='noreferrer noopener'
          aria-label='View transaction on the block explorer'
          className='inline-flex shrink-0 items-center rounded-sm p-1 text-ash transition-colors hover:bg-onyx hover:text-ghost'
        >
          <ExternalIcon size={13} />
        </a>
      ) : null}
    </span>
  )
}

/** Unix time as UTC, with "5 min ago" relative to the chain head. */
export function Timestamp({
  value,
  relative = true,
  className,
}: {
  value: number | null | undefined
  relative?: boolean
  className?: string
}) {
  const now = useChainNow()
  if (value == null) return <span className={cn('text-ash', className)}>—</span>
  return (
    <Tooltip content={formatTimestamp(value)}>
      <span className={cn('whitespace-nowrap', className)}>
        {relative && now != null ? timeAgo(value, now) : formatTimestamp(value)}
      </span>
    </Tooltip>
  )
}

/** A wei amount in the chain's native currency. */
export function NativeAmount({
  wei,
  digits = 6,
  className,
}: {
  wei: bigint | null | undefined
  digits?: number
  className?: string
}) {
  const { chainId } = useRuntimeConfig()
  if (wei == null) return <span className={cn('text-ash', className)}>—</span>
  return (
    <Tooltip content={`${wei.toString()} wei`}>
      <span className={cn('font-mono tnum whitespace-nowrap', className)}>
        {formatWei(wei, digits)} <span className='text-ash'>{nativeSymbol(chainId)}</span>
      </span>
    </Tooltip>
  )
}

const CHECK_STYLE: Record<CheckState, { cls: string; label: string }> = {
  pass: { cls: 'text-emerald border-emerald/30 bg-emerald/10', label: 'passed' },
  fail: { cls: 'text-red border-red/30 bg-red/10', label: 'failed' },
  unknown: { cls: 'text-ash border-charcoal bg-transparent', label: 'not checked yet' },
}

/** ✓ / ✗ / … for a verification check. */
export function CheckMark({ state, className }: { state: CheckState; className?: string }) {
  const s = CHECK_STYLE[state]
  return (
    <span
      role='img'
      aria-label={s.label}
      className={cn(
        'inline-flex h-4.5 w-4.5 shrink-0 items-center justify-center rounded-full border',
        s.cls,
        className
      )}
    >
      {state === 'pass' ? (
        <CheckIcon size={11} />
      ) : state === 'fail' ? (
        <CloseIcon size={11} />
      ) : (
        <span className='text-[9px] leading-none'>…</span>
      )}
    </span>
  )
}

/** An inline "what is this" affordance: an info glyph with the explanation on hover and focus. */
export function Explain({ children, className }: { children: ReactNode; className?: string }) {
  return (
    <Tooltip content={children}>
      <button
        type='button'
        aria-label='What is this?'
        className={cn(
          'inline-flex shrink-0 items-center rounded-sm p-0.5 align-middle text-ash hover:text-ghost',
          className
        )}
      >
        <InfoIcon size={13} />
      </button>
    </Tooltip>
  )
}
