import { useState, type ReactNode } from 'react'
import type { SortingState } from '@tanstack/react-table'
import { CheckMark, Explain, KeyModeBadge } from '~components'
import { useRuntimeConfig } from '~config/config-context'
import { useChain, type ProcessView } from '~data/hooks'
import { useDkgApplication } from '~data/queries'
import {
  Address,
  Badge,
  BlockCell,
  Callout,
  DataTable,
  ExternalIcon,
  Hash,
  KeyValue,
  Panel,
  SkeletonText,
  type AnyColumnDef,
} from '~kit'
import type { DkgCiphertextView } from '~data/services'
import { bigIntToHex, formatNumber } from '~lib/format'
import type { KeyModeName } from '~protocol/types'
import { reducedToCircom } from '~protocol/babyjubjub'
import { dkgApplicationUrl, dkgEpochUrl } from '../dkg-links'

const TRUST: Record<KeyModeName, { who: string; when: string; risk: string }> = {
  sequencer: {
    who: 'The organizer supplied this key at creation, normally one it got from a sequencer node (POST /processes/keys). That node derives the secret from its master secret and the process id and never stores it; whoever holds the secret is the only party that can decrypt.',
    when: 'After the end, the key holder decrypts the final accumulator (the encrypted sum of all ballots), proves the tally with the zkVM results program and publishes it with setProcessResults.',
    risk: 'The key holder could open every ballot published in the blobs, and nobody else can publish the results. This mode trusts one party with ballot secrecy.',
  },
  'dkg-automatic': {
    who: 'The key is one pool key of a davinci-dkg committee epoch. No sequencer and no organizer holds the secret; each committee member holds one share.',
    when: 'After the end, anyone can send the final accumulator to the committee (requestResultsDecryption); sequencers do it on their first heartbeat after the end. A threshold of members post partial decryptions, each with a Groth16 proof, and a combine yields each field’s total. The committee never reconstructs the secret: it decrypts only the final accumulator.',
    risk: 'A threshold of the epoch’s committee colluding could decrypt every ballot. If more than n − t members leave before the end, the results are lost.',
  },
  'dkg-locked': {
    who: 'The key is a committee pool key plus an organizer key. The organizer received its secret at creation; the registry never stores it.',
    when: 'The committee cannot post partial decryptions until the organizer reveals its secret (revealProcessKey). The organizer decides when the tally appears, not which one.',
    risk: 'Losing the organizer secret loses the results. Revealing it during voting drops the process to the automatic trust model.',
  },
}

function Label({ children, help }: { children: ReactNode; help: ReactNode }) {
  return (
    <span className='inline-flex items-center gap-1'>
      {children}
      <Explain>{help}</Explain>
    </span>
  )
}

function ExternalLink({ href, children }: { href: string; children: ReactNode }) {
  return (
    <a
      href={href}
      target='_blank'
      rel='noreferrer noopener'
      className='inline-flex items-center gap-1 text-[13px] text-emerald hover:underline'
    >
      {children}
      <ExternalIcon size={12} />
    </a>
  )
}

function PointValue({ x, y }: { x: bigint; y: bigint }) {
  return (
    <span className='inline-flex flex-col items-end'>
      <span className='inline-flex items-center gap-1'>
        <span className='text-ash'>x</span>
        <Hash value={bigIntToHex(x)} chars={8} />
      </span>
      <span className='inline-flex items-center gap-1'>
        <span className='text-ash'>y</span>
        <Hash value={bigIntToHex(y)} chars={8} />
      </span>
    </span>
  )
}

export function KeyTab({ view }: { view: ProcessView }) {
  const s = view.process.state
  if (!s) {
    return (
      <div data-testid='tab-key'>
        <SkeletonText lines={6} className='max-w-2xl' />
      </div>
    )
  }
  const trust = TRUST[s.keyMode]
  return (
    <div data-testid='tab-key' className='flex flex-col gap-6'>
      <div className='grid items-start gap-6 lg:grid-cols-2'>
        <Panel title='Key mode' label='Who can decrypt, and when' actions={<KeyModeBadge mode={s.keyMode} />}>
          <dl className='flex flex-col gap-3 text-[13px] leading-relaxed'>
            <div>
              <dt className='label-caps text-[11px] text-pewter'>Who holds the key</dt>
              <dd className='mt-1 text-silver'>{trust.who}</dd>
            </div>
            <div>
              <dt className='label-caps text-[11px] text-pewter'>How the tally is decrypted</dt>
              <dd className='mt-1 text-silver'>{trust.when}</dd>
            </div>
            <div>
              <dt className='label-caps text-[11px] text-pewter'>What you trust</dt>
              <dd className='mt-1 text-silver'>{trust.risk}</dd>
            </div>
          </dl>
        </Panel>
        <Panel
          title='Encryption key'
          label='BabyJubJub point'
          description='Voters encrypt each ballot field to this key with ElGamal. The genesis state root pins it as leaf 0x03, so it cannot change after creation, and every batch re-encrypts the ballots under it.'
        >
          <KeyValue
            items={[
              {
                label: <Label help='Twisted Edwards x coordinate, circomlib form.'>x</Label>,
                value: <Hash value={bigIntToHex(s.encryptionKey.x)} chars={10} />,
                hint: <span className='font-mono break-all'>{s.encryptionKey.x.toString()}</span>,
              },
              {
                label: <Label help='Twisted Edwards y coordinate.'>y</Label>,
                value: <Hash value={bigIntToHex(s.encryptionKey.y)} chars={10} />,
                hint: <span className='font-mono break-all'>{s.encryptionKey.y.toString()}</span>,
              },
            ]}
          />
        </Panel>
      </div>
      {s.keyMode !== 'sequencer' ? <DkgPanel view={view} /> : null}
    </div>
  )
}

