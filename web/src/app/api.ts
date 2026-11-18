/** The HTTP API. The types come from the server; see api.gen.ts. */
import type { Created, Credentials, NewProject, ProjectRow, Session, User } from "./api.gen"

async function call<T>(path: string, init?: RequestInit): Promise<T> {
  const res = await fetch(path, { headers: { "content-type": "application/json" }, ...init })
  if (!res.ok) throw new Error((await res.text()) || res.statusText)
  return (res.headers.get("content-type") ?? "").includes("json") ? (res.json() as Promise<T>) : (undefined as T)
}

const post = <T>(path: string, body: unknown): Promise<T> => call<T>(path, { method: "POST", body: JSON.stringify(body) })

export const api = {
  session: () => call<Session>("/api/session"),
  setup: (body: Credentials) => post<User>("/api/setup", body),
  login: (body: Credentials) => post<User>("/api/login", body),
  projects: () => call<ProjectRow[]>("/api/projects"),
  createProject: (body: NewProject) => post<Created>("/api/projects", body),
}
