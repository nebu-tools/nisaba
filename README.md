<img align="right" src="assets/branding/nisaba-logo.webp" alt="Nisaba logo: two mice write with quills on one long scroll under a stalk of grain" width="160" height="160">

# Nisaba

Nisaba is a self-hosted editor in which several people write one Typst document at the same time. The browser compiles the document and shows the pages as you type.

<br clear="right">

## Current state

Nisaba is not ready for use. The server is complete for its first stage; the web client is one plain screen. This much runs today:

- One server program, `nisaba`, that keeps its data in one directory: an SQLite database and the asset, font and package files. Only one server process uses a directory at a time.
- First-run setup of an administrator account, protected by a one-time setup code when the server can be reached from other machines, or with `nisaba setup` from the server's terminal.
- Sign-in with email and password, sign-out, sign-out everywhere, and password changes. A session ends 30 days after the client last renewed it, and at most 90 days after sign-in. Failed sign-ins slow down the email they were for, and the server refuses password work beyond a short queue. SECURITY.md says what this does not cover.
- Accounts that the administrator makes, lists and deletes.
- Projects that their owners create, rename and delete, and members with the roles owner, editor, commenter and viewer. A role change or a removal reaches an open connection at once. A commenter cannot change the document yet.
- Live editing of the project's files by several people at once, with sync over a WebSocket and a copy of unsent changes kept in the browser. Large changes go in parts of 1 MiB.
- A preview that compiles in the browser and draws each page on a canvas.
- PDF export from the server, in a child process with memory, time and parallel limits, as plain PDF or PDF/A-2b, and in the three views of tracked changes. The export uses the embedded fonts, fonts the administrator puts in the data directory, the project's own font files, and Typst packages from the server's cache, which the server fills from the package registry. On Linux 6.2 or newer the child is sandboxed with Landlock and seccomp. A server that cannot apply that sandbox refuses exports unless it was started with `--allow-unsandboxed-export`.
- `nisaba backup` while the server runs, and `nisaba restore` into an empty directory.
- One description of the HTTP API and the sync protocol, `api/openapi.json`, from which the web client's types are made.

A project's text and history may grow to 32 MiB. Assets do not count toward that limit, and asset storage has no limit yet.

This does not exist yet:

- Invitations and sign-in through OIDC. Until then the administrator makes accounts and an owner adds them to a project by email.
- Tracked changes and comments in the client, the check of a commenter's changes, references, checkpoints, history and restore.
- Deleting an asset. Archived projects, the admin screen, and package management beyond `nisaba packages add`.
- A choice of entry file. The entry file is always `main.typ`.
- Archive export and import (`.nisaba`), and close code 4409 for a project that was replaced.
- Selectable text in the preview and the jumps between source and preview.
- Clicking a problem to open its file, and underlines for problems in the editor. Problems are listed as text.
- Named steps while the typesetter loads. The preview only says "Preparing preview…".
- An offline banner, offline export, and a mark on projects this device holds.
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
cargo run --release -p nisaba-server -- serve --data ./data --listen 127.0.0.1:8080
```

Open http://127.0.0.1:8080 and create the administrator account. The server embeds the web client, so build the client before the server. A server built without it says so on every page; the API works without it.

To serve other machines, put a TLS reverse proxy in front of the server and tell the server the address people use:

```sh
nisaba serve --data /var/lib/nisaba --listen 0.0.0.0:8080 --public-url https://nisaba.example.org
```

The server then prints a setup code to its standard output at start; the setup screen asks for it. Or make the account on the server's machine with `nisaba setup --email you@example.org`, which reads the password from standard input. The server refuses a plain-http public address that is not loopback unless you pass `--allow-insecure-http`.

Every setting can also come from a TOML file (`--config nisaba.toml` or `NISABA_CONFIG`) or the environment: `--export-memory-mb` is `memory_mb` under `[export]` in the file and `NISABA_EXPORT_MEMORY_MB` in the environment. `nisaba help` lists every setting, and `nisaba check` prints the settings in effect and says whether the server could start with them. DESIGN.md section 14 has the table.

Exports are limited by `--export-memory-mb` (default 1024), `--export-seconds` (60), `--export-parallel` (2) and `--export-queue` (8). DESIGN.md section 8 says what each does. `--sync-transfer-mb` (default 256, at least 32) caps the bytes of all updates that arrive in parts at once; DESIGN.md section 4 says how. The server downloads a Typst package that its cache lacks from https://packages.typst.org; `--no-package-downloads` stops that, and `nisaba packages add @preview/cetz:0.3.4` fills the cache ahead of time.

The server prints one line at start that says whether exports are sandboxed. Exports are sandboxed only on Linux 6.2 or newer with Landlock (ABI 3) turned on and seccomp available. A sandboxed export child can open no file, create no socket, signal or trace no other process, and gain no privileges; SECURITY.md lists the promises. Elsewhere the server refuses every export with a message that says why. `--allow-unsandboxed-export` lets them run anyway. An unsandboxed export still sees only the project's own files, and the memory, CPU and time limits still apply where the system has them. It loses the sandbox: a bug in the compiler could then read or change files the server's user can, open network connections, or signal the server.

The server logs to standard error, as text or, with `--log-format json`, one JSON object per line. `GET /api/health` answers 200 while the server can read its database. On `SIGTERM` the server finishes the updates it has received, closes the sync connections with code 1012 and stops within a few seconds.

To back up while the server runs, and to restore into a new directory with the server stopped:

```sh
nisaba backup /srv/backup/nisaba --data /var/lib/nisaba
nisaba restore /srv/backup/nisaba --data /var/lib/nisaba-restored
```

A second backup into the same place copies only what is new. Restore writes only into an empty or new directory.

[CONTRIBUTING.md](CONTRIBUTING.md) lists the checks.

## License

Nisaba is free software under the GNU Affero General Public License, version 3 only. See [LICENSE](LICENSE).

The logo is a stand-in sketch until a hand-painted one replaces it.
