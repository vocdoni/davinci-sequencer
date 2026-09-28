import { paths } from '~routes/paths'
import type { LearnExamples } from '../examples'
import { A, C, OL, P, Section, SeeIt, SimpleTable, Term, UL } from '../prose'

export function Settlement({ ex }: { ex: LearnExamples }) {
  const t = ex.active
  return (
    <>
      <Section title='The call'>
        <P>
          A sequencer settles a batch with{' '}
          <C>submitStateTransition(processId, publicValues, proofBytes, commitments, ys, kzgProofs)</C>, sent as a blob
          transaction that carries exactly the transition’s blobs, in order. Anyone may send it; the checks below decide
          whether it lands.
        </P>
      </Section>

      <Section title='The checks, in order'>
        <OL>
          <li>The process exists, is Ready and is inside its voting window.</li>
          <li>
            <C>publicValues</C> is 512 bytes, the guest’s <C>ok</C> is 1 and its <C>fail_mask</C> is 0.
          </li>
          <li>
            The state root before the batch equals the process’s <C>latestStateRoot</C>. This is root continuity: a
            batch built on an old root, such as the loser of a race, reverts here.
          </li>
          <li>
            The census root matches. For origins 1, 2 and 4 it equals the stored root; for an on-chain census the census
            contract’s <C>getRootBlockNumber(root)</C> must be non-zero, at most the current block and at least the
            process’s creation block.
          </li>
          <li>
            <C>occupied_before</C> equals <C>votersCount</C>, the number of distinct ballot slots written so far. The
            guest cannot see the tree, so the registry pins it.
          </li>
          <li>
            The batch does not take the process past its maximum number of voters:{' '}
            <C>votersCount + votes − overwrites ≤ maxVoters</C>.
          </li>
          <li>
            There is at least one blob, the three blob arrays have <C>n_blobs</C> entries each, the transaction carries
            no blob past them, and <C>sha256(commitment₀ ‖ y₀ ‖ …)</C> equals the blob digest in the public values.
          </li>
          <li>
            The verifier accepts the PLONK proof:{' '}
            <C>verifySnarkProof(batchProgramVK, rootCVadcopFinal, publicValues, proofBytes)</C>. It hashes the{' '}
            <Term id='program-vk'>program vk</Term>, the public values and the setup root together, so a proof of
            another program or made under another setup fails.
          </li>
          <li>
            Every blob opens to its <C>y</C> at <C>z = sha256(process id ‖ root before ‖ commitment) mod r</C>, checked
            with the point-evaluation precompile against the transaction’s blob hashes.
          </li>
        </OL>
        <P>
          On success <C>latestStateRoot</C> becomes the root after, <C>votersCount</C> grows by{' '}
          <C>votes − overwrites</C>, <C>overwrittenVotesCount</C> by the overwrites and <C>batchNumber</C> by one, and
          the registry emits <C>ProcessStateTransitioned</C>.
        </P>
      </Section>

      <Section title='The public values it reads'>
        <P>
          The <Term id='public-values'>public values</Term> are the guest’s 64 output registers, each written as an
          8-byte little-endian word. A 256-bit value spans 8 registers.
        </P>
        <SimpleTable
          head={['Registers', 'Value']}
          rows={[
            ['0', <C key='ok'>ok</C>],
            ['1', <C key='fm'>fail_mask</C>],
            ['2–9', 'State root before'],
            ['10–17', 'State root after'],
            ['18', 'Votes in the batch'],
            ['19', 'Overwrites among them'],
            ['20–27', 'Census root'],
            ['28–35', 'Blob digest'],
            ['36', <C key='nb'>n_blobs</C>],
            ['42', <C key='ob'>occupied_before</C>],
          ]}
        />
      </Section>

      <Section title='What the guest proves, and what is left to the registry'>
        <P>
          The guest proves a transition from whatever root and census it is given. The registry adds what only the chain
          knows: the process’s last root, its census root, its count of voters, and every blob against its versioned
          hash. Together they make each transition a valid step from the previous one.
        </P>
        <UL>
          <li>
            The explorer recomputes most of these from public data: the guest’s verdict, root continuity, the census
            root, <C>occupied_before</C>, the vote counts, the blob count, the versioned hashes and the blob digest. It
            does not recompute the census root of an on-chain census (origin 3), and for an updated origin-2 census it
            only checks the root against the roots it has seen.
          </li>
          <li>
            The PLONK proof and the KZG openings are verified on chain; the explorer shows the program vk and setup root
            they were checked against.
          </li>
        </UL>
        {t ? <SeeIt to={paths.transition(t.id, t.transitions - 1)}>The checks of a recent transition</SeeIt> : null}
        <SeeIt to={`${paths.contracts()}#parameters`}>The program vk and setup root this registry pins</SeeIt>
        <P>
          Next: <A to={paths.learn('results')}>how results are produced</A>.
        </P>
      </Section>
    </>
  )
}
