// Links into a davinci-dkg explorer (DKG_EXPLORER_URL). Its routes are
// `/epochs/:id` and `/applications/:epoch/:aid`, with lowercase 0x hex ids.

const base = (url: string) => url.replace(/\/+$/, '')

export function dkgEpochUrl(explorer: string | undefined, epochId: string): string | null {
  return explorer ? `${base(explorer)}/epochs/${epochId.toLowerCase()}` : null
}

export function dkgApplicationUrl(explorer: string | undefined, epochId: string, aid: string): string | null {
  return explorer ? `${base(explorer)}/applications/${epochId.toLowerCase()}/${aid.toLowerCase()}` : null
}
