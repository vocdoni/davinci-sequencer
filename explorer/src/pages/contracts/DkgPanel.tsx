import { Link } from 'react-router-dom'
import { CheckMark, Timestamp } from '~components'
import { useRuntimeConfig } from '~config/config-context'
import {
  DKG_CIRCUITS_V6_KEY_HASHES,
  DKG_POOL_KEYS,
  type DkgDeployment,
  type DkgEpochPhase,
  type DkgEpochView,
} from '~data/deployment'
import type { ChainMeta } from '~indexer/types'
import {
  Address,
  Badge,
  BlockCell,
  Callout,
  ExternalIcon,
  Hash,
  KeyValue,
  Panel,
  SkeletonText,
  StatCell,
  StatRow,
  type BadgeTone,
} from '~kit'
import { formatDuration, formatNumber } from '~lib/format'
import { paths } from '~routes/paths'
import { DKG_VERIFIER_LABELS, dkgExplorerLink } from './model'
import { Code, SourceLink, SubHeading } from './parts'

const PHASES: Record<DkgEpochPhase, { label: string; tone: BadgeTone; description: string }> = {
  none: { label: 'Not created', tone: 'neutral', description: 'No epoch has been created yet.' },
  'committee-selection': {
    label: 'Committee selection',
    tone: 'warn',
    description: 'The lottery is drawing the committee: eligible operators claim slots until n are filled.',
  },
  'key-assembly': {
    label: 'Key assembly',
    tone: 'warn',
    description: 'Each member submits one proof-carrying contribution that deals its shares of all 16 pool keys.',
  },
  live: {
    label: 'Live',
    tone: 'ok',
    description:
      'finalizeEpoch stored all 16 pool keys: applications can register, submit ciphertexts and get them decrypted.',
  },
  aborted: {
    label: 'Aborted',
    tone: 'danger',
    description:
      'The committee did not fill in time, or too few members contributed during key assembly, so the epoch serves nobody. The nodes create the next one.',
  },
  completed: { label: 'Completed', tone: 'neutral', description: 'Reserved by the contract; not used.' },
}

export function DkgPhaseBadge({ phase }: { phase: DkgEpochPhase }) {
  const p = PHASES[phase]
  return (
    <Badge tone={p.tone} dot={phase === 'live'} title={p.description}>
      {p.label}
    </Badge>
  )
}

function ExternalText({ href, children }: { href: string; children: string }) {
  return (
    <a
      href={href}
      target='_blank'
      rel='noreferrer noopener'
      className='inline-flex items-center gap-1 text-[12px] text-pewter hover:text-emerald'
    >
      {children}
      <ExternalIcon size={12} />
    </a>
  )
}

/** Unix time of a future or past block, from the head and the average block time. */
function blockTime(chain: ChainMeta, block: number): number | null {
  if (chain.headTimestamp == null) return null
  return chain.headTimestamp + (block - chain.headBlock) * chain.blockTimeSeconds
}

