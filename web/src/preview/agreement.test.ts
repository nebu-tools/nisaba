/**
 * The preview and the export see the same files. The export compiles every
 * file of the document from scratch; the preview feeds a long-lived compiler
 * only what changed. After any sequence of creates, edits, renames, deletes
 * and swaps between text and asset, both must give the same result. This
 * drives the real compiler and the real document.
 */
import { readFileSync } from "node:fs"
import { LoroDoc, LoroMap, LoroText } from "loro-crdt/nodejs"
import { beforeAll, describe, expect, it } from "vitest"
import { JsLiveCompiler, initSync } from "../wasm-generated/nisaba_compile_wasm.js"
import { applyFile, type FileMessage } from "./apply"
import { Feed, type FileState, fileStates } from "./feed"
import type { Compiled, ToWorker } from "./messages"

const here = (path: string): URL => new URL(path, import.meta.url)
const fontsJson = readFileSync(here("../../public/fonts/fonts.json"), "utf8")
const catalogue = JSON.parse(fontsJson) as { file: string }[]

beforeAll(() => {
  initSync({ module: readFileSync(here("../wasm-generated/nisaba_compile_wasm_bg.wasm")) })
})

function compile(compiler: JsLiveCompiler): Compiled {
  compiler.set_today(2026, 1, 2)
  for (;;) {
    const result = JSON.parse(compiler.compile()) as Compiled
    if (result.missing_fonts.length === 0) return result
    for (const i of result.missing_fonts)
      compiler.add_font(i, readFileSync(here(`../../public/fonts/${catalogue[i]!.file}`)))
  }
}

/** Asset bytes by hash. A hash names its bytes, as on the server. */
const assetBytes = (hash: string): Uint8Array => new TextEncoder().encode(`asset ${hash}`)

/** What the export does: every file, into a fresh compiler. */
function exported(files: FileState[]): Compiled {
  const compiler = new JsLiveCompiler("main.typ", fontsJson)
  for (const f of files) {
    if (f.text !== undefined) compiler.write_text(f.path, f.text)
    else if (f.hash !== undefined) compiler.write_bytes(f.path, assetBytes(f.hash))
  }
  return compile(compiler)
}

/**
 * What a compile shows: the error messages, and the page fingerprints when it
 * worked. (A failed preview keeps showing its last good pages.)
 */
const outcome = (c: Compiled): { ok: boolean; pages: string[]; errors: string[] } => ({
  ok: c.ok,
  pages: c.ok ? c.pages : [],
  errors: c.problems.filter((p) => p.severity === "error").map((p) => p.message),
})

/** The preview: a feed whose worker is a live compiler in this thread. */
function preview(doc: LoroDoc): { compile(): Promise<Compiled> } {
  const compiler = new JsLiveCompiler("main.typ", fontsJson)
  let pending: Extract<ToWorker, { type: "compile" }> | undefined
  const feed = new Feed({
    files: () => fileStates(doc),
    post: (msg) => {
      if (msg.type === "compile") pending = msg
      else if (msg.type !== "render") applyFile(compiler, msg satisfies FileMessage)
    },
    restart: () => {
      throw new Error("the worker should not restart")
    },
    fetchAsset: (hash) => Promise.resolve(assetBytes(hash)),
    view: () => ({ visible: [], scale: 1 }),
    failed: (message) => {
      if (message) throw new Error(message)
    },
    setTimer: () => undefined,
    clearTimer: () => {},
  })
  return {
    async compile() {
      feed.changed()
      // An asset arrives after a turn of the event loop.
      for (let i = 0; !pending && i < 10; i++) await new Promise((r) => setTimeout(r, 0))
      if (!pending) throw new Error("the feed sent no compile")
      const result = compile(compiler)
      feed.done(pending.id)
      pending = undefined
      return result
    },
  }
}

function addText(doc: LoroDoc, path: string, text: string): LoroMap {
  const entry = doc.getMap("files").setContainer(`id-${path}`, new LoroMap())
  entry.set("path", path)
  entry.setContainer("text", new LoroText()).insert(0, text)
  doc.commit()
  return entry
}

