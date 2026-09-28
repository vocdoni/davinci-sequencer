import { Link } from 'react-router-dom'
import { Explain } from '~components'
import { CodeBlock, Disclosure } from '~components/code'
import { useChain, useReleaseCheck } from '~data/hooks'
import type { TransitionDetail } from '~indexer/selectors'
import { Address, Badge, Hash, KeyValue, Panel, Skeleton, Tooltip } from '~kit'
import { formatBytes } from '~lib/format'
import { paths } from '~routes/paths'

const byteLength = (hex: string | null | undefined) => (hex ? (hex.length - 2) / 2 : null)

/** The PLONK proof, the keys it was checked against and the verifier call. */
export function ProofPanel({ detail }: { detail: TransitionDetail }) {
  const { tx } = detail
  const chain = useChain()
  const registry = chain.registry
  const release = useReleaseCheck()
  const pin = (name: 'batchProgramVK' | 'rootCVadcopFinal') => release.checks.find((c) => c.pin === name)
  const pending = <Skeleton className='inline-block h-3 w-24' />

  const pinBadge = (name: 'batchProgramVK' | 'rootCVadcopFinal') => {
    const c = pin(name)
    if (!c || c.ok == null) return null
    const known = release.release ?? release.closest
    return c.ok ? (
      <Tooltip
        content={`Equal to the pin of ${known?.label ?? 'a known release'}. The contracts page compares every pin.`}
      >
        <span className='inline-flex'>
          <Badge tone='ok' size='sm'>
            known release
          </Badge>
        </span>
      </Tooltip>
    ) : (
      <Tooltip content='Not the pin of any release this explorer knows. The contracts page has the details.'>
        <span className='inline-flex'>
          <Badge tone='warn' size='sm'>
            unknown release
          </Badge>
        </span>
      </Tooltip>
    )
  }

  return (
    <Panel
      label='Validity'
      title='The proof'
      description='One ZisK PLONK proof covers the whole batch: every ballot proof, signature, census proof, state-tree update, re-encryption and the blob layout. Its size does not depend on the number of votes.'
    >
      <div className='flex flex-col gap-4' data-testid='proof'>
        <KeyValue
          columns={2}
          items={[
            {
              label: (
                <span className='inline-flex items-center gap-1'>
                  Proof size
                  <Explain>proofBytes: the PLONK proof, ABI-encoded as 24 words.</Explain>
                </span>
              ),
              value: tx ? formatBytes(byteLength(tx.proofBytes)) : pending,
              mono: true,
            },
            {
              label: (
                <span className='inline-flex items-center gap-1'>
                  Public values
                  <Explain>publicValues: the 64 registers above, 8 bytes each.</Explain>
                </span>
              ),
              value: tx ? formatBytes(byteLength(tx.publicValues)) : pending,
              mono: true,
            },
            {
              label: (
                <span className='inline-flex items-center gap-1'>
                  Program vk
                  <Explain>
                    batchProgramVK, a registry immutable. It identifies the exact vote-batch guest program; a change to
                    the guest changes it, and that takes a new registry.
                  </Explain>
                </span>
              ),
              value: registry ? (
                <span className='inline-flex items-center gap-2'>
                  <Hash value={registry.batchProgramVK} chars={10} />
                  {pinBadge('batchProgramVK')}
                </span>
              ) : (
                pending
              ),
            },
            {
              label: (
                <span className='inline-flex items-center gap-1'>
                  Setup root
                  <Explain>
                    rootCVadcopFinal: the ZisK setup root, a registry immutable. It moves only with the ZisK proving
                    setup, not with the guest.
                  </Explain>
                </span>
              ),
              value: registry ? (
                <span className='inline-flex items-center gap-2'>
                  <Hash value={registry.rootCVadcopFinal} chars={10} />
                  {pinBadge('rootCVadcopFinal')}
                </span>
              ) : (
                pending
              ),
            },
            {
              label: (
                <span className='inline-flex items-center gap-1'>
                  Verifier
                  <Explain>The ZiskVerifier contract the registry calls; its address is a registry immutable.</Explain>
                </span>
              ),
              value: registry ? <Address value={registry.ziskVerifier} /> : pending,
            },
            {
              label: 'Pins',
              value: (
                <Link to={paths.contracts()} className='text-silver hover:text-emerald'>
                  Compare with the known releases
                </Link>
              ),
            },
          ]}
        />
        <div className='flex flex-col gap-2'>
          <p className='text-[13px] leading-relaxed text-ash'>
            The registry makes this call inside submitStateTransition and reverts with InvalidProof if it fails. The
            verifier's public input is sha256(programVK ‖ publicValues ‖ rootCVadcopFinal), so a proof of another
            program or another setup does not verify, and neither does a proof whose public values were changed.
          </p>
          <CodeBlock
            code={`ZiskVerifier(${registry?.ziskVerifier ?? '<verifier>'}).verifySnarkProof(\n  batchProgramVK,    // ${registry?.batchProgramVK ?? '…'}\n  rootCVadcopFinal,  // ${registry?.rootCVadcopFinal ?? '…'}\n  publicValues,      // ${formatBytes(byteLength(tx?.publicValues))} from the calldata\n  proofBytes         // ${formatBytes(byteLength(tx?.proofBytes))} from the calldata\n)`}
            label='Copy the verifier call'
          />
        </div>
        {tx?.proofBytes ? (
          <Disclosure summary='proofBytes as sent'>
            <CodeBlock code={tx.proofBytes} wrap label='Copy proofBytes' maxHeight={240} />
          </Disclosure>
        ) : null}
      </div>
    </Panel>
  )
}
