/** Messages between the preview and its worker. */

export interface Problem {
  severity: "error" | "warning"
  message: string
  path?: string
  start?: number
  end?: number
}

/** What one compile produced; mirrors `Compiled` in nisaba-compile-core. */
export interface Compiled {
  pages: string[]
  sizes: [number, number][]
  problems: Problem[]
  ok: boolean
  missing_fonts: number[]
}

export type ToWorker =
  | { type: "text"; path: string; text: string }
  | { type: "bytes"; path: string; bytes: Uint8Array }
  | { type: "remove"; path: string }
  | { type: "compile"; id: number; visible: number[]; scale: number }
  | { type: "render"; index: number; scale: number }

export type FromWorker =
  | ({ type: "layout"; id: number; changed: number[] } & Compiled)
  | { type: "page"; index: number; bitmap: ImageBitmap }
  | { type: "done"; id: number; compileMs: number; renderMs: number; rendered: number; changed: number }
