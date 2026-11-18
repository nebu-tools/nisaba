/**
 * The preview worker: one long-lived Typst compiler per project.
 *
 * The page sends only files that changed. After each compile the worker
 * compares page fingerprints with the previous compile and renders only the
 * pages that changed, the visible ones first. Pages that changed off screen
 * are rendered when the page asks for them.
 *
 * Fonts are separate files. The compiler starts with only their catalogue;
 * when a compile asks for a font it does not have, the worker fetches that
 * file and compiles again. The browser caches font files for a year.
 *
 * Pages are rasterised here and sent as ImageBitmaps, which cost the main
 * thread almost nothing to draw.
 *
 * The worker does not retry. When it cannot start, a font does not load or
 * the compiler throws, it says `failed` and the page replaces it with a fresh
 * worker (see feed.ts).
 */
import init, { JsLiveCompiler } from "../wasm-generated/nisaba_compile_wasm.js"
import { applyFile, fetchOk, setLocalToday } from "./apply"
import type { Compiled, FromWorker, ToWorker } from "./messages"

interface FontEntry {
  file: string
}

let catalogue: FontEntry[] = []
const ready: Promise<JsLiveCompiler> = (async () => {
  const [, fonts] = await Promise.all([init(), fetchOk("/fonts/fonts.json").then((r) => r.text())])
  catalogue = JSON.parse(fonts) as FontEntry[]
  return new JsLiveCompiler("main.typ", fonts)
})()
let shown: string[] = []
let queue: ToWorker[] = []
let running = false
let broken = false

const post = (msg: FromWorker, transfer: Transferable[] = []): void => self.postMessage(msg, { transfer })

self.onmessage = (event: MessageEvent<ToWorker>) => {
  queue.push(event.data)
  void drain()
}

async function drain(): Promise<void> {
  if (running || broken) return
  running = true
  try {
    const compiler = await ready
    while (queue.length > 0) {
      // Apply every pending write, then compile once for the newest request.
      const batch = queue
      queue = []
      let compile: Extract<ToWorker, { type: "compile" }> | undefined
      for (const msg of batch) {
        if (msg.type === "compile") compile = msg
        else if (msg.type === "render") await render(compiler, msg.index, msg.scale)
        else applyFile(compiler, msg)
      }
      if (compile) await run(compiler, compile)
    }
  } catch (error) {
    broken = true
    post({ type: "failed", message: error instanceof Error ? error.message : String(error) })
  } finally {
    running = false
  }
}

/** Compiles until every font the document asks for is loaded. */
async function compileWithFonts(compiler: JsLiveCompiler): Promise<Compiled> {
  for (;;) {
    setLocalToday(compiler)
    const result = JSON.parse(compiler.compile()) as Compiled
    if (result.missing_fonts.length === 0) return result
    const files = await Promise.all(
      result.missing_fonts.map((i) => fetchOk(`/fonts/${catalogue[i]!.file}`).then((r) => r.arrayBuffer())),
    )
    result.missing_fonts.forEach((index, n) => compiler.add_font(index, new Uint8Array(files[n]!)))
  }
}

async function render(compiler: JsLiveCompiler, index: number, scale: number): Promise<void> {
  const raw = compiler.page_rgba(index, scale)
  if (!raw) return
  const view = new DataView(raw.buffer, raw.byteOffset, 8)
  const width = view.getUint32(0, true)
  const height = view.getUint32(4, true)
  const pixels = new Uint8ClampedArray(raw.buffer as ArrayBuffer, raw.byteOffset + 8, width * height * 4)
  const bitmap = await createImageBitmap(new ImageData(pixels, width, height))
  post({ type: "page", index, bitmap }, [bitmap])
}

async function run(compiler: JsLiveCompiler, msg: Extract<ToWorker, { type: "compile" }>): Promise<void> {
  const t0 = performance.now()
  const result = await compileWithFonts(compiler)
  const compileMs = performance.now() - t0
  const changed = result.pages.flatMap((fp, i) => (fp === shown[i] ? [] : [i]))
  shown = result.pages
  post({ type: "layout", id: msg.id, ...result, changed })
  const visible = new Set(msg.visible)
  const t1 = performance.now()
  let rendered = 0
  for (const i of changed) {
    if (visible.has(i) || (visible.size === 0 && i < 3)) {
      await render(compiler, i, msg.scale)
      rendered++
    }
  }
  post({ type: "done", id: msg.id, compileMs, renderMs: performance.now() - t1, rendered, changed: changed.length })
}
