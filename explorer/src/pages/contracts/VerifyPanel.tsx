import { useState } from 'react'
import { CodeBlock } from '~components/CodeBlock'
import type { ChainMeta } from '~indexer/types'
import { Panel } from '~kit'
import type { ReleaseMatch } from '~protocol/releases'
import { castCommands, releasePins, verifyDeploymentCommand } from './model'
import { Code, Segmented, SubHeading } from './parts'

type PinSource = 'chain' | 'release'

export function VerifyPanel({ chain, rpc, match }: { chain: ChainMeta; rpc: string; match: ReleaseMatch }) {
  const release = match.closest
  const [source, setSource] = useState<PinSource>('chain')
  const r = chain.registry
  const pins =
    source === 'release' && release
      ? releasePins(release)
      : {
          batchProgramVK: r?.batchProgramVK,
          resultsProgramVK: r?.resultsProgramVK,
          rootCVadcopFinal: r?.rootCVadcopFinal,
          ballotVKHash: r?.ballotVKHash,
        }
  const chainId = r?.chainID ?? chain.chainId
  const command = verifyDeploymentCommand({ rpc, chainId, registry: chain.registryAddress, pins })
  const setup = [
    'git clone --recurse-submodules -b zkvm https://github.com/vocdoni/davinci-contracts.git',
    'cd davinci-contracts',
    'forge build',
    command,
  ].join('\n')

  return (
    <Panel
      title='Verify it yourself'
      label='Without trusting this page'
      description='Everything above is read in your browser from the RPC the explorer is configured with. These commands run the same checks from a terminal, against a build of the source.'
    >
      <section aria-labelledby='verify-script' data-testid='verify-script'>
        <div className='flex flex-wrap items-center justify-between gap-3'>
          <SubHeading>
            <span id='verify-script'>Compare the deployment with a build</span>
          </SubHeading>
          <Segmented<PinSource>
            label='Pins in the command'
            value={source}
            onChange={setSource}
            options={[
              { value: 'chain', label: 'Pins from the chain' },
              ...(release ? [{ value: 'release' as const, label: `Pins of ${release.label}` }] : []),
            ]}
          />
        </div>
        <p className='mt-2 text-[13px] leading-relaxed text-ash'>
          davinci-contracts ships <Code>script/verify_deployment.py</Code>. It needs Python 3, Foundry’s{' '}
          <Code>cast</Code> and a <Code>forge build</Code> with the repository’s compiler settings, since it reads{' '}
          <Code>out/</Code>. Pass <Code>--cast</Code> when cast is not at <Code>~/.foundry/bin/cast</Code>.
        </p>
        <CodeBlock code={setup} label='Copy the commands' className='mt-3' />
        <p className='mt-2 text-[12px] leading-relaxed text-ash'>
          {source === 'chain'
            ? 'With the pins the registry holds, the pin checks pass by construction and the run is about the code: it proves the contracts at these addresses are the ones in the repository. Switch to the release pins to also check that the registry holds the released keys.'
            : `With the pins of ${release?.label}, the run also checks that the registry holds exactly the released keys.`}
        </p>
        <p className='mt-4 text-[13px] text-silver'>It checks that:</p>
        <ul className='mt-1.5 list-disc space-y-1 pl-5 text-[13px] leading-relaxed text-ash'>
          <li>
            the runtime code of <Code>ProcessRegistry</Code> and <Code>ZiskVerifier</Code> matches the local build, with
            immutables masked;
          </li>
          <li>
            the registry’s <Code>batchProgramVK</Code>, <Code>resultsProgramVK</Code>, <Code>rootCVadcopFinal</Code> and{' '}
            <Code>ballotVKHash</Code> equal the given pins, and so does the verifier’s{' '}
            <Code>getRootCVadcopFinal()</Code>;
          </li>
          <li>
            the registry’s <Code>chainID</Code> equals the RPC’s chain id, and <Code>--chain-id</Code> when given;
          </li>
          <li>
            when <Code>dkgAdapter()</Code> is set, the adapter’s code matches the local build and{' '}
            <Code>adapter.registry()</Code> is the registry.
          </li>
        </ul>
        <p className='mt-2 text-[12px] text-ash'>
          Each check prints OK or FAIL, and the exit status is 1 if any fails.
        </p>
      </section>

      <section className='mt-8' aria-labelledby='verify-cast'>
        <SubHeading>
          <span id='verify-cast'>Read the values one by one</span>
        </SubHeading>
        <p className='mt-2 text-[13px] leading-relaxed text-ash'>
          The same reads this page makes, with <Code>cast</Code>. The last line hashes the verifier’s runtime code and
          should print the verifier code hash shown under Pinned values.
        </p>
        <CodeBlock
          code={castCommands(rpc, chain.registryAddress, r?.ziskVerifier ?? null)}
          label='Copy the cast commands'
          className='mt-3'
        />
      </section>

      <section className='mt-8' aria-labelledby='verify-source'>
        <SubHeading>
          <span id='verify-source'>Rebuild the pins from source</span>
        </SubHeading>
        <dl className='mt-3 flex flex-col gap-4 text-[13px] leading-relaxed'>
          <div>
            <dt className='font-medium text-silver'>The two program keys</dt>
            <dd className='mt-1 text-ash'>
              In davinci-zkvm, <Code>scripts/build-guests.sh</Code> builds the guest programs. Source paths are
              remapped, so any checkout builds the same bytes, and CI rebuilds the committed ELFs the same way to check
              them. <Code>cargo-zisk setup -e &lt;elf&gt; -k &lt;proving-key&gt;</Code> then prints{' '}
              <Code>Root hash: [w0, w1, w2, w3]</Code> for each ELF when <Code>ZISK_CACHE_DIR</Code> is empty (a warm
              cache skips the print); the pin is those four 64-bit words as big-endian bytes, concatenated. This needs
              the ZisK toolchain and its STARK proving key.
              <CodeBlock
                className='mt-2'
                label='Copy the build commands'
                code={[
                  'git clone https://github.com/vocdoni/davinci-zkvm.git && cd davinci-zkvm',
                  'scripts/build-guests.sh',
                  'cargo-zisk setup -e circuit/elf/circuit.elf -k ~/.zisk/provingKey           # batchProgramVK',
                  'cargo-zisk setup -e circuit-results/elf/results.elf -k ~/.zisk/provingKey   # resultsProgramVK',
                ].join('\n')}
              />
            </dd>
          </div>
          <div>
            <dt className='font-medium text-silver'>The setup root</dt>
            <dd className='mt-1 text-ash'>
              <Code>rootCVadcopFinal</Code> belongs to the ZisK snark setup
              {release ? ` (ZisK ${release.zisk} for ${release.label})` : ''}, not to the guests. The verifier contract
              returns it from <Code>getRootCVadcopFinal()</Code>, and every PLONK job of a davinci-zkvm prover reports
              it as <Code>root_c_vadcop_final</Code>.
            </dd>
          </div>
          <div>
            <dt className='font-medium text-silver'>The verifier code hash</dt>
            <dd className='mt-1 text-ash'>
              After <Code>forge build</Code> in davinci-contracts, hash the verifier’s <Code>deployedBytecode</Code>. It
              has no immutables, so the hash equals the one of the deployed code.
              <CodeBlock
                className='mt-2'
                label='Copy the hash command'
                code='cast keccak $(jq -r .deployedBytecode.object out/ZiskVerifier.sol/ZiskVerifier.json)'
              />
            </dd>
          </div>
          <div>
            <dt className='font-medium text-silver'>The ballot key hash</dt>
            <dd className='mt-1 text-ash'>
              <Code>ballotVKHash</Code> is the sha256 of the ballot proof verification key’s wire bytes,{' '}
              <Code>davinci.BallotVKLeaf</Code> in the davinci-zkvm Go SDK. The key the sequencer accepts is the one its
              SDK embeds, <Code>rust-sdk/assets/ballot_proof_vkey.json</Code>, davinci-circom’s current key.
            </dd>
          </div>
        </dl>
      </section>

      <p className='mt-8 border-t border-charcoal pt-4 text-[12px] leading-relaxed text-ash'>
        Three checks refuse a mismatch outside this page: a sequencer’s boot check (it reads the same pins and the
        verifier code hash, and will not start on any difference), <Code>davinci_client::verify_registry</Code> for
        clients, and the script above.
      </p>
    </Panel>
  )
}
