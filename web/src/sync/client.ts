/**
 * The sync client: one Loro document per open project, kept on this device
 * and on the server.
 *
 * Rules:
 * - Every change, local or remote, goes to the device log as it happens.
 * - A change is saved when a version vector the server sent in Welcome or
 *   Ack includes it. The server sends those only for durable updates.
 * - On every connect the client sends what the server lacks, computed from
 *   the server's version vector in Welcome, never from a send queue.
 * - Frames longer than a part go in parts, both ways (frames.ts).
 */
import { LoroDoc, VersionVector } from "loro-crdt"
import { DeviceLog } from "./device-log"
import { Assembler, Close, PROTOCOL, split, type Frame } from "./frames"

/**
 * - saved, saving, offline, unsafe: DESIGN.md section 4.
 * - stopped: the server ended access (4004, 4401). The note says why.
 * - full: the project is at its size limit (4130). Edits stay on the device.
 * - failed: three malformed-frame failures in a row, or a protocol mismatch.
 *   Only a reload can help.
 */
export type SaveState = "saved" | "saving" | "offline" | "unsafe" | "stopped" | "full" | "failed"

const HEARTBEAT_MS = 20_000
/** WebSocket.OPEN, which not every test runtime defines. */
const OPEN = 1
/** Device log entries appended before they are replaced by one snapshot. */
export const COMPACT_AFTER = 50
/** Malformed-frame failures in a row before the client gives up. */
const MALFORMED_LIMIT = 3

export const NOTES = {
  full:
    "This project reached its size limit of 32 MiB for text and history, so the server cannot save new edits. " +
    "They are kept on this device. Copy your text into a new project to go on.",
  failed: "The server sent data this page could not read. Your changes are kept on this device. Reload to try again.",
  version: "Nisaba was updated. Reload to continue. Your changes are kept on this device.",
} as const

/** What the client needs from the browser. Tests pass their own. */
export interface ClientHost {
  openLog(project: string): Promise<DeviceLog>
  socket(project: string): WebSocket
  setTimeout(f: () => void, ms: number): unknown
}

const browser: ClientHost = {
  openLog: (project) => DeviceLog.open(project),
  socket: (project) => new WebSocket(`${location.protocol === "https:" ? "wss" : "ws"}://${location.host}/sync/${project}`),
  setTimeout: (f, ms) => setTimeout(f, ms),
}

export class SyncClient {
  readonly doc = new LoroDoc()
  /** Resolves when the document has content, from this device or the server. */
  readonly loaded: Promise<void>
  private ready!: () => void
  private acked: VersionVector | undefined
  private ws: WebSocket | undefined
  private device: DeviceLog | undefined
  private pendingWrites = 0
  private appended = 0
  private compacting = false
  private retry = 0
  private malformed = 0
  private end: { state: "stopped" | "full" | "failed"; note: string } | undefined
  private heartbeat: ReturnType<typeof setInterval> | undefined
  private readonly listeners = new Set<(state: SaveState, note?: string) => void>()

  constructor(
    private readonly project: string,
    private readonly host: ClientHost = browser,
  ) {
    this.loaded = new Promise((resolve) => (this.ready = resolve))
  }

  async start(): Promise<void> {
    try {
      this.device = await this.host.openLog(this.project)
      const { keys, blobs } = await this.device.load()
      if (blobs.length > 0) {
        this.doc.importBatch(blobs)
        if (blobs.length > COMPACT_AFTER) await this.device.compact(keys, this.doc.export({ mode: "snapshot" }))
        this.ready()
      }
    } catch {
      this.device = undefined
    }
    this.doc.subscribeLocalUpdates((update) => {
      this.keep(update)
      if (this.acked) this.send({ type: "update", update })
      this.emit()
    })
    this.connect()
  }

  /** Calls `listener` now and on every change of the save state. */
  subscribe(listener: (state: SaveState, note?: string) => void): () => void {
    this.listeners.add(listener)
    listener(this.state(), this.end?.note)
    return () => this.listeners.delete(listener)
  }

