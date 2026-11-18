// Drives the real binary and the embedded client in Chromium and prints
// measurements as JSON. Every run uses a free port and a fresh data directory,
// and stops the server and the browser when it ends.
//
//   node web/spike/measure.mjs target/release/nisaba
import { spawn } from "node:child_process"
import { mkdtempSync, readFileSync, rmSync } from "node:fs"
import { tmpdir } from "node:os"
import { join } from "node:path"
import { execFileSync } from "node:child_process"
import { chromium } from "@playwright/test"

const binary = process.argv[2] ?? "target/release/nisaba"
const fixturePath = join(tmpdir(), `nisaba-fixture-${process.pid}.json`)
execFileSync("node", [new URL("./fixture.mjs", import.meta.url).pathname, fixturePath])
const fixture = JSON.parse(readFileSync(fixturePath, "utf8"))
const data = mkdtempSync(join(tmpdir(), "nisaba-data-"))
const results = {}

const median = (xs) => [...xs].sort((a, b) => a - b)[Math.floor(xs.length / 2)]
const p95 = (xs) => [...xs].sort((a, b) => a - b)[Math.min(xs.length - 1, Math.floor(xs.length * 0.95))]
const round = (x) => Math.round(x)

const spawnAt = performance.now()
const server = spawn(binary, ["--data", data, "--listen", "127.0.0.1:0"], { stdio: ["ignore", "pipe", "inherit"] })
const base = await new Promise((resolve, reject) => {
  server.stdout.on("data", (chunk) => {
    const m = /listening on (http:\/\/\S+)/.exec(String(chunk))
    if (m) resolve(m[1])
  })
  server.on("exit", (code) => reject(new Error(`server exited ${code}`)))
})
results.serverListeningMs = round(performance.now() - spawnAt)

const browser = await chromium.launch()
try {
  // Cold start: empty data directory to a usable editor with a preview.
  {
    const context = await browser.newContext()
    const page = await context.newPage()
    const t0 = performance.now()
    await page.goto(base)
    await page.getByRole("heading", { name: "Set up Nisaba" }).waitFor()
    results.setupScreenMs = round(performance.now() - t0)
    await page.getByLabel("Name").fill("Sam Example")
    await page.getByLabel("Email").fill("sam@example.org")
    await page.getByLabel("Password").fill("correct horse battery")
    await page.getByRole("button", { name: "Create account" }).click()
    await page.getByLabel("New project").fill("Notes")
    await page.getByRole("button", { name: "Create" }).click()
    await page.locator(".cm-content").waitFor()
    results.coldStartToEditorMs = round(performance.now() - spawnAt)
    await page.locator(".page canvas").first().waitFor({ timeout: 180_000 })
    results.coldStartToPreviewMs = round(performance.now() - spawnAt)
    await context.storageState({ path: join(data, "state.json") })
    await context.close()
  }

  const context = await browser.newContext({ storageState: join(data, "state.json"), viewport: { width: 1600, height: 1000 }, deviceScaleFactor: Number(process.env.DPR ?? 1) })
  const created = await context.request.post(`${base}/api/projects`, { data: fixture })
  const { id } = await created.json()

  // First preview of the 30-page fixture, from a cold browser cache.
  const page = await context.newPage()
  const t0 = performance.now()
  await page.goto(`${base}/?p=${id}`)
  await page.locator(".page canvas").first().waitFor({ timeout: 180_000 })
  results.fixtureFirstPreviewMs = round(performance.now() - t0)
  results.fixturePages = await page.locator(".page").count()
  results.fixtureCycle0 = await page.evaluate(() => window.__nisaba.firstPreviewMs)

  // Keystroke to painted page while typing into a chapter.
  await page.locator("#files a", { hasText: "chapters/ch01.typ" }).click()
  await page.locator(".cm-content").click()
  await page.keyboard.press("Control+Home")
  for (let i = 0; i < 6; i++) await page.keyboard.press("ArrowDown")
  await page.evaluate(() => { window.__nisaba.cycles.length = 0 })
  for (const ch of "Periwinkles graze at night when the pool is calm. ") {
    await page.keyboard.type(ch)
    await page.waitForTimeout(120)
  }
  await page.waitForTimeout(1500)
  const cycles = await page.evaluate(() => window.__nisaba.cycles)
  const paint = await page.evaluate(() => window.__nisaba.paint)
  results.pagePixelsMedian = median(paint.map((x) => x.pixels))
  results.pagePaintMainThreadMedianMs = median(paint.map((x) => x.domMs))
  const k = cycles.map((c) => c.keystrokeToPaint)
  results.typing = {
    cycles: cycles.length,
    keystrokeToPaintMedianMs: round(median(k)), keystrokeToPaintP95Ms: round(p95(k)),
    compileMedianMs: round(median(cycles.map((c) => c.compileMs))),
    renderMedianMs: round(median(cycles.map((c) => c.renderMs))),
    toSendMedianMs: round(median(cycles.map((c) => c.toSend))),
    workerRoundTripMedianMs: round(median(cycles.map((c) => c.workerRoundTrip))),
    toFrameMedianMs: round(median(cycles.map((c) => c.toFrame))),
    pagesChangedMedian: median(cycles.map((c) => c.changed)),
    pagesRenderedMedian: median(cycles.map((c) => c.rendered)),
  }

  // Offline edit: typed with the server unreachable, survives a reload, then
  // reaches the server on reconnect.
  {
    const offline = await context.newPage()
    const sockets = []
    let online = false
    // While offline, the page's socket opens but the server never answers.
    await offline.routeWebSocket(/\/sync\//, (ws) => { if (online) ws.connectToServer(); else sockets.push(ws) })
    await offline.goto(`${base}/?p=${id}`)
    await offline.locator(".cm-content").click()
    await offline.keyboard.press("Control+End")
    await offline.keyboard.type("\nWritten while offline.")
    await offline.getByRole("status").filter({ hasText: "Offline · changes kept on this device" }).waitFor()
    await offline.waitForTimeout(300)
    await offline.reload()
    await offline.locator(".cm-content", { hasText: "Written while offline." }).waitFor({ timeout: 10_000 })
    results.offlineEditSurvivesReload = true
    const before = await (await context.request.get(`${base}/api/projects/${id}/files`)).text()
    results.serverHadOfflineEditBeforeReconnect = before.includes("Written while offline.")
    online = true
    for (const ws of sockets) await ws.close({ code: 4000, reason: "test reconnect" })
    await offline.getByRole("status").filter({ hasText: /^Saved$/ }).waitFor({ timeout: 30_000 })
    const after = await (await context.request.get(`${base}/api/projects/${id}/files`)).text()
    results.serverHasOfflineEditAfterReconnect = after.includes("Written while offline.")
    await offline.close()
  }

  // Server export from the same room state.
  const exportAt = performance.now()
  const pdf = await context.request.get(`${base}/api/projects/${id}/export.pdf`)
  results.exportStatus = pdf.status()
  results.exportMs = round(performance.now() - exportAt)
  results.exportBytes = (await pdf.body()).length
} finally {
  await browser.close()
  server.kill("SIGTERM")
  rmSync(data, { recursive: true, force: true })
  rmSync(fixturePath, { force: true })
}
console.log(JSON.stringify(results, null, 2))
