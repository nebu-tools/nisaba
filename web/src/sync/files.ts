/**
 * How a project lives in its Loro document: a root `files` map keyed by a
 * random file id, each entry a map with a `path` and either a `text`
 * container or the `hash` of an asset.
 */
import type { LoroDoc, LoroMap, LoroText } from "loro-crdt"

export interface ProjectFile {
  path: string
  text?: LoroText
  hash?: string
}

/** Every file of the project, sorted by path. */
export function projectFiles(doc: LoroDoc): ProjectFile[] {
  const files = doc.getMap("files")
  const out: ProjectFile[] = []
  for (const key of files.keys()) {
    const entry = files.get(key) as LoroMap | undefined
    if (!entry || typeof entry.get !== "function") continue
    const path = entry.get("path")
    if (typeof path !== "string") continue
    const text = entry.get("text") as LoroText | undefined
    const hash = entry.get("hash") as string | undefined
    out.push({ path, ...(text ? { text } : {}), ...(hash ? { hash } : {}) })
  }
  return out.sort((a, b) => a.path.localeCompare(b.path))
}
