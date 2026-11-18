#!/usr/bin/env node
// Builds the preview compiler for the browser and writes the fonts it loads.
//
//   web/src/wasm-generated/  the wasm-bindgen output
//   web/public/fonts/        one file per font and fonts.json
//
// Set CARGO_BUILD_JOBS to limit cargo's parallel jobs.
import { execFileSync } from "node:child_process"
import { mkdirSync, readFileSync } from "node:fs"
import { join } from "node:path"
import { fileURLToPath } from "node:url"

const root = fileURLToPath(new URL("../", import.meta.url))
const run = (cmd, args, opts = {}) => execFileSync(cmd, args, { cwd: root, stdio: "inherit", ...opts })

const version = readFileSync(join(root, "Cargo.lock"), "utf8").match(/name = "wasm-bindgen"\nversion = "([^"]+)"/)[1]
const installed = execFileSync("wasm-bindgen", ["--version"], { encoding: "utf8" }).trim()
if (installed !== `wasm-bindgen ${version}`) {
  throw new Error(`Install wasm-bindgen-cli ${version}; found ${installed}`)
}

run("cargo", ["build", "--locked", "--release", "--target", "wasm32-unknown-unknown", "-p", "nisaba-compile-wasm"])
const { target_directory: target } = JSON.parse(
  execFileSync("cargo", ["metadata", "--no-deps", "--format-version", "1"], { cwd: root, encoding: "utf8" }),
)
const output = join(root, "web/src/wasm-generated")
mkdirSync(output, { recursive: true })
run("wasm-bindgen", [
  join(target, "wasm32-unknown-unknown/release/nisaba_compile_wasm.wasm"),
  "--target",
  "web",
  "--out-dir",
  output,
])
run("cargo", ["run", "--locked", "-q", "-p", "nisaba-compile-core", "--example", "write-fonts", "--", join(root, "web/public/fonts")])
