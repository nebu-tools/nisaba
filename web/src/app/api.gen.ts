// Generated from api/openapi.json, which is generated from
// crates/nisaba-server/src/api. Do not edit.
// Regenerate: NISABA_WRITE_API=1 cargo test -p nisaba-server --lib api

/**
 * `PATCH /api/account`.
 */
export type AccountChange = {
  /**
   * The new display name, 1 to 100 characters.
   */
  name: string
}

/**
 * Every error the API answers has this shape.
 */
export type ApiError = {
  /**
   * Stable: the client may branch on it.
   */
  code: ErrorCode
  /**
   * The request field the error is about, when there is one.
   */
  field?: string | null
  /**
   * A sentence a person can read. It may change between versions.
   */
  message: string
  /**
   * The compile problems of a failed export.
   */
  problems?: Array<Problem> | null
  /**
   * When to try again, in seconds; also sent as `Retry-After`.
   */
  retry_after?: number | null
}

/**
 * The content hash of an uploaded asset.
 */
export type AssetAdded = {
  hash: string
}

/**
 * The query of `POST /api/projects/{project}/assets`.
 */
export type AssetQuery = {
  /**
   * Where the file goes in the project, such as `figures/limpet.png`.
   */
  path: string
}

/**
 * `POST /api/login`.
 */
export type Credentials = {
  email: string
  password: string
}

/**
 * The stable error codes. `ERROR_STATUS` gives the HTTP status of each,
 * and the API description what each means.
 */
export type ErrorCode = "invalid_request" | "invalid_field" | "not_signed_in" | "wrong_credentials" | "wrong_setup_code" | "cross_origin" | "forbidden" | "not_found" | "method_not_allowed" | "setup_done" | "email_taken" | "already_member" | "last_owner" | "sole_owner" | "last_admin" | "payload_too_large" | "project_full" | "range_not_satisfiable" | "compile_failed" | "export_out_of_memory" | "export_out_of_time" | "too_many_attempts" | "internal" | "package_unavailable" | "busy" | "try_again" | "export_busy" | "exports_off" | "stopping" | "web_client_missing" | "storage_full"

export type ExportMode = "sandboxed" | "unsandboxed" | "off"

/**
 * The query of `GET /api/projects/{project}/export.pdf`.
 */
export type ExportQuery = {
  /**
   * Default `pdf`.
   */
  standard?: Standard | null
  /**
   * The caller's local date, `YYYY-MM-DD`. Used when it lies within one
   * day of the server's UTC date; otherwise the UTC date is used.
   */
  today?: string | null
  /**
   * Which text of tracked changes to typeset. Default `written`.
   */
  view?: View | null
}

/**
 * One file of `GET /api/projects/{project}/files`: `text` for sources,
 * `hash` for assets.
 */
export type FileEntry = {
  /**
   * The SHA-256 of an asset's bytes, in hex.
   */
  hash?: string | null
  /**
   * The file's id in the document; random, never reused.
   */
  id: string
  path: string
  text?: string | null
}

/**
 * One entry of `fonts.json`.
 */
export type FontCatalogueEntry = {
  /**
   * The font file, relative to `/fonts/`.
   */
  file: string
  /**
   * The face's index inside the file.
   */
  index: number
  /**
   * Typst's description of the face: family, variant, coverage.
   */
  info: unknown
}

/**
 * `GET /api/health`.
 */
export type Health = {
  /**
   * Whether exports run: in the sandbox, without it, or not at all.
   */
  exports: ExportMode
  /**
   * `ok` while the server can read its database and is not stopping.
   */
  status: HealthStatus
  /**
   * The server's version.
   */
  version: string
  /**
   * Whether this binary holds the web client.
   */
  web_client: boolean
}

export type HealthStatus = "ok" | "failing" | "stopping"

/**
 * A file in a new project: `text` for sources, `base64` for assets.
 */
export type ImportFile = {
  base64?: string | null
  /**
   * A relative path such as `chapters/tides.typ`.
   */
  path: string
  text?: string | null
}

/**
 * One member of a project.
 */
