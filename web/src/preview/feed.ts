/**
 * What the preview sends its worker, and when: the state machine behind
 * `Preview`, kept apart from the DOM so it can be tested.
 *
 * Rules (DESIGN.md section 8):
 * - At most one compile is in flight. Edits during it mark the feed dirty
 *   and compile once more when it ends.
 * - Only changed files are sent; a file that left the document is removed,
 *   so the worker holds exactly the files an export would see.
 * - Every wait has a deadline and can be aborted: a compile has the
 *   watchdog, an asset fetch has FETCH_MS. So an edit or a Retry in any
 *   state other than idle leads to a compile of the newest text within
 *   FETCH_MS + WATCHDOG_MS + the longest retry delay.
 * - A failure terminates the worker, starts a fresh one, forgets what was
 *   sent, and shows an error. The feed retries by itself with a growing
 *   delay, at once on the next edit, when the browser comes back online, or
 *   when the person presses Retry.
 * - An asset that failed to load does not hold up the next compile. That
 *   compile runs without it, so Typst names the missing file, and the asset
 *   is fetched again beside it. When it arrives the feed compiles again.
 *
 * State and event to next state. "compile" means: send what changed, then
 * fetching when an asset is missing (and has not just failed), else
 * compiling.
 *
 * | state     | changed (edit)  | retry (button, online) | done(current)          | done(old) | worker failed | watchdog  | fetch ok         | fetch fails or deadline | retry timer |
 * |-----------|-----------------|------------------------|------------------------|-----------|---------------|-----------|------------------|-------------------------|-------------|
 * | idle      | compile         | idle                   | idle                   | idle      | failed        | -         | -                | -                       | -           |
 * | fetching  | fetching, dirty | fetching, dirty        | fetching               | fetching  | failed        | -         | idle, compile    | failed, skip the asset  | -           |
 * | compiling | compiling, dirty| compiling, dirty       | idle; compile if dirty | compiling | failed        | failed    | -                | -                       | -           |
 * | failed    | compile         | compile                | failed                 | failed    | failed        | -         | failed (ignored) | failed (ignored)        | compile     |
 *
 * "-" cannot happen: no timer or fetch of that kind is pending in that
 * state. A fetch that settles after the state moved on is ignored. A
 * background fetch of a skipped asset that arrives calls `changed`.
 */
import type { LoroDoc } from "loro-crdt"
import { projectFiles } from "../sync/files"
import type { FromWorker, ToWorker } from "./messages"

/** One file as the document holds it now. */
export interface FileState {
  path: string
  text?: string
  hash?: string
}

/** The files of a document as the feed compares them. */
export function fileStates(doc: LoroDoc): FileState[] {
  return projectFiles(doc).map((f) =>
    f.text ? { path: f.path, text: f.text.toString() } : { path: f.path, ...(f.hash ? { hash: f.hash } : {}) },
  )
}

export interface FeedHost {
  /** The project's files now. */
  files(): FileState[]
  /** Sends a message to the current worker. */
  post(msg: ToWorker): void
  /** Terminates the worker and starts a fresh one. */
  restart(): void
  /** Loads an asset's bytes; rejects when the server does not answer 200
   * or `signal` aborts. */
  fetchAsset(hash: string, signal: AbortSignal): Promise<Uint8Array>
  /** The pages that are on screen and the render scale. */
  view(): { visible: number[]; scale: number }
  /** Shows or clears the error. */
  failed(message: string | undefined): void
  setTimer(fn: () => void, ms: number): unknown
  clearTimer(handle: unknown): void
}

export type FeedState =
  | { kind: "idle" }
  | { kind: "fetching"; id: number }
  | { kind: "compiling"; id: number }
  | { kind: "failed"; message: string; attempt: number }

/** A compile that takes longer than this is treated as a hung worker. */
export const WATCHDOG_MS = 30_000
/** An asset fetch that takes longer than this is aborted. */
export const FETCH_MS = 15_000
/** Delays before the automatic retries; the last one repeats. */
export const RETRY_MS = [1_000, 2_000, 5_000, 10_000, 20_000]

/** What the worker holds at a path. */
type Sent = { kind: "text"; text: string } | { kind: "asset"; hash: string }

export class Feed {
  private current: FeedState = { kind: "idle" }
  private dirty = false
  private id = 0
  private readonly sent = new Map<string, Sent>()
  private readonly assets = new Map<string, Uint8Array>()
  /** Assets whose last fetch failed; the next compile runs without them. */
  private readonly skipped = new Set<string>()
  /** Fetches of skipped assets beside the compile. */
  private readonly background = new Set<string>()
  private readonly aborts = new Set<AbortController>()
  private watchdog: unknown
  private retryTimer: unknown
  /** Failures since the last compile that worked; picks the retry delay. */
  private attempt = 0
  private fetches = 0
  private stopped = false

  constructor(private readonly host: FeedHost) {}

  get state(): FeedState {
    return this.current
  }

  /** The id of the newest compile request. */
  get compileId(): number {
    return this.id
  }

  /** Whether a message from the worker belongs to the newest compile. A
   * layout or page of an older one, as from a worker that was replaced or a
   * compile the watchdog gave up on, must not reach the screen. */
  accepts(msg: FromWorker): boolean {
    return msg.type === "failed" || msg.id === this.id
  }

  /** Call after any change to the document. */
  changed(): void {
    if (this.current.kind === "failed") this.retry()
    else this.compile()
  }

