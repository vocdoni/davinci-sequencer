import type { ReactNode } from 'react'
import { CensusOriginBadge, CheckMark, Explain, Timestamp, TxLink } from '~components'
import { useChain, type ProcessView } from '~data/hooks'
import { useJsonDocument } from '~data/queries'
import { Address, Badge, BlockCell, Callout, Hash, KeyValue, Panel, ProgressBar, SkeletonText } from '~kit'
import { formatDuration, formatNumber, formatTimestamp } from '~lib/format'
import { NUM_FIELDS } from '~protocol/limits'
import { parseProcessId } from '~protocol/process-id'
import { CENSUS_ORIGIN_INFO } from '~protocol/types'
import { describeBallotMode } from '../ballot-mode'
import { toJson } from '../json'
import { browsableUri, fetchableUri, metadataChoices, metadataDescription, metadataTitle } from '../metadata'

/** Result cap of `newProcess`: maxValue ≤ 10^12 / maxVoters. */
const MAX_POSSIBLE_RESULT = 1_000_000_000_000n

const CENSUS_ROOT_RULE = {
  unknown: 'Not an origin the registry accepts.',
  'merkle-static': 'Every batch must be proven against the root fixed at creation.',
  'merkle-dynamic':
    'The organizer may replace the root while the process is Ready or Paused and before its end; each batch must use the current root.',
  'onchain-dynamic':
    'Each batch may use any root the census contract recorded at or after the creation block; the registry asks the contract at every settlement.',
  csp: 'Every vote carries a signature from the CSP signer; the root is that signer’s address and never changes.',
} as const

function Label({ children, help }: { children: ReactNode; help: ReactNode }) {
  return (
    <span className='inline-flex items-center gap-1'>
      {children}
      <Explain>{help}</Explain>
    </span>
  )
}

function UriLink({ uri }: { uri: string }) {
  const href = browsableUri(uri)
  if (!uri) return <span className='text-ash'>none</span>
  return href ? (
    <a
      href={href}
      target='_blank'
      rel='noreferrer noopener'
      className='font-mono text-[12px] text-silver hover:text-emerald'
    >
      {uri}
    </a>
  ) : (
    <span className='font-mono text-[12px]'>{uri}</span>
  )
}

export function OverviewTab({ view }: { view: ProcessView }) {
  const s = view.process.state
  if (!s) {
    return (
      <div data-testid='tab-overview'>
        <SkeletonText lines={8} className='max-w-2xl' />
      </div>
    )
  }
  return (
    <div data-testid='tab-overview' className='grid items-start gap-6 lg:grid-cols-2'>
      <div className='flex min-w-0 flex-col gap-6'>
        <BallotPanel view={view} />
        <CensusPanel view={view} />
      </div>
      <div className='flex min-w-0 flex-col gap-6'>
        <DatesPanel view={view} />
        <LimitsPanel view={view} />
        <ProcessIdPanel view={view} />
      </div>
      <div className='min-w-0 lg:col-span-2'>
        <MetadataPanel uri={s.metadataURI} numFields={s.ballotMode.numFields} />
      </div>
    </div>
  )
}

