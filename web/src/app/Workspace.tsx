/**
 * The workspace screen: the file list, the editor and the preview of one
 * project. The editor, the preview and the sync client are imperative
 * modules; this component mounts them and turns their events into signals.
 */
import { For, Show, createSignal, onCleanup, onMount, type JSX } from "solid-js"
import { Editor } from "../editor/editor"
import type { Problem } from "../preview/messages"
import { Preview } from "../preview/preview"
import { SyncClient, type SaveState } from "../sync/client"
import { projectFiles, type ProjectFile } from "../sync/files"

const STATUS: Record<SaveState, string> = {
  saved: "Saved",
  saving: "Saving…",
  offline: "Offline · changes kept on this device",
  unsafe: "Not saved on this device",
  stopped: "Stopped",
}

export function Workspace(props: { project: string }): JSX.Element {
  const opened = performance.now()
  const sync = new SyncClient(props.project)
  const [state, setState] = createSignal<{ state: SaveState; note?: string | undefined }>({ state: sync.state() })
  const [files, setFiles] = createSignal<ProjectFile[]>([])
  const [current, setCurrent] = createSignal("main.typ")
  const [problems, setProblems] = createSignal<Problem[] | undefined>()
  const [openFile, setOpen] = createSignal<(path: string) => void>(() => {})
  let editorEl!: HTMLElement
  let previewEl!: HTMLElement

  const beforeUnload = (event: BeforeUnloadEvent): void => {
    if (sync.unprotected) event.preventDefault()
  }
  window.addEventListener("beforeunload", beforeUnload)
  const unsubscribe = sync.subscribe((s, note) => setState({ state: s, note }))

  onMount(async () => {
    await sync.start()
    await sync.loaded
    const editor = new Editor(editorEl, sync.doc)
    const preview = new Preview(previewEl, sync.doc, { problems: setProblems }, opened)
    const open = (path: string): void => {
      const text = projectFiles(sync.doc).find((f) => f.path === path)?.text
      if (!text) return
      editor.open(text)
      setCurrent(path)
    }
    const refresh = (): void => {
      setFiles(projectFiles(sync.doc))
      preview.changed()
    }
    const unsubscribeDoc = sync.doc.subscribe(refresh)
    refresh()
    open(current())
    setOpen(() => open)
    onCleanup(() => {
      unsubscribeDoc()
      editor.destroy()
      preview.destroy()
    })
  })
  onCleanup(() => {
    unsubscribe()
    window.removeEventListener("beforeunload", beforeUnload)
  })

  const errors = (): string =>
    (problems() ?? [])
      .filter((p) => p.severity === "error")
      .map((p) => p.message)
      .join("\n")

  return (
    <div class="shell">
      <header>
        <a href="/">Projects</a>
        <span id="status" role="status" data-state={state().state}>
          {state().note ? `${STATUS[state().state]} (${state().note})` : STATUS[state().state]}
        </span>
      </header>
      <nav id="files" aria-label="Files">
        <For each={files()}>
          {(f) => (
            <a
              href="#"
              aria-current={f.path === current() ? "true" : undefined}
              onClick={(e) => (e.preventDefault(), openFile()(f.path))}
            >
              {f.path}
            </a>
          )}
        </For>
      </nav>
      <section id="editor" ref={(el) => (editorEl = el)} />
      <section id="preview" aria-label="Preview" ref={(el) => (previewEl = el)}>
        <Show when={problems()} fallback={<p class="empty note">Preparing preview…</p>}>
          <Show when={errors()}>
            <p class="note">{errors()}</p>
          </Show>
        </Show>
      </section>
    </div>
  )
}