const ciphertextColumns: AnyColumnDef<DkgCiphertextView>[] = [
  { id: 'index', header: 'DKG index', accessorKey: 'index', meta: { numeric: true, width: '110px' } },
  {
    id: 'field',
    header: 'Ballot field',
    accessorKey: 'field',
    cell: ({ row }) => `field ${row.original.field + 1}`,
    meta: { width: '120px' },
  },
  {
    id: 'completed',
    header: 'Combined',
    accessorFn: (r) => (r.completed ? 1 : 0),
    cell: ({ row }) => (
      <span className='inline-flex items-center gap-2'>
        <CheckMark state={row.original.completed ? 'pass' : 'unknown'} />
        {row.original.completed ? 'decrypted' : 'waiting for partials'}
      </span>
    ),
  },
  {
    id: 'plaintext',
    header: 'Plaintext',
    accessorFn: (r) => r.plaintext,
    cell: ({ row }) => (row.original.completed ? formatNumber(row.original.plaintext) : '—'),
    meta: { numeric: true },
  },
]

function DkgPanel({ view }: { view: ProcessView }) {
  const { dkgExplorerUrl } = useRuntimeConfig()
  const chain = useChain()
  const dkg = useDkgApplication(view.process.id)
  const [sorting, setSorting] = useState<SortingState>([])
  const s = view.process.state!
  const info = s.dkg
  const app = dkg.data
  const locked = s.keyMode === 'dkg-locked'
  const appUrl = info ? dkgApplicationUrl(dkgExplorerUrl, info.epochId, info.aid) : null
  const epochUrl = info ? dkgEpochUrl(dkgExplorerUrl, info.epochId) : null
  const converted = app ? reducedToCircom(app.applicationKey) : null
  const keyMatches = converted != null && converted.x === s.encryptionKey.x && converted.y === s.encryptionKey.y

  return (
    <Panel
      title='DKG application'
      label='davinci-dkg committee'
      description='The registry’s DKG adapter registered one application for this process on the committee’s epoch. The committee answers only ciphertexts the adapter submits for it.'
      actions={appUrl ? <ExternalLink href={appUrl}>Open in the DKG explorer</ExternalLink> : null}
    >
      {!info ? (
        <Callout tone='warn'>The process state carries no DKG data.</Callout>
      ) : (
        <div className='flex flex-col gap-5'>
          <div className='grid gap-6 lg:grid-cols-2'>
            <KeyValue
              items={[
                {
                  label: <Label help='The DKG run whose committee holds this key.'>Epoch</Label>,
                  value: (
                    <span className='inline-flex items-center gap-2'>
                      <Hash value={info.epochId} chars={10} />
                      {epochUrl ? <ExternalLink href={epochUrl}>epoch</ExternalLink> : null}
                    </span>
                  ),
                },
                {
                  label: (
                    <Label help='Application id: keccak256(chainid ‖ registry ‖ process id) reduced into the BabyJubJub base field, never zero. Anyone can recompute it.'>
                      aid
                    </Label>
                  ),
                  value: <Hash value={info.aid} chars={10} />,
                },
                {
                  label: (
                    <Label help='Each epoch deals 16 independent pool keys; every application claims one, so decryptions are scoped to it.'>
                      Pool index
                    </Label>
                  ),
                  value: app ? app.poolIndex : dkg.isLoading ? '…' : '—',
                  mono: true,
                },
                ...(app
                  ? [
                      {
                        label: 'Registered at block',
                        value: <BlockCell block={app.createdAtBlock} />,
                      },
                      {
                        label: 'Registrant',
                        value: <Address value={app.creator} />,
                        hint:
                          chain.registry?.dkgAdapter && app.creator === chain.registry.dkgAdapter.toLowerCase()
                            ? 'the registry’s DKG adapter'
                            : undefined,
                      },
                    ]
                  : []),
              ]}
            />
            {dkg.isLoading ? (
              <SkeletonText lines={5} />
            ) : dkg.error ? (
              <Callout tone='warn' title='Could not read the DKG contracts'>
                {dkg.error instanceof Error ? dkg.error.message : String(dkg.error)}
              </Callout>
            ) : app ? (
              <KeyValue
                items={[
                  {
                    label: (
                      <Label help='The committee’s key P_j, in the DKG’s reduced twisted Edwards form.'>Pool key</Label>
                    ),
                    value: app.poolKey ? <PointValue x={app.poolKey.x} y={app.poolKey.y} /> : '—',
                  },
                  {
                    label: (
                      <Label help='PK_org. In automatic mode it is the identity (0, 1): there is no organizer key.'>
                        Organizer key
                      </Label>
                    ),
                    value: locked ? (
                      <PointValue x={app.organizerPK.x} y={app.organizerPK.y} />
                    ) : (
                      <span className='text-ash'>none (automatic)</span>
                    ),
                  },
                  {
                    label: (
                      <Label help='P_j, plus PK_org when locked, in the reduced form. The registry stores it converted to circomlib form (same y, x scaled by a fixed constant); the check redoes that conversion.'>
                        Application key
                      </Label>
                    ),
                    value: (
                      <span className='inline-flex items-center gap-2'>
                        <PointValue x={app.applicationKey.x} y={app.applicationKey.y} />
                        <CheckMark state={keyMatches ? 'pass' : 'fail'} />
                      </span>
                    ),
                    hint: keyMatches
                      ? 'converted, it is the process encryption key'
                      : 'converted, it is not the process encryption key',
                  },
                  {
                    label: (
                      <Label help='Locked applications stay closed until the organizer reveals its secret; the DKG checks sk·G = PK_org.'>
                        Organizer secret
                      </Label>
                    ),
                    value: !locked ? (
                      <Badge>not needed</Badge>
                    ) : app.revealed ? (
                      <span className='inline-flex items-center gap-2'>
                        <Badge tone='ok'>revealed</Badge>
                        <Hash value={bigIntToHex(app.organizerSecret)} chars={6} />
                      </span>
                    ) : (
                      <Badge tone='warn'>sealed</Badge>
                    ),
                  },
                ]}
              />
            ) : (
              <p className='text-[13px] text-ash'>No DKG application was found for this process.</p>
            )}
          </div>

          <div>
            <div className='label-caps mb-2 inline-flex items-center gap-1 text-[11px] text-pewter'>
              Submitted ciphertexts
              <Explain>
                requestResultsDecryption submits one ciphertext per ballot field of the final accumulator. Every ballot
                and refresh adds a ciphertext to every field, so an option nobody picked still holds a real one; only a
                process that never tallied a ballot has identity fields, which are recorded as 0 without the committee.
              </Explain>
            </div>
            {!info.resultsRequested ? (
              <p className='text-[13px] text-ash'>
                Nothing yet. After the process ends, anyone can send the final accumulator (sequencers do on their first
                heartbeat after the end), and its ciphertexts appear here with their decryption state.
              </p>
            ) : app ? (
              <>
                <p className='mb-2 text-[13px] text-ash'>
                  {formatNumber(info.count)} ciphertext{info.count === 1 ? '' : 's'} from index{' '}
                  {formatNumber(info.firstIndex)}
                  {info.zeroSkipped
                    ? `; fields ${skippedFields(info.zeroSkipped)} were the identity, as no ballot was tallied, and were recorded as 0`
                    : ''}
                  .{' '}
                  {locked && !app.revealed
                    ? 'The committee waits for the organizer’s reveal before it can decrypt them.'
                    : null}
                </p>
                <div className='overflow-hidden rounded-sm border border-charcoal'>
                  <DataTable
                    data={app.ciphertexts}
                    columns={ciphertextColumns}
                    getRowId={(r) => String(r.index)}
                    sorting={sorting}
                    onSortingChange={setSorting}
                    empty={
                      <p className='p-4 text-[13px] text-ash'>
                        Every field was the identity, as no ballot was tallied: nothing was submitted.
                      </p>
                    }
                  />
                </div>
              </>
            ) : (
              <SkeletonText lines={3} />
            )}
          </div>
        </div>
      )}
    </Panel>
  )
}

function skippedFields(mask: number): string {
  const out: number[] = []
  for (let i = 0; i < 16; i++) if ((mask >> i) & 1) out.push(i + 1)
  return out.join(', ')
}
