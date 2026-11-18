/**
 * The page view: a column of page canvases fed by the preview worker.
 *
 * Rules (DESIGN.md section 8): the preview never shows a PDF, at most one
 * compile is in flight, and a failed compile shows its problems next to the
 * last good pages, never a server compile in their place.
 */
import type { LoroDoc } from "loro-crdt"
import { metrics } from "../app/metrics"
import { projectFiles } from "../sync/files"
import type { FromWorker, Problem, ToWorker } from "./messages"

export interface PreviewEvents {
  /** The problems of the newest compile. */
  problems(problems: Problem[]): void
}

export class Preview {
  private readonly worker = new Worker(new URL("./worker.ts", import.meta.url), { type: "module" })
  private readonly sent = new Map<string, string>()
  private readonly assets = new Map<string, Uint8Array>()
  private readonly pages: HTMLDivElement[] = []
  private readonly visible = new Set<number>()
  private readonly stale = new Set<number>()
  private readonly observer: IntersectionObserver
  private inFlight = false
  private dirty = false
  private id = 0
  private typedAt = 0
  private sentAt = 0

  constructor(
    private readonly pane: HTMLElement,
    private readonly doc: LoroDoc,
    private readonly events: PreviewEvents,
    private readonly opened = performance.now(),
  ) {
    this.observer = new IntersectionObserver((entries) => this.seen(entries), { root: pane, rootMargin: "100% 0px" })
    this.worker.onmessage = (event: MessageEvent<FromWorker>) => this.receive(event.data)
  }

  /** Call after any change to the document. */
  changed(): void {
    this.compile()
  }

  destroy(): void {
    this.worker.terminate()
    this.observer.disconnect()
  }

  private post(msg: ToWorker): void {
    this.worker.postMessage(msg)
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

  private compile(): void {
    if (this.inFlight) {
      this.dirty = true
      return
    }
    this.dirty = false
    for (const file of projectFiles(this.doc)) {
      if (file.text) {
        const text = file.text.toString()
        if (this.sent.get(file.path) !== text) {
          this.sent.set(file.path, text)
          this.post({ type: "text", path: file.path, text })
        }
      } else if (file.hash && this.sent.get(file.path) !== file.hash) {
        const { path, hash } = file
        this.sent.set(path, hash)
        const cached = this.assets.get(hash)
        if (cached) {
          this.post({ type: "bytes", path, bytes: cached })
          continue
        }
        this.inFlight = true
        void fetch(`/api/assets/${hash}`)
          .then((r) => r.arrayBuffer())
          .then((b) => {
            const bytes = new Uint8Array(b)
            this.assets.set(hash, bytes)
            this.post({ type: "bytes", path, bytes })
            this.inFlight = false
            this.compile()
          })
        return
      }
    }
    this.inFlight = true
    this.typedAt = metrics.typedAt
    this.sentAt = performance.now()
    this.post({ type: "compile", id: ++this.id, visible: [...this.visible], scale: this.scale() })
  }

  private receive(msg: FromWorker): void {
    if (msg.type === "layout") {
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
      this.inFlight = false
      if (this.dirty) this.compile()
    }
  }
}
