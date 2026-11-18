// loro-crdt's web build needs its WASM loaded before use. Every import of
// "loro-crdt" resolves here (see vite.config.ts), so it is loaded once.
import initializeLoro from "loro-crdt/web/loro_wasm.js"

await initializeLoro()
export * from "loro-crdt/web"
