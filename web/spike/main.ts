/**
 * The spike client: first-run setup, sign-in, project list, and an editor
 * with live preview. It is deliberately small; it proves the sync, storage
 * and preview paths, not the final interface.
 */
import { EditorView, basicSetup } from "codemirror"
import { LoroExtensions } from "loro-codemirror"
import { LoroMap, LoroText, UndoManager } from "loro-crdt"
import { SyncClient, type SaveState } from "./sync"

const app = document.getElementById("app")!

async function api<T>(path: string, init?: RequestInit): Promise<T> {
  const res = await fetch(path, { headers: { "content-type": "application/json" }, ...init })
  if (!res.ok) throw new Error((await res.text()) || res.statusText)
  return (res.headers.get("content-type") ?? "").includes("json") ? res.json() as Promise<T> : undefined as T
}

function html(markup: string): void {
  app.innerHTML = markup
}

function form(id: string, onSubmit: (data: FormData) => Promise<void>): void {
  const el = document.getElementById(id) as HTMLFormElement
  const error = el.querySelector(".error")!
  el.onsubmit = async (event) => {
    event.preventDefault()
    error.textContent = ""
    try { await onSubmit(new FormData(el)) } catch (e) { error.textContent = String((e as Error).message) }
  }
}

async function route(): Promise<void> {
  const session = await api<{ setup_needed: boolean; user: unknown }>("/api/session")
  const project = new URLSearchParams(location.search).get("p")
  if (session.setup_needed) return setup()
  if (!session.user) return login()
  if (project) return editor(project)
  return projects()
}

function setup(): void {
  html(`<main class="narrow"><h1>Set up Nisaba</h1>
    <p>This server has no accounts yet. The account you create here owns the server.</p>
    <form id="f"><label>Name <input name="name" autocomplete="name"></label>
    <label>Email <input name="email" type="email" required autocomplete="username"></label>
    <label>Password <input name="password" type="password" minlength="8" required autocomplete="new-password"></label>
    <button>Create account</button><p class="error" role="alert"></p></form></main>`)
  form("f", async (d) => {
    await api("/api/setup", { method: "POST", body: JSON.stringify(Object.fromEntries(d)) })
    await route()
  })
}

function login(): void {
  html(`<main class="narrow"><h1>Sign in</h1><form id="f">
    <label>Email <input name="email" type="email" required autocomplete="username"></label>
    <label>Password <input name="password" type="password" required autocomplete="current-password"></label>
    <button>Sign in</button><p class="error" role="alert"></p></form></main>`)
  form("f", async (d) => {
    await api("/api/login", { method: "POST", body: JSON.stringify(Object.fromEntries(d)) })
    await route()
  })
}

async function projects(): Promise<void> {
  const list = await api<{ id: string; name: string }[]>("/api/projects")
  html(`<main class="narrow"><h1>Projects</h1>
    ${list.length === 0 ? `<p class="empty">No projects yet. Start one below.</p>` : ""}
    <ul class="projects">${list.map((p) => `<li><a href="?p=${p.id}">${escape(p.name)}</a></li>`).join("")}</ul>
    <form id="f"><label>New project <input name="name" required placeholder="Name"></label>
    <button>Create</button><p class="error" role="alert"></p></form></main>`)
  form("f", async (d) => {
    const { id } = await api<{ id: string }>("/api/projects", { method: "POST", body: JSON.stringify({ name: d.get("name") }) })
    history.pushState(null, "", `?p=${id}`)
    await route()
  })
}

