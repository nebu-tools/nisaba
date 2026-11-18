/** The HTTP API. The types come from the server; see api.gen.ts. */
import type { ApiError, Credentials, ErrorCode, NewProject, Project, Session, SetupRequest, User } from "./api.gen"

/** A refusal from the server: its stable code and a sentence to show. */
export class ApiFailure extends Error {
  constructor(
    readonly code: ErrorCode,
    message: string,
    readonly field?: string | null,
  ) {
    super(message)
  }
}

async function call<T>(path: string, init?: RequestInit): Promise<T> {
  const res = await fetch(path, { headers: { "content-type": "application/json" }, ...init })
  if (!res.ok) {
    const body = (await res.json().catch(() => null)) as ApiError | null
    throw body ? new ApiFailure(body.code, body.message, body.field) : new ApiFailure("internal", res.statusText)
  }
  return (res.headers.get("content-type") ?? "").includes("json") ? (res.json() as Promise<T>) : (undefined as T)
}

const post = <T>(path: string, body: unknown): Promise<T> => call<T>(path, { method: "POST", body: JSON.stringify(body) })

export const api = {
  session: () => call<Session>("/api/session"),
  setup: (body: SetupRequest) => post<User>("/api/setup", body),
  login: (body: Credentials) => post<User>("/api/login", body),
  projects: () => call<Project[]>("/api/projects"),
  createProject: (body: NewProject) => post<Project>("/api/projects", body),
}

/** Today's local calendar date as YYYY-MM-DD; exports use it (DESIGN.md section 8). */
export function localDate(now = new Date()): string {
  const pad = (n: number): string => String(n).padStart(2, "0")
  return `${now.getFullYear()}-${pad(now.getMonth() + 1)}-${pad(now.getDate())}`
}

/** Where a project's PDF is exported, dated today on this device. */
export const exportUrl = (project: string, now = new Date()): string =>
  `/api/projects/${project}/export.pdf?today=${localDate(now)}`
