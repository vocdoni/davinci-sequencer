import { useEffect, useMemo, type ReactNode } from 'react'
import { Link } from 'react-router-dom'
import { CheckMark, Explain, Timestamp, TxLink } from '~components'
import { useDataSource } from '~data/context'
import { useStore, type ProcessView } from '~data/hooks'
import { useDkgApplication, useJsonDocument } from '~data/queries'
import type { CheckState } from '~indexer/selectors'
import { txKey } from '~indexer/types'
import { Address, Badge, BlockCell, Callout, Hash, KeyValue, Panel, SkeletonText } from '~kit'
import { formatNumber } from '~lib/format'
import { decodeResultsPublicValues, resultsFailBits, type ResultsPublics } from '~protocol/publics'
import type { KeyModeName } from '~protocol/types'
import { describeBallotMode } from '../ballot-mode'
import { fetchableUri, metadataChoices } from '../metadata'
import { tallyRows } from '../tally'
import { paths } from '~routes/paths'

interface Check {
  label: string
  state: CheckState
  detail: ReactNode
}

function CheckList({ checks }: { checks: Check[] }) {
  return (
    <ul className='flex flex-col divide-y divide-charcoal/60'>
      {checks.map((c) => (
        <li key={c.label} className='flex gap-3 py-2.5'>
          <CheckMark state={c.state} className='mt-0.5' />
          <div className='min-w-0'>
            <div className='text-[13px] text-silver'>{c.label}</div>
            <div className='text-xs leading-relaxed text-ash'>{c.detail}</div>
          </div>
        </li>
      ))}
    </ul>
  )
}

/** What the registry enforced before storing a tally, from ProcessRegistry.sol. */
const CONTRACT_RULES: Record<'sequencer' | 'dkg', string[]> = {
  sequencer: [
    'setProcessResults accepts only a sequencer-key process that is not canceled and has no results yet.',
    'The process must have ended: status Ended, or its end time passed.',
    'The public values must report that every check of the results program passed (ok = 1, fail mask 0).',
    'The state root the proof was made for must be the process’s latest state root, so the tally covers every settled batch.',
    'The PLONK must verify against the registry’s results program vk and ZisK setup root.',
    'It then stores the first numFields tallies from the public values and sets the status to Results.',
  ],
  dkg: [
    'requestResultsDecryption accepts only a DKG-key process that is not canceled, has no results and was not requested before, once it has ended by status or by time.',
    'It checks the submitted accumulator (the encrypted sum of all ballots) is leaf 0x04 of the latest state root, with every coordinate in range, so nobody can send another ciphertext for decryption.',
    'It moves the process to Ended, out of the organizer’s hands: the plaintexts become public on the DKG before they reach the registry, and a Ready process could otherwise be canceled after its organizer saw the tally.',
    'It submits one ciphertext per field to the committee through the DKG adapter. A field that is the identity is recorded as 0; only a process that never tallied a ballot has one, since every ballot and refresh adds a ciphertext to every field, even an option nobody picked.',
    'finalizeResultsFromDKG stores the result only when every submitted ciphertext has a completed combine on the DKG, whose partial decryptions and combine each carried a Groth16 proof the DKG contracts verified.',
  ],
}

const RESULTS_PROGRAM =
  'The results program proves, from the final state root alone, that the encryption key is leaf 0x03 and the accumulator leaf 0x04 of that tree, and that each tally is the decryption of the accumulator under that key (one Chaum–Pedersen proof per field). The key holder never reveals the key.'

export function ResultsTab({ view }: { view: ProcessView }) {
  const s = view.process.state
  if (!s) {
    return (
      <div data-testid='tab-results'>
        <SkeletonText lines={6} className='max-w-2xl' />
      </div>
    )
  }
  const results = view.process.results
  return (
    <div data-testid='tab-results' className='flex flex-col gap-6'>
      {results ? <TallyPanel view={view} /> : <NoResultsPanel view={view} />}
      <div className='grid items-start gap-6 lg:grid-cols-2'>
        {s.keyMode === 'sequencer' ? <SequencerProofPanel view={view} /> : <DkgDecryptionPanel view={view} />}
        <Panel
          title='What the contract checked'
          label='On-chain rules'
          description='The rules the registry enforces before it stores a tally. A result on-chain means all of them held.'
        >
          <ol className='flex list-decimal flex-col gap-2 pl-5 text-[13px] leading-relaxed text-silver marker:text-ash'>
            {CONTRACT_RULES[s.keyMode === 'sequencer' ? 'sequencer' : 'dkg'].map((r) => (
              <li key={r}>{r}</li>
            ))}
          </ol>
        </Panel>
      </div>
    </div>
  )
}

