// A sequencer node's own view of some processes (`GET /processes/{pid}`):
// whether it accepts votes, the root it has committed and whether it refused
// to serve the process. The sequencers page asks for one page of ids at a time.

import { useQueries, type UseQueryResult } from '@tanstack/react-query'
import { useRuntimeConfig } from '~config/config-context'
import type { Hex } from '~protocol/bytes'
import type { SequencerProcess } from '~protocol/sequencer-api'
import type { SequencerEndpoint } from './services'

export function useSequencerProcessViews(
  endpoint: SequencerEndpoint | null,
  pids: Hex[]
): Array<UseQueryResult<SequencerProcess>> {
  const config = useRuntimeConfig()
  const deployment = `${config.demo ? 'demo' : config.chainId}:${config.registryAddress.toLowerCase()}`
  return useQueries({
    queries: pids.map((pid) => ({
      queryKey: ['sequencer-process', deployment, endpoint?.index ?? -1, pid.toLowerCase()],
      queryFn: ({ signal }: { signal: AbortSignal }) => endpoint!.api.process(pid, signal),
      enabled: endpoint != null,
      staleTime: 30_000,
      refetchInterval: 60_000,
      retry: 0,
    })),
  })
}