  state(): SaveState {
    if (this.end) return this.end.state
    const cmp = this.acked ? this.doc.oplogVersion().compare(this.acked) : undefined
    if (cmp === 0 || cmp === -1) return "saved"
    if (!this.device) return "unsafe"
    return this.acked ? "saving" : "offline"
  }

  /** True while some change exists only in memory. */
  get unprotected(): boolean {
    const state = this.state()
    return state !== "saved" && (this.pendingWrites > 0 || !this.device)
  }

  private emit(): void {
    const state = this.state()
    for (const listener of this.listeners) listener(state, this.end?.note)
  }

  private send(frame: Frame): void {
    const ws = this.ws
    if (ws?.readyState !== OPEN) return
    for (const message of split(frame)) ws.send(message)
  }

  private keep(update: Uint8Array): void {
    if (!this.device) return
    const device = this.device
    this.pendingWrites++
    device
      .append(update)
      .then(() => {
        if (++this.appended >= COMPACT_AFTER) void this.compact(device)
      })
      .catch(() => (this.device = undefined))
      .finally(() => {
        this.pendingWrites--
        this.emit()
      })
  }

  /** Replaces the entries written so far with one snapshot. The snapshot
   * holds everything in the document, so an entry written meanwhile is
   * covered twice, which Loro ignores, and never lost. */
  private async compact(device: DeviceLog): Promise<void> {
    if (this.compacting) return
    this.compacting = true
    try {
      const { keys } = await device.load()
      await device.compact(keys, this.doc.export({ mode: "snapshot" }))
      this.appended = 0
    } catch {
      // The entries stay; the next append tries again.
    } finally {
      this.compacting = false
    }
  }

  private stop(state: "stopped" | "full" | "failed", note: string): void {
    this.end = { state, note }
    this.emit()
  }

  private connect(): void {
    const ws = this.host.socket(this.project)
    const parts = new Assembler()
    ws.binaryType = "arraybuffer"
    this.ws = ws
    ws.onopen = () => {
      this.retry = 0
      const version = this.doc.oplogVersion()
      this.send({ type: "hello", protocol: PROTOCOL, version: version.length() === 0 ? new Uint8Array() : version.encode() })
      this.heartbeat = setInterval(() => this.send({ type: "heartbeat" }), HEARTBEAT_MS)
    }
    ws.onmessage = (event) => {
      try {
        const frame = parts.push(new Uint8Array(event.data as ArrayBuffer))
        if (frame) this.receive(frame)
      } catch {
        // A frame that does not decode, or an update Loro refuses.
        ws.close(Close.malformed, "malformed frame")
      }
    }
    ws.onclose = (event) => {
      clearInterval(this.heartbeat)
      if (this.ws === ws) this.ws = undefined
      this.acked = undefined
      if (event.code === Close.malformed) this.malformed++
      if (this.malformed >= MALFORMED_LIMIT) return this.stop("failed", NOTES.failed)
      switch (event.code) {
        case Close.version:
          return this.stop("failed", NOTES.version)
        case Close.tooLarge:
          return this.stop("full", NOTES.full)
        case Close.forbidden:
        case Close.notFound:
        case Close.signedOut:
          return this.stop("stopped", event.reason)
      }
      this.emit()
      const delay = Math.min(30_000, 500 * 2 ** this.retry++) * (0.5 + Math.random())
      this.host.setTimeout(() => this.connect(), delay)
    }
  }

  private receive(frame: Frame): void {
    switch (frame.type) {
      case "welcome": {
        if (frame.catchUp.length > 0) this.importRemote(frame.catchUp)
        this.acked = VersionVector.decode(frame.version)
        this.ready()
        const cmp = this.doc.oplogVersion().compare(this.acked)
        if (cmp !== 0 && cmp !== -1) this.send({ type: "update", update: this.doc.export({ mode: "update", from: this.acked }) })
        break
      }
      case "update":
        this.importRemote(frame.update)
        break
      case "ack":
        this.acked = VersionVector.decode(frame.version)
        this.malformed = 0
        break
      case "presence":
      case "heartbeat":
      case "hello":
      case "begin":
      case "part":
        break
    }
    this.emit()
  }

  private importRemote(update: Uint8Array): void {
    this.doc.import(update)
    this.keep(update)
  }
}
