/**
 * The source editor: CodeMirror bound to one Loro text, with undo through
 * Loro so it follows the document rather than the editor's own history.
 */
import { EditorView, basicSetup } from "codemirror"
import { LoroExtensions } from "loro-codemirror"
import { type LoroDoc, type LoroText, UndoManager } from "loro-crdt"
import { metrics } from "../app/metrics"

export class Editor {
  private view: EditorView | undefined
  private readonly undo: UndoManager

  constructor(
    private readonly parent: HTMLElement,
    private readonly doc: LoroDoc,
  ) {
    this.undo = new UndoManager(doc, {})
  }

  /** Shows `text` in the editor, replacing the file shown before. */
  open(text: LoroText): void {
    this.view?.destroy()
    this.view = new EditorView({
      parent: this.parent,
      extensions: [
        basicSetup,
        EditorView.lineWrapping,
        LoroExtensions(this.doc, undefined, this.undo, () => text),
        EditorView.domEventHandlers({ keydown: () => void (metrics.typedAt = performance.now()) }),
      ],
    })
  }

  destroy(): void {
    this.view?.destroy()
  }
}