function BallotPanel({ view }: { view: ProcessView }) {
  const bm = view.process.state!.ballotMode
  const d = describeBallotMode(bm)
  return (
    <Panel title='Ballot' label={d.kind === 'unsatisfiable' ? d.label : `Reads as: ${d.label}`} description={d.summary}>
      <ul className='flex flex-col gap-1.5 text-[13px] leading-relaxed text-silver' data-testid='ballot-rules'>
        {d.rules.map((r) => (
          <li key={r} className='flex gap-2'>
            <span aria-hidden='true' className='mt-2 h-1 w-1 shrink-0 rounded-full bg-emerald' />
            {r}
          </li>
        ))}
      </ul>
      <p className='mt-3 text-xs leading-relaxed text-ash'>
        Each voter&apos;s ballot proof enforces these rules on the encrypted ballot, and the batch program checks every
        ballot proof against the ballot mode pinned in state leaf 0x02, so a ballot built for other rules is rejected.
      </p>
      <KeyValue
        className='mt-4'
        columns={2}
        items={[
          {
            label: (
              <Label help={`Numbers per ballot, 1 to ${NUM_FIELDS}. Unused capacity is padded and skipped.`}>
                Fields
              </Label>
            ),
            value: bm.numFields,
            mono: true,
          },
          {
            label: <Label help='No two fields may carry the same value.'>Unique values</Label>,
            value: bm.uniqueValues ? 'yes' : 'no',
            mono: true,
          },
          {
            label: <Label help='Lowest value a field may take.'>Min value</Label>,
            value: formatNumber(bm.minValue),
            mono: true,
          },
          {
            label: <Label help='Highest value a field may take.'>Max value</Label>,
            value: formatNumber(bm.maxValue),
            mono: true,
          },
          {
            label: (
              <Label help='Each value is raised to this power before summing: 2 makes votes cost their square.'>
                Cost exponent
              </Label>
            ),
            value: bm.costExponent,
            mono: true,
          },
          {
            label: <Label help='Groups of fields for multi-question ballots; 0 when unused.'>Group size</Label>,
            value: bm.groupSize,
            mono: true,
          },
          {
            label: <Label help='Lower bound of the cost sum; 0 means none.'>Min sum</Label>,
            value: formatNumber(bm.minValueSum),
            mono: true,
          },
          {
            label: <Label help="Upper bound of the cost sum; 0 means the voter's census weight.">Max sum</Label>,
            value: bm.maxValueSum === 0n ? '0 (weight)' : formatNumber(bm.maxValueSum),
            mono: true,
          },
        ]}
      />
    </Panel>
  )
}

function CensusPanel({ view }: { view: ProcessView }) {
  const { process: p } = view
  const c = p.state!.census
  const updates = p.censusUpdates
  const isCsp = c.origin === 'csp'
  const cspAddress = isCsp ? `0x${c.root.slice(-40)}` : null
  return (
    <Panel
      title='Census'
      label='Who may vote'
      description={CENSUS_ORIGIN_INFO[c.origin].description}
      actions={<CensusOriginBadge origin={c.origin} />}
    >
      <Callout tone='info'>{CENSUS_ROOT_RULE[c.origin]}</Callout>
      <KeyValue
        className='mt-3'
        items={[
          {
            label: (
              <Label
                help={
                  isCsp
                    ? 'The address of the credential service provider whose signatures admit voters, stored as a big-endian integer.'
                    : c.origin === 'onchain-dynamic'
                      ? 'The census contract’s root when the process was created, kept for information: batches are checked against the contract instead.'
                      : 'Root of the lean-IMT Merkle tree of eligible voters and their weights, a big-endian integer. Voters prove membership against it.'
                }
              >
                {isCsp ? 'CSP signer' : c.origin === 'onchain-dynamic' ? 'Root at creation' : 'Census root'}
              </Label>
            ),
            value: cspAddress ? <Address value={cspAddress} /> : <Hash value={c.root} chars={10} />,
          },
          ...(c.origin === 'onchain-dynamic'
            ? [
                {
                  label: (
                    <Label help='The ICensusValidator contract the registry asks, at every settlement, whether it held the batch’s census root.'>
                      Census contract
                    </Label>
                  ),
                  value: <Address value={c.contractAddress} />,
                },
              ]
            : []),
          {
            label: (
              <Label
                help={
                  isCsp
                    ? 'Where voters get their signatures.'
                    : 'Where sequencers download the census; they check its root before serving votes.'
                }
              >
                Census URI
              </Label>
            ),
            value: <UriLink uri={c.uri} />,
          },
        ]}
      />
      {c.origin === 'merkle-dynamic' ? (
        <div className='mt-4'>
          <div className='label-caps mb-2 text-[11px] text-pewter'>Census updates</div>
          {updates.length === 0 ? (
            <p className='text-[13px] text-ash'>The organizer has not replaced the census root.</p>
          ) : (
            <ul className='flex flex-col divide-y divide-charcoal/60 text-[13px]'>
              {updates.map((u, i) => (
                <li key={`${u.block}:${i}`} className='flex flex-wrap items-center gap-x-3 gap-y-1 py-2'>
                  <Timestamp value={u.timestamp} className='text-ash' />
                  <Hash value={u.value.root} chars={8} />
                  <span className='min-w-0 flex-1 truncate'>
                    <UriLink uri={u.value.uri} />
                  </span>
                  {u.tx ? <TxLink hash={u.tx} chars={4} /> : null}
                </li>
              ))}
            </ul>
          )}
        </div>
      ) : null}
    </Panel>
  )
}