  /** Retry now: the Retry button, the browser coming back online. */
  retry(): void {
    if (this.current.kind === "idle") return
    if (this.current.kind !== "failed") {
      this.dirty = true
      return
    }
    this.host.clearTimer(this.retryTimer)
    this.current = { kind: "idle" }
    this.host.failed(undefined)
    this.compile()
  }

  /** The worker finished compile `id`. */
  done(id: number): void {
    // A done of an older compile is ignored.
    if (this.current.kind !== "compiling" || this.current.id !== id) return
    this.host.clearTimer(this.watchdog)
    this.current = { kind: "idle" }
    // A compile that worked resets the retry delays.
    this.attempt = 0
    if (this.dirty) this.compile()
  }

  /** The worker died, could not start, or reported a failure. */
  workerFailed(message: string): void {
    this.fail(`The preview stopped: ${message}`)
  }

  stop(): void {
    this.stopped = true
    this.host.clearTimer(this.watchdog)
    this.host.clearTimer(this.retryTimer)
    for (const abort of this.aborts) abort.abort()
  }

  private compile(): void {
    if (this.current.kind !== "idle") {
      if (this.current.kind !== "failed") this.dirty = true
      return
    }
    this.dirty = false
    const files = this.host.files()
    const present = new Set(files.map((f) => f.path))
    for (const path of this.sent.keys()) {
      if (!present.has(path)) {
        this.sent.delete(path)
        this.host.post({ type: "remove", path })
      }
    }
    const missing: string[] = []
    for (const file of files) {
      const held = this.sent.get(file.path)
      if (file.text !== undefined) {
        if (held?.kind !== "text" || held.text !== file.text) {
          this.sent.set(file.path, { kind: "text", text: file.text })
          this.host.post({ type: "text", path: file.path, text: file.text })
        }
      } else if (file.hash !== undefined) {
        if (held?.kind === "asset" && held.hash === file.hash) continue
        const bytes = this.assets.get(file.hash)
        if (bytes) {
          this.sent.set(file.path, { kind: "asset", hash: file.hash })
          this.host.post({ type: "bytes", path: file.path, bytes })
        } else if (this.skipped.has(file.hash)) {
          // The worker must not keep an older file at this path.
          if (held) {
            this.sent.delete(file.path)
            this.host.post({ type: "remove", path: file.path })
          }
          this.fetchBeside(file.hash)
        } else missing.push(file.hash)
      }
    }
    if (missing.length > 0) {
      this.fetch(missing)
      return
    }
    const id = ++this.id
    this.current = { kind: "compiling", id }
    this.watchdog = this.host.setTimer(() => {
      if (this.current.kind === "compiling" && this.current.id === id)
        this.fail(`The preview took longer than ${WATCHDOG_MS / 1000} s and was stopped.`)
    }, WATCHDOG_MS)
    this.host.post({ type: "compile", id, ...this.host.view() })
  }

  /** Fetches `hash` with a deadline, into `assets`. */
  private load(hash: string): Promise<void> {
    const abort = new AbortController()
    this.aborts.add(abort)
    return new Promise<void>((resolve, reject) => {
      const deadline = this.host.setTimer(() => {
        abort.abort()
        reject(new Error(`the server did not answer within ${FETCH_MS / 1000} s`))
      }, FETCH_MS)
      abort.signal.addEventListener("abort", () => this.host.clearTimer(deadline))
      this.host
        .fetchAsset(hash, abort.signal)
        .then((bytes) => {
          this.assets.set(hash, bytes)
          resolve()
        }, reject)
        .finally(() => {
          this.host.clearTimer(deadline)
          this.aborts.delete(abort)
        })
    })
  }

  private fetch(hashes: string[]): void {
    // A fetch that ends after a failure and a retry is not the current one.
    const id = ++this.fetches
    this.current = { kind: "fetching", id }
    const current = (): boolean => this.current.kind === "fetching" && this.current.id === id
    const unique = [...new Set(hashes)]
    const failed: string[] = []
    Promise.all(unique.map((hash) => this.load(hash).catch((error: unknown) => (failed.push(hash), Promise.reject(error))))).then(
      () => {
        if (!current()) return
        this.current = { kind: "idle" }
        this.compile()
      },
      (error: unknown) => {
        if (!current()) return
        for (const hash of failed) this.skipped.add(hash)
        this.fail(`An image could not be loaded: ${error instanceof Error ? error.message : String(error)}`)
      },
    )
  }

  /** Fetches a skipped asset without holding up the compile. */
  private fetchBeside(hash: string): void {
    if (this.background.has(hash)) return
    this.background.add(hash)
    this.load(hash).then(
      () => {
        this.background.delete(hash)
        this.skipped.delete(hash)
        if (!this.stopped) this.changed()
      },
      () => this.background.delete(hash),
    )
  }

  private fail(message: string): void {
    this.host.clearTimer(this.watchdog)
    this.host.clearTimer(this.retryTimer)
    if (this.current.kind === "fetching") for (const abort of this.aborts) abort.abort()
    this.host.restart()
    this.sent.clear()
    this.dirty = false
    const attempt = this.attempt++
    this.current = { kind: "failed", message, attempt }
    this.host.failed(message)
    this.retryTimer = this.host.setTimer(() => this.retry(), RETRY_MS[Math.min(attempt, RETRY_MS.length - 1)]!)
  }
}
