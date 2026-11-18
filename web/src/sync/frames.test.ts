import { describe, expect, it } from "vitest"
import { type Frame, FrameError, PROTOCOL, decode, encode } from "./frames"

const b = (...xs: number[]): Uint8Array => new Uint8Array(xs)

const frames: Frame[] = [
  { type: "hello", protocol: PROTOCOL, version: b() },
  { type: "hello", protocol: 9, version: b(1, 2, 3) },
  { type: "welcome", catchUp: b(4, 5), version: b(6) },
  { type: "update", update: b(7, 8, 9) },
  { type: "ack", version: b(10) },
  { type: "presence", state: b(11, 12) },
  { type: "heartbeat" },
]

describe("frames", () => {
  it("round trip", () => {
    for (const frame of frames) expect(decode(encode(frame))).toEqual(frame)
  })

  it("match the server's bytes", () => {
    expect([...encode({ type: "hello", protocol: 1, version: b(255) })]).toEqual([1, 1, 0, 0, 0, 1, 255])
    expect([...encode({ type: "heartbeat" })]).toEqual([6])
  })

  it("refuse every cut", () => {
    for (const frame of frames) {
      const bytes = encode(frame)
      for (let cut = 0; cut < bytes.length; cut++) expect(() => decode(bytes.subarray(0, cut))).toThrow(FrameError)
    }
  })

  it("refuse trailing bytes and unknown types", () => {
    expect(() => decode(b(6, 0))).toThrow("trailing")
    expect(() => decode(b(0))).toThrow("unknown")
    expect(() => decode(b(7))).toThrow("unknown")
  })
})
