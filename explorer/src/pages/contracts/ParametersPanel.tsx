import { useState, type ReactNode } from 'react'
import { useNetworkStats } from '~data/hooks'
import type { DeploymentDetails } from '~data/deployment'
import type { ChainMeta } from '~indexer/types'
import { Address, Hash, Panel, Skeleton, Toggle } from '~kit'
import { formatNumber } from '~lib/format'
import { PIN_LABELS } from '~protocol/releases'
import { PIN_DETAILS } from './model'
import { Code } from './parts'

interface Param {
  id: string
  title: string
  source: string
  value: ReactNode
  what: ReactNode
  why: ReactNode
  hint?: ReactNode
}

export function ParametersPanel({ chain, details }: { chain: ChainMeta; details: DeploymentDetails | undefined }) {
  const [full, setFull] = useState(false)
  const stats = useNetworkStats()
  const r = chain.registry
  const hash = (v: string | null | undefined) =>
    v ? <Hash value={v} chars={10} full={full} /> : <Skeleton className='h-4 w-40' />
  const loading = <Skeleton className='h-4 w-24' />

  const pin = (id: 'batchProgramVK' | 'resultsProgramVK' | 'rootCVadcopFinal' | 'ballotVKHash'): Param => ({
    id,
    title: PIN_LABELS[id],
    source: PIN_DETAILS[id].source,
    value: hash(r?.[id]),
    what: PIN_DETAILS[id].what,
    why: PIN_DETAILS[id].why,
  })

  const params: Param[] = [
    pin('batchProgramVK'),
    pin('resultsProgramVK'),
    pin('rootCVadcopFinal'),
    pin('ballotVKHash'),
    {
      id: 'ziskVerifier',
      title: 'Proof verifier',
      source: 'registry ziskVerifier()',
      value: r ? <Address value={r.ziskVerifier} chars={6} /> : loading,
      what: 'The ZisK PLONK verifier contract the registry calls. It is fixed at deployment.',
      why: 'Every transition and every sequencer-key tally is accepted or refused by this contract, so its code has to be the released one (next row).',
    },
    {
      id: 'ziskVerifierCodeHash',
      title: PIN_LABELS.ziskVerifierCodeHash,
      source: PIN_DETAILS.ziskVerifierCodeHash.source,
      value: r ? hash(r.ziskVerifierCodeHash) : loading,
      what: PIN_DETAILS.ziskVerifierCodeHash.what,
      why: PIN_DETAILS.ziskVerifierCodeHash.why,
    },
    {
      id: 'verifierRootC',
      title: 'Verifier’s own setup root',
      source: 'verifier getRootCVadcopFinal()',
      value: details ? hash(details.verifierRootC) : loading,
      what: 'The setup root compiled into the verifier contract.',
      why: 'A sequencer refuses to start unless the verifier answers with the pinned root, so the registry’s copy and the verifier’s must agree.',
    },
    {
      id: 'chainID',
      title: 'Chain id',
      source: 'registry chainID()',
      value: r ? <span className='font-mono tnum text-ghost'>{r.chainID}</span> : loading,
      what: 'The chain the registry was deployed for, a constructor argument.',
      why: 'It is folded into every process id through the prefix below, and sequencers refuse to start unless it equals the chain’s own id.',
    },
    {
      id: 'pidPrefix',
      title: 'Process id prefix',
      source: 'registry pidPrefix()',
      value: r ? (
        <span className='font-mono tnum text-ghost'>0x{r.pidPrefix.toString(16).padStart(8, '0')}</span>
      ) : (
        loading
      ),
      what: (
        <>
          The low 4 bytes of <Code>keccak256(chainID ‖ registry)</Code>.
        </>
      ),
      why: (
        <>
          Every process id carries it in bytes 20 to 23, after the organizer address, so an id from another registry or
          chain reverts with <Code>UnknownProcessIdPrefix</Code>.
        </>
      ),
    },
    {
      id: 'processCount',
      title: 'Processes created',
      source: 'registry processCount()',
      value: r ? <span className='font-mono tnum text-ghost'>{formatNumber(r.processCount)}</span> : loading,
      hint: r ? `${formatNumber(stats.processes)} indexed by this explorer` : null,
      what: 'How many processes this registry has created.',
      why: 'The explorer’s own index should reach the same number once its scan has caught up.',
    },
    {
      id: 'dkgAdapter',
      title: 'DKG adapter',
      source: 'registry dkgAdapter()',
      value: r ? (
        r.dkgAdapter ? (
          <Address value={r.dkgAdapter} chars={6} />
        ) : (
          <span className='text-[13px] text-ash'>none</span>
        )
      ) : (
        loading
      ),
      what: 'The registry’s link to davinci-dkg, created by its constructor when a DKG manager was given.',
      why: (
        <>
          Zero means the DKG key modes are disabled and <Code>newProcess</Code> in a DKG mode reverts{' '}
          <Code>DKGDisabled</Code>. Otherwise it is the only address allowed to submit ciphertexts to the committee for
          these processes.
        </>
      ),
    },
  ]

  return (
    <Panel
      title='Pinned values'
      label='Registry parameters'
      description='What the registry was deployed with. None of these can change: a new guest or a new ZisK setup needs a new registry.'
      actions={<Toggle checked={full} onChange={setFull} label='Full values' />}
    >
      <ul className='-my-3 divide-y divide-charcoal'>
        {params.map((p) => (
          <li
            key={p.id}
            data-testid={`param-${p.id}`}
            className='grid gap-x-8 gap-y-2 py-4 lg:grid-cols-[minmax(0,1fr)_minmax(0,1fr)]'
          >
            <div className='min-w-0'>
              <div className='text-[13px] font-semibold text-ghost'>{p.title}</div>
              <div className='mt-0.5 font-mono text-[11px] text-ash'>{p.source}</div>
              <div className='mt-2 min-w-0'>{p.value}</div>
              {p.hint ? <div className='mt-1 text-[11px] text-ash'>{p.hint}</div> : null}
            </div>
            <div className='min-w-0 text-[12px] leading-relaxed text-ash'>
              <p>
                <span className='text-pewter'>What it is. </span>
                {p.what}
              </p>
              <p className='mt-1'>
                <span className='text-pewter'>Why it matters. </span>
                {p.why}
              </p>
            </div>
          </li>
        ))}
      </ul>
    </Panel>
  )
}
