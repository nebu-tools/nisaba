// Generated from crates/nisaba-server/src/api.rs. Do not edit.
// Regenerate: NISABA_WRITE_API=1 cargo test -p nisaba-server api

export type Session = { 
/**
 * No account exists yet.
 */
setup_needed: boolean, 
/**
 * Setup asks for the code the server printed to its log.
 */
setup_code_needed: boolean, user: User | null, };

export type User = { id: string, email: string, name: string, };

export type SetupRequest = { email: string, password: string, name: string, 
/**
 * The code from the server log; needed unless the server is reachable
 * only from this machine.
 */
setup_code?: string, };

export type Credentials = { email: string, password: string, };

export type ProjectRow = { id: string, name: string, role: string, };

export type NewProject = { name: string, files: Array<ImportFile>, };

export type ImportFile = { path: string, text?: string, base64?: string, };

export type Created = { id: string, };

export type FileEntry = { id: string, path: string, text?: string, hash?: string, };

export type AssetAdded = { hash: string, };