export type Member = {
  email: string
  name: string
  role: Role
  user_id: string
}

/**
 * `POST /api/admin/users`: an account made by an administrator.
 */
export type NewAccount = {
  email: string
  is_admin?: boolean
  /**
   * The display name; the email when empty.
   */
  name?: string
  /**
   * At least 8 characters. The person changes it after signing in.
   */
  password: string
}

/**
 * `POST /api/projects/{project}/members`: adds the account with this
 * email.
 */
export type NewMember = {
  email: string
  role: Role
}

/**
 * `POST /api/projects`.
 */
export type NewProject = {
  /**
   * The first files. Without any, the project gets a `main.typ`.
   */
  files?: Array<ImportFile>
  /**
   * 1 to 200 characters.
   */
  name: string
}

/**
 * `POST /api/account/password`. Every other session of the account ends.
 */
export type PasswordChange = {
  current_password: string
  /**
   * At least 8 characters.
   */
  new_password: string
}

/**
 * A compile problem in an export that failed.
 */
export type Problem = {
  /**
   * End of the range, as a UTF-16 offset.
   */
  end: number | null
  message: string
  /**
   * The project file the problem points into, when Typst named one. A
   * file of a package is written `@namespace/name:version/path`.
   */
  path: string | null
  /**
   * `error` or `warning`.
   */
  severity: string
  /**
   * Start of the range, as a UTF-16 offset.
   */
  start: number | null
}

/**
 * A project as the project list shows it.
 */
export type Project = {
  /**
   * Seconds since 1970.
   */
  created_at: number
  id: string
  name: string
  /**
   * The caller's role in it.
   */
  role: Role
  /**
   * The last change to its document or name, to the minute, in seconds
   * since 1970. The list is sorted by it, newest first.
   */
  updated_at: number
}

/**
 * `PATCH /api/projects/{project}`.
 */
export type ProjectChange = {
  /**
   * 1 to 200 characters.
   */
  name: string
}

/**
 * A role in one project. DESIGN.md section 6 lists what each may do. A
 * commenter may not change the document until build step N4 adds the
 * check of suggestions, so today a commenter reads like a viewer.
 */
export type Role = "owner" | "editor" | "commenter" | "viewer"

/**
 * `PATCH /api/projects/{project}/members/{user}`.
 */
export type RoleChange = {
  role: Role
}

/**
 * `GET /api/session`: what the client needs to pick its first screen.
 */
export type Session = {
  /**
   * When the session ends, in seconds since 1970, or null without one.
   * The server moves it forward when this route is called, at most once
   * a day, up to 90 days after sign-in.
   */
  expires_at: number | null
  /**
   * Setup asks for the code the server printed to its log.
   */
  setup_code_needed: boolean
  /**
   * No account exists yet.
   */
  setup_needed: boolean
  /**
   * The signed-in person, or null.
   */
  user: User | null
}

/**
 * `POST /api/setup`: the first account, which is the administrator.
 */
export type SetupRequest = {
  email: string
  /**
   * The display name; the email when empty.
   */
  name?: string
  /**
   * At least 8 characters.
   */
  password: string
  /**
   * The code from the server log; needed unless the server is reachable
   * only from this machine.
   */
  setup_code?: string | null
}

export type Standard = "pdf" | "pdf-a-2b"

/**
 * A person with an account, without the password hash.
 */
export type User = {
  email: string
  id: string
  /**
   * Server administrators manage accounts. Being one gives no access to
   * projects.
   */
  is_admin: boolean
  name: string
}

/**
 * The text of a document with tracked changes (DESIGN.md section 5).
 */
export type View = "written" | "applied" | "original"

