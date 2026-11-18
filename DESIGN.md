# Nisaba design

Nisaba is a collaborative editor for Typst documents. Several people edit one
project at the same time, review each other's changes, discuss them in
comments, cite references, and see a live preview of the typeset pages. This
document describes how Nisaba works and why.

The design follows five rules.

1. One program. A single Rust binary serves the web client, the API and the
   sync endpoint. It needs a data directory and nothing else.
2. One source of text. The Loro document of a project is the only place where
   document text exists. Every read, every export and every history view takes
   text from it. No API writes a document body.
3. No lost acknowledgement. When the client shows "Saved", the edit is on the
   server's disk and survives `kill -9`.
4. No lost edit on the device. The client writes every change to IndexedDB as
   it happens. An edit made offline survives a reload and reaches the server
   when the connection returns.
5. One compiler. The browser preview and the server export use the same
   incremental compiler code over the same files.

## Contents

1. Repository layout
2. Data on disk
3. The project document
4. Sync protocol
5. Tracked changes, comments and history
6. Accounts and sign-in
7. References
8. Compile pipeline, fonts and packages
9. Web client
10. Export and the project archive
11. Performance targets
12. Out of scope
13. Build order

## 1. Repository layout

```
crates/
  nisaba-server/      the `nisaba` binary: HTTP, accounts, sync, storage,
                      export, history, archive import and export
  nisaba-sync/        rooms and the wire protocol
  nisaba-compile-core/ LiveCompiler: incremental Typst compile, page
                      fingerprints, raster and PDF output
  nisaba-compile-wasm/ wasm-bindgen wrapper of LiveCompiler for the browser
web/
  src/app/            screens and the workspace shell
  src/sync/           sync client and the device log in IndexedDB
  src/editor/         CodeMirror setup, review marks, comments, presence
  src/preview/        preview worker and page view
  bench/              the test document generator and the benchmark script
  e2e/                Playwright tests against the real binary
```

The server crate is a library with a thin `main.rs`, so integration tests
start the real binary on a free port with a temporary data directory.

## 2. Data on disk

The data directory holds one SQLite database and three directories of plain
files.

```
DATA/
  nisaba.db           SQLite in WAL mode
  nisaba.db-wal
  assets/ab/cdef...   asset bytes, named by SHA-256
  fonts/              fonts the administrator adds
  packages/           cached Typst packages, as .tar.gz
```

### Why SQLite for the update log

The log could also be one append-only file per project. I chose SQLite for
four reasons:

- Compaction must replace many log entries with one snapshot. In SQLite the
  snapshot write and the deletion of the entries it covers are one
  transaction. With files, the same step needs a write to a temporary file,
  a rename, a directory sync and a recovery rule for a crash between them.
- Accounts, sessions, memberships and checkpoints need a database in any
  case. One store gives one backup and one consistency rule.
- A file per project holds an open handle and an fsync per project. SQLite
  shares one handle and one fsync per commit across all projects.
- The database answers "what does this project hold" with a query, not with
  a directory scan.

Asset bytes stay out of the database. They are immutable and can be large,
and plain files are easy to copy, deduplicate and check.

### Tables

| Table | Holds |
|---|---|
| `users` | id, email, display name, Argon2id password hash (nullable for OIDC-only users), `is_admin`, created time |
| `identities` | OIDC issuer and subject linked to a user |
| `sessions` | SHA-256 of the session token, user, expiry |
| `invitations` | SHA-256 of the invitation token, project, role, creator, expiry, used time |
| `projects` | id, name, entrypoint path, epoch, created and updated time |
| `members` | project, user, role (`owner`, `editor`, `commenter`, `viewer`) |
| `doc_updates` | project, sequence number, Loro update bytes |
| `doc_snapshots` | project, Loro snapshot bytes |
| `checkpoints` | project, id, name, Loro frontiers, author, time, kind (`manual`, `auto`, `restore`, `import`) |
| `assets` | asset hash, size |
| `project_assets` | project, asset hash: the assets uploaded to or imported into that project |
| `settings` | key and value; holds the SHA-256 of the first-run setup code while one is open |

The connection uses `journal_mode=WAL`, `synchronous=FULL` and
`foreign_keys=ON`. `synchronous=FULL` makes each commit durable before
SQLite returns. The server acknowledges an update only after that return.

### Writes

An update from a client goes through the project's room:

1. The room checks that the bytes decode as a Loro update and that the
   sender's role allows the change.
2. The room skips the update when the server already holds all of it.
3. The room inserts the bytes into `doc_updates` and commits.
4. The room imports the update into its in-memory document.
5. The room sends the update to the other clients in the project.
6. The room sends the sender an `Ack` with its version vector.

An asset upload writes the bytes to a temporary file, syncs it, renames it to
its hash and syncs the directory. Then, in one transaction, it records the
asset and a `project_assets` row for the project it was uploaded to. Only then
does the server add the file entry to the Loro document. A crash can leave an
unreferenced asset file, never a reference to a missing file.

