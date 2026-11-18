/**
 * The page view: a column of page canvases fed by the preview worker.
 *
 * Rules (DESIGN.md section 8): the preview never shows a PDF, and a failed
 * compile shows its problems next to the last good pages, never a server
 * compile in their place. What is sent to the worker and when is decided by
 * `Feed` (feed.ts); this class owns the worker, the DOM and the metrics.
 */
import type { LoroDoc } from "loro-crdt"
import { metrics } from "../app/metrics"
import { fetchOk } from "./apply"
import { Feed, fileStates } from "./feed"
import type { FromWorker, Problem, ToWorker } from "./messages"

export interface PreviewEvents {
  /** The problems of the newest compile. */
  problems(problems: Problem[]): void
  /** The preview failed and retries by itself, or it recovered (`undefined`). */
  failed(message: string | undefined): void
}

export class Preview {
  private worker: Worker | undefined
  private readonly feed: Feed
  private readonly pages: HTMLDivElement[] = []
  private readonly visible = new Set<number>()
  private readonly stale = new Set<number>()
  private readonly observer: IntersectionObserver
  private readonly online = (): void => this.feed.retry()
  private typedAt = 0
  private sentAt = 0

  constructor(
    private readonly pane: HTMLElement,
    private readonly doc: LoroDoc,
    project: string,
    private readonly events: PreviewEvents,
    private readonly opened = performance.now(),
  ) {
    this.observer = new IntersectionObserver((entries) => this.seen(entries), { root: pane, rootMargin: "100% 0px" })
    this.feed = new Feed({
      files: () => fileStates(this.doc),
      post: (msg) => this.post(msg),
      restart: () => this.start(),
      fetchAsset: async (hash) =>
        new Uint8Array(await (await fetchOk(`/api/projects/${project}/assets/${hash}`)).arrayBuffer()),
      view: () => ({ visible: [...this.visible], scale: this.scale() }),
      failed: (message) => this.events.failed(message),
      setTimer: (fn, ms) => setTimeout(fn, ms),
      clearTimer: (handle) => clearTimeout(handle as ReturnType<typeof setTimeout> | undefined),
    })
    this.start()
    window.addEventListener("online", this.online)
  }

  /** Call after any change to the document. */
  changed(): void {
    this.feed.changed()
  }

  /** Retries at once after a failure. */
  retry(): void {
    this.feed.retry()
  }

  destroy(): void {
    this.feed.stop()
    this.worker?.terminate()
    this.observer.disconnect()
    window.removeEventListener("online", this.online)
  }

  /** Starts a fresh worker in place of the current one. */
  private start(): void {
    this.worker?.terminate()
    const worker = new Worker(new URL("./worker.ts", import.meta.url), { type: "module" })
    worker.onmessage = (event: MessageEvent<FromWorker>) => this.receive(event.data)
    worker.onerror = (event) => {
      event.preventDefault()
      this.feed.workerFailed(event.message || "the worker crashed")
    }
    worker.onmessageerror = () => this.feed.workerFailed("a message from the worker could not be read")
    this.worker = worker
    // The new worker has drawn nothing yet.
    for (let i = 0; i < this.pages.length; i++) this.stale.add(i)
  }

  private post(msg: ToWorker): void {
    if (msg.type === "compile") {
      this.typedAt = metrics.typedAt
      this.sentAt = performance.now()
    }
    this.worker?.postMessage(msg)
  }

  /** Device pixels per Typst point at the pane's width (A4 is 595 pt wide). */
  private scale(): number {
    return Math.min(4, ((this.pane.clientWidth - 32) / 595) * devicePixelRatio)
  }

  private seen(entries: IntersectionObserverEntry[]): void {
    for (const entry of entries) {
      const i = Number((entry.target as HTMLElement).dataset.index)
      if (entry.isIntersecting) {
        this.visible.add(i)
        if (this.stale.delete(i)) this.post({ type: "render", index: i, scale: this.scale() })
      } else this.visible.delete(i)
    }
  }

  private receive(msg: FromWorker): void {
    if (msg.type === "failed") {
      this.feed.workerFailed(msg.message)
    } else if (msg.type === "layout") {
      this.events.problems(msg.problems)
      while (this.pages.length < msg.sizes.length) {
        const div = document.createElement("div")
        div.className = "page"
        div.dataset.index = String(this.pages.length)
        this.pane.append(div)
        this.observer.observe(div)
        this.pages.push(div)
      }
      for (const div of this.pages.splice(msg.sizes.length)) {
        this.observer.unobserve(div)
        div.remove()
      }
      msg.sizes.forEach(([w, h], i) => (this.pages[i]!.style.aspectRatio = `${w} / ${h}`))
      for (const i of msg.changed) this.stale.add(i)
    } else if (msg.type === "page") {
      this.stale.delete(msg.index)
      const t = performance.now()
      const { bitmap } = msg
      const pixels = bitmap.width * bitmap.height
      const div = this.pages[msg.index]
      if (div) {
        let canvas = div.querySelector("canvas")
        if (!canvas) {
          canvas = document.createElement("canvas")
          div.append(canvas)
        }
        canvas.width = bitmap.width
        canvas.height = bitmap.height
        canvas.getContext("bitmaprenderer")!.transferFromImageBitmap(bitmap)
      }
      metrics.paint.push({ pixels, domMs: performance.now() - t })
    } else {
      const typed = this.typedAt
      const sent = this.sentAt
      const received = performance.now()
      this.typedAt = 0
      // Measure to the frame that shows the new pages.
      requestAnimationFrame(() => {
        const now = performance.now()
        if (metrics.firstPreviewMs < 0 && this.pages.length > 0) metrics.firstPreviewMs = now - this.opened
        if (typed > 0) {
          metrics.cycles.push({
            keystrokeToPaint: now - typed,
            toSend: sent - typed,
            workerRoundTrip: received - sent,
            toFrame: now - received,
            compileMs: msg.compileMs,
            renderMs: msg.renderMs,
            rendered: msg.rendered,
            changed: msg.changed,
          })
        }
      })
      this.feed.done(msg.id)
    }
  }
}