/** The HTTP status of each error code. */
export const ERROR_STATUS: Record<ErrorCode, number> = {
  /**
   * The request could not be read: not JSON, a missing or unknown field, or a bad query or path.
   */
  invalid_request: 400,
  /**
   * A field has a value the server does not accept; `field` names it.
   */
  invalid_field: 400,
  /**
   * No valid session: sign in first.
   */
  not_signed_in: 401,
  /**
   * Wrong email or password, or wrong current password.
   */
  wrong_credentials: 401,
  /**
   * The setup code is missing or wrong.
   */
  wrong_setup_code: 403,
  /**
   * The request changes state and its Origin header is not this server's public address.
   */
  cross_origin: 403,
  /**
   * The caller's role or account does not allow this.
   */
  forbidden: 403,
  /**
   * No such thing, or the caller may not see it. A project the caller is not a member of is not found.
   */
  not_found: 404,
  /**
   * The path exists, but not with this method.
   */
  method_not_allowed: 405,
  /**
   * The server already has an administrator.
   */
  setup_done: 409,
  /**
   * Another account has this email.
   */
  email_taken: 409,
  /**
   * The account is already a member of the project.
   */
  already_member: 409,
  /**
   * The project would have no owner left.
   */
  last_owner: 409,
  /**
   * The account is the only owner of a project; delete the projects or give them another owner first.
   */
  sole_owner: 409,
  /**
   * The server would have no administrator left.
   */
  last_admin: 409,
  /**
   * The request body is larger than this route takes.
   */
  payload_too_large: 413,
  /**
   * The project is at its 32 MiB limit for text and history.
   */
  project_full: 413,
  /**
   * The byte range lies outside the file.
   */
  range_not_satisfiable: 416,
  /**
   * The document has errors; `problems` lists them.
   */
  compile_failed: 422,
  /**
   * The export needed more memory than its limit.
   */
  export_out_of_memory: 422,
  /**
   * The export took longer than its limit.
   */
  export_out_of_time: 422,
  /**
   * Too many failed sign-ins for this email; wait `retry_after`.
   */
  too_many_attempts: 429,
  /**
   * The server failed. The log says why.
   */
  internal: 500,
  /**
   * The package registry could not be reached or sent something unusable.
   */
  package_unavailable: 502,
  /**
   * All password workers are busy; try again after `retry_after`.
   */
  busy: 503,
  /**
   * A large update to this project is on its way and holds the space; try again in a minute.
   */
  try_again: 503,
  /**
   * All export slots and the queue are full; try again in a minute.
   */
  export_busy: 503,
  /**
   * This server cannot sandbox exports and was not allowed to run them without.
   */
  exports_off: 503,
  /**
   * The server is stopping; try again in a minute.
   */
  stopping: 503,
  /**
   * This server was built without the web client.
   */
  web_client_missing: 503,
  /**
   * The server's disk is full; nothing was written.
   */
  storage_full: 507,
}

/** The sync protocol version. */
export const PROTOCOL = 2
/** The longest WebSocket message's payload; longer frames go in parts. */
export const PART = 1048576
/** The client sends a Heartbeat this often. */
export const HEARTBEAT_SECONDS = 20
/** The server closes a socket that sends nothing for this long. */
export const IDLE_SECONDS = 60

/** The type byte of each sync frame. */
export const FrameType = {
  /**
   * Opens the session with the client's version vector, empty when it holds nothing. From client. Fields: protocol (u8), version (blob).
   */
  hello: 1,
  /**
   * Every operation the client lacks, and the server's version vector. From server. Fields: catchUp (blob), version (blob).
   */
  welcome: 2,
  /**
   * A Loro update. From both. Fields: update (blob).
   */
  update: 3,
  /**
   * The server's version vector after an update was committed. From server. Fields: version (blob).
   */
  ack: 4,
  /**
   * Loro ephemeral-store bytes: cursors and selections. From both. Fields: state (blob).
   */
  presence: 5,
  /**
   * Keeps the socket open. The server answers each one. From both. Fields: none.
   */
  heartbeat: 6,
  /**
   * The length of the frame that follows in parts. From both. Fields: total (u32).
   */
  begin: 7,
  /**
   * One part of a long frame, numbered from 0. From both. Fields: index (u32), bytes (blob).
   */
  part: 8,
} as const

