/**
 * What the preview sends its worker, and when: the state machine behind
 * `Preview`, kept apart from the DOM so it can be tested.
 *
 * Rules (DESIGN.md section 8):
 * - At most one compile is in flight. Edits during it mark the feed dirty
 *   and compile once more when it ends.
 * - Only changed files are sent; a file that left the document is removed,
 *   so the worker holds exactly the files an export would see.
 * - Every way a step can end clears the gate: a compile that finishes, a
 *   worker that dies, a compile that outlasts the watchdog, an asset that
 *   fails to load.
 * - A failure terminates the worker, starts a fresh one, forgets what was
 *   sent, and shows an error. The feed retries by itself with a growing
 *   delay, at once on the next edit, when the browser comes back online, or
 *   when the person presses Retry.
 */
import type { LoroDoc } from "loro-crdt"
import { projectFiles } from "../sync/files"
import type { ToWorker } from "./messages"

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
  /** Loads an asset's bytes; rejects when the server does not answer 200. */
  fetchAsset(hash: string): Promise<Uint8Array>
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
/** Delays before the automatic retries; the last one repeats. */
export const RETRY_MS = [1_000, 2_000, 5_000, 10_000, 20_000]

export class Feed {
  private current: FeedState = { kind: "idle" }
  private dirty = false
  private id = 0
  /** What the worker holds: a file's text, or `#` and an asset hash. */
  private readonly sent = new Map<string, string>()
  private readonly assets = new Map<string, Uint8Array>()
  private watchdog: unknown
  private retryTimer: unknown
  /** Failures since the last compile that worked; picks the retry delay. */
  private attempt = 0
  private fetches = 0

  constructor(private readonly host: FeedHost) {}

  get state(): FeedState {
    return this.current
  }

  /** The id of the newest compile request. */
  get compileId(): number {
    return this.id
  }

  /** Call after any change to the document. */
  changed(): void {
    if (this.current.kind === "failed") this.retry()
    else this.compile()
  }

  /** Retry now: the Retry button, the browser coming back online. */
  retry(): void {
    if (this.current.kind !== "failed") return
    this.host.clearTimer(this.retryTimer)
    this.current = { kind: "idle" }
    this.host.failed(undefined)
    this.compile()
  }

  /** The worker finished compile `id`. */
  done(id: number): void {
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
    this.host.clearTimer(this.watchdog)
    this.host.clearTimer(this.retryTimer)
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
      if (file.text !== undefined) {
        if (this.sent.get(file.path) !== file.text) {
          this.sent.set(file.path, file.text)
          this.host.post({ type: "text", path: file.path, text: file.text })
        }
      } else if (file.hash !== undefined) {
        const key = `#${file.hash}`
        if (this.sent.get(file.path) === key) continue
        const bytes = this.assets.get(file.hash)
        if (bytes) {
          this.sent.set(file.path, key)
          this.host.post({ type: "bytes", path: file.path, bytes })
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

  private fetch(hashes: string[]): void {
    // A fetch that ends after a failure and a retry is not the current one.
    const id = ++this.fetches
    this.current = { kind: "fetching", id }
    const current = (): boolean => this.current.kind === "fetching" && this.current.id === id
    const unique = [...new Set(hashes)]
    Promise.all(unique.map((hash) => this.host.fetchAsset(hash).then((bytes) => this.assets.set(hash, bytes)))).then(
      () => {
        if (!current()) return
        this.current = { kind: "idle" }
        this.compile()
      },
      (error: unknown) => {
        if (!current()) return
        this.fail(`An image could not be loaded: ${error instanceof Error ? error.message : String(error)}`)
      },
    )
  }

  private fail(message: string): void {
    this.host.clearTimer(this.watchdog)
    this.host.clearTimer(this.retryTimer)
    this.host.restart()
    this.sent.clear()
    this.dirty = false
    const attempt = this.attempt++
    this.current = { kind: "failed", message, attempt }
    this.host.failed(message)
    this.retryTimer = this.host.setTimer(() => this.retry(), RETRY_MS[Math.min(attempt, RETRY_MS.length - 1)]!)
  }
}
