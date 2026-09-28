import { Hash } from '~kit'
import type { Point } from '~protocol/babyjubjub'
import { isIdentity } from '~protocol/babyjubjub'
import type { Ciphertext } from '~protocol/blob'
import { bigIntToHex } from '~lib/format'

/** A BabyJubJub point as its two coordinates; the identity says so. */
export function PointValue({ point }: { point: Point }) {
  if (isIdentity(point)) return <span className='font-mono text-[12px] text-ash'>identity (0, 1)</span>
  return (
    <span className='inline-flex min-w-0 flex-col gap-0.5'>
      <span className='inline-flex min-w-0 items-center gap-1'>
        <span className='font-mono text-[11px] text-ash'>x</span>
        <Hash value={bigIntToHex(point.x)} chars={8} />
      </span>
      <span className='inline-flex min-w-0 items-center gap-1'>
        <span className='font-mono text-[11px] text-ash'>y</span>
        <Hash value={bigIntToHex(point.y)} chars={8} />
      </span>
    </span>
  )
}

/** ElGamal ciphertexts, one row per ballot field. */
export function CiphertextTable({ ciphertexts, firstField = 0 }: { ciphertexts: Ciphertext[]; firstField?: number }) {
  return (
    <div className='scroll-slim overflow-x-auto'>
      <table className='w-full min-w-[520px] border-collapse text-[12px]'>
        <thead>
          <tr className='label-caps text-[10px] text-pewter'>
            <th scope='col' className='w-16 border-b border-charcoal px-2 py-1.5 text-left'>
              Field
            </th>
            <th scope='col' className='border-b border-charcoal px-2 py-1.5 text-left'>
              c1
            </th>
            <th scope='col' className='border-b border-charcoal px-2 py-1.5 text-left'>
              c2
            </th>
          </tr>
        </thead>
        <tbody>
          {ciphertexts.map((ct, i) => (
            <tr key={i} className='border-b border-charcoal/60 last:border-b-0'>
              <td className='px-2 py-1.5 font-mono text-silver tnum'>{firstField + i}</td>
              <td className='px-2 py-1.5'>
                <PointValue point={ct.c1} />
              </td>
              <td className='px-2 py-1.5'>
                <PointValue point={ct.c2} />
              </td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  )
}
