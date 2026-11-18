// Generated from crates/nisaba-server/src/api.rs. Do not edit.
// Regenerate: NISABA_WRITE_API=1 cargo test -p nisaba-server api

export type Session = { setup_needed: boolean, user: User | null, };

export type User = { id: string, email: string, name: string, };

export type Credentials = { email: string, password: string, name: string, };

export type ProjectRow = { id: string, name: string, role: string, };

export type NewProject = { name: string, files: Array<ImportFile>, };

export type ImportFile = { path: string, text?: string, base64?: string, };

export type Created = { id: string, };

export type FileEntry = { id: string, path: string, text?: string, hash?: string, };

export type AssetAdded = { hash: string, };
