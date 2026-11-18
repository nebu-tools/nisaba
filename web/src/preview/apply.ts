/** Applies a file message to a compiler; shared by the worker and its tests. */
import type { JsLiveCompiler } from "../wasm-generated/nisaba_compile_wasm.js"
import type { ToWorker } from "./messages"

export type FileMessage = Extract<ToWorker, { type: "text" | "bytes" | "remove" }>

export function applyFile(compiler: JsLiveCompiler, msg: FileMessage): void {
  if (msg.type === "text") compiler.write_text(msg.path, msg.text)
  else if (msg.type === "bytes") compiler.write_bytes(msg.path, msg.bytes)
  else compiler.remove(msg.path)
}

/** Sets the compiler's date to the local calendar date (DESIGN.md section 8). */
export function setLocalToday(compiler: JsLiveCompiler, now = new Date()): void {
  compiler.set_today(now.getFullYear(), now.getMonth() + 1, now.getDate())
}

/** Fetches a URL and fails unless the server answers 200. */
export async function fetchOk(url: string): Promise<Response> {
  const response = await fetch(url)
  if (!response.ok) throw new Error(`${url} answered ${response.status}`)
  return response
}
