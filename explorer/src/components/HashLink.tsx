import type { ReactNode } from 'react'
import { Link, useLocation } from 'react-router-dom'

/**
 * A link to a section of the current page. A plain `href="#id"` makes the
 * router restore the scroll position of the page's first entry (the top), so
 * in-page anchors go through the router instead, which scrolls to the id.
 */
export function HashLink({ id, className, children }: { id: string; className?: string; children: ReactNode }) {
  const { search } = useLocation()
  return (
    <Link to={{ search, hash: id }} className={className}>
      {children}
    </Link>
  )
}
