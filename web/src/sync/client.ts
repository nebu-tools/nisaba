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
 */
import { LoroDoc, VersionVector } from "loro-crdt"
import { DeviceLog } from "./device-log"
import { Close, PROTOCOL, decode, encode, type Frame } from "./frames"

export type SaveState = "saved" | "saving" | "offline" | "unsafe" | "stopped"

const HEARTBEAT_MS = 20_000
/** Codes after which reconnecting cannot help. */
const FINAL: number[] = [Close.forbidden, Close.notFound, Close.version, Close.signedOut]

export class SyncClient {
  readonly doc = new LoroDoc()
  /** Resolves when the document has content, from this device or the server. */
  readonly loaded: Promise<void>
  private ready!: () => void
  private acked: VersionVector | undefined
  private ws: WebSocket | undefined
  private device: DeviceLog | undefined
  private pendingWrites = 0
  private retry = 0
  private stopped = false
  private heartbeat: ReturnType<typeof setInterval> | undefined
  private readonly listeners = new Set<(state: SaveState, note?: string) => void>()

  constructor(private readonly project: string) {
    this.loaded = new Promise((resolve) => (this.ready = resolve))
  }

  async start(): Promise<void> {
    try {
      this.device = await DeviceLog.open(this.project)
      const { keys, blobs } = await this.device.load()
      if (blobs.length > 0) {
        this.doc.importBatch(blobs)
        if (blobs.length > 50) await this.device.compact(keys, this.doc.export({ mode: "snapshot" }))
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
    listener(this.state())
    return () => this.listeners.delete(listener)
  }

  state(): SaveState {
    if (this.stopped) return "stopped"
    const cmp = this.acked ? this.doc.oplogVersion().compare(this.acked) : undefined
    if (cmp === 0 || cmp === -1) return "saved"
    if (!this.device) return "unsafe"
    return this.acked ? "saving" : "offline"
  }

  /** True while some change exists only in memory. */
  get unprotected(): boolean {
    return this.state() !== "saved" && (this.pendingWrites > 0 || !this.device)
  }

  private emit(note?: string): void {
    const state = this.state()
    for (const listener of this.listeners) listener(state, note)
  }

  private send(frame: Frame): void {
    if (this.ws?.readyState === WebSocket.OPEN) this.ws.send(encode(frame))
  }

  private keep(update: Uint8Array): void {
    if (!this.device) return
    this.pendingWrites++
    this.device
      .append(update)
      .catch(() => (this.device = undefined))
      .finally(() => {
        this.pendingWrites--
        this.emit()
      })
  }

  private connect(): void {
    const url = `${location.protocol === "https:" ? "wss" : "ws"}://${location.host}/sync/${this.project}`
    const ws = new WebSocket(url)
    ws.binaryType = "arraybuffer"
    this.ws = ws
    ws.onopen = () => {
      this.retry = 0
      const version = this.doc.oplogVersion()
      this.send({ type: "hello", protocol: PROTOCOL, version: version.length() === 0 ? new Uint8Array() : version.encode() })
      this.heartbeat = setInterval(() => this.send({ type: "heartbeat" }), HEARTBEAT_MS)
    }
    ws.onmessage = (event) => this.receive(decode(new Uint8Array(event.data as ArrayBuffer)))
    ws.onclose = (event) => {
      clearInterval(this.heartbeat)
      this.acked = undefined
      if (FINAL.includes(event.code)) {
        this.stopped = true
        this.emit(event.reason)
        return
      }
      this.emit()
      const delay = Math.min(30_000, 500 * 2 ** this.retry++) * (0.5 + Math.random())
      setTimeout(() => this.connect(), delay)
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
        break
      case "presence":
      case "heartbeat":
      case "hello":
        break
    }
    this.emit()
  }

  private importRemote(update: Uint8Array): void {
    this.doc.import(update)
    this.keep(update)
  }
}
