// Loads the shared test vectors in `tests/vectors/` (see its README).

import { readFileSync } from 'node:fs'
import { resolve } from 'node:path'

const DIR = resolve(__dirname, '../../tests/vectors')

export function vectorBytes(name: string): Uint8Array {
  return new Uint8Array(readFileSync(resolve(DIR, name)))
}

/** JSON with every bare integer of 16+ digits read as a string, so u64 values survive. */
export function loadJsonBig<T = unknown>(name: string): T {
  const text = readFileSync(resolve(DIR, name), 'utf8')
  return JSON.parse(text.replace(/([[,:]\s*)(-?\d{16,})(?=\s*[,\]}])/g, '$1"$2"')) as T
}
