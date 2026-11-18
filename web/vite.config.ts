import { readFileSync, readdirSync, statSync, writeFileSync } from "node:fs"
import { join } from "node:path"
import { brotliCompressSync, constants, gzipSync } from "node:zlib"
import type { Plugin } from "vite"
import solid from "vite-plugin-solid"
import { defineConfig } from "vitest/config"

const OUT = "dist-server"

/** Writes a .br and a .gz copy next to each large output file. The server sends the best one the browser accepts. */
function precompress(): Plugin {
  const walk = (dir: string): string[] =>
    readdirSync(dir).flatMap((name) => {
      const path = join(dir, name)
      return statSync(path).isDirectory() ? walk(path) : [path]
    })
  return {
    name: "nisaba-precompress",
    apply: "build",
    closeBundle() {
      for (const path of walk(OUT)) {
        if (/\.(br|gz|png|webp|woff2)$/.test(path) || statSync(path).size < 1024) continue
        const data = readFileSync(path)
        writeFileSync(`${path}.br`, brotliCompressSync(data, { params: { [constants.BROTLI_PARAM_QUALITY]: 11 } }))
        writeFileSync(`${path}.gz`, gzipSync(data, { level: 9 }))
      }
    },
  }
}

export default defineConfig({
  plugins: [solid(), precompress()],
  // Every "loro-crdt" import goes through src/loro.ts, which loads Loro's WASM first.
  resolve: { alias: [{ find: /^loro-crdt$/, replacement: new URL("./src/loro.ts", import.meta.url).pathname }] },
  worker: { format: "es" },
  build: { outDir: OUT, emptyOutDir: true, target: "es2022" },
  server: { proxy: { "/api": { target: "http://127.0.0.1:8080", ws: true } } },
  test: { environment: "node", include: ["src/**/*.test.ts"] },
})
