<img align="right" src="assets/branding/nisaba-logo.webp" alt="Nisaba logo: two mice write with quills on one long scroll under a stalk of grain" width="160" height="160">

# Nisaba

Nisaba is a self-hosted editor in which several people write one Typst document at the same time. The browser compiles the document and shows the pages as you type.

<br clear="right">

## Current state

Nisaba is not ready for use. This much runs today:

- One server program, `nisaba`, that keeps its data in one directory: an SQLite database and the asset files.
- First-run setup of an owner account, sign-in with email and password, and sign-out.
- A project list. You can create a project and open it.
- Live editing of the project's files by several people at once, with sync over a WebSocket and a copy of unsent changes kept in the browser.
- A preview that compiles in the browser and draws each page on a canvas.
- PDF export from the server.

This does not exist yet:

- Invitations, roles other than owner, and sign-in through OIDC.
- Tracked changes, comments, references, checkpoints, history and restore.
- Archived projects, the admin screen, font and package management.
- Selectable text in the preview and the jumps between source and preview.
- Most screens of the client. There is one plain screen with the file list, the editor and the preview.

[DESIGN.md](DESIGN.md) describes the whole design and the order of the work.

## Build and run

You need Rust (`rust-toolchain.toml` picks the version), `wasm-bindgen-cli` at the version of `wasm-bindgen` in `Cargo.lock`, Bun 1.3 and Node 22 or newer.

```sh
node scripts/build-wasm.mjs
cd web
bun install --frozen-lockfile
bun run build
cd ..
cargo run --release -p nisaba-server -- --data ./data --listen 127.0.0.1:8080
```

Open http://127.0.0.1:8080 and create the owner account. The server embeds the web client, so build the client before the server.

[CONTRIBUTING.md](CONTRIBUTING.md) lists the checks.

## License

Nisaba is free software under the GNU Affero General Public License, version 3 only. See [LICENSE](LICENSE).

The logo is a stand-in sketch until a hand-painted one replaces it.