function TallyPanel({ view }: { view: ProcessView }) {
  const s = view.process.state!
  const results = view.process.results!
  const metadata = useJsonDocument(fetchableUri(s.metadataURI))
  const labels = metadataChoices(metadata.data, s.ballotMode.numFields)
  const rows = tallyRows(results.values, labels)
  const total = results.values.reduce((a, v) => a + v, 0n)
  const mode = describeBallotMode(s.ballotMode)
  return (
    <Panel
      title='Tally'
      label='Final result'
      description={`${mode.summary} Each total is the sum of the values voters gave that field, over every voter’s latest ballot.`}
      actions={
        <span className='font-mono text-[12px] text-ash tnum'>
          {formatNumber(view.row.votersCount)} voters · sum {formatNumber(total)}
        </span>
      }
    >
      <ul className='flex flex-col gap-3' data-testid='tally'>
        {rows.map((r) => (
          <li key={r.field} className='grid grid-cols-[minmax(0,1fr)_auto] items-center gap-x-4 gap-y-1'>
            <span className='truncate text-[13px] text-silver'>
              {r.label}
              {labels ? <span className='ml-2 font-mono text-[11px] text-ash'>field {r.field + 1}</span> : null}
            </span>
            <span className='font-mono text-[13px] text-ghost tnum'>
              {formatNumber(r.value)}
              <span className='ml-2 inline-block w-14 text-right text-ash'>{(r.share * 100).toFixed(1)}%</span>
            </span>
            <div className='col-span-2 h-2 overflow-hidden rounded-pill bg-onyx' aria-hidden='true'>
              <div className='h-full rounded-pill bg-emerald' style={{ width: `${r.ofMax * 100}%` }} />
            </div>
          </li>
        ))}
      </ul>
      <p className='mt-4 text-xs text-ash'>
        Published <Timestamp value={results.timestamp} /> in block {formatNumber(results.block)}
        {labels ? '. Option names come from the organizer’s metadata, which the chain does not check.' : '.'}
      </p>
    </Panel>
  )
}

const NEXT: Record<KeyModeName, string> = {
  sequencer:
    'After the end, the holder of the election key (normally the sequencer node that issued it) decrypts the accumulator, proves the tally with the zkVM results program and calls setProcessResults. Only the key holder can.',
  'dkg-automatic':
    'After the end, anyone can send the accumulator to the committee with requestResultsDecryption; sequencers do it on their first heartbeat after the end. A threshold of members then post partial decryptions and a combine per field, and anyone can call finalizeResultsFromDKG to store the tally.',
  'dkg-locked':
    'After the end, anyone can send the accumulator to the committee with requestResultsDecryption; sequencers do it on their first heartbeat after the end. The committee can decrypt only after the organizer reveals its secret (revealProcessKey); then anyone can call finalizeResultsFromDKG.',
}

function NoResultsPanel({ view }: { view: ProcessView }) {
  const s = view.process.state!
  const phase = view.row.phase
  const request = view.process.decryptionRequest
  let title = 'No results yet'
  let body: string = NEXT[s.keyMode]
  if (phase === 'canceled') {
    title = 'Canceled: no results'
    body =
      'The organizer canceled this process. The registry refuses results for a canceled process, so none will appear.'
  } else if (request) {
    title = 'Decryption requested'
    body =
      s.keyMode === 'dkg-locked'
        ? 'The accumulator went to the committee. Its members post partial decryptions once the organizer reveals its secret; after every field is combined, anyone can finalize the result.'
        : 'The accumulator went to the committee. Once a threshold of members have posted partial decryptions and each field is combined, anyone can finalize the result.'
  } else if (phase === 'ended' || phase === 'closed') {
    title = 'Voting is over; results pending'
  }
  return (
    <div data-testid='no-results'>
      <Callout tone={phase === 'canceled' ? 'warn' : 'info'} title={title}>
        <p>{body}</p>
        {phase !== 'canceled' && !request && phase !== 'ended' && phase !== 'closed' ? (
          <p className='mt-1'>
            The process {view.row.endTime ? 'ends' : 'has no end time yet'}{' '}
            {view.row.endTime ? <Timestamp value={view.row.endTime} /> : null}
            {phase === 'paused' ? ', and it is paused now' : ''}.
          </p>
        ) : null}
      </Callout>
    </div>
  )
}

