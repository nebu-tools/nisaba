/**
 * Sync protocol version 2: the frame encoding. See DESIGN.md section 4.
 *
 * Each WebSocket binary message is one frame: a type byte and its fields.
 * A blob is a big-endian u32 length and that many bytes.
 *
 * No message is longer than PART bytes of payload. A longer frame goes as a
 * Begin that declares its encoded length, then Parts numbered from 0 whose
 * bytes are the encoded frame. This is the same code as the server's
 * `nisaba_sync::protocol`, in both directions.
 */

export const PROTOCOL = 2

/** The longest message: 1 MiB. */
export const PART = 1 << 20

export type Frame =
  | { type: "hello"; protocol: number; version: Uint8Array }
  | { type: "welcome"; catchUp: Uint8Array; version: Uint8Array }
  | { type: "update"; update: Uint8Array }
  | { type: "ack"; version: Uint8Array }
  | { type: "presence"; state: Uint8Array }
  | { type: "heartbeat" }
  | { type: "begin"; total: number }
  | { type: "part"; index: number; bytes: Uint8Array }

const HELLO = 1
const WELCOME = 2
const UPDATE = 3
const ACK = 4
const PRESENCE = 5
const HEARTBEAT = 6
const BEGIN = 7
const PART_TYPE = 8

/** WebSocket close codes the server sends. */
export const Close = {
  normal: 1000,
  /** The server holds as many unfinished large updates as it may. */
  tryAgain: 1013,
  malformed: 4000,
  tooSlow: 4002,
  /** A large update did not arrive by its deadline; nothing of it was kept. */
  timedOut: 4008,
  forbidden: 4003,
  notFound: 4004,
  full: 4029,
  tooLarge: 4130,
  version: 4400,
  signedOut: 4401,
  serverError: 4500,
} as const

export class FrameError extends Error {}

/** Lengths, totals and part numbers are u32 on the wire. DataView would
 * wrap a larger number silently. */
function u32(n: number, what: string): number {
  if (!Number.isInteger(n) || n < 0 || n > 0xffff_ffff) throw new FrameError(`${what} ${n} does not fit in 32 bits`)
  return n
}

export function encode(frame: Frame): Uint8Array<ArrayBuffer> {
  const blobs = (...bs: Uint8Array[]): number => bs.reduce((n, b) => n + 4 + b.length, 0)
  let out: Uint8Array<ArrayBuffer>
  let at = 1
  const put = (b: Uint8Array): void => {
    new DataView(out.buffer).setUint32(at, u32(b.length, "a blob of"))
    out.set(b, at + 4)
    at += 4 + b.length
  }
  switch (frame.type) {
    case "hello":
      out = new Uint8Array(2 + blobs(frame.version))
      out[0] = HELLO
      if (!Number.isInteger(frame.protocol) || frame.protocol < 0 || frame.protocol > 255) {
        throw new FrameError(`protocol ${frame.protocol} does not fit in a byte`)
      }
      out[1] = frame.protocol
      at = 2
      put(frame.version)
      break
    case "welcome":
      out = new Uint8Array(1 + blobs(frame.catchUp, frame.version))
      out[0] = WELCOME
      put(frame.catchUp)
      put(frame.version)
      break
    case "update":
      out = new Uint8Array(1 + blobs(frame.update))
      out[0] = UPDATE
      put(frame.update)
      break
    case "ack":
      out = new Uint8Array(1 + blobs(frame.version))
      out[0] = ACK
      put(frame.version)
      break
    case "presence":
      out = new Uint8Array(1 + blobs(frame.state))
      out[0] = PRESENCE
      put(frame.state)
      break
    case "heartbeat":
      out = new Uint8Array([HEARTBEAT])
      break
    case "begin":
      out = new Uint8Array(5)
      out[0] = BEGIN
      new DataView(out.buffer).setUint32(1, u32(frame.total, "a Begin of"))
      break
    case "part":
      out = new Uint8Array(5 + blobs(frame.bytes))
      out[0] = PART_TYPE
      new DataView(out.buffer).setUint32(1, u32(frame.index, "part"))
      at = 5
      put(frame.bytes)
      break
  }
  return out
}

