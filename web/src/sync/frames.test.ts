import { describe, expect, it } from "vitest"
import { Assembler, type Frame, FrameError, PROTOCOL, decode, encode, split } from "./frames"

const b = (...xs: number[]): Uint8Array => new Uint8Array(xs)

const frames: Frame[] = [
  { type: "hello", protocol: PROTOCOL, version: b() },
  { type: "hello", protocol: 9, version: b(1, 2, 3) },
  { type: "welcome", catchUp: b(4, 5), version: b(6) },
  { type: "update", update: b(7, 8, 9) },
  { type: "ack", version: b(10) },
  { type: "presence", state: b(11, 12) },
  { type: "heartbeat" },
  { type: "begin", total: 70000 },
  { type: "part", index: 3, bytes: b(13) },
]

describe("frames", () => {
  it("round trip", () => {
    for (const frame of frames) expect(decode(encode(frame))).toEqual(frame)
  })

  it("match the server's bytes", () => {
    expect([...encode({ type: "hello", protocol: 1, version: b(255) })]).toEqual([1, 1, 0, 0, 0, 1, 255])
    expect([...encode({ type: "heartbeat" })]).toEqual([6])
    expect([...encode({ type: "begin", total: 258 })]).toEqual([7, 0, 0, 1, 2])
    expect([...encode({ type: "part", index: 1, bytes: b(9) })]).toEqual([8, 0, 0, 0, 1, 0, 0, 0, 1, 9])
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
    expect(() => decode(b(9))).toThrow("unknown")
  })
})

describe("parts", () => {
  const big: Frame = { type: "update", update: Uint8Array.from({ length: 45 }, (_, i) => i) }
  const messages = split(big, 10)
  const feed = (sent: Uint8Array[]): Frame | undefined => {
    const parts = new Assembler(10)
    let out: Frame | undefined
    for (const m of sent) out = parts.push(m)
    return out
  }

  it("a short frame goes whole", () => {
    expect(split({ type: "heartbeat" }, 10)).toEqual([encode({ type: "heartbeat" })])
  })

  it("a long frame comes back whole after the last part, not before", () => {
    expect(messages.length).toBe(1 + 5)
    const parts = new Assembler(10)
    for (const m of messages.slice(0, -1)) expect(parts.push(m)).toBeUndefined()
    expect(parts.push(messages.at(-1)!)).toEqual(big)
  })

  it("every split length comes back whole", () => {
    for (let part = 1; part <= 60; part++) {
      const parts = new Assembler(part)
      let out: Frame | undefined
      for (const m of split(big, part)) out = parts.push(m)
      expect(out).toEqual(big)
    }
  })

  it("refuse a part missing, repeated, out of order, or too long", () => {
    const [begin, p0, p1, p2, p3, p4] = messages as [Uint8Array, ...Uint8Array[]]
    expect(() => feed([begin, p0!, p2!])).toThrow("expected part 1, got 2")
    expect(() => feed([begin, p0!, p0!])).toThrow("expected part 1, got 0")
    expect(() => feed([begin, p1!])).toThrow("expected part 0, got 1")
    const shorter = encode({ type: "begin", total: 45 })
    expect(() => feed([shorter, p0!, p1!, p2!, p3!, p4!])).toThrow("more bytes")
    expect(feed([begin, p0!, p1!, p2!, p3!, p4!])).toEqual(big)
  })

  it("refuse a part without a Begin and another frame among the parts", () => {
    expect(() => feed([messages[1]!])).toThrow("without a Begin")
    expect(() => feed([messages[0]!, encode({ type: "heartbeat" })])).toThrow("among the parts")
  })

  it("ask before keeping any part", () => {
    const parts = new Assembler(10)
    expect(() =>
      parts.push(messages[0]!, () => {
        throw new FrameError("too large")
      }),
    ).toThrow("too large")
  })
})