function SequencerProofPanel({ view }: { view: ProcessView }) {
  const store = useStore()
  const source = useDataSource()
  const s = view.process.state!
  const results = view.process.results
  const tx = results?.tx ? store.txDetails[txKey(results.tx)] : undefined
  useEffect(() => {
    if (results?.tx && !tx) source.ensureTxDetails([results.tx])
  }, [source, results?.tx, tx])

  const decoded = useMemo((): { publics: ResultsPublics | null; error: string | null } => {
    if (!tx?.publicValues) return { publics: null, error: tx?.decodeError ?? null }
    try {
      return { publics: decodeResultsPublicValues(tx.publicValues), error: null }
    } catch (err) {
      return { publics: null, error: err instanceof Error ? err.message : String(err) }
    }
  }, [tx])

  if (!results) {
    return (
      <Panel title='How the result is produced' label='zkVM results proof'>
        <p className='text-[13px] leading-relaxed text-silver'>{RESULTS_PROGRAM}</p>
        <p className='mt-2 text-[13px] leading-relaxed text-ash'>
          The proof is wrapped as a PLONK and checked by the same verifier as the batches, under the registry’s results
          program vk.
        </p>
      </Panel>
    )
  }

  const pub = decoded.publics
  const lastRoot = view.transitions[view.transitions.length - 1]?.rootAfter ?? view.process.genesisRoot
  const nf = s.ballotMode.numFields
  const checks: Check[] = [
    {
      label: 'The results program passed every check',
      state: pub ? (pub.ok && pub.failMask === 0 ? 'pass' : 'fail') : 'unknown',
      detail: pub
        ? `ok = ${pub.ok ? 1 : 0}, fail mask = ${pub.failMask}${pub.failMask ? ` (${resultsFailBits(pub.failMask).join(', ')})` : ''}`
        : 'Waiting for the transaction’s calldata',
    },
    {
      label: 'Proven against the final state root',
      state:
        pub && lastRoot
          ? pub.stateRoot === lastRoot && pub.stateRoot === s.latestStateRoot
            ? 'pass'
            : 'fail'
          : 'unknown',
      detail: pub ? (
        <span className='inline-flex flex-wrap items-center gap-1'>
          public values register 2..9 <Hash value={pub.stateRoot} chars={6} /> against the last transition’s root
        </span>
      ) : (
        'public values register 2..9 against the last transition’s root'
      ),
    },
    {
      label: 'The stored tally is the proven one',
      state: pub
        ? results.values.length === nf && results.values.every((v, i) => pub.results[i] === v)
          ? 'pass'
          : 'fail'
        : 'unknown',
      detail: `registers 10..${9 + 2 * nf}, one 64-bit value per field, against the ProcessResultsSet event`,
    },
    {
      label: 'The PLONK verified on-chain',
      state: 'pass',
      detail:
        'The registry emits ProcessResultsSet only after the verifier accepted the proof under the results program vk.',
    },
  ]

  return (
    <Panel title='How the result was produced' label='zkVM results proof' description={RESULTS_PROGRAM}>
      <KeyValue
        items={[
          { label: 'Transaction', value: results.tx ? <TxLink hash={results.tx} /> : '—' },
          {
            label: (
              <span className='inline-flex items-center gap-1'>
                Sender
                <Explain>Anyone may submit a results proof; in practice the node holding the key does.</Explain>
              </span>
            ),
            value: <Address value={results.sender} />,
          },
          { label: 'Block', value: <BlockCell block={results.block} /> },
          { label: 'Time', value: <Timestamp value={results.timestamp} /> },
        ]}
      />
      {decoded.error ? (
        <Callout tone='warn' className='mt-3'>
          The calldata could not be decoded: {decoded.error}
        </Callout>
      ) : null}
      <div className='mt-3'>
        <CheckList checks={checks} />
      </div>
    </Panel>
  )
}