An asset belongs to the projects that hold a `project_assets` row for it.
Bytes are reachable only through such a project, by a member of it:
`GET /api/projects/{id}/assets/{hash}` answers 404 unless the caller is a
member and the project holds the asset, and the export resolves only assets
the project holds. A hash in a document grants nothing. Someone who learns the
hash of another project's image and writes it into their own document gets a
compile error that the file is missing, not the bytes. Uploading the same
bytes to another project adds a row for that project; the file on disk is
shared.

A project's stored document, its snapshot plus its log, may grow to 32 MiB.
An update past that is refused with close code 4130, and an upload or import
that would pass it gets HTTP 413. The limit holds after a restart, because the
room counts what the store holds.

### Compaction

The room counts log entries and bytes since the last snapshot. After 500
entries or 8 MB, it exports a full snapshot. In one transaction the store
writes the snapshot and deletes every log entry whose end version is inside
the snapshot's version vector. An entry that arrived while the snapshot was
being made stays in the log. Opening a room imports the snapshot and then the
remaining log entries.

The snapshot keeps the full edit history. History is how checkpoints, version
views and restores work, so the design never trims it. A busy project grows by
about the size of its typed text times a small factor. The project archive
(section 10) can export a project without history if a project grows too
large.

An asset file is deleted when no `project_assets` row refers to it. Rows go
away only when a project is deleted. An asset that a file stopped using stays,
because history can still show that file.

### Backup and restore

`nisaba backup DEST` writes a consistent copy while the server runs. It
copies the database with SQLite's online backup API, then copies the asset,
font and package files that are not already in `DEST`. The database copy comes
first. Every asset the copied database refers to was on disk before the
database row that refers to it, so the copy is complete. A second run copies
only new assets.

To restore, stop the server, replace the data directory with the backup and
start the server. Clients that hold edits made after the backup still have
them in IndexedDB. When they reconnect, the server's version vector shows that
it lacks those edits, and the clients send them again. A restore from backup
therefore loses only edits that no open client still holds.

## 3. The project document

Each project is one Loro document. One document per project, rather than one
per file, gives four things: a checkpoint is one version of the whole
project; a restore is one update; renaming or moving a file is a map change,
not a copy between documents; and a client needs one socket per project.

The document has these root containers:

| Container | Type | Content |
|---|---|---|
| `files` | map | file id to a map with `path` and either `text` (a Loro text) or `hash` (an asset's SHA-256) |
| `threads` | map | thread id to a comment thread (section 5) |
| `references` | map | reference id to one CSL-JSON item as a JSON string (section 7) |
| `settings` | map | entrypoint path and other project settings |

File ids are random. Two people who create `notes.typ` at the same moment get
two entries, and the file list shows both. One entry never silently replaces
the other's text. The client shows a conflict marker and the users rename one
of the files.

The server changes the document in only one way. It builds a Loro update with
its own peer id and passes the update through the room, as a browser would.
Seeding a new project, adding an uploaded asset, importing an archive and
restoring a checkpoint all work this way. No server code writes text into the
in-memory document directly.

## 4. Sync protocol

Nisaba speaks its own protocol, version 1. Each WebSocket binary message is
one frame: a `u8` type followed by its fields. A blob is a big-endian `u32`
length followed by that many bytes.

| Type | Frame | Direction | Fields |
|---|---|---|---|
| 1 | Hello | client to server | protocol version (`u8`), client version vector (blob) |
| 2 | Welcome | server to client | catch-up (blob), server version vector (blob) |
| 3 | Update | both | Loro update (blob) |
| 4 | Ack | server to client | server version vector (blob) |
| 5 | Presence | both | Loro ephemeral-store bytes (blob) |
| 6 | Heartbeat | both | none |

The socket URL `/sync/{project}` names the project, so Hello carries no
project id. The WebSocket close frame carries every error as a code and a
reason (below), so there is no error frame and no goodbye frame. A frame with
an unknown type, a short frame or bytes after the end of a frame closes the
socket with 4000.

The session cookie authenticates the socket. The server refuses the upgrade
with HTTP 403 unless the `Origin` header equals the public address (section
6), so a hostile page cannot open a socket with the user's cookie. A handshake
without an `Origin` header is refused too. A socket without a valid session
is accepted and closed at once with 4401; a socket for a project that does
not exist, or of which the user is not a member, is closed with 4004. The two
cases look the same, so a socket does not reveal which project ids exist.

### The connection

1. The client opens the socket and sends Hello with the version vector of its
   local document, or an empty blob when it holds nothing.
2. The server sends Welcome. The catch-up holds every operation the client
   lacks, as Loro update or snapshot bytes; the client imports either kind
   the same way. It is empty when the client lacks nothing. The version
   vector is what the server holds.
3. The client exports every change the server's version vector does not
   include and sends it as one Update.
4. From then on, the client sends each local change as an Update when the
   change happens. It does not wait for the previous Ack.
5. The server sends an Ack after each update it has committed. An update the
   server already holds gets an Ack without a second write.

The client does not keep a queue of unsent updates. It computes what to send
from the last version vector it received in a Welcome or an Ack. A lost frame,
a dropped socket or a server restart therefore needs no special case: the next
connection sends the difference again, and Loro ignores operations it already
holds.

### The no-lost-ack rule

The server sends an Ack only after the update is committed to SQLite with
`synchronous=FULL`. The Ack carries a version vector, not a sequence number.
The room writes an update to the log before it imports it, so its document
never holds an operation that is not durable, and every version vector in a
Welcome or an Ack is safe to count as saved.
The client says "Saved" only when the acknowledged version vector includes
the version vector of its local document. These two rules mean that "Saved"
is never shown for an edit that a crash can remove. The integration test
`an_acknowledged_edit_survives_kill_9` sends 50 edits, waits for the Ack that
covers them, sends `SIGKILL` to the server at once, restarts it on the same
data directory, and checks the text through the HTTP read and through a new
sync client.

### The device log

The client stores every update it makes or receives in an IndexedDB object
store, in the order it saw them. When a project opens, the client imports
these entries before it connects, so the editor shows the last state the
device knew, also offline. When there are more than 50 entries, the client
replaces them with one snapshot in one IndexedDB transaction.

Before the first Ack of a session, and while offline, the device log is the
only copy of new edits. The save indicator therefore has four states:

| State | Text | Meaning |
|---|---|---|
| saved | Saved | The server acknowledged every local change. |
| saving | Saving… | Connected; some changes are not acknowledged yet. |
| offline | Offline · changes kept on this device | Not connected; every change is in IndexedDB. |
| unsafe | Not saved on this device | IndexedDB failed or is unavailable, and some changes exist only in memory. |

In the unsafe state the page asks for confirmation before it closes.

### Close codes and failure handling

| Code | Meaning | Client action |
|---|---|---|
| 1000 | Normal close | Reconnect when the page needs the project again. |
| 1006 | Connection lost | Reconnect with backoff. |
| 4000 | Malformed frame | Reconnect with backoff; after three failures, show "Sync failed" with a reload button. |
| 4004 | Unknown project, or the user is not a member | Stop; the save state says "Stopped" with the reason. |
| 4002 | Too slow | The client did not read its frames and its queue on the server filled. Reconnect with backoff. |
| 4003 | Not allowed | Reload the role; continue read-only or leave the project. Local changes stay in IndexedDB. |
| 4029 | Too many connections: the project has too many sockets, or the account has 32 open | Reconnect with backoff. |
| 4130 | Update too large, or the project is at its 32 MiB limit | Show "This change is too large to save"; keep it on the device. |
| 4400 | Protocol version mismatch | Show "Nisaba was updated. Reload to continue." Local changes stay in IndexedDB and go out after the reload. |
| 4401 | No valid session | Stop; the save state says "Stopped" with the reason. A reload shows the sign-in screen. Local changes stay in IndexedDB. |
| 4409 | Project replaced | The project's epoch changed (section 10). Offer the unsent changes as a download, then clear the device log and reload. |
| 4500 | Server error | Reconnect with backoff. |

Backoff starts at 0.5 s, doubles to 30 s and has random jitter. The client
sends a Heartbeat every 20 s and the server answers with one. The server
closes a socket that sends nothing for 60 s.

The epoch is a random value per project. It changes only when an archive
import replaces the project's document. A restore from backup does not change
it, because there the clients' extra edits are wanted.

### Presence

Cursors and selections travel as Loro ephemeral-store bytes in Presence
frames. The server sends each one to the other clients in the project. It
does not store them and keeps no list of who is present; each client builds
that list from the frames it receives.

## 5. Tracked changes, comments and history

### Tracked changes

Tracked changes are Loro rich-text marks on the file's text. Marks belong to
the characters they cover, so they move correctly when other people edit
around them at the same time.

There are two mark keys:

- `ins` covers text that someone proposes to add. Its value is
  `{id, author, time}`. It expands at its end, so typing at the end of a
  suggestion extends it.
- `del` covers text that someone proposes to remove. Its value is the same
  shape. It never expands. The text stays in the document until someone
  accepts the deletion.

In suggesting mode, the editor turns each edit into marks. Typed text gets an
`ins` mark. Deleting text that is not one's own suggestion puts a `del` mark
on it instead of removing it. Deleting text inside one's own `ins` mark
removes it.

Accepting an insertion removes its mark. Accepting a deletion removes the
text. Rejecting does the reverse. Each action is one ordinary Loro change, so
undo, history and sync need nothing extra.

Commenters may only suggest. The room checks each update from a commenter:
the update may add text only inside a new `ins` mark by that commenter, and
may remove text only inside that commenter's own `ins` marks. It may add `del`
marks and change thread containers. The room refuses any other update with
close code 4003. Until this check exists (build step N4), the room refuses
every update from a commenter.

The export reads marks too. "As written" leaves the text as it is, with
suggested insertions and suggested deletions both present. "With suggestions
applied" removes `del` text and keeps `ins` text. "Original" removes `ins`
text and keeps `del` text. Each view is a short function over the text and its
marks. The server and the client each have one copy of it, and a shared test
fixture keeps the two copies equal.

### Comments

A thread is a map in `threads`:

```
{ file: file id,
  start: Loro cursor (encoded), end: Loro cursor (encoded),
  quote: the text the thread was opened on,
  resolved: bool,
  messages: list of { id, author, time, body, edited } }
```

Loro cursors stay attached to characters through concurrent edits. When the
anchored text is deleted, both cursors fall on the same position. The panel
then shows the thread under "Text removed" with its stored quote. Comment
bodies are plain text with line breaks. Each message is a separate list item,
so two replies at once never overwrite each other.

### History and checkpoints

Loro keeps every operation, so any past version can be rebuilt from its
frontiers. A checkpoint is a row with a name and the frontiers of the
document at that moment.

- The server makes an automatic checkpoint for a project after 30 minutes of
  editing activity, and when the last client leaves after a session of
  editing.
- A user with the editor or owner role can make a named checkpoint.
- The server makes a checkpoint before every restore and every archive
  import into an existing project.

The history panel lists checkpoints. Selecting one shows the project's files
at that version, and a diff of each changed file against the current text.
The server builds a version view by forking the document at the frontiers.

A restore never rewinds the document. The server computes, for each file,
the change from the current text to the checkpoint's text, using Loro's text
diff. It applies the changes as one update through the room. Clients see a
normal edit, which they can undo, and nobody's later work is lost in history.

## 6. Accounts and sign-in

### The public address

`--public-url` names the address people type into the browser, such as
`https://nisaba.example.org`: a scheme, a host and an optional port, nothing
else. Without it, a server that listens on a loopback address uses
`http://` and that address, and a server that listens on any other address
refuses to start. A plain `http://` public address is accepted only when its
host is loopback (`localhost`, a name under `.localhost`, `127.0.0.0/8` or
`::1`). `--allow-insecure-http` accepts a plain-http address on the network
anyway; passwords and session cookies then cross it unencrypted. TLS is left
to a reverse proxy in front of the server.

Three things follow from the public address: the session cookie (below), the
origin check on every request that changes state, and the origin check on
the sync socket. Both origin checks compare the `Origin` header with the
public address, ignoring ASCII case.

### First run

When the database has no users and the server can be reached from other
machines, the server makes a setup code at start: 128 random bits written as
eight groups of four hex digits. It prints the code once on standard output
and stores only its SHA-256 in `settings`. Each start replaces the code. The
server counts as reachable from other machines unless both the listen
address and the public address are loopback; then it makes no code.

The setup page asks for the code when one is open, and for a name, an email
address and a password of at least 8 characters. The server compares the
SHA-256 of the code, with everything but hex digits dropped and in lower
case, against the stored hash in constant time. Checking that no user exists,
checking the code, inserting the account and deleting the code are one
`BEGIN IMMEDIATE` transaction, so two setups at once make exactly one owner;
the other gets 409. A wrong code gets 403. Setup has no rate limit: a limit
would let anyone lock the owner out, and 128 bits cannot be guessed.

The first account is the server administrator. Without the code, a stranger
on the same network could take the server before its owner does.

### Local accounts

Passwords use Argon2id with the `argon2` crate's default parameters. At most
four hashes run at once, on blocking threads; further sign-ins wait. A
sign-in for an unknown email checks the password against a fixed hash, so
both cases take the same time.

The server stores a session as the SHA-256 of a random 32-byte token. Behind
an https public address the browser holds the token in a cookie named
`__Host-nisaba_session` with `Secure`, `HttpOnly`, `SameSite=Lax` and
`Path=/`; the `__Host-` prefix makes the browser refuse it unless it is
secure, has that path and names no domain. Behind plain http the cookie is
`nisaba_session` with the same attributes except `Secure`. Sessions last 30
days from sign-in. Signing out deletes the session row; making a session
deletes the expired ones.

Every request other than `GET` and `HEAD` must carry an `Origin` header equal
to the public address, or the server answers 403. Browsers send `Origin` on
every such request, so a page of another site cannot make one with the
user's cookie, and `SameSite=Lax` keeps the cookie off cross-site
subrequests. There are no separate CSRF tokens. `GET` routes change nothing.

The server allows 10 sign-in attempts per minute per account; the eleventh
gets 429.

Other limits: a request body may be 64 KiB, an asset upload 32 MiB and a
project import 64 MiB (413 beyond). An account may hold 32 sync sockets at
once; the next one is closed with 4029.

### Roles

| Action | owner | editor | commenter | viewer |
|---|---|---|---|---|
| Read files, preview, export | yes | yes | yes | yes |
| Edit text, upload files, manage references | yes | yes | no | no |
| Suggest changes, comment | yes | yes | yes | no |
| Accept or reject suggestions | yes | yes | no | no |
| Make and restore checkpoints | yes | yes | no | no |
| Invite and change members | yes | no | no | no |
| Rename, archive or delete the project | yes | no | no | no |

The server administrator manages accounts and server settings. Being an
administrator gives no access to projects.

### Invitations

An owner makes an invitation for a role. The server returns a link with a
random token and stores only its hash. The link works once and expires after
seven days. Following it, a new person makes an account (or signs in with
OIDC), and an existing user signs in. Nisaba does not send email; the owner
sends the link. The administrator can make a password-reset link for any
account in the same way.

### OIDC

OIDC is optional. The administrator sets an issuer URL, a client id and a
client secret in the server settings. Sign-in uses the authorization-code
flow with PKCE. The server links an identity to a user by issuer and subject.
An unknown identity can sign in only through an invitation, unless the
administrator turns on open sign-up for that issuer. Nisaba reads no roles
or groups from the identity provider. Project roles live only in `members`.

## 7. References

Nisaba uses [pinakes](https://github.com/nebu-tools/pinakes), a separate Rust
library, for reference import, citation keys, duplicate detection and
Hayagriva output. pinakes works on CSL-JSON items, the same form the project
document stores, so Nisaba keeps no reference model of its own.

### In Nisaba

A project's references live in the `references` map of its document, one
CSL-JSON item per entry, keyed by a random id. A change replaces the whole
item. References therefore sync, appear in history and go into the archive
like everything else.

There is one citation system: Typst's own. Authors cite with `@key` and print
the list with `#bibliography`. The compiler sees the project's library as a
generated file `references.yml` in Hayagriva format, built by pinakes.
Authors write `#bibliography("references.yml")`. A
project can also carry its own `.bib` file; Typst reads it directly. Nisaba
adds no citation syntax and no numbering step of its own.

The references panel imports a file or pasted text in any format pinakes
reads, shows duplicates before it adds anything, and lets the user edit an entry.
The editor completes `@` with keys from the library and from `.bib` files in
the project.

## 8. Compile pipeline, fonts and packages

### LiveCompiler

`nisaba-compile-core` has one public compiler type, `LiveCompiler`. A host
creates it for an entrypoint, writes each file once, and then writes only the
files that change. The compiler holds its Typst world for its lifetime, so
Typst's memoized results survive between compiles and a small edit
recompiles only what it affects.

```rust
impl LiveCompiler {
    pub fn new(entry: &str, fonts: &[FontEntry]) -> Result<Self, String>; // catalogue only, no font data
    pub fn add_font(&mut self, index: usize, data: Vec<u8>);
    pub fn set_today(&mut self, year: i32, month: u8, day: u8);
    pub fn write(&mut self, path: &str, contents: FileContents); // Text or Bytes
    pub fn remove(&mut self, path: &str);
    pub fn compile(&mut self) -> Compiled; // page hashes, sizes, problems, missing fonts
    pub fn page_rgba(&self, index: usize, pixel_per_pt: f32) -> Option<(u32, u32, Vec<u8>)>;
    pub fn pdf(&self) -> Result<Vec<u8>, String>;
}
```

Build step N10 adds three methods for the text layer and the jumps between
source and preview:

```rust
    pub fn page_text(&self, index: usize) -> Vec<TextRun>;  // text, position, size
    pub fn source_at(&self, index: usize, x: f32, y: f32) -> Option<(String, usize)>;
    pub fn preview_at(&self, path: &str, offset: usize) -> Vec<(usize, f32, f32)>;
```

`compile` returns a 128-bit hash for each page. Equal hashes mean equal pages.
When a compile fails, the compiler keeps the last good layout, and `compile`
reports the problems together with the old pages. Each problem has a path and
a UTF-16 range, which is what the editor uses.

### The preview

The browser runs `LiveCompiler` in a dedicated worker.

1. The page sends the worker each file once, then only files whose text or
   hash changed, and a removal for each path that left the document, so a
   renamed or deleted file is gone from the preview as it is from the export.
   Asset bytes come from `/api/projects/{id}/assets/{hash}` and are cached by
   hash. A response other than 200 is a failure (below).
2. While a compile runs, or assets load, the page collects further changes.
   When the step ends, it sends one new compile request. At most one compile
   is in flight.
3. The worker compiles, compares page hashes with the previous compile, and
   reports the new page sizes and the list of changed pages.
4. The worker rasterizes the changed pages that are on screen, at the pane's
   width times the device pixel ratio. It sends each page as an `ImageBitmap`,
   which the page draws into a canvas without copying.
5. Changed pages that are off screen are marked stale. The page asks for them
   when they scroll into view.

Rasterizing in the worker matters. An SVG page of the test document is about
400 KB of markup. Inserting one into the page cost the main thread about
210 ms, and keystroke-to-page latency was 640 ms. With bitmaps the main thread
spends under 1 ms per page, and the median latency is 82 to 88 ms (section 11).

### The text layer

Each page comes with a text layer: the page's text runs, built from the page
frame, laid out as transparent text over the bitmap at the same positions. A
user can select and copy text in the preview. A click in the preview opens
the source file at the clicked text, which Typst finds from the span of that
text. A command in the editor and the palette jumps from the cursor to the
matching place in the preview and scrolls it into view. The worker sends the
text layer with the bitmap of the same page, so the two never disagree.

### When the preview fails

`Feed` (web/src/preview/feed.ts) decides what the page sends and when; its
states are idle, fetching assets, compiling and failed, and its unit tests
drive each path. Every path out of fetching and compiling clears the gate:

- The worker answers `done` for the current compile: idle, and compile again
  if anything changed meanwhile.
- The worker says `failed` (the compiler or its fonts could not load, a font
  file did not answer 200, or the compiler threw), the worker raises an
  error, or a message cannot be read: failed.
- A compile runs longer than 30 s: failed. The worker is busy and cannot be
  asked to stop, so it is terminated.
- An asset does not load: failed.

On failure the page terminates the worker, starts a new one, forgets what it
sent, and shows the message with "Retry now". It retries by itself after 1,
2, 5, 10 and then every 20 s, at once on the next edit, and when the browser
comes back online. A retry sends every file again. A compile that works
resets the delays. A `done` from a worker that was replaced is ignored. The
preview never switches to a server compile.

### The date

`datetime.today()` is the caller's local calendar date. The worker sets the
date from the browser's clock before every compile, so a preview left open
past midnight shows the new day. The export link carries the local date as
`?today=YYYY-MM-DD`. The server uses it when it lies within one day of the
server's UTC date, which covers every time zone, and otherwise uses the UTC
date. A request without the parameter, or with a malformed one, gets the UTC
date. Two exports of the same files on the same local day are therefore
equal byte for byte.

### Rules for the preview

- The preview path has no PDF. The worker draws pages from the compiled
  layout; only export makes a PDF.
- At most one compile is in flight.
- The preview never falls back to a server compile. A failed or slow compile
  shows a message and a retry button and stays in the browser.
- The preview holds exactly the document's files. The test
  `agreement.test.ts` renames and deletes files and checks that the preview's
  compiler gives the same result as a fresh compile of every file, which is
  what the export does; the server test `export_follows_a_rename_and_a_delete`
  checks the export side with the same document.

### Export

`GET /api/projects/{id}/export.pdf?today=YYYY-MM-DD` reads the files from
the project's room, resolves the assets the project holds, and compiles them
in a child process. The server keeps no compilers between exports. Typst's PDF
output depends only on its input, so the export equals, byte for byte, a
compile of the same files anywhere else. The test
`export_reads_the_same_state_as_sync` checks this: it edits through sync,
exports through HTTP, compiles the client's copy of the text locally and
compares the bytes. The same property lets an offline client make the PDF
itself.

A document is a program, and `#("x" * 1000000000000)` or a loop that never
ends is a valid one. The export therefore runs in a child: the server starts
its own binary with the hidden subcommand `__export`, an empty environment
and the temporary directory as working directory, writes the files to its
standard input and reads the PDF from its standard output. The child:

1. sets `RLIMIT_AS` to the memory limit, `RLIMIT_CPU` to the time limit plus
   one second, and `RLIMIT_CORE` to 0;
2. reads the request and loads the embedded fonts;
3. on Linux with Landlock (5.13 and later), takes away all file access, all
   TCP binds and connects, and abstract Unix sockets and signals to other
   processes (`AccessFs`, `AccessNet` and `Scope` at ABI 6, best effort, so
   an older kernel enforces what it knows);
4. compiles and writes the PDF, or writes the problems and exits with 2.

The server kills the child when the deadline passes, and when the request is
dropped. It answers:

| Outcome | Status | Message |
|---|---|---|
| PDF | 200 | |
| Compile problems | 422 | the problems |
| Allocation failed or `SIGKILL`/`SIGABRT` | 422 | "the export ran out of memory: …" with the limit |
| Deadline passed or `SIGXCPU` | 422 | "the export ran out of time: …" with the limit |
| All slots and the queue full | 503 with `Retry-After: 30` | "the server is busy with other exports; try again in a minute" |
| Anything else | 500 | what the child wrote |

The limits are settings:

| Flag | Default | Meaning |
|---|---|---|
| `--export-memory-mb` | 1024 | address space of one export child, at least 64 |
| `--export-seconds` | 60 | wall-clock deadline, at least 1 |
| `--export-parallel` | 2 | exports that run at once |
| `--export-queue` | 8 | exports that wait for a slot; beyond this, 503 |

The server tests run exports with small limits: a terabyte string under
512 MiB answers "out of memory", a nested loop under 2 s answers "out of
time" while a second export with no queue answers 503, and a normal export
works after each.

Linux is the target. On other systems the child still runs as a separate
process with the deadline, the kill, the parallel cap and the queue, so the
server survives any document, but there is no memory limit and no file or
network restriction: a document that allocates without end can exhaust the
machine's memory before the deadline.

An export option for PDF/A-2b output is planned.

### Fonts

Typst needs fonts. The WebAssembly module holds none. The WASM build writes
each font as its own file named by its hash, plus `fonts.json`, the catalogue
of every font with its family, style and weight. The server embeds them with
the web client and serves the font files with a one-year immutable cache
lifetime; `fonts.json` is revalidated on each visit. The compiler starts with
the catalogue only. When a compile needs a face whose data it lacks, it
reports the face in `missing_fonts`. The worker fetches those files, adds
them and compiles again, so a document downloads only the faces it uses. The
server export adds every font from its embedded copy before it compiles.
The fonts are the Typst defaults: Libertinus Serif, New Computer Modern and DejaVu Sans
Mono. The administrator can add fonts to `DATA/fonts`. A project can carry
its own font files, and the compiler adds any `.ttf`, `.otf` or `.woff2` file
in the project.

### Packages

A document can import packages from the Typst package registry, such as
`@preview/cetz:0.3.4`. The server answers `GET /packages/{namespace}/{name}/{version}.tar.gz`
from `DATA/packages`. On a cache miss, and if the administrator allows
network access, it downloads the package from the registry, checks its size,
and stores it. The browser worker asks the server for a package the first
time a compile reports it missing, unpacks it into the compiler, and keeps
the archive in IndexedDB. The server export uses the same cache.

Offline, a client can use every package it has cached, and the server can use
every package in `DATA/packages`. `nisaba packages add @preview/cetz:0.3.4`
fills the cache ahead of time for servers without network access. A package
that is not available gives a compile problem that names the package and says
that it is not available offline.

## 9. Web client

### Solid for the interface

Screens, panels, dialogs, lists and the command palette use Solid
(`solid-js`). The client has seven screens, seven panels and many lists that
change while the user looks at them: files, people present, suggestions,
threads, references, problems and checkpoints. Solid's fine-grained signals
update the DOM nodes that read a value and nothing else. There is no
virtual-DOM pass, so there is no render cycle between an event and the
screen.

CodeMirror, the preview canvases, the two workers and the sync client stay
imperative modules. A Solid component creates each one in its element when
it mounts and destroys it on cleanup. Sync and compile events become signals:
the sync client sets the save state, the people present and the file list,
the preview worker sets the pages and the problems, and components read those
signals.

The client has one module per screen and per panel. It has no `effect`
library and no schema library.

### HTTP API types

The HTTP API types have one source, the Rust structs in `nisaba-server`.
`ts-rs` generates the TypeScript types from them into
`web/src/app/api.gen.ts`. The generated file is committed, and CI fails when
it differs from what the server code generates.

### Screens

| Screen | Content |
|---|---|
| Setup | First-run form: setup code, name, email, password. |
| Sign in | Email and password, and "Sign in with …" when OIDC is set. |
| Invitation | Make an account or sign in, then join the project. |
| Projects | Projects you are a member of, most recent first; search; "New project"; "Import archive". |
| Workspace | The editor for one project (below). |
| Account | Name, password, editor preferences, sign out. |
| Administration | Users, password-reset links, OIDC settings, package cache. Administrators only. |

### The workspace

```
┌───────────────────────────────────────────────────────────────────────┐
│ ‹ Projects / Rock pools ▾   chapters/ch02.typ     Saved  ●●  Share  Export │
├──┬────────────┬──────────────────────────────┬──────────────────┬──┤
│F │ file tree  │                              │                  │P │
│O │ or outline │          editor              │     preview      │R │
│R │ or refs    │                              │  or review list  │  │
│S │ or search  │                              │                  │  │
│H │ or history │                              │                  │  │
└──┴────────────┴──────────────────────────────┴──────────────────┴──┘
```

The top bar has five controls. On the left are the way back to projects
and the project name, which opens a menu with Rename, Project settings,
Download archive and Delete. Next is the open file's path. On the right are
the save state, the people present, Share and Export.

Everything else moves to two vertical rails. The left rail switches the left
panel between Files, Outline, References, Search and History. The right rail
switches the right panel between Preview and Review, the list of suggestions
and comment threads. Each rail button has a keyboard shortcut. The command
palette (Ctrl+K or Cmd+K) lists every command, file and heading, so no feature
depends on finding its button. Account settings and sign-out move into the
account menu on the Projects screen and the palette.

Project search runs in the client. The client holds every text file of the
project, so it searches them directly and shows matches by file with their
lines. Replace-all is one Loro change.

### Loading, empty, offline and error states

Loading:

- Opening a project shows the file tree and the editor from the device log
  as soon as IndexedDB answers, before the socket connects.
- The first time, the preview loads the compiler. The preview pane says
  "Loading the typesetter (first time only)" and shows the bytes received.
  After that the browser cache serves it.
- When a step takes longer than 3 s, the screen names the step: "Waiting for
  the server", "Loading the typesetter", "Typesetting 28 pages".

Empty:

- No projects: "Start a project" and "Import an archive", with one sentence
  that says what a project is.
- An empty file: a hint line in the editor that disappears on typing.
- No references: "Import references" with the accepted formats.
- No comments: "Select text and press Ctrl+Alt+M to comment."

Offline:

- The save state says "Offline · changes kept on this device". After 10 s
  offline, a quiet banner says that editing and the preview continue and that
  the changes go to the server on reconnection.
- Export uses the browser compiler. Share, invitations and history, which need
  the server, are disabled and say why.
- A project never opened on this device cannot open offline. The Projects
  screen marks the projects the device holds.

Errors:

- A compile error keeps the last good pages, dims them, and labels them
  "Showing the last version that compiled". A strip under the preview lists
  the problems. Selecting a problem opens its file at its range. The editor
  underlines the same ranges.
- Losing write access (4003) turns the editor read-only with a banner. Local
  changes stay on the device and the banner offers them as a download.
- An expired session (4401) stops sync and says so in the save state; a
  reload shows the sign-in screen. Unsent changes stay in IndexedDB.
- When the session request fails, the app shows the error and a Retry
  button, and retries by itself every 5 s.
- When IndexedDB is unavailable, as in some private windows, the save state
  says "Not saved on this device" while changes are unacknowledged, and the
  page asks for confirmation before it closes.

## 10. Export and the project archive

The Export button offers PDF (as written, with suggestions applied, or
original; normal or PDF/A-2b) and the project archive.

The project archive is a zip file:

```
rock-pools.zip
  nisaba.json         format version, project name, entrypoint, export time
  files/...           every project file at its path, text and assets
  references.json     the library as a CSL-JSON array
  comments.json       threads: file path, quoted text, line and column, messages
  history.loro        optional: the Loro document with full history
```

`files/` is a complete Typst project. `typst compile files/main.typ` works on
it without Nisaba, provided the library is written out as
`files/references.yml`, which the export does. `history.loro` is included
unless the user turns it off.

Importing an archive creates a new project. When `history.loro` is present
and valid, the server imports it, so history, suggestions and comment anchors
come back exactly. Otherwise it builds a new document from `files/`,
`references.json` and `comments.json`. An owner can also import an archive
into an existing project, which replaces its document. The server first makes
a checkpoint, then changes the project's epoch, so open clients get close code
4409 and do not mix their caches with the new document.

Import limits: at most 500 MB uncompressed, 10,000 entries, and a compression
ratio of at most 100 to 1 per entry. Paths are normalized. An entry with an
absolute path, a `..` component or a duplicate path stops the import.

## 11. Performance targets

| Measure | Target | Measured |
|---|---|---|
| Binary start to listening, empty data directory | < 200 ms | 50–54 ms |
| Empty data directory to a usable editor (setup, new project, editor open) | < 2 s | 0.58–0.62 s |
| Empty data directory to the first preview page | < 3 s | 0.82–0.88 s |
| First preview of a 28-page project with figures and 60 references, fresh browser context | < 2 s | 1.39–1.46 s |
| Keystroke to painted page, median, 28 pages, pixel ratio 1 | < 100 ms | 82–88 ms |
| Keystroke to painted page, 95th percentile, pixel ratio 1 | < 200 ms | 101–148 ms |
| Keystroke to painted page, median / 95th percentile, pixel ratio 2 | < 120 / 200 ms | 82 / 107 ms |
| Incremental compile of one changed chapter | < 80 ms | 61–66 ms (median) |
| Raster of one A4 page in the worker, pixel ratio 1 / 2 | < 30 ms | 9 / 25 ms |
| Server PDF export of the 28-page project | < 1 s | 0.29–0.41 s |
| Update to Ack on a local network, 95th percentile | < 50 ms | not measured |
| Room open, snapshot plus 500 log entries | < 100 ms | not measured |
| First visit, compiler download on a 50 Mbit/s link | < 3 s | 7.8 MB module (brotli) plus the faces the document uses; about 1.3 s by size, not measured on a link |
| Concurrent editors in one project | 20 | not measured |
| Open projects on one server, 2 GB RAM | 200 | not measured |

Each measured range covers three runs on a 12-core x86-64 workstation with headless
Chromium, a release build, the server on a loopback address, and a fresh
browser profile and data directory for each run. The test document is
generated by `web/bench/fixture.mjs`: a field guide to rock pools in ten
chapter files, with five SVG figures, five PNG figures, an outline and 60
BibTeX entries cited throughout.

To reproduce:

```
node scripts/build-wasm.mjs
cd web && bun install --frozen-lockfile && bun run build && cd ..
cargo build --release -p nisaba-server
node web/bench/measure.mjs target/release/nisaba
DPR=2 node web/bench/measure.mjs target/release/nisaba
cargo test -p nisaba-server
```

## 12. Out of scope

- Running more than one server process for one data directory, replication
  and high availability.
- Postgres, object storage, an external identity provider as a requirement, a
  reverse proxy as a requirement.
- Organizations and multi-tenancy.
- Sending email.
- DOCX import. DOCX export is not in the first version; it comes through
  [tabellion](https://github.com/nebu-tools/tabellion) once tabellion has a
  release.
- LaTeX and Markdown as source formats.
- Search across projects on the server.
- Trimming old history.
- End-to-end encryption.
- Editing on phones. Phones can read, preview and comment.
- Signing in for the first time while offline.
- A plugin system.

## 13. Build order

- N2: `nisaba-server` is complete: accounts and sessions, projects, members,
  rooms, compaction, backup, export, static files and the generated API types.
- N3: the web client core: the sync client, the device log, the save states,
  the preview worker and page view, the workspace shell, and the setup,
  sign-in and projects screens.
- N4: tracked changes and comments: the marks, the commenter check on the
  server and the review panel.
- N5: references through pinakes: the panel, import, duplicates,
  `references.yml` and `@` completion.
- N6: assets, fonts and the package cache, with the administration screen.
- N7: checkpoints, history and restore.
- N8: invitations, roles and OIDC.
- N9: the project archive and close code 4409.
- N10: the preview text layer and the jumps between source and preview.
- N11: end-to-end tests of the whole product, and the performance table as a
  CI check.