/** WebSocket close codes, by name. */
export const Close = {
  /**
   * Normal close. Client: Reconnect when the page needs the project again.
   */
  normal: 1000,
  /**
   * The connection was lost. The browser reports this code; no server sends it. Client: Reconnect with backoff. The server does not send this code.
   */
  lost: 1006,
  /**
   * The server is stopping. It finished every update it had received. Client: Reconnect with backoff.
   */
  restarting: 1012,
  /**
   * Try again later: the server holds too many large updates in parts at once, or an unfinished one holds the space this update needs in its project. Client: Reconnect with backoff and send again.
   */
  tryAgain: 1013,
  /**
   * A malformed frame, or parts out of order. Client: Reconnect with backoff. After three in a row with no Ack between them, stop and show "Sync failed" with a Reload button.
   */
  malformed: 4000,
  /**
   * The client did not read its frames: its queue on the server filled, or one frame waited 30 s to be sent. Client: Reconnect with backoff.
   */
  tooSlow: 4002,
  /**
   * The user's role does not allow this update, or the role changed so that the user may no longer write. Client: Reload the role and continue read-only. Local changes stay in IndexedDB.
   */
  forbidden: 4003,
  /**
   * The project does not exist, the user is not a member, the user was removed from it, or it was deleted. Client: Stop; the save state says "Stopped" with the reason.
   */
  notFound: 4004,
  /**
   * An update in parts did not finish before its deadline. Client: Reconnect with backoff and send again.
   */
  timedOut: 4008,
  /**
   * Too many connections: the project has too many sockets, or the account has 32 open. Client: Reconnect with backoff.
   */
  full: 4029,
  /**
   * The update, or its declared length, does not fit beside what the project stores, under its 32 MiB. Client: Stop; do not retry. The save state says "Not saved: the project is full".
   */
  tooLarge: 4130,
  /**
   * The client speaks another protocol version. Client: Show "Nisaba was updated. Reload to continue."
   */
  version: 4400,
  /**
   * No valid session: never had one, signed out, signed out everywhere, expired, ended by a password change elsewhere, or the account was deleted. Client: Stop; the save state says "Stopped" with the reason. A reload shows the sign-in screen.
   */
  signedOut: 4401,
  /**
   * Planned with the project archive: the project's document was replaced. Client: Offer the unsent changes as a download, then clear the device log and reload. The server does not send this code.
   */
  replaced: 4409,
  /**
   * The server could not store the update, for example because its disk is full. Client: Reconnect with backoff.
   */
  serverError: 4500,
} as const

export type CloseCode = (typeof Close)[keyof typeof Close]

/** Fixed limits of the server. */
export const LIMITS = {
  assetBytes: 33554432,
  documentBytes: 33554432,
  emailMaxBytes: 254,
  importBytes: 67108864,
  nameMaxChars: 100,
  passwordMaxBytes: 1024,
  passwordMinChars: 8,
  pathMaxBytes: 512,
  presenceBytes: 65536,
  projectNameMaxChars: 200,
  requestBodyBytes: 65536,
  sessionMaxSeconds: 7776000,
  sessionSeconds: 2592000,
  socketsPerAccount: 32,
  socketsPerProject: 64,
} as const

