/** The HTTP API. The types come from the server; see api.gen.ts. */
import type { Created, Credentials, NewProject, ProjectRow, Session, SetupRequest, User } from "./api.gen"

async function call<T>(path: string, init?: RequestInit): Promise<T> {
  const res = await fetch(path, { headers: { "content-type": "application/json" }, ...init })
  if (!res.ok) throw new Error((await res.text()) || res.statusText)
  return (res.headers.get("content-type") ?? "").includes("json") ? (res.json() as Promise<T>) : (undefined as T)
}

const post = <T>(path: string, body: unknown): Promise<T> => call<T>(path, { method: "POST", body: JSON.stringify(body) })

export const api = {
  session: () => call<Session>("/api/session"),
  setup: (body: SetupRequest) => post<User>("/api/setup", body),
  login: (body: Credentials) => post<User>("/api/login", body),
  projects: () => call<ProjectRow[]>("/api/projects"),
  createProject: (body: NewProject) => post<Created>("/api/projects", body),
}

/** Today's local calendar date as YYYY-MM-DD; exports use it (DESIGN.md section 8). */
export function localDate(now = new Date()): string {
  const pad = (n: number): string => String(n).padStart(2, "0")
  return `${now.getFullYear()}-${pad(now.getMonth() + 1)}-${pad(now.getDate())}`
}

/** Where a project's PDF is exported, dated today on this device. */
export const exportUrl = (project: string, now = new Date()): string =>
  `/api/projects/${project}/export.pdf?today=${localDate(now)}`
