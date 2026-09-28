import { Link } from 'react-router-dom'
import { CheckMark, Explain, Timestamp } from '~components'
import { useChain, useIndexer, useReleaseCheck } from '~data/hooks'
import { Address, Badge, BlockCell, buttonClasses, Hash, KeyValue, Panel, Skeleton, Tooltip } from '~kit'
import { formatNumber } from '~lib/format'
import { PIN_LABELS, type PinCheck } from '~protocol/releases'
import { paths } from '~routes/paths'

const PIN_HELP: Record<PinCheck['pin'], string> = {
  batchProgramVK:
    'Identifies the vote-batch zkVM program. The registry verifies every state transition against it, so a proof of any other program fails.',
  resultsProgramVK: 'Identifies the results zkVM program that proves a sequencer-key tally.',
  rootCVadcopFinal: 'Root of the ZisK proving setup both proofs are wrapped under.',
  ziskVerifierCodeHash: 'keccak256 of the PLONK verifier contract code the registry calls.',
  ballotVKHash:
    'Hash of the verification key of the voter ballot proofs. It becomes state leaf 0x07 at creation, and the batch program checks the key it uses against it.',
}

/** Chain, head, the deployment's contracts and its pins against the known releases. */
export function NetworkCard() {
  const chain = useChain()
  const release = useReleaseCheck()
  const { loading, lastBlock } = useIndexer()
  const registry = chain.registry

  return (
    <Panel
      title='Network'
      label='Deployment'
      description='Where this explorer reads from, and whether the deployment runs a known release.'
      actions={
        <Link to={paths.contracts()} className={buttonClasses('ghost', 'sm')}>
          Verify the deployment
        </Link>
      }
    >
      <KeyValue
        items={[
          {
            label: 'Chain',
            value: (
              <>
                {chain.networkName} <span className='font-mono text-ash'>· id {chain.chainId}</span>
              </>
            ),
          },
          {
            label: 'Head block',
            value: chain.headBlock ? (
              <span className='inline-flex items-center gap-2'>
                <BlockCell block={chain.headBlock} />
                <Timestamp value={chain.headTimestamp} className='text-ash' />
              </span>
            ) : (
              <Skeleton className='h-3 w-24' />
            ),
            hint: lastBlock ? `events indexed to block ${formatNumber(lastBlock)}` : undefined,
          },
          {
            label: (
              <span className='inline-flex items-center gap-1'>
                Registry
                <Explain>
                  The ProcessRegistry contract: it stores every process and settles every state transition and result
                  after verifying its proof.
                </Explain>
              </span>
            ),
            value: <Address value={chain.registryAddress} />,
          },
          {
            label: (
              <span className='inline-flex items-center gap-1'>
                Verifier
                <Explain>The ZisK PLONK verifier the registry calls for every batch and results proof.</Explain>
              </span>
            ),
            value: registry ? <Address value={registry.ziskVerifier} /> : <Skeleton className='h-3 w-28' />,
          },
          {
            label: (
              <span className='inline-flex items-center gap-1'>
                DKG adapter
                <Explain>
                  The registry's link to the davinci-dkg committee contracts. Without it the two DKG key modes are
                  disabled.
                </Explain>
              </span>
            ),
            value: registry ? (
              registry.dkgAdapter ? (
                <Address value={registry.dkgAdapter} />
              ) : (
                <span className='text-ash'>none: DKG key modes disabled</span>
              )
            ) : (
              <Skeleton className='h-3 w-28' />
            ),
          },
        ]}
      />

      <div className='mt-4 border-t border-charcoal pt-4'>
        <div className='flex flex-wrap items-center justify-between gap-2'>
          <span className='label-caps inline-flex items-center gap-1 text-[11px] text-pewter'>
            Release pins
            <Explain>
              The registry fixes what its proofs must come from: two program keys, the ZisK setup root, the ballot proof
              key and the verifier code. The explorer compares each with the davinci-zkvm releases it knows. A full
              match means this registry accepts proofs from that release's programs only.
            </Explain>
          </span>
          {release.release ? (
            <Badge tone='ok'>matches {release.release.label}</Badge>
          ) : loading || !release.complete ? (
            <Badge>reading…</Badge>
          ) : (
            <Badge tone='danger'>no known release</Badge>
          )}
        </div>
        <ul className='mt-3 flex flex-col gap-2' aria-label='Release pin checks'>
          {release.checks.map((c) => (
            <li key={c.pin} className='flex min-w-0 items-center gap-2 text-[13px]'>
              <CheckMark state={c.ok == null ? 'unknown' : c.ok ? 'pass' : 'fail'} />
              <Tooltip content={PIN_HELP[c.pin]}>
                <span className='min-w-0 flex-1 truncate text-silver'>{PIN_LABELS[c.pin]}</span>
              </Tooltip>
              {c.actual ? <Hash value={c.actual} chars={6} /> : <span className='text-[12px] text-ash'>…</span>}
            </li>
          ))}
        </ul>
        {release.closest && !release.release && release.complete ? (
          <p className='mt-3 text-xs leading-relaxed text-ash'>
            Compared with {release.closest.label}, the closest known release. A mismatch is either a newer release this
            explorer does not list yet or a deployment of other programs; check it on the contracts page.
          </p>
        ) : null}
      </div>
    </Panel>
  )
}
