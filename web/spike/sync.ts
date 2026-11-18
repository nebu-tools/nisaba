/**
 * Sync protocol 3 with an IndexedDB copy of every update.
 *
 * Rules:
 * - Every change, local or remote, is written to IndexedDB as it happens.
 * - The server answers each update with an Ack carrying its version vector.
 *   A change is saved when an acknowledged version vector includes it.
 * - On every (re)connect the client pushes everything the server lacks,
 *   computed from the server's version vector, never from a send queue.
 */
import { LoroDoc, VersionVector } from "loro-crdt"

const HELLO = 1, WELCOME = 2, UPDATE = 3, SNAPSHOT = 4, ERROR = 7, ACK = 9
const PROTOCOL = 3

export type SaveState = "saved" | "saving" | "offline" | "unsafe" | "replaced"

function bytes(out: number[], b: Uint8Array): void {
  const n = b.length
  out.push((n >>> 24) & 255, (n >>> 16) & 255, (n >>> 8) & 255, n & 255)
  for (const x of b) out.push(x)
}

function hello(project: string, peer: bigint, lastVV: Uint8Array): Uint8Array<ArrayBuffer> {
  const out: number[] = [HELLO, PROTOCOL]
  bytes(out, new TextEncoder().encode(project))
  for (let i = 7; i >= 0; i--) out.push(Number((peer >> BigInt(i * 8)) & 255n))
  bytes(out, new Uint8Array())
  bytes(out, lastVV)
  return new Uint8Array(out)
}

function update(b: Uint8Array): Uint8Array<ArrayBuffer> {
  const out = new Uint8Array(5 + b.length)
  out[0] = UPDATE
  new DataView(out.buffer).setUint32(1, b.length)
  out.set(b, 5)
  return out
}

function blob(view: DataView, at: number): [Uint8Array, number] {
  const n = view.getUint32(at)
  return [new Uint8Array(view.buffer, view.byteOffset + at + 4, n), at + 4 + n]
}

/** The device copy: a list of Loro blobs per project, compacted on open. */
class DeviceLog {
  private constructor(private db: IDBDatabase, private project: string) {}

  static async open(project: string): Promise<DeviceLog> {
    const db = await new Promise<IDBDatabase>((resolve, reject) => {
      const req = indexedDB.open("nisaba", 1)
      req.onupgradeneeded = () => {
        const store = req.result.createObjectStore("log", { autoIncrement: true })
        store.createIndex("project", "project")
      }
      req.onsuccess = () => resolve(req.result)
      req.onerror = () => reject(req.error)
    })
    return new DeviceLog(db, project)
  }

  private done(tx: IDBTransaction): Promise<void> {
    return new Promise((resolve, reject) => {
      tx.oncomplete = () => resolve()
      tx.onerror = tx.onabort = () => reject(tx.error)
    })
  }

  async load(): Promise<{ keys: IDBValidKey[]; blobs: Uint8Array[] }> {
    const tx = this.db.transaction("log")
    const index = tx.objectStore("log").index("project")
    const [keys, rows] = await Promise.all([
      new Promise<IDBValidKey[]>((r) => { const q = index.getAllKeys(this.project); q.onsuccess = () => r(q.result) }),
      new Promise<{ bytes: Uint8Array }[]>((r) => { const q = index.getAll(this.project); q.onsuccess = () => r(q.result) }),
    ])
    return { keys, blobs: rows.map((row) => row.bytes) }
  }

  append(b: Uint8Array): Promise<void> {
    const tx = this.db.transaction("log", "readwrite")
    tx.objectStore("log").add({ project: this.project, bytes: b.slice() })
    return this.done(tx)
  }

  /** Replaces the rows in `keys` with one snapshot, atomically. */
  compact(keys: IDBValidKey[], snapshot: Uint8Array): Promise<void> {
    const tx = this.db.transaction("log", "readwrite")
    const store = tx.objectStore("log")
    for (const key of keys) store.delete(key)
    store.add({ project: this.project, bytes: snapshot })
    return this.done(tx)
  }

  /** Forgets this project's device copy. */
  clear(keys: IDBValidKey[]): Promise<void> {
    const tx = this.db.transaction("log", "readwrite")
    for (const key of keys) tx.objectStore("log").delete(key)
    return this.done(tx)
  }
}

