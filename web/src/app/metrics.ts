/** Timings the benchmark in web/bench/measure.mjs reads from `window.__nisaba`. */
export interface Metrics {
  typedAt: number
  firstPreviewMs: number
  cycles: Record<string, number>[]
  paint: Record<string, number>[]
}

export const metrics: Metrics = { typedAt: 0, firstPreviewMs: -1, cycles: [], paint: [] }
;(globalThis as unknown as { __nisaba: Metrics }).__nisaba = metrics