/** Every route: its method, path, who may call it, what it takes and answers, and the error codes it may send. */
export interface Routes {
  /**
   * The web client. Every path no other route takes gets its `index.html`, except under `/assets/`, `/fonts/`, `/api/`, `/packages/` and `/sync/`.
   */
  web_client: {
    method: "GET"
    path: "/"
    auth: "anyone"
    query: never
    body: never
    reply: { 200: Blob }
    errors: "internal" | "web_client_missing"
  }
  /**
   * Changes the display name.
   */
  account_update: {
    method: "PATCH"
    path: "/api/account"
    auth: "session"
    query: never
    body: AccountChange
    reply: { 200: User }
    errors: "invalid_request" | "invalid_field" | "not_signed_in" | "cross_origin" | "payload_too_large" | "internal"
  }
  /**
   * Changes the password. Every other session of the account ends and its sync sockets close with 4401; this session stays. A wrong current password counts as a failed sign-in.
   */
  password_change: {
    method: "POST"
    path: "/api/account/password"
    auth: "session"
    query: never
    body: PasswordChange
    reply: { 204: void }
    errors: "invalid_request" | "invalid_field" | "not_signed_in" | "wrong_credentials" | "cross_origin" | "payload_too_large" | "too_many_attempts" | "internal" | "busy"
  }
  /**
   * Every account, sorted by email.
   */
  users_list: {
    method: "GET"
    path: "/api/admin/users"
    auth: "admin"
    query: never
    body: never
    reply: { 200: Array<User> }
    errors: "not_signed_in" | "forbidden" | "not_found" | "internal"
  }
  /**
   * Makes an account with a first password.
   */
  user_create: {
    method: "POST"
    path: "/api/admin/users"
    auth: "admin"
    query: never
    body: NewAccount
    reply: { 201: User }
    errors: "invalid_request" | "invalid_field" | "not_signed_in" | "cross_origin" | "forbidden" | "not_found" | "email_taken" | "payload_too_large" | "internal" | "busy"
  }
  /**
   * Deletes an account, its sessions and its memberships. Its sync sockets close with 4401. Refused while it is the only owner of a project, or the last administrator.
   */
  user_delete: {
    method: "DELETE"
    path: "/api/admin/users/{user}"
    auth: "admin"
    query: never
    body: never
    reply: { 204: void }
    errors: "invalid_request" | "not_signed_in" | "cross_origin" | "forbidden" | "not_found" | "sole_owner" | "last_admin" | "internal"
  }
  /**
   * Whether the server can read its database. Answers 503 with the same body while it cannot, and while it stops.
   */
  health: {
    method: "GET"
    path: "/api/health"
    auth: "anyone"
    query: never
    body: never
    reply: { 200: Health; 503: Health }
    errors: "internal"
  }
  /**
   * Signs in and sets the session cookie.
   */
  login: {
    method: "POST"
    path: "/api/login"
    auth: "anyone"
    query: never
    body: Credentials
    reply: { 200: User }
    errors: "invalid_request" | "wrong_credentials" | "cross_origin" | "payload_too_large" | "too_many_attempts" | "internal" | "busy"
  }
  /**
   * Ends this session and closes its sync sockets with 4401. Works without a session too.
   */
  logout: {
    method: "POST"
    path: "/api/logout"
    auth: "anyone"
    query: never
    body: never
    reply: { 204: void }
    errors: "cross_origin" | "internal"
  }
  /**
   * Ends every session of the account, this one too, and closes their sync sockets with 4401.
   */
  logout_everywhere: {
    method: "POST"
    path: "/api/logout-everywhere"
    auth: "session"
    query: never
    body: never
    reply: { 204: void }
    errors: "not_signed_in" | "cross_origin" | "internal"
  }
  /**
   * The projects the caller is a member of, most recently changed first.
   */
  projects_list: {
    method: "GET"
    path: "/api/projects"
    auth: "session"
    query: never
    body: never
    reply: { 200: Array<Project> }
    errors: "not_signed_in" | "internal"
  }
  /**
   * Makes a project with the caller as its owner.
   */
  project_create: {
    method: "POST"
    path: "/api/projects"
    auth: "session"
    query: never
    body: NewProject
    reply: { 201: Project }
    errors: "invalid_request" | "invalid_field" | "not_signed_in" | "cross_origin" | "payload_too_large" | "project_full" | "internal" | "storage_full"
  }
  /**
   * Deletes a project: its document, history, memberships and the asset files no other project holds. Its sync sockets close with 4004.
   */
  project_delete: {
    method: "DELETE"
    path: "/api/projects/{project}"
    auth: "owner"
    query: never
    body: never
    reply: { 204: void }
    errors: "invalid_request" | "not_signed_in" | "cross_origin" | "forbidden" | "not_found" | "internal"
  }
  /**
   * One project.
   */
  project_get: {
    method: "GET"
    path: "/api/projects/{project}"
    auth: "member"
    query: never
    body: never
    reply: { 200: Project }
    errors: "invalid_request" | "not_signed_in" | "not_found" | "internal"
  }
  /**
   * Renames a project.
   */
  project_update: {
    method: "PATCH"
    path: "/api/projects/{project}"
    auth: "owner"
    query: never
    body: ProjectChange
    reply: { 200: Project }
    errors: "invalid_request" | "invalid_field" | "not_signed_in" | "cross_origin" | "forbidden" | "not_found" | "payload_too_large" | "internal"
  }
  /**
   * Stores the body as an asset of the project and adds a file entry for it at `path`.
   */
  asset_upload: {
    method: "POST"
    path: "/api/projects/{project}/assets"
    auth: "editor"
    query: AssetQuery
    body: Blob | ArrayBuffer | Uint8Array
    reply: { 201: AssetAdded }
    errors: "invalid_request" | "invalid_field" | "not_signed_in" | "cross_origin" | "forbidden" | "not_found" | "payload_too_large" | "project_full" | "internal" | "try_again" | "storage_full"
  }
  /**
   * The bytes of an asset the project holds. Takes one byte range (`Range: bytes=a-b`) and answers 206.
   */
  asset_get: {
    method: "GET"
    path: "/api/projects/{project}/assets/{hash}"
    auth: "member"
    query: never
    body: never
    reply: { 200: Blob; 206: Blob }
    errors: "invalid_request" | "not_signed_in" | "not_found" | "range_not_satisfiable" | "internal"
  }
  /**
   * Typesets the project as the server holds it now and answers the PDF.
   */
  export: {
    method: "GET"
    path: "/api/projects/{project}/export.pdf"
    auth: "member"
    query: ExportQuery
    body: never
    reply: { 200: Blob }
    errors: "invalid_request" | "not_signed_in" | "not_found" | "compile_failed" | "export_out_of_memory" | "export_out_of_time" | "internal" | "export_busy" | "exports_off" | "stopping"
  }
  /**
   * Every file of the project as the server holds it now, sorted by path.
   */
  files: {
    method: "GET"
    path: "/api/projects/{project}/files"
    auth: "member"
    query: never
    body: never
    reply: { 200: Array<FileEntry> }
    errors: "invalid_request" | "not_signed_in" | "not_found" | "internal"
  }
  /**
   * The members of a project, sorted by name.
   */
  members_list: {
    method: "GET"
    path: "/api/projects/{project}/members"
    auth: "member"
    query: never
    body: never
    reply: { 200: Array<Member> }
    errors: "invalid_request" | "not_signed_in" | "not_found" | "internal"
  }
  /**
   * Adds the account with this email to the project. An email with no account is an invalid field.
   */
  member_add: {
    method: "POST"
    path: "/api/projects/{project}/members"
    auth: "owner"
    query: never
    body: NewMember
    reply: { 201: Member }
    errors: "invalid_request" | "invalid_field" | "not_signed_in" | "cross_origin" | "forbidden" | "not_found" | "already_member" | "payload_too_large" | "internal"
  }
  /**
   * Removes a member: an owner removes anyone, any member removes themself. Their sync sockets to the project close with 4004.
   */
  member_remove: {
    method: "DELETE"
    path: "/api/projects/{project}/members/{user}"
    auth: "member"
    query: never
    body: never
    reply: { 204: void }
    errors: "invalid_request" | "not_signed_in" | "cross_origin" | "forbidden" | "not_found" | "last_owner" | "internal"
  }
  /**
   * Changes a member's role. A member who can no longer write has their sync sockets closed with 4003; one who now may write can do so at once.
   */
  member_update: {
    method: "PATCH"
    path: "/api/projects/{project}/members/{user}"
    auth: "owner"
    query: never
    body: RoleChange
    reply: { 200: Member }
    errors: "invalid_request" | "not_signed_in" | "cross_origin" | "forbidden" | "not_found" | "last_owner" | "payload_too_large" | "internal"
  }
  /**
   * The signed-in person and whether setup is needed. Moves the session's end forward, at most once a day, and sends the cookie again when it does. The client calls this when it starts and at least once a day while it stays open.
   */
  session: {
    method: "GET"
    path: "/api/session"
    auth: "anyone"
    query: never
    body: never
    reply: { 200: Session }
    errors: "internal"
  }
  /**
   * First-run setup: makes the first account, the administrator, and signs it in.
   */
  setup: {
    method: "POST"
    path: "/api/setup"
    auth: "anyone"
    query: never
    body: SetupRequest
    reply: { 201: User }
    errors: "invalid_request" | "invalid_field" | "cross_origin" | "wrong_setup_code" | "setup_done" | "payload_too_large" | "internal" | "busy"
  }
  /**
   * The catalogue of every font the server holds: the embedded ones and those in the data directory's `fonts/`. Revalidated on each visit.
   */
  fonts: {
    method: "GET"
    path: "/fonts/fonts.json"
    auth: "anyone"
    query: never
    body: never
    reply: { 200: Array<FontCatalogueEntry> }
    errors: "internal"
  }
  /**
   * One font file of the catalogue, named by its hash; cached for a year.
   */
  font_file: {
    method: "GET"
    path: "/fonts/{file}"
    auth: "anyone"
    query: never
    body: never
    reply: { 200: Blob }
    errors: "invalid_request" | "not_found" | "internal"
  }
  /**
   * A Typst package as `.tar.gz`; `archive` is `{version}.tar.gz`. Served from the server's cache, which downloads a missing package from the registry when the server allows that.
   */
  package: {
    method: "GET"
    path: "/packages/{namespace}/{name}/{archive}"
    auth: "session"
    query: never
    body: never
    reply: { 200: Blob }
    errors: "invalid_request" | "not_signed_in" | "not_found" | "internal" | "package_unavailable"
  }
  /**
   * The sync socket (`x-nisaba-sync`). A handshake whose Origin is not the public address gets 403. Every other refusal is a close code after the upgrade.
   */
  sync: {
    method: "GET"
    path: "/sync/{project}"
    auth: "anyone"
    query: never
    body: never
    reply: { 101: void }
    errors: "invalid_request" | "cross_origin" | "internal"
  }
}

