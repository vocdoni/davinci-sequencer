import type { ReactNode } from 'react'
import { Link } from 'react-router-dom'
import { Panel } from '~kit'
import { paths } from '~routes/paths'

function Block({ title, children }: { title: string; children: ReactNode }) {
  return (
    <section className='flex flex-col gap-2'>
      <h3 className='text-[14px] font-semibold text-ghost'>{title}</h3>
      <div className='flex flex-col gap-2 text-[13px] leading-relaxed text-ash'>{children}</div>
    </section>
  )
}

/** What a vote check shows and what it cannot, re-encryption and silent revoting, in plain words. */
export function VoteExplainers({ pid }: { pid: string | null }) {
  return (
    <Panel label='Reading the answer' title='What a vote check tells you' className='min-w-0'>
      <div className='grid gap-6 md:grid-cols-2' data-testid='vote-explainers'>
        <Block title='What a found vote id proves'>
          <p>
            A ballot with this vote id passed every check of the zkVM guest: its ballot proof, its signature and the
            voter's census membership. Its batch inserted the vote id into the process's state tree, wrote the ballot
            into the voter's slot and added it to the encrypted tally, and the registry verified the proof of all of
            that before the batch settled.
          </p>
          <p>
            The blob that lists the vote id is tied to the transaction by its versioned hash when it comes from the
            beacon. One from a sequencer's archive is tied by position only, not checked against the transaction's blob
            hashes; the inclusion panel says which.
          </p>
          <p>
            The vote id itself comes from the app you voted with: 2^63 plus the low 63 bits of Poseidon(process id,
            address, k), where k is the ballot's secret randomness. A new ballot gets a new vote id.
          </p>
        </Block>
        <Block title='What it does not prove'>
          <p>
            What you voted. The ballot is encrypted, and the blob keeps vote ids and slot updates in two lists. The
            lists are sorted separately. In a small batch the new vote ids and the new slots can still be matched.
          </p>
          <p>
            That this ballot is the one that counts in the end. If the same voter votes again, the new ballot takes the
            slot and the tally drops the old one. The old vote id stays in the tree, so it still shows as included.
          </p>
          <p>
            The result. The tally is decrypted once, after the vote ends
            {pid ? (
              <>
                ; the process's{' '}
                <Link to={paths.process(pid, 'results')} className='text-silver hover:text-emerald'>
                  results tab
                </Link>{' '}
                shows how it was checked.
              </>
            ) : (
              '.'
            )}
          </p>
        </Block>
        <Block title='Why the ciphertext on-chain is not the one you sent'>
          <p>
            Before storing a ballot the sequencer re-encrypts it: it adds an encryption of zero under the election key
            to every ciphertext. The vote inside does not change, and the zkVM proof checks that the stored ballot is
            exactly that re-encryption of the ballot your proof covers.
          </p>
          <p>
            The randomness comes from a secret seed the sequencer draws for each batch and never writes down, and no
            scalar is used twice. The prover deletes its copy only when it runs without DAVINCI_KEEP_INPUTS=1, an
            operator setting nothing on chain shows. Without the batch seed, nobody can match the ciphertext you sent to
            the stored one, so you cannot prove which ballot your slot holds. It does not hide whose slot it is: the
            sequencer that sealed the batch knows the seed, and with a Merkle census the slot follows from your address.
          </p>
        </Block>
        <Block title='Silent revoting'>
          <p>
            You can vote again while the process is open. The new ballot replaces the old one in your slot, and the
            tally subtracts the old ballot and adds the new one.
          </p>
          <p>
            Every batch also re-encrypts a random sample of occupied slots it did not write, and adds an encryption of
            zero to the tally for each, which changes no count. In the blob an overwrite and a refresh look the same: a
            slot whose ciphertexts changed. Nobody watching the chain can tell whether you voted again or your slot was
            only refreshed, so a revote stays deniable.
          </p>
          <p>
            What stays public is how many overwrites each batch had, and the first time a slot appears, since refreshes
            only touch slots already written. With a Merkle census the slot follows from your address, so that you
            voted, and when, is public too.
          </p>
        </Block>
      </div>
    </Panel>
  )
}