export function DkgPanel({
  chain,
  dkg,
  loading,
  error,
}: {
  chain: ChainMeta
  dkg: DkgDeployment | null | undefined
  loading: boolean
  error: string | null
}) {
  const config = useRuntimeConfig()
  const r = chain.registry
  const description =
    'The davinci-dkg committee that holds the election keys of DKG-mode processes and decrypts their tallies.'

  if (r && !r.dkgAdapter) {
    return (
      <Panel title='DKG committee' label='Threshold keys' description={description}>
        <Callout title='The DKG key modes are disabled on this registry'>
          It was deployed without a DKG manager, so it has no adapter and <Code>newProcess</Code> in a DKG mode reverts
          with <Code>DKGDisabled</Code>. Every process here uses a sequencer key.
        </Callout>
      </Panel>
    )
  }
  if (!dkg) {
    return (
      <Panel title='DKG committee' label='Threshold keys' description={description}>
        {error && !loading ? (
          <Callout tone='warn' title='Could not read the DKG contracts'>
            {error}
          </Callout>
        ) : (
          <SkeletonText lines={5} className='max-w-xl' />
        )}
      </Panel>
    )
  }

  const e = dkg.newestEpoch
  const epochLink = e ? dkgExplorerLink(config.dkgExplorerUrl, 'epoch', e.id) : null
  const nextStart = dkg.nextEpochStartBlock
  return (
    <Panel
      title='DKG committee'
      label='Threshold keys'
      description={description}
      actions={
        config.dkgExplorerUrl ? <ExternalText href={config.dkgExplorerUrl}>DKG explorer</ExternalText> : undefined
      }
    >
      <StatRow>
        <StatCell
          label='Newest epoch'
          value={e ? `#${e.nonce}` : 'none'}
          hint={e ? PHASES[e.phase].label : 'no epoch created yet'}
          mono
        />
        <StatCell
          label='Threshold'
          value={e ? `${e.threshold} of ${e.committeeSize}` : '—'}
          hint='members needed to decrypt'
          mono
        />
        <StatCell
          label='Pool keys claimed'
          value={e?.poolKeysClaimed != null ? `${e.poolKeysClaimed} / ${DKG_POOL_KEYS}` : '—'}
          hint='one per application'
          mono
        />
        <StatCell
          label='Operators'
          value={dkg.activeCount != null ? formatNumber(dkg.activeCount) : '—'}
          hint={dkg.nodeCount != null ? `active, of ${formatNumber(dkg.nodeCount)} registered` : 'active'}
          mono
        />
      </StatRow>

      {e ? <EpochDetails epoch={e} epochLink={epochLink} /> : null}

      <div className='mt-6 grid gap-6 lg:grid-cols-2'>
        <div>
          <SubHeading>Where a new DKG process gets its key</SubHeading>
          <div className='mt-2 text-[13px] leading-relaxed text-ash' data-testid='registration-epoch'>
            {dkg.registrationEpoch ? (
              <>
                A <span className='text-silver'>DKG automatic</span> process created now takes the next free pool key of
                epoch <Hash value={dkg.registrationEpoch} chars={8} className='align-middle' /> (the adapter’s{' '}
                <Code>registrationEpoch()</Code>: the newest Live epoch with a free key, looking back at most 8 epochs).
                A <span className='text-silver'>DKG locked</span> process names its epoch itself, because the
                organizer’s proof of possession binds it; clients read the same value.
              </>
            ) : dkg.registrationEpochReverted ? (
              <>
                No Live epoch with a free pool key among the last 8, so <Code>registrationEpoch()</Code> reverts and
                automatic processes revert <Code>NoLiveEpoch</Code> until a new epoch is Live. Locked processes name
                their epoch and are unaffected.
              </>
            ) : (
              '…'
            )}
          </div>
        </div>
        <div>
          <SubHeading>Epoch cadence</SubHeading>
          <KeyValue
            className='mt-1'
            items={[
              {
                label: 'Epoch length',
                value:
                  dkg.epochDurationBlocks != null
                    ? `${formatNumber(dkg.epochDurationBlocks)} blocks · ~${formatDuration(dkg.epochDurationBlocks * chain.blockTimeSeconds)}`
                    : '…',
              },
              {
                label: 'Next epoch possible from',
                value:
                  nextStart != null ? (
                    <span className='inline-flex items-center gap-2'>
                      <BlockCell block={nextStart} />
                      <Timestamp value={blockTime(chain, nextStart)} className='text-[12px] text-ash' />
                    </span>
                  ) : (
                    '…'
                  ),
                hint: 'or earlier, once the newest pool is nearly spent or the epoch aborted',
              },
              {
                label: 'Epoch policy bounds',
                value:
                  dkg.minThreshold != null
                    ? `t ≥ ${dkg.minThreshold}, n ≥ ${dkg.minCommitteeSize}, 1 ≤ α ≤ ${(dkg.maxLotteryAlphaBps ?? 0) / 10_000}`
                    : '…',
                hint: 'whoever creates an epoch picks t, n and α within these',
              },
              {
                label: 'Inactivity window',
                value:
                  dkg.inactivityWindow != null
                    ? `${formatNumber(dkg.inactivityWindow)} blocks · ~${formatDuration(dkg.inactivityWindow * chain.blockTimeSeconds)}`
                    : '…',
                hint: 'an operator silent this long can be marked inactive',
              },
            ]}
          />
        </div>
      </div>

      <Registration dkg={dkg} adapter={r?.dkgAdapter ?? null} />

      <div className='mt-6'>
        <SubHeading>Groth16 verifiers</SubHeading>
        <p className='mt-1 text-[12px] leading-relaxed text-ash'>
          Every contribution, finalization, partial decryption and combine is one proof-carrying call, checked by one of
          these; there is no dispute phase. Each verifier reports the SHA-256 of its circuit’s proving key, compared
          here with the davinci-dkg <Code>circuits-v6</Code> release, whose files the committee’s nodes download and
          check against the same hashes.
        </p>
        <ul className='mt-3 flex flex-col divide-y divide-charcoal rounded-md border border-charcoal'>
          {dkg.verifiers.map((v) => {
            const expected = DKG_CIRCUITS_V6_KEY_HASHES[v.name]
            const state = v.keyHash == null ? 'unknown' : v.keyHash === expected ? 'pass' : 'fail'
            return (
              <li
                key={v.name}
                className='flex flex-col gap-2 p-3 md:flex-row md:items-center md:justify-between'
                data-testid={`dkg-verifier-${v.name}`}
              >
                <div className='flex min-w-0 items-center gap-2.5'>
                  <CheckMark state={state} />
                  <span className='text-[13px] text-silver'>{DKG_VERIFIER_LABELS[v.name].name}</span>
                </div>
                <div className='flex min-w-0 flex-wrap items-center gap-x-3 gap-y-1 md:justify-end'>
                  {v.keyHash ? (
                    <span className='inline-flex items-center gap-1 text-[12px] text-ash'>
                      key <Hash value={v.keyHash} chars={8} />
                    </span>
                  ) : null}
                  {v.address ? (
                    <span className='inline-flex items-center gap-1'>
                      <Address value={v.address} chars={4} />
                      <SourceLink address={v.address} />
                    </span>
                  ) : null}
                </div>
              </li>
            )
          })}
        </ul>
      </div>

      <Callout className='mt-6' title='What you trust in the DKG modes'>
        A threshold of an epoch’s committee could decrypt every ballot of the processes keyed on that epoch (with the
        organizer secret as well, in locked mode); the design trusts that threshold not to collude. A process’s key
        belongs to one epoch’s committee and there is no resharing, so if more than n − t of its members leave before
        the process ends, its results are lost.{' '}
        <Link
          to={paths.learn('key-modes')}
          className='text-pewter underline-offset-2 hover:text-emerald hover:underline'
        >
          The key modes, explained
        </Link>
      </Callout>
    </Panel>
  )
}