/** Method and path of every route. */
export const ROUTES = {
  web_client: { method: "GET", path: "/" },
  account_update: { method: "PATCH", path: "/api/account" },
  password_change: { method: "POST", path: "/api/account/password" },
  users_list: { method: "GET", path: "/api/admin/users" },
  user_create: { method: "POST", path: "/api/admin/users" },
  user_delete: { method: "DELETE", path: "/api/admin/users/{user}" },
  health: { method: "GET", path: "/api/health" },
  login: { method: "POST", path: "/api/login" },
  logout: { method: "POST", path: "/api/logout" },
  logout_everywhere: { method: "POST", path: "/api/logout-everywhere" },
  projects_list: { method: "GET", path: "/api/projects" },
  project_create: { method: "POST", path: "/api/projects" },
  project_delete: { method: "DELETE", path: "/api/projects/{project}" },
  project_get: { method: "GET", path: "/api/projects/{project}" },
  project_update: { method: "PATCH", path: "/api/projects/{project}" },
  asset_upload: { method: "POST", path: "/api/projects/{project}/assets" },
  asset_get: { method: "GET", path: "/api/projects/{project}/assets/{hash}" },
  export: { method: "GET", path: "/api/projects/{project}/export.pdf" },
  files: { method: "GET", path: "/api/projects/{project}/files" },
  members_list: { method: "GET", path: "/api/projects/{project}/members" },
  member_add: { method: "POST", path: "/api/projects/{project}/members" },
  member_remove: { method: "DELETE", path: "/api/projects/{project}/members/{user}" },
  member_update: { method: "PATCH", path: "/api/projects/{project}/members/{user}" },
  session: { method: "GET", path: "/api/session" },
  setup: { method: "POST", path: "/api/setup" },
  fonts: { method: "GET", path: "/fonts/fonts.json" },
  font_file: { method: "GET", path: "/fonts/{file}" },
  package: { method: "GET", path: "/packages/{namespace}/{name}/{archive}" },
  sync: { method: "GET", path: "/sync/{project}" },
} as const satisfies { [K in keyof Routes]: { method: Routes[K]["method"]; path: Routes[K]["path"] } }