function DatesPanel({ view }: { view: ProcessView }) {
  const { process: p, row } = view
  const s = p.state!
  return (
    <Panel title='Dates' label='Voting window'>
      <KeyValue
        items={[
          {
            label: 'Created',
            value: (
              <span className='inline-flex flex-wrap items-center justify-end gap-2'>
                <Timestamp value={row.createdAt} />
                <BlockCell block={p.createdBlock} />
                {p.createdTx ? <TxLink hash={p.createdTx} chars={4} /> : null}
              </span>
            ),
          },
          { label: 'Start', value: <Timestamp value={s.startTime} />, hint: formatTimestamp(s.startTime) },
          {
            label: <Label help='Start time plus duration. Batches settle only inside this window.'>End</Label>,
            value: <Timestamp value={row.endTime} />,
            hint: formatTimestamp(row.endTime),
          },
          { label: 'Duration', value: formatDuration(s.duration), mono: true },
        ]}
      />
      <div className='mt-4'>
        <div className='label-caps mb-2 inline-flex items-center gap-1 text-[11px] text-pewter'>
          Duration changes
          <Explain>
            While a process is Ready or Paused and before its end, the organizer may only extend it. Ending it early
            (status Ended) sets the duration to the time elapsed since the start.
          </Explain>
        </div>
        {p.durationChanges.length === 0 ? (
          <p className='text-[13px] text-ash'>The duration has not changed since creation.</p>
        ) : (
          <ul className='flex flex-col divide-y divide-charcoal/60 text-[13px]'>
            {p.durationChanges.map((c, i) => (
              <li key={`${c.block}:${i}`} className='flex flex-wrap items-center gap-x-3 gap-y-1 py-2'>
                <Timestamp value={c.timestamp} className='text-ash' />
                <span className='flex-1 text-silver'>
                  duration {formatDuration(c.value)}, ends {formatTimestamp(s.startTime + c.value)}
                </span>
                {c.tx ? <TxLink hash={c.tx} chars={4} /> : null}
              </li>
            ))}
          </ul>
        )}
      </div>
    </Panel>
  )
}

function LimitsPanel({ view }: { view: ProcessView }) {
  const { process: p, row } = view
  const s = p.state!
  const worst = BigInt(s.maxVoters) * s.ballotMode.maxValue
  return (
    <Panel title='Limits' label='Capacity'>
      <ProgressBar
        value={row.votersCount}
        total={Math.max(s.maxVoters, 1)}
        label='Voters against the maximum'
        tone={row.votersCount >= s.maxVoters ? 'warn' : 'accent'}
      />
      <KeyValue
        className='mt-3'
        items={[
          {
            label: (
              <Label help='The registry refuses a batch that would take the voter count above this. The organizer may change it while the process is Ready or Paused, but never below the current voter count.'>
                Max voters
              </Label>
            ),
            value: formatNumber(s.maxVoters),
            mono: true,
            hint:
              p.maxVotersChanges.length > 0
                ? `changed ${p.maxVotersChanges.length} time${p.maxVotersChanges.length === 1 ? '' : 's'}, last to ${formatNumber(p.maxVotersChanges[p.maxVotersChanges.length - 1]!.value)}`
                : 'unchanged since creation',
          },
          {
            label: (
              <Label help='Every ballot carries 16 encrypted fields; the process uses the first numFields and the rest are fixed padding.'>
                Fields in use
              </Label>
            ),
            value: `${s.ballotMode.numFields} of ${NUM_FIELDS}`,
            mono: true,
          },
          {
            label: (
              <Label help='The registry requires maxValue × maxVoters to stay within 10^12, so any tally stays inside the bounded search that decrypts it.'>
                Largest possible tally per field
              </Label>
            ),
            value: formatNumber(worst),
            mono: true,
            hint: `cap ${formatNumber(MAX_POSSIBLE_RESULT)}`,
          },
        ]}
      />
    </Panel>
  )
}

