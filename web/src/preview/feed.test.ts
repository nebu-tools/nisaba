import { describe, expect, it } from "vitest"
import { FETCH_MS, Feed, type FeedHost, type FeedState, type FileState, RETRY_MS, WATCHDOG_MS } from "./feed"
import type { FromWorker, ToWorker } from "./messages"

/** A host whose timers, assets and worker the test drives by hand. */
class FakeHost implements FeedHost {
  docFiles: FileState[] = [{ path: "main.typ", text: "a" }]
  sent: ToWorker[] = []
  restarts = 0
  error: string | undefined
  timers = new Map<number, { fn: () => void; ms: number }>()
  pending = new Map<string, { resolve: (b: Uint8Array) => void; reject: (e: Error) => void; signal: AbortSignal }>()
  private nextTimer = 0

  files = (): FileState[] => this.docFiles
  post = (msg: ToWorker): void => void this.sent.push(msg)
  restart = (): void => void this.restarts++
  fetchAsset = (hash: string, signal: AbortSignal): Promise<Uint8Array> =>
    new Promise((resolve, reject) => {
      this.pending.set(hash, { resolve, reject, signal })
      signal.addEventListener("abort", () => reject(new Error("aborted")))
    })
  view = (): { visible: number[]; scale: number } => ({ visible: [], scale: 1 })
  failed = (message: string | undefined): void => void (this.error = message)
  setTimer = (fn: () => void, ms: number): unknown => {
    this.timers.set(++this.nextTimer, { fn, ms })
    return this.nextTimer
  }
  clearTimer = (handle: unknown): void => void this.timers.delete(handle as number)

  has(ms: number): boolean {
    return [...this.timers.values()].some((t) => t.ms === ms)
  }

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
    // The text compiles without the image; the image is fetched beside it.
    expect(feed.state).toEqual({ kind: "compiling", id: 1 })
    const sent = host.take()
    expect(sent.some((m) => m.type === "bytes")).toBe(false)
    expect(compiles(sent)).toEqual([1])
    feed.done(1)
    host.pending.get("h1")!.resolve(new Uint8Array([1]))
    await settle()
    const again = host.take()
    expect(again.filter((m) => m.type === "bytes").map((m) => m.path)).toEqual(["a.png"])
    expect(compiles(again)).toEqual([2])
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

  it("aborts a fetch that never settles at the deadline, and the retry compiles the newest text", async () => {
    const host = new FakeHost()
    const feed = new Feed(host)
    host.docFiles = [
      { path: "a.png", hash: "abc" },
      { path: "main.typ", text: "a" },
    ]
    feed.changed()
    host.docFiles = [
      { path: "a.png", hash: "abc" },
      { path: "main.typ", text: "newest" },
    ]
    feed.changed()
    feed.retry()
    expect(feed.state.kind).toBe("fetching")
    const hung = host.pending.get("abc")!
    host.fire(FETCH_MS)
    await settle()
    expect(hung.signal.aborted).toBe(true)
    expect(feed.state.kind).toBe("failed")
    expect(host.error).toContain(`within ${FETCH_MS / 1000} s`)
    host.take()
    host.fire(RETRY_MS[0]!)
    const sent = host.take()
    expect(sent).toContainEqual({ type: "text", path: "main.typ", text: "newest" })
    expect(compiles(sent)).toEqual([1])
  })

  it("text and an asset never share a key: the text '#<hash>' is not the asset", async () => {
    const host = new FakeHost()
    const feed = new Feed(host)
    host.docFiles = [
      { path: "main.typ", text: "a" },
      { path: "x", hash: "abc" },
    ]
    feed.changed()
    host.pending.get("abc")!.resolve(new Uint8Array([1]))
    await settle()
    feed.done(1)
    host.take()
    host.docFiles = [
      { path: "main.typ", text: "a" },
      { path: "x", text: "#abc" },
    ]
    feed.changed()
    expect(host.take()).toEqual([
      { type: "text", path: "x", text: "#abc" },
      { type: "compile", id: 2, visible: [], scale: 1 },
    ])
    feed.done(2)
    host.docFiles = [
      { path: "main.typ", text: "a" },
      { path: "x", hash: "abc" },
    ]
    feed.changed()
    expect(host.take()).toEqual([
      { type: "bytes", path: "x", bytes: new Uint8Array([1]) },
      { type: "compile", id: 3, visible: [], scale: 1 },
    ])
  })

  it("an edit while a compile hangs reaches the worker within the watchdog and one retry", () => {
    const host = new FakeHost()
    const feed = new Feed(host)
    feed.changed()
    host.docFiles = [{ path: "main.typ", text: "newest" }]
    feed.changed()
    host.take()
    host.fire(WATCHDOG_MS)
    host.fire(RETRY_MS[0]!)
    expect(host.take()).toEqual([
      { type: "text", path: "main.typ", text: "newest" },
      { type: "compile", id: 2, visible: [], scale: 1 },
    ])
  })
})