function DkgDecryptionPanel({ view }: { view: ProcessView }) {
  const s = view.process.state!
  const dkg = useDkgApplication(view.process.id)
  const request = view.process.decryptionRequest
  const results = view.process.results
  const app = dkg.data
  const locked = s.keyMode === 'dkg-locked'
  const completed = app?.ciphertexts.filter((c) => c.completed).length ?? 0
  const submitted = s.dkg?.count ?? 0

  const checks: Check[] = request
    ? [
        {
          label: 'The accumulator is the one in the final state root',
          state: 'pass',
          detail:
            'ResultsDecryptionRequested is emitted only after the registry verified the accumulator’s inclusion as leaf 0x04.',
        },
        ...(locked
          ? [
              {
                label: 'The organizer revealed its secret',
                state: (app ? (app.revealed ? 'pass' : 'unknown') : 'unknown') as CheckState,
                detail: app?.revealed
                  ? 'The DKG checked sk·G = PK_org when it accepted the reveal.'
                  : 'Until the reveal the DKG refuses every partial decryption and combine.',
              },
            ]
          : []),
        {
          label: 'Every submitted ciphertext is combined',
          state: (app ? (completed === submitted ? 'pass' : 'unknown') : 'unknown') as CheckState,
          detail: app
            ? `${completed} of ${submitted} combined on the DKG`
            : dkg.isLoading
              ? 'Reading the DKG contracts…'
              : 'The DKG state could not be read',
        },
        ...(results && app && app.ciphertexts.length > 0
          ? [
              {
                label: 'The stored tally is the committee’s plaintexts',
                state: (app.ciphertexts.every((c) => c.completed && results.values[c.field] === c.plaintext)
                  ? 'pass'
                  : 'fail') as CheckState,
                detail: 'Each combined plaintext against its field in ProcessResultsSet.',
              },
            ]
          : []),
      ]
    : []

  return (
    <Panel
      title={results ? 'How the result was produced' : 'How the result will be produced'}
      label='DKG threshold decryption'
      description='The committee decrypts only the final accumulator, one ciphertext per field, and never reconstructs the election secret. There is no results PLONK in the DKG modes: the committee’s Groth16 proofs and the registry’s inclusion check replace it.'
    >
      {request ? (
        <KeyValue
          items={[
            {
              label: 'Decryption request',
              value: (
                <span className='inline-flex flex-wrap items-center justify-end gap-2'>
                  {request.tx ? <TxLink hash={request.tx} chars={4} /> : null}
                  <Timestamp value={request.timestamp} className='text-ash' />
                </span>
              ),
              hint: `block ${formatNumber(request.block)}`,
            },
            {
              label: 'Ciphertexts',
              value: `${formatNumber(request.count)} from DKG index ${formatNumber(request.firstIndex)}`,
              mono: true,
            },
            { label: 'Epoch', value: <Hash value={request.epochId} chars={8} /> },
            { label: 'aid', value: <Hash value={request.aid} chars={8} /> },
            {
              label: 'Finalized',
              value: results?.tx ? (
                <span className='inline-flex flex-wrap items-center justify-end gap-2'>
                  <TxLink hash={results.tx} chars={4} />
                  <Timestamp value={results.timestamp} className='text-ash' />
                </span>
              ) : (
                <Badge tone='warn'>not yet</Badge>
              ),
            },
          ]}
        />
      ) : (
        <p className='text-[13px] leading-relaxed text-ash'>{NEXT[s.keyMode]}</p>
      )}
      {checks.length > 0 ? (
        <div className='mt-3'>
          <CheckList checks={checks} />
        </div>
      ) : null}
      <p className='mt-3 text-xs text-ash'>
        The per-field decryption state is on the{' '}
        <Link to={paths.process(view.process.id, 'key')} className='text-emerald hover:underline'>
          Encryption key
        </Link>{' '}
        tab.
      </p>
    </Panel>
  )
}
