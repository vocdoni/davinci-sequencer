import { paths } from '~routes/paths'
import type { LearnExamples } from '../examples'
import { A, C, P, Section, SeeIt, Step, Steps, Term, UL } from '../prose'

export function VerifyAuditor({ ex }: { ex: LearnExamples }) {
  const t = ex.active
  return (
    <>
      <Section title='What you can check'>
        <P>
          That the contracts are the published ones and are pinned to the released guests, that every transition is a
          proven step from the previous state, that the data behind each one is public, and that the results are the
          decryption of the final tally. None of it needs a sequencer while the beacon still serves the blobs: the
          registry, the settlement transactions and their blobs are enough.
        </P>
      </Section>

      <Section title='Step by step'>
        <Steps>
          <Step n={1} title='Check the deployment'>
            <P>
              The contracts page lists every address with its verified source, reads back how the contracts point at
              each other, and compares the registry’s pins (both <Term id='program-vk'>program vks</Term>, the ZisK
              setup root, the verifier code hash and the ballot VK hash) with the known davinci-zkvm releases.
            </P>
            <SeeIt to={`${paths.contracts()}#release`}>The pins against the known releases</SeeIt>
          </Step>
          <Step n={2} title='Do it again without the explorer'>
            <P>
              Run davinci-contracts’ <C>script/verify_deployment.py</C>, which compares the deployed code with a local
              build and reads back every pin, and rebuild the pins from the davinci-zkvm source. The contracts page
              fills in the command for this deployment.
            </P>
            <SeeIt to={`${paths.contracts()}#verify`}>The commands, filled in</SeeIt>
          </Step>
          <Step n={3} title='Check every transition'>
            <P>
              A transition page recomputes, from public data, what <C>submitStateTransition</C> enforced: the guest’s
              verdict and fail mask, root continuity, that the proven new root is the event’s, the census root,{' '}
              <C>occupied_before</C> against the previous voter count, the vote counts, the blob count, each commitment
              against the transaction’s versioned hash, and the blob digest. The census root is not recomputed for an
              on-chain census (origin 3), and for an updated origin-2 census it is only checked against the roots the
              explorer has seen. The proof and the blob openings themselves were verified on chain, against the pins you
              checked in step 1.
            </P>
            {t ? <SeeIt to={paths.transition(t.id, t.transitions - 1)}>The checks of a recent transition</SeeIt> : null}
          </Step>
          <Step n={4} title='Follow each process’s root chain'>
            <P>
              A process’s transitions tab chains the roots from the genesis root the registry computed at creation,
              through every transition, to the registry’s latest root. Every link must hold; a gap would be highlighted.
            </P>
            {t ? (
              <SeeIt to={paths.process(t.id, 'transitions')}>The root chain of an active process</SeeIt>
            ) : (
              <SeeIt to={paths.processes()}>All processes</SeeIt>
            )}
          </Step>
          <Step n={5} title='Rebuild the state from the blobs'>
            <P>
              The transition page decodes each blob: the vote ids, the slot updates and the new encrypted tally. To
              replay whole processes, run a davinci-sequencer node without a key, an <Term id='observer'>observer</Term>
              : it follows every process, replays every transition from its blobs, and accepts one only if the replay
              gives the event’s new root. That checks that what was tallied is what was recorded, independently of the
              sequencers that settled it.
            </P>
          </Step>
          <Step n={6} title='Check the results'>
            <P>
              With a sequencer key, the results transaction carries a proof of the results guest, verified against{' '}
              <C>resultsProgramVK</C> and the process’s final root. With a DKG key, the decryption request carries the
              accumulator’s inclusion proof under the final root, and the committee’s partial decryptions and combines
              each carry a Groth16 proof on the DKG contracts.
            </P>
            {ex.withResults ? (
              <SeeIt to={paths.process(ex.withResults.id, 'results')}>How a tally was produced</SeeIt>
            ) : null}
          </Step>
          <Step n={7} title='Check the committee'>
            <P>
              For DKG-mode processes, the contracts page shows the committee’s newest epoch, its members and threshold,
              whether application registration is open, and the key hashes of the four Groth16 verifiers.
            </P>
            <SeeIt to={`${paths.contracts()}#dkg`}>The DKG committee</SeeIt>
          </Step>
        </Steps>
      </Section>

      <Section title='What the explorer does not do for you'>
        <UL>
          <li>
            It does not re-verify the PLONK proofs or the KZG openings in the browser; the registry did, on chain.
          </li>
          <li>
            It does not recompute KZG commitments from blob bytes. A blob from the beacon is tied to its transaction by
            its commitment’s versioned hash; one from a sequencer’s archive only by position, and the page says so.
          </li>
          <li>
            Its release table is the one it was built with. A pin it does not recognise may be a newer release; check it
            against the davinci-zkvm source.
          </li>
        </UL>
        <P>
          The background: <A to={paths.learn('settlement')}>what the registry checks</A>,{' '}
          <A to={paths.learn('blobs')}>the blobs</A> and <A to={paths.learn('key-modes')}>the key modes</A>.
        </P>
      </Section>
    </>
  )
}