export class SyncClient {
  readonly doc = new LoroDoc()
  private acked: VersionVector | undefined
  private ws: WebSocket | undefined
  private device!: DeviceLog
  private pendingWrites = 0
  private deviceFailed = false
  private retry = 0
  private stopped = false
  private listeners = new Set<(s: SaveState, note?: string) => void>()
  private ready!: () => void
  /** Resolves when the document has content: from this device or the server. */
  readonly loaded = new Promise<void>((r) => { this.ready = r })

  constructor(private project: string) {}

  async start(): Promise<void> {
    const peer = BigInt(Math.floor(Math.random() * 2 ** 52)) * 2n + 1n
    this.doc.setPeerId(peer)
    try {
      this.device = await DeviceLog.open(this.project)
      const { keys, blobs } = await this.device.load()
      if (blobs.length > 0) {
        this.doc.importBatch(blobs)
        if (blobs.length > 50) await this.device.compact(keys, this.doc.export({ mode: "snapshot" }))
        this.ready()
      }
    } catch {
      this.deviceFailed = true
    }
    this.doc.subscribeLocalUpdates((b) => {
      this.keep(b)
      if (this.ws?.readyState === WebSocket.OPEN && this.acked) this.ws.send(update(b))
      this.emit()
    })
    this.connect(peer)
  }

  onState(f: (s: SaveState, note?: string) => void): void {
    this.listeners.add(f)
    f(this.state())
  }

  state(): SaveState {
    if (this.stopped) return "replaced"
    const local = this.doc.oplogVersion()
    const cmp = this.acked ? local.compare(this.acked) : undefined
    if (cmp === 0 || cmp === -1) return "saved"
    if (this.deviceFailed) return "unsafe"
    return this.ws?.readyState === WebSocket.OPEN && this.acked ? "saving" : "offline"
  }

  private emit(note?: string): void {
    const s = this.state()
    for (const f of this.listeners) f(s, note)
  }

  private keep(b: Uint8Array): void {
    if (this.deviceFailed) return
    this.pendingWrites++
    this.device.append(b).catch(() => { this.deviceFailed = true }).finally(() => {
      this.pendingWrites--
      this.emit()
    })
  }

  private connect(peer: bigint): void {
    if (this.stopped) return
    const url = `${location.protocol === "https:" ? "wss" : "ws"}://${location.host}/sync/${this.project}`
    const ws = new WebSocket(url)
    ws.binaryType = "arraybuffer"
    this.ws = ws
    ws.onopen = () => {
      this.retry = 0
      ws.send(hello(this.project, peer, this.doc.oplogVersion().encode()))
    }
    ws.onmessage = (event) => this.receive(new Uint8Array(event.data as ArrayBuffer))
    ws.onclose = (event) => {
      this.acked = undefined
      if (event.code === 4409 || event.code === 4400) {
        this.stopped = true
        this.emit(event.reason)
        return
      }
      this.emit()
      const delay = Math.min(30_000, 500 * 2 ** this.retry++) * (0.5 + Math.random())
      setTimeout(() => this.connect(peer), delay)
    }
  }

  private receive(frame: Uint8Array): void {
    const view = new DataView(frame.buffer, frame.byteOffset, frame.byteLength)
    switch (frame[0]) {
      case WELCOME: {
        const [, at] = blob(view, 2)
        if (frame[at] === 1 || frame[at] === 2) {
          const [b] = blob(view, at + 1)
          this.doc.import(b)
          this.keep(b)
        }
        break
      }
      case UPDATE:
      case SNAPSHOT: {
        const [b] = blob(view, 1)
        this.doc.import(b)
        this.keep(b)
        break
      }
      case ACK: {
        const first = this.acked === undefined
        const [b] = blob(view, 1)
        this.acked = VersionVector.decode(b)
        if (first) {
          this.ready()
          // Push whatever the server lacks: edits made offline or lost in flight.
          const missing = this.doc.export({ mode: "update", from: this.acked })
          const cmp = this.doc.oplogVersion().compare(this.acked)
          if (cmp !== 0 && cmp !== -1) this.ws?.send(update(missing))
        }
        break
      }
      case ERROR: {
        const [msg] = blob(view, 3)
        this.emit(new TextDecoder().decode(msg))
        break
      }
    }
    this.emit()
  }

  /** True while some change exists only in memory. */
  get unprotected(): boolean {
    return this.state() !== "saved" && (this.pendingWrites > 0 || this.deviceFailed)
  }
}
