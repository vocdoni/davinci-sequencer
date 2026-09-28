import { paths } from '~routes/paths'
import type { LearnExamples } from '../examples'
import { A, C, P, Section, SeeIt, Term, UL } from '../prose'

export function SilentRevoting({ ex }: { ex: LearnExamples }) {
  const t = ex.active
  return (
    <>
      <Section title='Revoting'>
        <P>
          A voter may vote again while the process is open. The new ballot goes to the same <Term id='slot'>slot</Term>{' '}
          and replaces the old one: the guest subtracts the overwritten ballot from the encrypted tally and adds the new
          one. The registry counts distinct slots written (<C>votersCount</C>) and{' '}
          <Term id='overwrite'>overwrites</Term> (<C>overwrittenVotesCount</C>).
        </P>
        <P>
          Revoting only helps a voter if nobody can tell whose ballot was replaced. Two mechanisms make it deniable:
          re-encryption, and silent refreshes of occupied slots the batch did not write.
        </P>
      </Section>

      <Section title='Re-encryption'>
        <P>
          Before a ballot is stored, the sequencer <Term id='re-encryption'>re-encrypts</Term> it: it adds an encryption
          of zero with a fresh random scalar to every ciphertext. The plaintext does not change. Without the batch seed,
          nobody can match the ciphertext a voter sent to the stored one, so a voter cannot prove which ballot their
          slot holds. It does not hide whose slot it is: the sequencer that sealed the batch knows the seed, and with a
          Merkle census the slot follows from the address.
        </P>
        <P>
          The scalars come from one secret seed per batch, drawn from the operating system’s randomness, through a
          SHA-256 chain that also takes the state root before the batch; every element is used once, so no scalar
          repeats within a transition or across them. The guest recomputes the chain from the seed and verifies every
          re-encryption. The seed exists in the sequencer’s memory for one batch only and is never written or logged.
          The prover deletes its copy of the input after proving only when it runs without <C>DAVINCI_KEEP_INPUTS=1</C>.
          That is an operator setting you have to trust; nothing on chain shows it.
        </P>
      </Section>

      <Section title='Silent refreshes'>
        <P>
          Every batch also re-randomizes occupied slots it did not write: the same re-encryption, applied in place, with
          no change of plaintext. The sequencer picks those slots from the operating system’s randomness, never from
          public data, because a selection anyone could compute would let them subtract the refreshes and spot the
          overwrites.
        </P>
        <P>The guest requires at least this many refreshes per batch:</P>
        <P>
          <C>min(target, occupied_before − overwrites)</C>, with{' '}
          <C>target = min(2048, max(16, 2 · overwrites, votes))</C>
        </P>
        <P>
          The refreshed ballots are written back like any other, and the guest adds their encryptions of zero to the
          encrypted tally as well, so the tally moves with every slot the batch touched.
        </P>
      </Section>

      <Section title='What an observer can and cannot see'>
        <UL>
          <li>
            The blob of each transition lists every slot the batch wrote in one sorted list. An overwrite and a silent
            refresh look the same there, so which occupied slots were overwritten and which were only refreshed stays
            hidden.
          </li>
          <li>
            A slot’s first write is public, because refreshes only touch occupied slots. With a Merkle census (origins 1
            to 3) the slot follows from the voter’s address, so who voted and when is public.
          </li>
          <li>
            How many votes and how many overwrites a batch carried is public: they are in the proof’s public values and
            in the registry’s counters. Which slots were overwritten is not.
          </li>
          <li>
            <C>occupied_before</C>, the number of slots written before the batch, is public too. The guest cannot see
            the tree, so the registry checks it against its own count of voters.
          </li>
          <li>
            A voter can find their own <Term id='vote-id'>vote id</Term> in the blob of the transition that included it.
            Vote ids are only ever inserted, so an earlier vote id stays in the tree after a revote; the tally counts
            only the latest ballot of the slot.
          </li>
        </UL>
        <P>
          One known limit: re-encryption scalars are SHA-256 digests reduced modulo the BN254 prime, which leaves them 2
          <sup>−7.6</sup> away from uniform modulo the subgroup order.
        </P>
      </Section>

      <Section title='Where to see it'>
        {t ? (
          <SeeIt
            to={paths.transition(t.id, t.transitions - 1)}
            hint='Overwrites and refreshes look alike; first writes do not.'
          >
            The slot updates of a recent transition
          </SeeIt>
        ) : null}
        <SeeIt to={paths.votes()}>Look up a vote</SeeIt>
        <P>
          Next: <A to={paths.learn('blobs')}>what the blobs publish</A>.
        </P>
      </Section>
    </>
  )
}