function ProcessIdPanel({ view }: { view: ProcessView }) {
  const chain = useChain()
  const parsed = parseProcessId(view.process.id)
  const expected = chain.registry?.pidPrefix
  const prefixHex = `0x${parsed.prefix.toString(16).padStart(8, '0')}`
  return (
    <Panel
      title='Process id'
      label='Decoded'
      description='31 bytes: the organizer’s address, a 4-byte prefix of this registry and chain, and the organizer’s nonce.'
    >
      <KeyValue
        items={[
          { label: 'Organizer', value: <Address value={parsed.organizer} />, hint: 'bytes 0–19' },
          {
            label: (
              <Label help='The last 4 bytes of keccak256(chainId ‖ registry address). The registry refuses ids with another prefix, so an id cannot be replayed on another chain or registry.'>
                Registry prefix
              </Label>
            ),
            value: (
              <span className='inline-flex items-center gap-2'>
                <span className='font-mono'>{prefixHex}</span>
                {expected != null ? <CheckMark state={expected === parsed.prefix ? 'pass' : 'fail'} /> : null}
              </span>
            ),
            hint:
              expected == null
                ? 'bytes 20–23'
                : expected === parsed.prefix
                  ? 'bytes 20–23, matches this registry'
                  : 'bytes 20–23, does not match this registry',
          },
          {
            label: (
              <Label help='How many processes the organizer had created on this registry before this one.'>Nonce</Label>
            ),
            value: formatNumber(parsed.nonce),
            mono: true,
            hint: 'bytes 24–30',
          },
        ]}
      />
    </Panel>
  )
}

function MetadataPanel({ uri, numFields }: { uri: string; numFields: number }) {
  const doc = useJsonDocument(fetchableUri(uri))
  const title = metadataTitle(doc.data)
  const description = metadataDescription(doc.data)
  const choices = metadataChoices(doc.data, numFields)
  return (
    <Panel
      title='Metadata'
      label='Published by the organizer'
      description='The registry stores only this URI. The document (title, questions, options) is not verified on-chain; read it as the organizer’s description.'
    >
      <KeyValue items={[{ label: 'Metadata URI', value: <UriLink uri={uri} /> }]} />
      <div className='mt-3' data-testid='process-metadata'>
        {!uri ? (
          <p className='text-[13px] text-ash'>This process has no metadata document.</p>
        ) : !fetchableUri(uri) ? (
          <p className='text-[13px] text-ash'>
            A browser cannot fetch this URI (only http, https and ipfs are read), so the explorer does not show it.
          </p>
        ) : doc.isLoading ? (
          <SkeletonText lines={3} />
        ) : doc.error ? (
          <Callout tone='warn' title='Could not read the metadata'>
            {doc.error instanceof Error ? doc.error.message : String(doc.error)}. It may be offline, block cross-origin
            requests, or not be JSON.
          </Callout>
        ) : doc.data !== undefined ? (
          <div className='flex flex-col gap-3'>
            {title ? <div className='text-[15px] font-semibold text-ghost'>{title}</div> : null}
            {description ? <p className='text-[13px] leading-relaxed text-ash'>{description}</p> : null}
            {choices ? (
              <div className='flex flex-wrap gap-2'>
                {choices.map((c, i) => (
                  <Badge key={i}>
                    field {i + 1}: {c}
                  </Badge>
                ))}
              </div>
            ) : null}
            <details className='rounded-sm border border-charcoal'>
              <summary className='cursor-pointer px-3 py-2 text-[13px] text-pewter hover:text-ghost'>
                The document as JSON
              </summary>
              <pre className='max-h-80 overflow-auto border-t border-charcoal p-3 text-[11px] leading-relaxed text-silver scroll-slim'>
                {toJson(doc.data)}
              </pre>
            </details>
          </div>
        ) : null}
      </div>
    </Panel>
  )
}
