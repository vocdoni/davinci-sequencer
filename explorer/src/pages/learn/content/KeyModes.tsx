import { paths } from '~routes/paths'
import type { LearnExamples } from '../examples'
import { A, C, H3, P, Section, SeeIt, SimpleTable, Term, UL } from '../prose'

export function KeyModes({ ex }: { ex: LearnExamples }) {
  return (
    <>
      <Section title='Why the key matters'>
        <P>
          Every ballot is encrypted under the process’s key. Whoever can use the matching secret could open any ballot
          published in the blobs, and is the one who can publish the tally. The key mode, chosen in <C>newProcess</C>,
          decides who that is. The organizer only ever talks to the registry; in the DKG modes the committee sits behind
          it. The zkVM guests are the same in every mode.
        </P>
        <SimpleTable
          head={['Mode', 'Who holds the secret', 'How the tally is published']}
          rows={[
            [
              'Sequencer',
              'One sequencer node, the one that handed out the key.',
              'That node decrypts and proves it with the results guest.',
            ],
            [
              'DKG automatic',
              'Nobody holds it whole: each member of a davinci-dkg committee holds one share.',
              'A threshold of the committee decrypts, once the process has ended.',
            ],
            [
              'DKG locked',
              'The committee, plus an organizer secret.',
              'The committee decrypts only after the organizer reveals its secret.',
            ],
          ]}
        />
      </Section>

      <Section title='Sequencer key'>
        <P>
          The organizer asks a sequencer for the election key of the process id it is about to create (
          <C>POST /processes/keys</C>). The node derives the secret from its own master secret and the process id, so it
          stores nothing per process and a key handed out for one id is useless under any other.
        </P>
        <P>
          That node is the only one that can publish the results, through a results proof. It also holds a key that
          opens every ballot in the blobs, so this mode trusts one node with ballot secrecy. It suits testing, and users
          who accept that trust.
        </P>
      </Section>

      <Section title='DKG automatic'>
        <P>
          The process key is one of the <Term id='pool-key'>pool keys</Term> of a davinci-dkg{' '}
          <Term id='epoch'>epoch</Term>. The registry’s adapter registers a DKG application for the process on the
          newest Live epoch with a free pool key, and the key of the process is that pool key. No sequencer and no
          organizer holds the secret; each committee member holds one share.
        </P>
        <P>
          After the end, anyone can ask for the decryption; sequencers do it on their first heartbeat after the end. The
          registry checks the encrypted tally against the final state root and hands one ciphertext per ballot field to
          the committee; a <Term id='threshold'>threshold</Term> of its members decrypt them, each step with a Groth16
          proof.
        </P>
      </Section>

      <Section title='DKG locked'>
        <P>
          The key is the pool key plus an organizer key, <C>P_j + PK_org</C>. The organizer’s secret <C>sk_org</C> is
          returned to the organizer at creation and never stored by the registry. The committee’s partial decryptions do
          not begin until the organizer calls <C>revealProcessKey</C> with it, which lets the organizer decide when the
          tally appears, but not which one.
        </P>
        <UL>
          <li>Losing the organizer secret loses the results.</li>
          <li>
            A reveal works at any time. An organizer that reveals during voting drops the process to the automatic trust
            model.
          </li>
          <li>
            The organizer’s proof of possession binds the epoch, so a locked process names its epoch; clients read it
            from the adapter’s <C>registrationEpoch()</C>.
          </li>
        </UL>
      </Section>

      <Section title='What the committee never does'>
        <P>
          It never reconstructs the election secret, which would open every ballot in the blobs. It threshold-decrypts
          only the final accumulator, one ciphertext per ballot field. A field whose ciphertext is the identity (a
          process that never tallied a ballot) decrypts to 0 under any key, so the registry records 0 for it without
          asking the committee.
        </P>
      </Section>

      <Section title='Trust and accepted risks'>
        <H3>Sequencer key</H3>
        <UL>
          <li>The key-holding node can decrypt every ballot and is the only party able to publish the results.</li>
        </UL>
        <H3>DKG keys</H3>
        <UL>
          <li>
            A threshold of an epoch’s committee can decrypt every ballot of the processes keyed on that epoch (with the
            organizer secret as well, in locked mode). The design trusts the threshold not to collude.
          </li>
          <li>
            A process’s key belongs to one epoch’s committee and there is no resharing. If more than n − t of its
            members leave before the process ends, its results are lost.
          </li>
          <li>
            The committee’s search for a decrypted value stops at 2<sup>50</sup> per field. DAVINCI caps results well
            below that.
          </li>
          <li>
            Anyone can register applications on the DKG, and each takes a pool key, so sixteen cheap calls spend an
            epoch. The nodes then create the next epoch at once, and DKG-mode process creation pauses for one epoch
            setup.
          </li>
          <li>
            Between the end and the first decryption request, the organizer can still cancel a Ready or Paused process
            without having seen the tally, the same power it has with a sequencer key. The request itself moves the
            process to Ended, which only the registry can leave, because the decrypted values become public on the DKG
            before they reach the registry.
          </li>
        </UL>
      </Section>

      <Section title='Where to see it'>
        <P>
          A process’s <strong className='font-medium text-silver'>Encryption key</strong> tab shows its mode, its key
          and, in the DKG modes, the epoch, the application id, the pool key and whether the organizer secret was
          revealed. The contracts page shows the committee itself.
        </P>
        {ex.dkg ? (
          <SeeIt to={paths.process(ex.dkg.id, 'key')}>The key of a DKG-mode process</SeeIt>
        ) : ex.newest ? (
          <SeeIt to={paths.process(ex.newest.id, 'key')}>The key of the newest process</SeeIt>
        ) : null}
        <SeeIt to={`${paths.contracts()}#dkg`}>The DKG committee behind this registry</SeeIt>
        <P>
          Next: <A to={paths.learn('results')}>how results are produced</A>.
        </P>
      </Section>
    </>
  )
}