function escape(s: string): string {
  return s.replace(/[&<>"]/g, (c) => `&#${c.charCodeAt(0)};`)
}

const STATUS: Record<SaveState, string> = {
  saved: "Saved",
  saving: "Saving…",
  offline: "Offline · changes kept on this device",
  unsafe: "Not saved on this device",
  replaced: "This project was replaced. Reload to continue.",
}

interface FileView { path: string; text?: LoroText; hash?: string }

function filesOf(sync: SyncClient): FileView[] {
  const files = sync.doc.getMap("files")
  const out: FileView[] = []
  for (const key of files.keys()) {
    const entry = files.get(key) as LoroMap | undefined
    if (!entry || typeof entry.get !== "function") continue
    const path = entry.get("path") as string
    const text = entry.get("text") as LoroText | undefined
    out.push({ path, text: text ?? undefined, hash: (entry.get("hash") as string | undefined) ?? undefined })
  }
  return out.sort((a, b) => a.path.localeCompare(b.path))
}

/** Measurements the browser tests read. */
const metrics = { typedAt: 0, firstPreviewMs: -1, cycles: [] as Record<string, number>[], paint: [] as Record<string, number>[] }
;(window as unknown as { __nisaba: unknown }).__nisaba = metrics

async function editor(project: string): Promise<void> {
  const opened = performance.now()
  html(`<div class="shell"><header><a href="/">Projects</a><span id="status" role="status"></span></header>
    <nav id="files" aria-label="Files"></nav><section id="editor"></section>
    <section id="preview" aria-label="Preview"><p class="empty" id="preview-note">Preparing preview…</p></section></div>`)
  const sync = new SyncClient(project)
  const status = document.getElementById("status")!
  sync.onState((s, note) => {
    status.textContent = note ? `${STATUS[s]} (${note})` : STATUS[s]
    status.dataset.state = s
  })
  window.addEventListener("beforeunload", (event) => { if (sync.unprotected) event.preventDefault() })
  await sync.start()
  await sync.loaded

  const preview = startPreview(sync, opened)
  let view: EditorView | undefined
  const undo = new UndoManager(sync.doc, {})
  const open = (path: string): void => {
    const file = filesOf(sync).find((f) => f.path === path)
    if (!file?.text) return
    view?.destroy()
    const text = file.text
    view = new EditorView({
      parent: document.getElementById("editor")!,
      extensions: [basicSetup, EditorView.lineWrapping, LoroExtensions(sync.doc, undefined, undo, () => text),
        EditorView.domEventHandlers({ keydown: () => { metrics.typedAt = performance.now() } })],
    })
    for (const a of document.querySelectorAll("#files a")) a.toggleAttribute("aria-current", a.textContent === path)
  }
  const listFiles = (): void => {
    const nav = document.getElementById("files")!
    nav.innerHTML = filesOf(sync).map((f) => `<a href="#" data-path="${escape(f.path)}">${escape(f.path)}</a>`).join("")
    for (const a of nav.querySelectorAll<HTMLAnchorElement>("a")) a.onclick = (e) => { e.preventDefault(); open(a.dataset.path!) }
  }
  listFiles()
  open("main.typ")
  sync.doc.subscribe(() => { preview.changed() })
  preview.changed()
}

function startPreview(sync: SyncClient, opened: number): { changed(): void } {
  const worker = new Worker(new URL("./preview-worker.ts", import.meta.url), { type: "module" })
  const pane = document.getElementById("preview")!
  const note = document.getElementById("preview-note")!
  const sent = new Map<string, string>()
  const assets = new Map<string, Uint8Array>()
  let pages: HTMLDivElement[] = []
  const visible = new Set<number>()
  const stale = new Set<number>()
  let inFlight = false
  let dirty = false
  let id = 0
  let typedAt = 0
  let sentAt = 0

  // Device pixels per Typst point at the pane's current width (A4 is 595 pt).
  const scale = (): number => Math.min(4, ((pane.clientWidth - 32) / 595) * devicePixelRatio)

  const observer = new IntersectionObserver((entries) => {
    for (const entry of entries) {
      const i = Number((entry.target as HTMLElement).dataset.index)
      if (entry.isIntersecting) {
        visible.add(i)
        if (stale.delete(i)) worker.postMessage({ type: "render", index: i, scale: scale() })
      } else visible.delete(i)
    }
  }, { root: pane, rootMargin: "100% 0px" })

  const compile = (): void => {
    if (inFlight) { dirty = true; return }
    dirty = false
    for (const file of filesOf(sync)) {
      if (file.text) {
        const text = file.text.toString()
        if (sent.get(file.path) !== text) { sent.set(file.path, text); worker.postMessage({ type: "text", path: file.path, text }) }
      } else if (file.hash && sent.get(file.path) !== file.hash) {
        const { path, hash } = file
        sent.set(path, hash)
        const cached = assets.get(hash)
        if (cached) { worker.postMessage({ type: "bytes", path, bytes: cached }); continue }
        inFlight = true
        void fetch(`/api/assets/${hash}`).then((r) => r.arrayBuffer()).then((b) => {
          assets.set(hash, new Uint8Array(b))
          worker.postMessage({ type: "bytes", path, bytes: new Uint8Array(b) })
          inFlight = false
          compile()
        })
        return
      }
    }
    inFlight = true
    typedAt = metrics.typedAt
    sentAt = performance.now()
    worker.postMessage({ type: "compile", id: ++id, visible: [...visible], scale: scale() })
  }

  worker.onmessage = (event: MessageEvent) => {
    const msg = event.data as Record<string, unknown>
    if (msg.type === "layout") {
      const sizes = msg.sizes as [number, number][]
      const problems = msg.problems as { severity: string; message: string }[]
      note.textContent = msg.ok ? "" : problems.filter((p) => p.severity === "error").map((p) => p.message).join("\n")
      while (pages.length < sizes.length) {
        const div = document.createElement("div")
        div.className = "page"
        div.dataset.index = String(pages.length)
        pane.append(div)
        observer.observe(div)
        pages.push(div)
      }
      for (const div of pages.splice(sizes.length)) { observer.unobserve(div); div.remove() }
      sizes.forEach(([w, h], i) => { pages[i]!.style.aspectRatio = `${w} / ${h}` })
      for (const i of msg.changed as number[]) stale.add(i)
    } else if (msg.type === "page") {
      const i = msg.index as number
      stale.delete(i)
      const t = performance.now()
      const bitmap = msg.bitmap as ImageBitmap
      const div = pages[i]
      const pixels = bitmap.width * bitmap.height
      if (div) {
        let canvas = div.querySelector("canvas")
        if (!canvas) { canvas = document.createElement("canvas"); div.append(canvas) }
        canvas.width = bitmap.width
        canvas.height = bitmap.height
        canvas.getContext("bitmaprenderer")!.transferFromImageBitmap(bitmap)
      }
      metrics.paint.push({ pixels, domMs: performance.now() - t })
    } else if (msg.type === "done") {
      const typed = typedAt
      const sent = sentAt
      const received = performance.now()
      typedAt = 0
      // Measure to the frame that shows the new pages.
      requestAnimationFrame(() => {
        const now = performance.now()
        if (metrics.firstPreviewMs < 0 && pages.length > 0) metrics.firstPreviewMs = now - opened
        if (typed > 0) {
          metrics.cycles.push({ keystrokeToPaint: now - typed, toSend: sent - typed, workerRoundTrip: received - sent, toFrame: now - received, compileMs: msg.compileMs as number,
            renderMs: msg.renderMs as number, rendered: msg.rendered as number, changed: msg.changed as number })
        }
      })
      inFlight = false
      if (dirty) compile()
    }
  }
  return { changed: compile }
}

void route()
window.addEventListener("popstate", () => void route())
