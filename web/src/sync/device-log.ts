/**
 * The device copy of a project: every Loro update in IndexedDB, written as it
 * happens, so an edit survives a reload or a crash before the server has it.
 */

export class DeviceLog {
  private constructor(
    private readonly db: IDBDatabase,
    private readonly project: string,
  ) {}

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

  private request<T>(req: IDBRequest<T>): Promise<T> {
    return new Promise((resolve, reject) => {
      req.onsuccess = () => resolve(req.result)
      req.onerror = () => reject(req.error)
    })
  }

  async load(): Promise<{ keys: IDBValidKey[]; blobs: Uint8Array[] }> {
    const index = this.db.transaction("log").objectStore("log").index("project")
    const [keys, rows] = await Promise.all([
      this.request(index.getAllKeys(this.project)),
      this.request(index.getAll(this.project) as IDBRequest<{ bytes: Uint8Array }[]>),
    ])
    return { keys, blobs: rows.map((row) => row.bytes) }
  }

  append(bytes: Uint8Array): Promise<void> {
    const tx = this.db.transaction("log", "readwrite")
    tx.objectStore("log").add({ project: this.project, bytes: bytes.slice() })
    return this.done(tx)
  }

  /** Replaces the rows in `keys` with one snapshot, in one transaction. */
  compact(keys: IDBValidKey[], snapshot: Uint8Array): Promise<void> {
    const tx = this.db.transaction("log", "readwrite")
    const store = tx.objectStore("log")
    for (const key of keys) store.delete(key)
    store.add({ project: this.project, bytes: snapshot })
    return this.done(tx)
  }
}
