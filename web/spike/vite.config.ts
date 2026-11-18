import { fileURLToPath } from "node:url"
import { defineConfig } from "vite"

// Builds the client the server embeds (web/dist-server).
export default defineConfig({
  root: fileURLToPath(new URL(".", import.meta.url)),
  resolve: {
    alias: [{ find: /^loro-crdt$/, replacement: fileURLToPath(new URL("../src/loro.ts", import.meta.url)) }],
  },
  worker: { format: "es" },
  build: { outDir: fileURLToPath(new URL("../dist-server", import.meta.url)), emptyOutDir: true, target: "es2022" },
})
