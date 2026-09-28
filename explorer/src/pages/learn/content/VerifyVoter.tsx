import { paths } from '~routes/paths'
import type { LearnExamples } from '../examples'
import { A, C, P, Section, SeeIt, Step, Steps, Term } from '../prose'

export function VerifyVoter({ ex }: { ex: LearnExamples }) {
  return (
    <>
      <Section title='What you can check'>
        <P>
          That your vote reached the chain in a proven transition, that it is under a state root the registry holds, and
          that it went into the tally that was decrypted.
        </P>
        <P>
          Your ballot stays encrypted on chain. Only the holder of the election key can open it: the sequencer that
          issued it in sequencer mode, or a threshold of the committee in the DKG modes. After re-encryption the stored
          ballot no longer matches the one your client sent, so you cannot prove which ballot your slot holds.
        </P>
      </Section>

      <Section title='Step by step'>
        <Steps>
          <Step n={1} title='Keep your process id and vote id'>
            <P>
              Keep both from your voting app: the <Term id='process-id'>process id</Term> of the election, <C>0x</C> and
              62 hex digits, and the <Term id='vote-id'>vote id</Term> of your ballot, <C>0x</C> and 16 hex digits.
            </P>
          </Step>
          <Step n={2} title='Look it up'>
            <P>
              Open the vote lookup and enter both. If this explorer is configured with a sequencer, it shows the status
              that node reports: pending (queued), aggregated (in a batch being proved), processed (proved, settlement
              pending), settled (on chain), or an error with its reason.
            </P>
            <SeeIt to={paths.votes()}>The vote lookup</SeeIt>
          </Step>
          <Step n={3} title='Find the transition that included it'>
            <P>
              The explorer reads the blobs of the process’s transitions and finds the one that lists your vote id. That
              transition is on chain: the registry verified its proof and checked its blobs against it. A blob from the
              beacon is tied to the transaction by its commitment’s versioned hash. One from a sequencer’s archive is
              tied by position only, not checked against the transaction’s blob hashes, and the page says which.
            </P>
          </Step>
          <Step n={4} title='Check the tracker proof'>
            <P>
              With a sequencer configured, the explorer also fetches your <Term id='tracker-proof'>tracker proof</Term>:
              the path from your vote id’s leaf to a state root. Your browser checks that the path reaches that root and
              that the root is one the registry has held for the process. That shows your vote was recorded as cast.
            </P>
          </Step>
          <Step n={5} title='See it counted'>
            <P>
              When results are in, the process’s Results tab shows the tally. The guest added your ballot to the
              encrypted tally in the transition that included it, and what gets decrypted is the tally under the final
              state root, which grows from that transition. The registry checks that link before it accepts the results.
            </P>
            {ex.withResults ? (
              <SeeIt to={paths.process(ex.withResults.id, 'results')}>The results of a finished process</SeeIt>
            ) : null}
          </Step>
        </Steps>
      </Section>

      <Section title='If you voted more than once'>
        <P>
          Your latest vote replaces the earlier one in your ballot slot, and only it is counted. Your earlier vote ids
          stay in the tree, since vote ids are only ever added. Watching the chain, nobody can tell whether your slot
          was overwritten or only refreshed. Its first write is public, and with a Merkle census so is whose slot it is:
          see <A to={paths.learn('silent-revoting')}>revoting and silent refreshes</A>.
        </P>
      </Section>
    </>
  )
}