/** Every cell of the table at the top of feed.ts. */
describe("the preview feed's state table", () => {
  type Kind = FeedState["kind"]
  const withAsset: FileState[] = [
    { path: "a.png", hash: "h1" },
    { path: "main.typ", text: "a" },
  ]

  /** A feed in `kind`. The fetching and failed feeds have a fetch pending. */
  function into(kind: Kind): { host: FakeHost; feed: Feed } {
    const host = new FakeHost()
    const feed = new Feed(host)
    if (kind === "fetching" || kind === "failed") host.docFiles = withAsset
    if (kind !== "idle") feed.changed()
    if (kind === "failed") feed.workerFailed("x")
    if (kind === "idle") {
      feed.changed()
      feed.done(1)
    }
    expect(feed.state.kind).toBe(kind)
    host.take()
    return { host, feed }
  }

  const events: Record<string, (host: FakeHost, feed: Feed) => Promise<void> | void> = {
    changed: (host, feed) => {
      host.docFiles = host.docFiles.map((f) => (f.text !== undefined ? { ...f, text: "newest" } : f))
      feed.changed()
    },
    retry: (_, feed) => feed.retry(),
    "done(current)": (_, feed) => feed.done(feed.compileId),
    "done(old)": (_, feed) => feed.done(feed.compileId - 1),
    "worker failed": (_, feed) => feed.workerFailed("x"),
    watchdog: (host) => host.fire(WATCHDOG_MS),
    "fetch ok": async (host) => {
      host.pending.get("h1")!.resolve(new Uint8Array([1]))
      await settle()
    },
    "fetch fails": async (host) => {
      host.pending.get("h1")!.reject(new Error("answered 500"))
      await settle()
    },
    deadline: async (host) => {
      host.fire(FETCH_MS)
      await settle()
    },
    "retry timer": (host) => host.fire(RETRY_MS[0]!),
  }

  /** Whether the event can happen in this feed. */
  const possible: Record<string, (host: FakeHost) => boolean> = {
    watchdog: (host) => host.has(WATCHDOG_MS),
    deadline: (host) => host.has(FETCH_MS),
    "fetch ok": (host) => host.pending.has("h1"),
    "fetch fails": (host) => host.pending.has("h1"),
    "retry timer": (host) => host.has(RETRY_MS[0]!),
  }

  const table: Record<Kind, Record<string, Kind | null>> = {
    idle: {
      changed: "compiling",
      retry: "idle",
      "done(current)": "idle",
      "done(old)": "idle",
      "worker failed": "failed",
      watchdog: null,
      "fetch ok": null,
      "fetch fails": null,
      deadline: null,
      "retry timer": null,
    },
    fetching: {
      changed: "fetching",
      retry: "fetching",
      "done(current)": "fetching",
      "done(old)": "fetching",
      "worker failed": "failed",
      watchdog: null,
      "fetch ok": "compiling",
      "fetch fails": "failed",
      deadline: "failed",
      "retry timer": null,
    },
    compiling: {
      changed: "compiling",
      retry: "compiling",
      "done(current)": "idle",
      "done(old)": "compiling",
      "worker failed": "failed",
      watchdog: "failed",
      "fetch ok": null,
      "fetch fails": null,
      deadline: null,
      "retry timer": null,
    },
    // The fetch of this failed feed was aborted when it failed; its late
    // answer is ignored. A new compile fetches the asset again.
    failed: {
      changed: "fetching",
      retry: "fetching",
      "done(current)": "failed",
      "done(old)": "failed",
      "worker failed": "failed",
      watchdog: null,
      "fetch ok": "failed",
      "fetch fails": "failed",
      deadline: null,
      "retry timer": "fetching",
    },
  }

  for (const [kind, row] of Object.entries(table) as [Kind, Record<string, Kind | null>][]) {
    for (const [event, next] of Object.entries(row)) {
      it(`${kind} + ${event} -> ${next ?? "cannot happen"}`, async () => {
        const { host, feed } = into(kind)
        if (next === null) {
          expect(possible[event]!(host)).toBe(false)
          return
        }
        await events[event]!(host, feed)
        expect(feed.state.kind).toBe(next)
      })
    }
  }

  it("compiling + done(current) with an edit pending compiles the newest text", () => {
    const { host, feed } = into("compiling")
    void events.changed!(host, feed)
    feed.done(feed.compileId)
    expect(host.take()).toContainEqual({ type: "text", path: "main.typ", text: "newest" })
    expect(feed.state.kind).toBe("compiling")
  })

  it("fetching + retry compiles the newest text once the fetch settles", async () => {
    const { host, feed } = into("fetching")
    void events.changed!(host, feed)
    feed.retry()
    await events["fetch ok"]!(host, feed)
    expect(host.take()).toContainEqual({ type: "text", path: "main.typ", text: "newest" })
  })
})

describe("what reaches the screen", () => {
  it("drops a late layout and page of an older compile", () => {
    const host = new FakeHost()
    const feed = new Feed(host)
    feed.changed()
    // The watchdog gives up on compile 1 and the retry sends compile 2.
    host.fire(WATCHDOG_MS)
    host.fire(RETRY_MS[0]!)
    expect(feed.compileId).toBe(2)
    const bitmap = {} as ImageBitmap
    const layout = (id: number): FromWorker => ({
      type: "layout",
      id,
      changed: [0],
      pages: ["p"],
      sizes: [[1, 1]],
      problems: [],
      ok: true,
      missing_fonts: [],
    })
    expect(feed.accepts(layout(1))).toBe(false)
    expect(feed.accepts({ type: "page", id: 1, index: 0, bitmap })).toBe(false)
    expect(feed.accepts(layout(2))).toBe(true)
    expect(feed.accepts({ type: "page", id: 2, index: 0, bitmap })).toBe(true)
    expect(feed.accepts({ type: "failed", message: "x" })).toBe(true)
  })
})
