/**
 * Sync protocol version 1: the frame encoding. See DESIGN.md section 4.
 *
 * Each WebSocket binary message is one frame: a type byte and its fields.
 * A blob is a big-endian u32 length and that many bytes.
 */

export const PROTOCOL = 1

export type Frame =
  | { type: "hello"; protocol: number; version: Uint8Array }
  | { type: "welcome"; catchUp: Uint8Array; version: Uint8Array }
  | { type: "update"; update: Uint8Array }
  | { type: "ack"; version: Uint8Array }
  | { type: "presence"; state: Uint8Array }
  | { type: "heartbeat" }

const HELLO = 1
const WELCOME = 2
const UPDATE = 3
const ACK = 4
const PRESENCE = 5
const HEARTBEAT = 6

/** WebSocket close codes the server sends. */
export const Close = {
  normal: 1000,
  malformed: 4000,
  tooSlow: 4002,
  forbidden: 4003,
  full: 4029,
  tooLarge: 4130,
  version: 4400,
  serverError: 4500,
} as const

export class FrameError extends Error {}

export function encode(frame: Frame): Uint8Array<ArrayBuffer> {
  const blobs = (...bs: Uint8Array[]): number => bs.reduce((n, b) => n + 4 + b.length, 0)
  let out: Uint8Array<ArrayBuffer>
  let at = 1
  const put = (b: Uint8Array): void => {
    new DataView(out.buffer).setUint32(at, b.length)
    out.set(b, at + 4)
    at += 4 + b.length
  }
  switch (frame.type) {
    case "hello":
      out = new Uint8Array(2 + blobs(frame.version))
      out[0] = HELLO
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
    default:
      throw new FrameError(`unknown frame type ${type}`)
  }
  if (at !== buf.length) throw new FrameError(`${buf.length - at} trailing bytes`)
  return frame
}
