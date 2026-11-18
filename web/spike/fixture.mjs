// Writes a ~30-page Typst project about rock pools to a JSON file the
// project-create API accepts: chapters in separate files, SVG and PNG
// figures, and a 60-entry BibTeX bibliography cited throughout.
//
//   node web/spike/fixture.mjs out.json
import { writeFileSync } from "node:fs"
import { deflateSync } from "node:zlib"

const animals = ["barnacle", "limpet", "anemone", "periwinkle", "shore crab", "sea star", "mussel", "blenny", "prawn", "whelk"]
const words = "tide pool water rock shelter algae light temperature salinity oxygen wave exposure grazing predation settlement larva shell hold surface night day season storm current sand pebble crevice".split(" ")

let seed = 7
const rand = () => ((seed = (seed * 1103515245 + 12345) % 2 ** 31) / 2 ** 31)
const pick = (xs) => xs[Math.floor(rand() * xs.length)]
const sentence = () => {
  const n = 12 + Math.floor(rand() * 14)
  const s = Array.from({ length: n }, () => pick(words)).join(" ")
  return s[0].toUpperCase() + s.slice(1) + "."
}

const keys = []
let bib = ""
for (let i = 0; i < 60; i++) {
  const key = `${pick(["shore", "reef", "kelp", "marsh", "dune"])}${1960 + i}`
  keys.push(key)
  const type = i % 3 === 0 ? "book" : "article"
  bib += `@${type}{${key},\n  title = {On the ${pick(animals)} and the ${pick(words)} of the ${pick(words)}},\n  author = {${pick(["Hale", "Okafor", "Lindqvist", "Moreau", "Tanaka", "Ruiz"])}, ${pick(["A.", "B.", "C.", "D."])} and ${pick(["Price", "Novak", "Singh", "Byrne"])}, ${pick(["E.", "F.", "G."])}},\n  year = {${1960 + i}},\n  ${type === "book" ? "publisher = {Coastal Press}" : `journal = {Journal of Shore Ecology},\n  volume = {${1 + (i % 40)}},\n  pages = {${10 + i}--${30 + i}}`}\n}\n\n`
}

const svg = (i) => `<svg xmlns="http://www.w3.org/2000/svg" width="400" height="200" viewBox="0 0 400 200">
<rect width="400" height="200" fill="#e8f1f2"/>
${Array.from({ length: 24 }, (_, k) => `<rect x="${10 + k * 16}" y="${190 - ((k * 37 + i * 11) % 150)}" width="12" height="${(k * 37 + i * 11) % 150}" fill="#2f5d50"/>`).join("")}
<text x="12" y="20" font-size="14">Figure ${i}</text></svg>`

function png(w, h) {
  const crc = (buf) => { let c = ~0; for (const b of buf) { c ^= b; for (let k = 0; k < 8; k++) c = (c >>> 1) ^ (0xedb88320 & -(c & 1)) } return ~c >>> 0 }
  const chunk = (type, data) => {
    const out = Buffer.alloc(12 + data.length)
    out.writeUInt32BE(data.length, 0); out.write(type, 4, "ascii"); data.copy(out, 8)
    out.writeUInt32BE(crc(Buffer.concat([Buffer.from(type, "ascii"), data])), 8 + data.length)
    return out
  }
  const raw = Buffer.alloc((w * 3 + 1) * h)
  for (let y = 0; y < h; y++) for (let x = 0; x < w; x++) {
    const o = y * (w * 3 + 1) + 1 + x * 3
    raw[o] = (x * 255) / w; raw[o + 1] = 120 + (y * 100) / h; raw[o + 2] = 160
  }
  const ihdr = Buffer.alloc(13); ihdr.writeUInt32BE(w, 0); ihdr.writeUInt32BE(h, 4); ihdr[8] = 8; ihdr[9] = 2
  return Buffer.concat([Buffer.from([137, 80, 78, 71, 13, 10, 26, 10]), chunk("IHDR", ihdr), chunk("IDAT", deflateSync(raw)), chunk("IEND", Buffer.alloc(0))])
}

const files = [{ path: "refs.bib", text: bib }]
let main = `#set document(title: "Rock pools")\n#set page(paper: "a4", numbering: "1")\n#set text(size: 11pt)\n#set heading(numbering: "1.1")\n\n#align(center, text(20pt)[*A Field Guide to Rock Pools*])\n#outline()\n#pagebreak()\n\n`
for (let c = 1; c <= 10; c++) {
  const name = `chapters/ch${String(c).padStart(2, "0")}.typ`
  main += `#include "${name}"\n`
  let body = `= The ${animals[c - 1]}\n\n`
  for (let s = 1; s <= 3; s++) {
    body += `== ${pick(words)} and ${pick(words)}\n\n`
    for (let p = 0; p < 4; p++) {
      body += Array.from({ length: 5 }, sentence).join(" ") + ` See @${pick(keys)}.\n\n`
    }
  }
  const fig = c % 2 === 0 ? `figures/fig${c}.png` : `figures/fig${c}.svg`
  body += `#figure(image("../${fig}", width: 80%), caption: [Counts of ${animals[c - 1]}s by pool.])\n\n`
  files.push({ path: name, text: body })
  files.push(c % 2 === 0
    ? { path: fig, base64: png(320, 160).toString("base64") }
    : { path: fig, base64: Buffer.from(svg(c)).toString("base64") })
}
main += `\n#pagebreak()\n#bibliography("refs.bib")\n`
files.push({ path: "main.typ", text: main })

writeFileSync(process.argv[2] ?? "fixture.json", JSON.stringify({ name: "A Field Guide to Rock Pools", files }))