describe("preview and export", () => {
  it("agree after a rename and after a delete", async () => {
    const doc = new LoroDoc()
    addText(doc, "main.typ", '#include "part.typ"\n= Title')
    const part = addText(doc, "part.typ", "Part one.")
    const live = preview(doc)

    const first = (await live.compile())
    expect(first.ok).toBe(true)
    expect(outcome(first)).toEqual(outcome(exported(fileStates(doc))))

    // Rename: the old path must be gone from the preview too.
    part.set("path", "renamed.typ")
    doc.commit()
    const afterRename = outcome((await live.compile()))
    expect(afterRename.ok).toBe(false)
    expect(afterRename.errors.join()).toMatch(/part\.typ/)
    expect(afterRename).toEqual(outcome(exported(fileStates(doc))))

    // Point main.typ at the new name: both compile again.
    addText(doc, "main.typ", '#include "renamed.typ"\n= Title')
    expect(outcome((await live.compile()))).toEqual(outcome(exported(fileStates(doc))))

    // Delete the included file.
    doc.getMap("files").delete("id-part.typ")
    doc.commit()
    const afterDelete = outcome((await live.compile()))
    expect(afterDelete.ok).toBe(false)
    expect(afterDelete).toEqual(outcome(exported(fileStates(doc))))
  })
})

/** A small deterministic generator, so a failure can be replayed by seed. */
function random(seed: number): (n: number) => number {
  let x = seed * 2654435761 + 1
  return (n) => {
    x ^= x << 13
    x ^= x >>> 17
    x ^= x << 5
    return (x >>> 0) % n
  }
}

const PATHS = ["a.txt", "b.txt", "c.txt"]
/** Texts that look like what the old feed used for assets. */
const TEXTS = ["one", "two", "#h1", "#h2", "asset h1"]
const HASHES = ["h1", "h2"]

describe("preview and export over generated sequences", () => {
  it("agree after every step", async () => {
    for (let seed = 1; seed <= 10; seed++) {
      const pick = random(seed)
      const doc = new LoroDoc()
      addText(doc, "main.typ", PATHS.map((p) => `#read("${p}")`).join("\n"))
      const files = doc.getMap("files")
      const live = preview(doc)
      const steps: string[] = []
      let next = 0
      for (let step = 0; step < 10; step++) {
        const entries = fileStates(doc).filter((f) => f.path !== "main.typ")
        const id = (path: string): string => files.keys().find((k) => (files.get(k) as LoroMap).get("path") === path)!
        const free = PATHS.filter((p) => !entries.some((e) => e.path === p))
        const op = pick(6)
        const target = entries[pick(Math.max(1, entries.length))]
        if ((op === 0 || !target) && free.length > 0) {
          const path = free[pick(free.length)]!
          const entry = files.setContainer(`id-${next++}`, new LoroMap())
          entry.set("path", path)
          if (pick(2) === 0) {
            const text = TEXTS[pick(TEXTS.length)]!
            entry.setContainer("text", new LoroText()).insert(0, text)
            steps.push(`create ${path} text ${text}`)
          } else {
            const hash = HASHES[pick(HASHES.length)]!
            entry.set("hash", hash)
            steps.push(`create ${path} asset ${hash}`)
          }
        } else if (!target) {
          continue
        } else if (op === 1 && target.text !== undefined) {
          const text = (files.get(id(target.path)) as LoroMap).get("text") as LoroText
          const extra = TEXTS[pick(TEXTS.length)]!
          text.insert(text.length, extra)
          steps.push(`edit ${target.path} +${extra}`)
        } else if (op === 2 && free.length > 0) {
          const path = free[pick(free.length)]!
          ;(files.get(id(target.path)) as LoroMap).set("path", path)
          steps.push(`rename ${target.path} ${path}`)
        } else if (op === 3) {
          files.delete(id(target.path))
          steps.push(`delete ${target.path}`)
        } else if (target.hash !== undefined) {
          const entry = files.get(id(target.path)) as LoroMap
          entry.delete("hash")
          // The text the old feed mistook for this asset.
          const text = pick(2) === 0 ? `#${target.hash}` : TEXTS[pick(TEXTS.length)]!
          entry.setContainer("text", new LoroText()).insert(0, text)
          steps.push(`asset to text ${target.path} ${text}`)
        } else {
          const entry = files.get(id(target.path)) as LoroMap
          entry.delete("text")
          const hash = target.text?.startsWith("#") ? target.text.slice(1) : HASHES[pick(HASHES.length)]!
          entry.set("hash", hash)
          steps.push(`text to asset ${target.path} ${hash}`)
        }
        doc.commit()
        const got = outcome(await live.compile())
        expect(got, `seed ${seed}: ${steps.join("; ")}`).toEqual(outcome(exported(fileStates(doc))))
      }
    }
  }, 120_000)
})
