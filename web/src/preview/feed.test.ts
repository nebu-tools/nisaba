import { describe, expect, it } from "vitest"
import { Feed, type FeedHost, type FileState, RETRY_MS, WATCHDOG_MS } from "./feed"
import type { ToWorker } from "./messages"

/** A host whose timers, assets and worker the test drives by hand. */
class FakeHost implements FeedHost {
  docFiles: FileState[] = [{ path: "main.typ", text: "a" }]
  sent: ToWorker[] = []
  restarts = 0
  error: string | undefined
  timers = new Map<number, { fn: () => void; ms: number }>()
  pending = new Map<string, { resolve: (b: Uint8Array) => void; reject: (e: Error) => void }>()
  private nextTimer = 0

  files = (): FileState[] => this.docFiles
  post = (msg: ToWorker): void => void this.sent.push(msg)
  restart = (): void => void this.restarts++
  fetchAsset = (hash: string): Promise<Uint8Array> =>
    new Promise((resolve, reject) => this.pending.set(hash, { resolve, reject }))
  view = (): { visible: number[]; scale: number } => ({ visible: [], scale: 1 })
  failed = (message: string | undefined): void => void (this.error = message)
  setTimer = (fn: () => void, ms: number): unknown => {
    this.timers.set(++this.nextTimer, { fn, ms })
    return this.nextTimer
  }
  clearTimer = (handle: unknown): void => void this.timers.delete(handle as number)

  /** Fires the pending timer with delay `ms`. */
  fire(ms: number): void {
    const entry = [...this.timers].find(([, t]) => t.ms === ms)
    if (!entry) throw new Error(`no ${ms} ms timer`)
    this.timers.delete(entry[0])
    entry[1].fn()
  }

  take(): ToWorker[] {
    const out = this.sent
    this.sent = []
    return out
  }
}

const compiles = (msgs: ToWorker[]): number[] => msgs.flatMap((m) => (m.type === "compile" ? [m.id] : []))
const settle = (): Promise<void> => new Promise((r) => setTimeout(r, 0))