function EpochDetails({ epoch: e, epochLink }: { epoch: DkgEpochView; epochLink: string | null }) {
  const config = useRuntimeConfig()
  return (
    <div className='mt-6 grid gap-6 lg:grid-cols-2' data-testid='dkg-epoch'>
      <div>
        <SubHeading>Epoch #{e.nonce}</SubHeading>
        <KeyValue
          className='mt-1'
          items={[
            {
              label: 'Epoch id',
              value: (
                <span className='inline-flex items-center gap-2'>
                  <Hash value={e.id} chars={8} />
                  {epochLink ? <ExternalText href={epochLink}>DKG explorer</ExternalText> : null}
                </span>
              ),
              hint: 'the manager’s 4-byte prefix and the epoch nonce',
            },
            {
              label: 'Phase',
              value: <DkgPhaseBadge phase={e.phase} />,
              hint: PHASES[e.phase].description,
            },
            { label: 'Created at block', value: <BlockCell block={e.startBlock} /> },
            {
              label: 'Contributions accepted',
              value: `${e.contributionCount} of ${e.committeeSize}`,
              hint: `at least ${e.minValidContributions} needed to finalize`,
            },
            {
              label: 'Applications',
              value: e.applications != null ? formatNumber(e.applications) : '—',
              hint: 'DAVINCI processes and any other application on this epoch',
            },
          ]}
        />
      </div>
      <div>
        <SubHeading>Committee, in slot order</SubHeading>
        <p className='mt-1 text-[12px] leading-relaxed text-ash'>
          Drawn by an on-chain lottery from the registered operators, first come first served among the eligible ones.
          Any {e.threshold} of them can decrypt; fewer cannot.
        </p>
        {e.committee.length ? (
          <ol className='mt-2 flex flex-col gap-1.5'>
            {e.committee.map((a, i) => {
              const link = dkgExplorerLink(config.dkgExplorerUrl, 'operator', a)
              return (
                <li key={a} className='flex items-center gap-2 text-[12px]'>
                  <span className='w-5 text-right font-mono text-ash'>{i + 1}</span>
                  <Address value={a} chars={6} />
                  {link ? <ExternalText href={link}>operator</ExternalText> : null}
                </li>
              )
            })}
          </ol>
        ) : (
          <p className='mt-2 text-[12px] text-ash'>Not selected yet.</p>
        )}
      </div>
    </div>
  )
}

function Registration({ dkg, adapter }: { dkg: DkgDeployment; adapter: string | null }) {
  const reg = dkg.registration
  if (reg.kind === 'registrar') {
    const isAdapter = adapter != null && reg.address === adapter.toLowerCase()
    return (
      <Callout className='mt-6' title='Application registration is restricted' tone='info'>
        Only <Address value={reg.address} chars={6} className='align-middle' />
        {isAdapter ? ', this registry’s adapter,' : ''} may register applications on this DKGAppManager, so this
        committee serves only that integrator.
      </Callout>
    )
  }
  if (reg.kind === 'unknown') return null
  return (
    <Callout className='mt-6' title='Anyone can register an application' tone='ok'>
      The DKGAppManager has {reg.reason === 'no-gate' ? 'no registrar' : 'no registrar set'}: any contract or account
      can register an application on a Live epoch, so other applications can share this committee with DAVINCI. Each
      registration claims one of the epoch’s {DKG_POOL_KEYS} pool keys. Once one key or fewer is left the contract
      allows the next epoch early and the nodes create it; if the pool runs out first, DKG-mode process creation waits
      for it (about one epoch setup).
    </Callout>
  )
}
