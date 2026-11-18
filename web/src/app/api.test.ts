import { describe, expect, it } from "vitest"
import { exportUrl, localDate } from "./api"

describe("the export date", () => {
  it("is the local calendar date", () => {
    // 23:30 local time on 31 December is still that day here, whatever UTC says.
    const late = new Date(2026, 11, 31, 23, 30)
    expect(localDate(late)).toBe("2026-12-31")
    expect(localDate(new Date(2027, 0, 1, 0, 5))).toBe("2027-01-01")
    expect(exportUrl("p1", late)).toBe("/api/projects/p1/export.pdf?today=2026-12-31")
  })
})