/** Decodes one frame. The blobs are views into `buf`, not copies. */
export function decode(buf: Uint8Array): Frame {
  const view = new DataView(buf.buffer, buf.byteOffset, buf.byteLength)
  let at = 0
  const u8 = (): number => {
    if (at + 1 > buf.length) throw new FrameError("truncated")
    return buf[at++]!
  }
  const u32 = (): number => {
    if (at + 4 > buf.length) throw new FrameError("truncated")
    const n = view.getUint32(at)
    at += 4
    return n
  }
  const blob = (): Uint8Array => {
    if (at + 4 > buf.length) throw new FrameError("truncated")
    const n = view.getUint32(at)
    if (at + 4 + n > buf.length) throw new FrameError("truncated")
    const b = buf.subarray(at + 4, at + 4 + n)
    at += 4 + n
    return b
  }
  let frame: Frame
  const type = u8()
  switch (type) {
    case HELLO:
      frame = { type: "hello", protocol: u8(), version: blob() }
      break
    case WELCOME:
      frame = { type: "welcome", catchUp: blob(), version: blob() }
      break
    case UPDATE:
      frame = { type: "update", update: blob() }
      break
    case ACK:
      frame = { type: "ack", version: blob() }
      break
    case PRESENCE:
      frame = { type: "presence", state: blob() }
      break
    case HEARTBEAT:
      frame = { type: "heartbeat" }
      break
    case BEGIN:
      frame = { type: "begin", total: u32() }
      break
    case PART_TYPE:
      frame = { type: "part", index: u32(), bytes: blob() }
      break
    default:
      throw new FrameError(`unknown frame type ${type}`)
  }
  if (at !== buf.length) throw new FrameError(`${buf.length - at} trailing bytes`)
  return frame
}

/** The messages that carry `frame`: itself when its encoding is at most
 * `part` bytes, else a Begin and Parts of at most `part` bytes. */
export function split(frame: Frame, part = PART): Uint8Array<ArrayBuffer>[] {
  const bytes = encode(frame)
  if (bytes.length <= part) return [bytes]
  const out = [encode({ type: "begin", total: bytes.length })]
  for (let at = 0, index = 0; at < bytes.length; at += part, index++) {
    out.push(encode({ type: "part", index, bytes: bytes.subarray(at, at + part) }))
  }
  return out
}

/**
 * Puts frames sent in parts back together, for one connection. It keeps the
 * parts as they come and joins them at the end, so it never holds more than
 * arrived, whatever a Begin declares. Nothing comes out until the last
 * part, so a frame is used whole or not at all. A part missing, repeated or
 * out of order, more bytes than declared, a Part without a Begin, or another
 * frame among the parts is a FrameError, and the frame is dropped.
 */
export class Assembler {
  private open: { total: number; parts: Uint8Array[]; at: number; next: number } | undefined

  constructor(private readonly part = PART) {}

  /** Takes one message; returns the frame it completes. `allow` sees the
   * declared length of a Begin before any part is kept and may throw. */
  push(message: Uint8Array, allow: (total: number) => void = () => {}): Frame | undefined {
    if (message.length > this.part + 9) throw new FrameError("message longer than a part")
    const frame = decode(message)
    const open = this.open
    if (!open) {
      if (frame.type === "part") throw new FrameError("a Part without a Begin")
      if (frame.type !== "begin") return frame
      if (frame.total === 0) throw new FrameError("a Begin of 0 bytes")
      allow(frame.total)
      this.open = { total: frame.total, parts: [], at: 0, next: 0 }
      return undefined
    }
    this.open = undefined
    if (frame.type !== "part") throw new FrameError("another frame among the parts")
    if (frame.index !== open.next || frame.bytes.length === 0) {
      throw new FrameError(`expected part ${open.next}, got ${frame.index}`)
    }
    if (open.at + frame.bytes.length > open.total) throw new FrameError("more bytes than the Begin declared")
    open.parts.push(frame.bytes)
    open.at += frame.bytes.length
    open.next++
    if (open.at < open.total) {
      this.open = open
      return undefined
    }
    const buf = new Uint8Array(open.total)
    let at = 0
    for (const part of open.parts) {
      buf.set(part, at)
      at += part.length
    }
    const whole = decode(buf)
    if (whole.type === "begin" || whole.type === "part") throw new FrameError("parts inside parts")
    return whole
  }

  /** Forgets an unfinished frame, as after a reconnect. */
  reset(): void {
    this.open = undefined
  }
}
