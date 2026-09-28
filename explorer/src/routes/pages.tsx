import { lazy, Suspense, type ReactNode } from 'react'
import { SkeletonText } from '~kit'

// Each page is its own chunk and its own folder under src/pages/.
export const OverviewPage = lazy(() => import('~pages/overview').then((m) => ({ default: m.OverviewPage })))
export const ProcessesPage = lazy(() => import('~pages/processes').then((m) => ({ default: m.ProcessesPage })))
export const ProcessPage = lazy(() => import('~pages/process').then((m) => ({ default: m.ProcessPage })))
export const TransitionPage = lazy(() => import('~pages/transition').then((m) => ({ default: m.TransitionPage })))
export const TxPage = lazy(() => import('~pages/transition/tx').then((m) => ({ default: m.TxPage })))
export const VotesPage = lazy(() => import('~pages/votes').then((m) => ({ default: m.VotesPage })))
export const ContractsPage = lazy(() => import('~pages/contracts').then((m) => ({ default: m.ContractsPage })))
export const SequencersPage = lazy(() => import('~pages/sequencers').then((m) => ({ default: m.SequencersPage })))
export const LearnPage = lazy(() => import('~pages/learn').then((m) => ({ default: m.LearnPage })))
export const KitPage = lazy(() => import('~pages/kit').then((m) => ({ default: m.KitPage })))

function Loading() {
  return <SkeletonText lines={6} className='max-w-2xl' />
}

/** A lazy page with its loading skeleton. */
export function Lazy({ children }: { children: ReactNode }) {
  return <Suspense fallback={<Loading />}>{children}</Suspense>
}
