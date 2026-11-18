/**
 * The preview worker: one long-lived Typst compiler per project.
 *
 * The page sends only files that changed. After each compile the worker
 * compares page fingerprints with the previous compile and renders only the
 * pages that changed, the visible ones first. Pages that changed but are
 * off screen are rendered when the page asks for them.
 *
 * Pages are rasterised here and sent as ImageBitmaps. Inserting a text-heavy
 * page as SVG cost the main thread about 200 ms (a 400 KB document per page);
 * drawing a bitmap costs it almost nothing.
 */
import init, { JsLiveCompiler } from "../src/wasm-generated/nisaba_compile_wasm.js"

type In =
  | { type: "text"; path: string; text: string }
  | { type: "bytes"; path: string; bytes: Uint8Array }
  | { type: "remove"; path: string }
  | { type: "compile"; id: number; visible: number[]; scale: number }
  | { type: "render"; index: number; scale: number }

interface Compiled {
  pages: string[]
  sizes: [number, number][]
  problems: { severity: string; message: string; path?: string; start?: number; end?: number }[]
  ok: boolean
}

let compiler: JsLiveCompiler | undefined
const ready = init().then(() => { compiler = new JsLiveCompiler("main.typ") })
let shown: string[] = []
let queue: In[] = []
let running = false

self.onmessage = (event: MessageEvent<In>) => {
  queue.push(event.data)
  void drain()
}

async function drain(): Promise<void> {
  if (running) return
  running = true
  await ready
  while (queue.length > 0) {
    // Apply every pending write, then compile once for the newest request.
    const batch = queue
    queue = []
    let compile: Extract<In, { type: "compile" }> | undefined
    for (const msg of batch) {
      if (msg.type === "text") compiler!.write_text(msg.path, msg.text)
      else if (msg.type === "bytes") compiler!.write_bytes(msg.path, msg.bytes)
      else if (msg.type === "remove") compiler!.remove(msg.path)
      else if (msg.type === "compile") compile = msg
      else if (msg.type === "render") await render(msg.index, msg.scale)
    }
    if (compile) await run(compile)
  }
  running = false
}

async function render(index: number, scale: number): Promise<void> {
  const raw = compiler!.page_rgba(index, scale)
  if (!raw) return
  const view = new DataView(raw.buffer, raw.byteOffset, 8)
  const width = view.getUint32(0, true)
  const height = view.getUint32(4, true)
  const pixels = new Uint8ClampedArray(raw.buffer as ArrayBuffer, raw.byteOffset + 8, width * height * 4)
  const bitmap = await createImageBitmap(new ImageData(pixels, width, height))
  self.postMessage({ type: "page", index, bitmap }, { transfer: [bitmap] })
}

async function run(msg: Extract<In, { type: "compile" }>): Promise<void> {
  const t0 = performance.now()
  const result = JSON.parse(compiler!.compile()) as Compiled
  const compileMs = performance.now() - t0
  const changed = result.pages.flatMap((fp, i) => (fp === shown[i] ? [] : [i]))
  shown = result.pages
  self.postMessage({ type: "layout", id: msg.id, ...result, changed, compileMs })
  const visible = new Set(msg.visible)
  const t1 = performance.now()
  let rendered = 0
  for (const i of changed) {
    if (visible.has(i) || visible.size === 0 && i < 3) { await render(i, msg.scale); rendered++ }
  }
  self.postMessage({ type: "done", id: msg.id, compileMs, renderMs: performance.now() - t1, rendered, changed: changed.length })
}
