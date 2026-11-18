<img align="right" src="assets/branding/nisaba-logo.webp" alt="Nisaba logo: two mice write with quills on one long scroll under a stalk of grain" width="160" height="160">

# Nisaba

Nisaba is a self-hosted editor in which several people write one Typst document at the same time. The browser compiles the document and shows the pages as you type.

<br clear="right">

## Current state

Nisaba is not ready for use. This much runs today:

- One server program, `nisaba`, that keeps its data in one directory: an SQLite database and the asset files.
- First-run setup of an owner account, protected by a one-time setup code when the server can be reached from other machines; sign-in with email and password, and sign-out. Sign-out closes the open sync sockets of that session.
- Failed sign-ins slow down the email they were for, and the server refuses password work beyond a short queue. SECURITY.md says what this does not cover.
- A project list. You can create a project and open it.
- Live editing of the project's files by several people at once, with sync over a WebSocket and a copy of unsent changes kept in the browser. Large changes go in parts of 1 MiB.
- A preview that compiles in the browser and draws each page on a canvas.
- PDF export from the server, in a child process with memory, time and parallel limits. On Linux, Landlock also takes away its file and network access. A server that cannot apply that sandbox refuses exports unless it was started with `--allow-unsandboxed-export`.

A project's text and history may grow to 32 MiB. Assets do not count toward that limit.

This does not exist yet:

- Invitations, roles other than owner, and sign-in through OIDC.
- Tracked changes, comments, references, checkpoints, history and restore.
- Backup and restore with `nisaba backup DEST`. Until then, stop the server and copy the data directory.
- A limit on asset storage. Asset storage has no limit yet.
- Deleting an asset, and renaming or deleting a project.
- Archived projects, the admin screen, font and package management. Only the embedded fonts work; fonts in the data directory or in a project are not loaded.
- A choice of entry file. The entry file is always `main.typ`.
- Read-only access for a member whose role changes (close code 4003). Today the socket just stops.
- Archive export and import (`.nisaba`), and close code 4409 for a project that was replaced.
- Selectable text in the preview and the jumps between source and preview.
- Clicking a problem to open its file, and underlines for problems in the editor. Problems are listed as text.
- Named steps while the typesetter loads. The preview only says "Preparing preview…".
- An offline banner, offline export, and a mark on projects this device holds.
- PDF/A export.
- Browser tests that drive the real page (Playwright).
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

To serve other machines, put a TLS reverse proxy in front of the server and tell the server the address people use:

```sh
cargo run --release -p nisaba-server -- --data ./data --listen 0.0.0.0:8080 --public-url https://nisaba.example.org
```

The server then prints a setup code to its log at start; the setup screen asks for it. It refuses a plain-http public address that is not loopback unless you pass `--allow-insecure-http`.

Exports are limited by `--export-memory-mb` (default 1024), `--export-seconds` (60), `--export-parallel` (2) and `--export-queue` (8). DESIGN.md section 8 says what each does.

The server prints one line at start that says whether exports are sandboxed. Exports are sandboxed only on Linux 5.13 or newer with Landlock turned on. Elsewhere the server refuses every export with a message that says why. `--allow-unsandboxed-export` lets them run anyway. An unsandboxed export still sees only the project's own files, and the memory and time limits still apply where the system has them. What it loses is the kernel's guarantee that a bug in the compiler cannot read other files on the server or open network connections.

[CONTRIBUTING.md](CONTRIBUTING.md) lists the checks.

## License

Nisaba is free software under the GNU Affero General Public License, version 3 only. See [LICENSE](LICENSE).

The logo is a stand-in sketch until a hand-painted one replaces it.
