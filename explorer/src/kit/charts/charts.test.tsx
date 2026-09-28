import { afterAll, beforeAll, describe, expect, it } from 'vitest'
import { render, screen } from '@testing-library/react'
import { Donut, Sparkline, StackedBars } from './index'

// jsdom gives every element a zero rect, so ChartFrame would only ever render
// its skeleton and none of the drawing code would run. Pin a width for the
// suite: that is what makes these smoke tests worth anything.
const realRect = HTMLElement.prototype.getBoundingClientRect
beforeAll(() => {
  HTMLElement.prototype.getBoundingClientRect = function rect() {
    return { width: 800, height: 240, top: 0, left: 0, right: 800, bottom: 240, x: 0, y: 0, toJSON: () => ({}) }
  }
})
afterAll(() => {
  HTMLElement.prototype.getBoundingClientRect = realRect
})

/** No chart may ever emit a NaN into an SVG attribute. */
function expectNoNaN(container: HTMLElement) {
  expect(container.innerHTML).not.toContain('NaN')
  expect(container.innerHTML).not.toContain('Infinity')
}

const activity = [
  { label: '1', values: { claims: 4, contributions: 3 } },
  { label: '2', values: { claims: 6, contributions: 5 } },
  { label: '3', values: { claims: 0, contributions: 0 } },
]
const series = [
  { key: 'claims', label: 'claims' },
  { key: 'contributions', label: 'contributions' },
]

describe('StackedBars', () => {
  it('draws a bar per datum and a legend per series', () => {
    const { container } = render(<StackedBars data={activity} series={series} />)
    expect(screen.getByRole('img', { name: /stacked activity/i })).toBeInTheDocument()
    expect(screen.getByText('claims')).toBeInTheDocument()
    // Two non-zero segments in each of the first two columns.
    expect(container.querySelectorAll('rect[fill="var(--color-series-1)"]').length).toBe(2)
    expectNoNaN(container)
  })

  it('renders an empty state rather than an empty axis', () => {
    render(<StackedBars data={[]} series={series} />)
    expect(screen.getByText('No activity in this range')).toBeInTheDocument()
  })

  it('renders a skeleton while loading', () => {
    const { container } = render(<StackedBars data={activity} series={series} loading />)
    expect(container.querySelector('.animate-skeleton')).toBeTruthy()
  })
})

describe('Donut', () => {
  it('draws one path per slice plus the centre value', () => {
    const { container } = render(
      <Donut
        slices={[
          { label: 'a', value: 3 },
          { label: 'b', value: 1 },
        ]}
        centerValue={4}
        centerLabel='total'
      />
    )
    expect(container.querySelectorAll('path').length).toBe(2)
    expect(screen.getByText('total')).toBeInTheDocument()
    expectNoNaN(container)
  })

  it('is empty rather than a zero-radius ring', () => {
    render(<Donut slices={[{ label: 'a', value: 0 }]} />)
    expect(screen.getByText('Nothing to break down')).toBeInTheDocument()
  })
})

describe('Sparkline', () => {
  it('draws a line and a last-point dot', () => {
    const { container } = render(<Sparkline values={[1, 4, 2, 8]} />)
    expect(container.querySelectorAll('path').length).toBe(2)
    expect(container.querySelector('circle')).toBeTruthy()
    expectNoNaN(container)
  })

  it('renders a dash for an empty series', () => {
    render(<Sparkline values={[]} />)
    expect(screen.getByText('—')).toBeInTheDocument()
  })

  it('survives a flat series', () => {
    const { container } = render(<Sparkline values={[5, 5, 5]} />)
    expectNoNaN(container)
  })
})
