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

/** Milliseconds from `now` to just after the next local midnight, when
 * `datetime.today()` changes. */
export function untilMidnight(now = new Date()): number {
  const next = new Date(now.getFullYear(), now.getMonth(), now.getDate() + 1, 0, 0, 1)
  return next.getTime() - now.getTime()
}

/** Fetches a URL and fails unless the server answers 200. */
export async function fetchOk(url: string, signal?: AbortSignal): Promise<Response> {
  const response = await fetch(url, signal ? { signal } : {})
  if (!response.ok) throw new Error(`${url} answered ${response.status}`)
  return response
}
