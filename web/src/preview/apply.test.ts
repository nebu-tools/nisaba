import { describe, expect, it } from "vitest"
import { untilMidnight } from "./apply"

describe("the midnight recompile", () => {
  it("waits until just after the next local midnight", () => {
    const evening = new Date(2026, 10, 18, 23, 59, 0)
    expect(untilMidnight(evening)).toBe(61_000)
    const morning = new Date(2026, 10, 18, 0, 0, 30)
    expect(untilMidnight(morning)).toBe(24 * 3600_000 - 29_000)
  })
})
