import { LoroDoc } from "loro-crdt/nodejs"
import { describe, expect, it, vi } from "vitest"
import { COMPACT_AFTER, NOTES, SyncClient, type ClientHost, type SaveState } from "./client"
import type { DeviceLog } from "./device-log"
import { Assembler, Close, PART, encode, split, type Frame } from "./frames"

// The browser build of Loro loads its WASM with fetch; Node uses its own.
vi.mock("../loro", () => import("loro-crdt/nodejs"))

/** A socket the test drives by hand. */
class FakeSocket {
  readyState = 0
  binaryType = ""
  sent: Uint8Array[] = []
  onopen: (() => void) | null = null
  onmessage: ((e: { data: ArrayBuffer }) => void) | null = null
  onclose: ((e: { code: number; reason: string }) => void) | null = null
  open(): void {
    this.readyState = 1
    this.onopen?.()
  }
  send(m: Uint8Array): void {
    this.sent.push(m.slice())
  }
  deliver(m: Uint8Array): void {
    const copy = m.slice()
    this.onmessage?.({ data: copy.buffer })
  }
  frame(f: Frame): void {
    for (const m of split(f)) this.deliver(m)
  }
  close(code: number, reason = ""): void {
    if (this.readyState === 3) return
    this.readyState = 3
    this.onclose?.({ code, reason })
  }
  /** What the client sent, put back together. */
  frames(): Frame[] {
    const parts = new Assembler()
    return this.sent.map((m) => parts.push(m)).filter((f): f is Frame => f !== undefined)
  }
}

class FakeLog {
  rows = new Map<number, Uint8Array>()
  next = 0
  compactions = 0
  load(): Promise<{ keys: IDBValidKey[]; blobs: Uint8Array[] }> {
    return Promise.resolve({ keys: [...this.rows.keys()], blobs: [...this.rows.values()] })
  }
  append(b: Uint8Array): Promise<void> {
    this.rows.set(this.next++, b.slice())
    return Promise.resolve()
  }
  compact(keys: IDBValidKey[], snapshot: Uint8Array): Promise<void> {
    for (const k of keys) this.rows.delete(k as number)
    this.rows.set(this.next++, snapshot)
    this.compactions++
    return Promise.resolve()
  }
}

function setup(): {
  client: SyncClient
  sockets: FakeSocket[]
  log: FakeLog
  states: [SaveState, string | undefined][]
  reconnect: () => void
} {
  const sockets: FakeSocket[] = []
  const log = new FakeLog()
  let timer: (() => void) | undefined
  const host: ClientHost = {
    openLog: () => Promise.resolve(log as unknown as DeviceLog),
    socket: () => {
      const s = new FakeSocket()
      sockets.push(s)
      return s as unknown as WebSocket
    },
    setTimeout: (f) => (timer = f),
  }
  const client = new SyncClient("p", host)
  const states: [SaveState, string | undefined][] = []
  client.subscribe((s, note) => states.push([s, note]))
  const reconnect = (): void => {
    const f = timer
    timer = undefined
    f?.()
  }
  return { client, sockets, log, states, reconnect }
}

const flush = (): Promise<void> => new Promise((r) => setTimeout(r, 0))

/** An empty server: Welcome with nothing. */
function welcome(s: FakeSocket): void {
  s.open()
  s.frame({ type: "welcome", catchUp: new Uint8Array(), version: new LoroDoc().oplogVersion().encode() })
}

describe("sync client", () => {
  it("sends a large backlog in parts and takes a large catch-up in parts", async () => {
    const { client, sockets } = setup()
    await client.start()
    const server = new LoroDoc()
    server.getText("t").insert(0, "x".repeat(3 * PART))
    server.commit()
    const s = sockets[0]!
    s.open()
    s.frame({ type: "welcome", catchUp: server.export({ mode: "snapshot" }), version: server.oplogVersion().encode() })
    expect(client.doc.getText("t").length).toBe(3 * PART)
    client.doc.getText("t").insert(0, "y".repeat(2 * PART))
    client.doc.commit()
    expect(s.sent.every((m) => m.length <= PART + 9)).toBe(true)
    const update = s.frames().find((f) => f.type === "update")
    expect(update?.type).toBe("update")
    if (update?.type !== "update") return
    server.import(update.update)
    expect(server.getText("t").length).toBe(5 * PART)
  })

  it("after three malformed-frame failures, says Sync failed and stops reconnecting", async () => {
    const { client, sockets, states, reconnect } = setup()
    await client.start()
    for (let i = 0; i < 3; i++) {
      const s = sockets.at(-1)!
      welcome(s)
      s.deliver(new Uint8Array([99]))
      expect(s.readyState).toBe(3)
      if (i < 2) reconnect()
    }
    expect(client.state()).toBe("failed")
    expect(states.at(-1)).toEqual(["failed", NOTES.failed])
    reconnect()
    expect(sockets.length).toBe(3)
  })

  it("counts a 4000 from the server, and an Ack resets the count", async () => {
    const { client, sockets, reconnect } = setup()
    await client.start()
    for (let i = 0; i < 5; i++) {
      const s = sockets.at(-1)!
      welcome(s)
      if (i % 2 === 1) s.frame({ type: "ack", version: new LoroDoc().oplogVersion().encode() })
      s.close(Close.malformed)
      reconnect()
    }
    expect(client.state()).not.toBe("failed")
  })

  it("stops at 4130 with what happened and what to do, and keeps the edits on the device", async () => {
    const { client, sockets, log } = setup()
    await client.start()
    const s = sockets[0]!
    welcome(s)
    client.doc.getText("t").insert(0, "kept")
    client.doc.commit()
    await flush()
    s.close(Close.tooLarge, "the project is at its size limit")
    expect(client.state()).toBe("full")
    const fresh = new LoroDoc()
    fresh.importBatch([...log.rows.values()])
    expect(fresh.getText("t").toString()).toBe("kept")
    expect(sockets.length).toBe(1)
  })

  it("says Nisaba was updated at 4400", async () => {
    const { client, sockets } = setup()
    await client.start()
    sockets[0]!.close(Close.version)
    expect(client.state()).toBe("failed")
    expect(client.subscribe(() => {})).toBeTypeOf("function")
  })

  it("compacts the device log every 50 entries while the project is open", async () => {
    const { client, sockets, log } = setup()
    await client.start()
    welcome(sockets[0]!)
    for (let i = 0; i < COMPACT_AFTER * 2 + 3; i++) {
      client.doc.getText("t").insert(0, "a")
      client.doc.commit()
      await flush()
    }
    await flush()
    expect(log.compactions).toBe(2)
    expect(log.rows.size).toBeLessThanOrEqual(COMPACT_AFTER)
    const fresh = new LoroDoc()
    fresh.importBatch([...log.rows.values()])
    expect(fresh.getText("t").length).toBe(COMPACT_AFTER * 2 + 3)
  })

  it("encodes nothing unexpected for a heartbeat", () => {
    expect(encode({ type: "heartbeat" })).toEqual(new Uint8Array([6]))
  })
})
