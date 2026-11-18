/**
 * The preview and the export see the same files. The export compiles every
 * file of the document from scratch; the preview feeds a long-lived compiler
 * only what changed. After a rename and after a delete both must give the
 * same result. This drives the real compiler and the real document.
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

/** What the export does: every file, into a fresh compiler. */
function exported(files: FileState[]): Compiled {
  const compiler = new JsLiveCompiler("main.typ", fontsJson)
  for (const f of files) if (f.text !== undefined) compiler.write_text(f.path, f.text)
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
function preview(doc: LoroDoc): { compile(): Compiled } {
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
    fetchAsset: () => Promise.reject(new Error("no assets here")),
    view: () => ({ visible: [], scale: 1 }),
    failed: (message) => {
      if (message) throw new Error(message)
    },
    setTimer: () => undefined,
    clearTimer: () => {},
  })
  return {
    compile() {
      feed.changed()
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
  it("agree after a rename and after a delete", () => {
    const doc = new LoroDoc()
    addText(doc, "main.typ", '#include "part.typ"\n= Title')
    const part = addText(doc, "part.typ", "Part one.")
    const live = preview(doc)

    const first = live.compile()
    expect(first.ok).toBe(true)
    expect(outcome(first)).toEqual(outcome(exported(fileStates(doc))))

    // Rename: the old path must be gone from the preview too.
    part.set("path", "renamed.typ")
    doc.commit()
    const afterRename = outcome(live.compile())
    expect(afterRename.ok).toBe(false)
    expect(afterRename.errors.join()).toMatch(/part\.typ/)
    expect(afterRename).toEqual(outcome(exported(fileStates(doc))))

    // Point main.typ at the new name: both compile again.
    addText(doc, "main.typ", '#include "renamed.typ"\n= Title')
    expect(outcome(live.compile())).toEqual(outcome(exported(fileStates(doc))))

    // Delete the included file.
    doc.getMap("files").delete("id-part.typ")
    doc.commit()
    const afterDelete = outcome(live.compile())
    expect(afterDelete.ok).toBe(false)
    expect(afterDelete).toEqual(outcome(exported(fileStates(doc))))
  })
})
