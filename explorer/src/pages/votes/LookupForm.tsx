import { useEffect, useId, useMemo, useState, type FormEvent } from 'react'
import { useNavigate } from 'react-router-dom'
import { useStore } from '~data/hooks'
import { useTransitionBlobs } from '~data/queries'
import { Button, Card, Input } from '~kit'
import { formatVoteId } from '~protocol/blob'
import { paths } from '~routes/paths'
import { validateLookup } from './lookup'

/**
 * Process id and vote id. Submitting a valid pair opens its lookup URL, so a
 * result can be shared and reloaded.
 */
export function LookupForm({ initialPid, initialVote }: { initialPid: string; initialVote: string }) {
  const navigate = useNavigate()
  const store = useStore()
  const listId = useId()
  const [pid, setPid] = useState(initialPid)
  const [vote, setVote] = useState(initialVote)
  const [touched, setTouched] = useState(false)
  const query = validateLookup(pid, vote)

  // The store is republished as a new object on every change; its arrays may not be.
  const known = useMemo(() => [...store.processOrder].reverse().slice(0, 200), [store])
  // The newest settled transition, for the example.
  const latest = useMemo(() => {
    const key = store.transitionOrder[store.transitionOrder.length - 1]
    return key ? store.transitions[key]! : null
  }, [store])
  const [wantExample, setWantExample] = useState(false)
  const example = useTransitionBlobs(latest?.processId, latest?.index, { enabled: wantExample })
  const exampleId = example.data?.decoded?.voteIds[0]

  useEffect(() => {
    if (!wantExample || !latest || exampleId == null) return
    setWantExample(false)
    navigate(paths.vote(latest.processId, formatVoteId(exampleId)))
  }, [wantExample, latest, exampleId, navigate])

  const exampleFailed = wantExample && (example.error != null || (example.data != null && exampleId == null))

  const onSubmit = (e: FormEvent) => {
    e.preventDefault()
    setTouched(true)
    if (query.pid && query.voteId != null) navigate(paths.vote(query.pid, formatVoteId(query.voteId)))
  }

  return (
    <Card>
      <form
        onSubmit={onSubmit}
        noValidate
        className='grid gap-3 md:grid-cols-[minmax(0,2fr)_minmax(0,1fr)_auto] md:items-start'
      >
        <Input
          label='Process id'
          mono
          value={pid}
          onChange={(e) => setPid(e.target.value)}
          placeholder='0x + 62 hex digits'
          list={listId}
          autoComplete='off'
          spellCheck={false}
          error={touched ? query.pidError : undefined}
          hint='The election you voted in. Pick one of the known processes or paste its id.'
        />
        <datalist id={listId}>
          {known.map((p) => (
            <option key={p} value={p} />
          ))}
        </datalist>
        <Input
          label='Vote id'
          mono
          value={vote}
          onChange={(e) => setVote(e.target.value)}
          placeholder='0x8000000000000001'
          autoComplete='off'
          spellCheck={false}
          error={touched ? query.voteError : undefined}
          hint='0x and 16 hex digits, from the app you voted with.'
        />
        <Button type='submit' variant='primary' className='justify-center md:mt-[22px]'>
          Look up
        </Button>
      </form>
      <div className='mt-3 flex flex-wrap items-center gap-x-3 gap-y-1 text-[12px] text-ash'>
        <span>No vote id at hand?</span>
        <Button
          size='sm'
          variant='ghost'
          disabled={!latest}
          loading={wantExample && !exampleFailed}
          onClick={() => setWantExample(true)}
        >
          Fill in an example
        </Button>
        <span>
          {latest
            ? exampleFailed
              ? 'The newest transition’s blobs could not be read, so there is no example to show.'
              : 'It takes the first vote id of the newest settled transition.'
            : 'No transition has settled yet, so there is no example.'}
        </span>
      </div>
    </Card>
  )
}
