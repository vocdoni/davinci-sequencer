// The route table exports data, not components; fast refresh does not apply.
/* eslint-disable react-refresh/only-export-components */
import { createBrowserRouter, type RouteObject } from 'react-router-dom'
import { Shell } from '~app/Shell'
import { RouteError } from '~pages/error'
import { NotFoundPage } from '~pages/not-found'
import { patterns } from './paths'
import {
  ContractsPage,
  KitPage,
  LearnPage,
  OverviewPage,
  Lazy,
  ProcessesPage,
  ProcessPage,
  SequencersPage,
  TransitionPage,
  TxPage,
  VotesPage,
} from './pages'

/** The route table, shared by the browser router and the tests' memory router. */
export const routes: RouteObject[] = [
  {
    path: '/',
    element: <Shell />,
    errorElement: <RouteError />,
    children: [
      {
        index: true,
        element: (
          <Lazy>
            <OverviewPage />
          </Lazy>
        ),
      },
      {
        path: patterns.processes,
        element: (
          <Lazy>
            <ProcessesPage />
          </Lazy>
        ),
      },
      {
        path: patterns.process,
        element: (
          <Lazy>
            <ProcessPage />
          </Lazy>
        ),
      },
      {
        path: patterns.transition,
        element: (
          <Lazy>
            <TransitionPage />
          </Lazy>
        ),
      },
      {
        path: patterns.processTab,
        element: (
          <Lazy>
            <ProcessPage />
          </Lazy>
        ),
      },
      {
        path: patterns.tx,
        element: (
          <Lazy>
            <TxPage />
          </Lazy>
        ),
      },
      {
        path: patterns.votes,
        element: (
          <Lazy>
            <VotesPage />
          </Lazy>
        ),
      },
      {
        path: patterns.vote,
        element: (
          <Lazy>
            <VotesPage />
          </Lazy>
        ),
      },
      {
        path: patterns.contracts,
        element: (
          <Lazy>
            <ContractsPage />
          </Lazy>
        ),
      },
      {
        path: patterns.sequencers,
        element: (
          <Lazy>
            <SequencersPage />
          </Lazy>
        ),
      },
      {
        path: patterns.learn,
        element: (
          <Lazy>
            <LearnPage />
          </Lazy>
        ),
      },
      {
        path: patterns.learnTopic,
        element: (
          <Lazy>
            <LearnPage />
          </Lazy>
        ),
      },
      {
        path: patterns.kit,
        element: (
          <Lazy>
            <KitPage />
          </Lazy>
        ),
      },
      { path: '*', element: <NotFoundPage /> },
    ],
  },
]

export const ROUTER_FUTURE = {
  v7_relativeSplatPath: true,
  v7_fetcherPersist: true,
  v7_normalizeFormMethod: true,
  v7_partialHydration: true,
  v7_skipActionErrorRevalidation: true,
} as const

export const router = createBrowserRouter(routes, { future: ROUTER_FUTURE })