describe("the preview feed", () => {
  it("keeps one compile in flight and compiles once more for edits during it", () => {
    const host = new FakeHost()
    const feed = new Feed(host)
    feed.changed()
    expect(host.take()).toEqual([
      { type: "text", path: "main.typ", text: "a" },
      { type: "compile", id: 1, visible: [], scale: 1 },
    ])
    host.docFiles = [{ path: "main.typ", text: "ab" }]
    feed.changed()
    host.docFiles = [{ path: "main.typ", text: "abc" }]
    feed.changed()
    expect(host.take()).toEqual([])
    feed.done(1)
    expect(host.take()).toEqual([
      { type: "text", path: "main.typ", text: "abc" },
      { type: "compile", id: 2, visible: [], scale: 1 },
    ])
    feed.done(2)
    expect(feed.state).toEqual({ kind: "idle" })
  })

  it("ignores the end of a compile it no longer waits for", () => {
    const host = new FakeHost()
    const feed = new Feed(host)
    feed.changed()
    feed.done(7)
    expect(feed.state).toEqual({ kind: "compiling", id: 1 })
  })

  it("removes files that left the document", () => {
    const host = new FakeHost()
    const feed = new Feed(host)
    host.docFiles = [
      { path: "main.typ", text: "a" },
      { path: "old.typ", text: "b" },
    ]
    feed.changed()
    feed.done(1)
    host.take()
    host.docFiles = [
      { path: "main.typ", text: "a" },
      { path: "new.typ", text: "b" },
    ]
    feed.changed()
    expect(host.take()).toEqual([
      { type: "remove", path: "old.typ" },
      { type: "text", path: "new.typ", text: "b" },
      { type: "compile", id: 2, visible: [], scale: 1 },
    ])
  })

  it("loads each asset once and compiles after it arrives", async () => {
    const host = new FakeHost()
    const feed = new Feed(host)
    host.docFiles = [
      { path: "main.typ", text: "a" },
      { path: "a.png", hash: "h1" },
      { path: "b.png", hash: "h1" },
    ]
    feed.changed()
    expect(feed.state.kind).toBe("fetching")
    expect([...host.pending.keys()]).toEqual(["h1"])
    feed.changed()
    expect(compiles(host.sent)).toEqual([])
    host.pending.get("h1")!.resolve(new Uint8Array([1]))
    await settle()
    const sent = host.take()
    expect(sent.filter((m) => m.type === "bytes").map((m) => m.path)).toEqual(["a.png", "b.png"])
    expect(compiles(sent)).toEqual([1])
  })

  it("clears the gate when an asset fails, and retries by itself", async () => {
    const host = new FakeHost()
    const feed = new Feed(host)
    host.docFiles = [
      { path: "main.typ", text: "a" },
      { path: "a.png", hash: "h1" },
    ]
    feed.changed()
    const first = host.pending.get("h1")!
    first.reject(new Error("/api/projects/p/assets/h1 answered 404"))
    await settle()
    expect(feed.state.kind).toBe("failed")
    expect(host.error).toContain("answered 404")
    expect(host.restarts).toBe(1)
    host.take()
    host.fire(RETRY_MS[0]!)
    expect(host.error).toBeUndefined()
    expect(feed.state.kind).toBe("fetching")
    host.pending.get("h1")!.resolve(new Uint8Array([1]))
    await settle()
    expect(compiles(host.take())).toEqual([1])
  })

  it("ignores an asset that arrives after a failure and a retry", async () => {
    const host = new FakeHost()
    const feed = new Feed(host)
    host.docFiles = [
      { path: "main.typ", text: "a" },
      { path: "a.png", hash: "h1" },
    ]
    feed.changed()
    const stale = host.pending.get("h1")!
    feed.workerFailed("x")
    feed.retry()
    const fresh = host.pending.get("h1")!
    expect(fresh).not.toBe(stale)
    stale.resolve(new Uint8Array([1]))
    await settle()
    expect(compiles(host.sent)).toEqual([])
    fresh.resolve(new Uint8Array([1]))
    await settle()
    expect(compiles(host.take())).toEqual([1])
  })

  it("restarts a dead worker and sends it every file again", () => {
    const host = new FakeHost()
    const feed = new Feed(host)
    host.docFiles = [
      { path: "main.typ", text: "a" },
      { path: "b.typ", text: "b" },
    ]
    feed.changed()
    host.take()
    feed.workerFailed("the worker crashed")
    expect(host.restarts).toBe(1)
    expect(feed.state).toMatchObject({ kind: "failed", attempt: 0 })
    expect(host.error).toContain("the worker crashed")
    // An edit retries at once.
    feed.changed()
    expect(host.error).toBeUndefined()
    expect(host.take()).toEqual([
      { type: "text", path: "main.typ", text: "a" },
      { type: "text", path: "b.typ", text: "b" },
      { type: "compile", id: 2, visible: [], scale: 1 },
    ])
  })

  it("stops a compile that outlasts the watchdog", () => {
    const host = new FakeHost()
    const feed = new Feed(host)
    feed.changed()
    host.fire(WATCHDOG_MS)
    expect(feed.state.kind).toBe("failed")
    expect(host.restarts).toBe(1)
    // The old worker's answer comes too late to count.
    feed.done(1)
    expect(feed.state.kind).toBe("failed")
    feed.retry()
    expect(feed.state).toEqual({ kind: "compiling", id: 2 })
  })

  it("waits longer after each failure, and starts over after a compile works", () => {
    const host = new FakeHost()
    const feed = new Feed(host)
    const delays: number[] = []
    for (let i = 0; i < RETRY_MS.length + 1; i++) {
      feed.changed()
      feed.workerFailed("x")
      const [timer] = [...host.timers.values()].filter((t) => t.ms !== WATCHDOG_MS)
      delays.push(timer!.ms)
      host.fire(timer!.ms)
    }
    expect(delays).toEqual([...RETRY_MS, RETRY_MS.at(-1)])
    feed.done(feed.compileId)
    feed.changed()
    feed.workerFailed("x")
    expect(feed.state).toMatchObject({ kind: "failed", attempt: 0 })
  })

  it("leaves no timer behind when stopped", () => {
    const host = new FakeHost()
    const feed = new Feed(host)
    feed.changed()
    feed.workerFailed("x")
    feed.stop()
    expect(host.timers.size).toBe(0)
  })
})
