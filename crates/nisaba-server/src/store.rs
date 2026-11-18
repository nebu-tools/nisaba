//! Everything Nisaba keeps on disk.
//!
//! A data directory holds `nisaba.db` (an `SQLite` database in WAL mode) and `assets/`
//! (content-addressed files). The database holds accounts, sessions, projects,
//! memberships, and each project's Loro update log and latest snapshot.
//!
//! The update log is written first: an update is committed with
//! `synchronous = FULL` before the server acknowledges it, so an acknowledged
//! edit survives `kill -9` and power loss. A snapshot is written in the same
//! transaction that deletes the log rows it covers, so the log and snapshot
//! never disagree about what is durable.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use nisaba_sync::{DocId, OpLogStore, Snapshot, SnapshotStore, SyncError, SyncResult};
use rusqlite::{Connection, OptionalExtension, params};
use sha2::{Digest, Sha256};

const SCHEMA: &str = r"
CREATE TABLE IF NOT EXISTS users (
  id TEXT PRIMARY KEY,
  email TEXT NOT NULL UNIQUE,
  name TEXT NOT NULL,
  password_hash TEXT NOT NULL,
  created_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS sessions (
  token_hash BLOB PRIMARY KEY,
  user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  expires_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS projects (
  id TEXT PRIMARY KEY,
  name TEXT NOT NULL,
  created_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS members (
  project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
  user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  role TEXT NOT NULL CHECK (role IN ('owner','editor','commenter','viewer')),
  PRIMARY KEY (project_id, user_id)
);
CREATE TABLE IF NOT EXISTS doc_updates (
  project_id TEXT NOT NULL,
  seq INTEGER NOT NULL,
  bytes BLOB NOT NULL,
  PRIMARY KEY (project_id, seq)
);
CREATE TABLE IF NOT EXISTS doc_snapshots (
  project_id TEXT PRIMARY KEY,
  vv BLOB NOT NULL,
  bytes BLOB NOT NULL
);
CREATE TABLE IF NOT EXISTS assets (
  hash TEXT PRIMARY KEY,
  size INTEGER NOT NULL
);
";

/// The data directory: one `SQLite` database and a folder of assets.
#[derive(Clone)]
pub struct Store {
    conn: Arc<Mutex<Connection>>,
    assets: PathBuf,
}

/// A row of the `users` table, without the password hash.
#[derive(Clone, Debug, serde::Serialize)]
pub struct User {
    pub id: String,
    pub email: String,
    pub name: String,
}

/// A project as the project list shows it.
#[derive(Clone, Debug, serde::Serialize)]
pub struct ProjectRow {
    pub id: String,
    pub name: String,
    pub role: String,
}

fn storage(error: impl std::fmt::Display) -> SyncError {
    SyncError::Storage(error.to_string())
}

#[must_use]
pub fn now_secs() -> i64 {
    i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs()),
    )
    .unwrap_or(i64::MAX)
}

impl Store {
    /// Opens (creating if needed) the data directory at `dir`.
    pub fn open(dir: &Path) -> Result<Self, String> {
        std::fs::create_dir_all(dir.join("assets")).map_err(|e| e.to_string())?;
        let conn = Connection::open(dir.join("nisaba.db")).map_err(|e| e.to_string())?;
        conn.execute_batch(
            "PRAGMA journal_mode = WAL; PRAGMA synchronous = FULL; PRAGMA foreign_keys = ON;",
        )
        .map_err(|e| e.to_string())?;
        conn.execute_batch(SCHEMA).map_err(|e| e.to_string())?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            assets: dir.join("assets"),
        })
    }

    fn with<T>(&self, f: impl FnOnce(&mut Connection) -> rusqlite::Result<T>) -> Result<T, String> {
        let mut conn = self.conn.lock().map_err(|_| "database lock poisoned")?;
        f(&mut conn).map_err(|e| e.to_string())
    }

    pub fn user_count(&self) -> Result<i64, String> {
        self.with(|c| c.query_row("SELECT count(*) FROM users", [], |r| r.get(0)))
    }

    pub fn create_user(&self, id: &str, email: &str, name: &str, hash: &str) -> Result<(), String> {
        self.with(|c| {
            c.execute(
                "INSERT INTO users (id, email, name, password_hash, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![id, email, name, hash, now_secs()],
            )
            .map(drop)
        })
    }

    pub fn user_by_email(&self, email: &str) -> Result<Option<(User, String)>, String> {
        self.with(|c| {
            c.query_row(
                "SELECT id, email, name, password_hash FROM users WHERE email = ?1",
                [email],
                |r| {
                    Ok((
                        User {
                            id: r.get(0)?,
                            email: r.get(1)?,
                            name: r.get(2)?,
                        },
                        r.get(3)?,
                    ))
                },
            )
            .optional()
        })
    }

    pub fn create_session(&self, token: &str, user_id: &str, ttl_secs: i64) -> Result<(), String> {
        let hash = Sha256::digest(token.as_bytes()).to_vec();
        self.with(|c| {
            c.execute(
                "INSERT INTO sessions (token_hash, user_id, expires_at) VALUES (?1, ?2, ?3)",
                params![hash, user_id, now_secs() + ttl_secs],
            )
            .map(drop)
        })
    }

    pub fn delete_session(&self, token: &str) -> Result<(), String> {
        let hash = Sha256::digest(token.as_bytes()).to_vec();
        self.with(|c| {
            c.execute("DELETE FROM sessions WHERE token_hash = ?1", [hash])
                .map(drop)
        })
    }

    pub fn session_user(&self, token: &str) -> Result<Option<User>, String> {
        let hash = Sha256::digest(token.as_bytes()).to_vec();
        self.with(|c| {
            c.query_row(
                "SELECT u.id, u.email, u.name FROM sessions s JOIN users u ON u.id = s.user_id
                 WHERE s.token_hash = ?1 AND s.expires_at > ?2",
                params![hash, now_secs()],
                |r| {
                    Ok(User {
                        id: r.get(0)?,
                        email: r.get(1)?,
                        name: r.get(2)?,
                    })
                },
            )
            .optional()
        })
    }

    pub fn create_project(&self, id: &str, name: &str, owner: &str) -> Result<(), String> {
        self.with(|c| {
            let tx = c.transaction()?;
            tx.execute(
                "INSERT INTO projects (id, name, created_at) VALUES (?1, ?2, ?3)",
                params![id, name, now_secs()],
            )?;
            tx.execute(
                "INSERT INTO members (project_id, user_id, role) VALUES (?1, ?2, 'owner')",
                params![id, owner],
            )?;
            tx.commit()
        })
    }

    pub fn projects_for(&self, user_id: &str) -> Result<Vec<ProjectRow>, String> {
        self.with(|c| {
            let mut stmt = c.prepare(
                "SELECT p.id, p.name, m.role FROM projects p JOIN members m ON m.project_id = p.id
                 WHERE m.user_id = ?1 ORDER BY p.created_at DESC",
            )?;
            stmt.query_map([user_id], |r| {
                Ok(ProjectRow {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    role: r.get(2)?,
                })
            })?
            .collect()
        })
    }

    pub fn role(&self, project_id: &str, user_id: &str) -> Result<Option<String>, String> {
        self.with(|c| {
            c.query_row(
                "SELECT role FROM members WHERE project_id = ?1 AND user_id = ?2",
                params![project_id, user_id],
                |r| r.get(0),
            )
            .optional()
        })
    }

    /// Stores an asset under its SHA-256 and returns the hash. The file is
    /// written and synced before its row is committed.
    pub fn put_asset(&self, bytes: &[u8]) -> Result<String, String> {
        let hash = hex(&Sha256::digest(bytes));
        let path = self.asset_path(&hash);
        if !path.exists() {
            std::fs::create_dir_all(path.parent().expect("asset path has a parent"))
                .map_err(|e| e.to_string())?;
            let tmp = path.with_extension("tmp");
            {
                use std::io::Write as _;
                let mut file = std::fs::File::create(&tmp).map_err(|e| e.to_string())?;
                file.write_all(bytes).map_err(|e| e.to_string())?;
                file.sync_all().map_err(|e| e.to_string())?;
            }
            std::fs::rename(&tmp, &path).map_err(|e| e.to_string())?;
        }
        let size = i64::try_from(bytes.len()).unwrap_or(i64::MAX);
        self.with(|c| {
            c.execute(
                "INSERT OR IGNORE INTO assets (hash, size) VALUES (?1, ?2)",
                params![hash, size],
            )
            .map(drop)
        })?;
        Ok(hash)
    }

    #[must_use]
    pub fn get_asset(&self, hash: &str) -> Option<Vec<u8>> {
        if hash.len() != 64 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        std::fs::read(self.asset_path(hash)).ok()
    }

    fn asset_path(&self, hash: &str) -> PathBuf {
        self.assets.join(&hash[..2]).join(&hash[2..])
    }

    /// Number of log rows for a project (for tests and diagnostics).
    pub fn log_len(&self, project_id: &str) -> Result<i64, String> {
        self.with(|c| {
            c.query_row(
                "SELECT count(*) FROM doc_updates WHERE project_id = ?1",
                [project_id],
                |r| r.get(0),
            )
        })
    }
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

#[async_trait::async_trait]
impl OpLogStore for Store {
    async fn append(&self, doc: &DocId, update: &[u8]) -> SyncResult<()> {
        self.with(|c| {
            c.execute(
                "INSERT INTO doc_updates (project_id, seq, bytes) VALUES (?1,
                   (SELECT coalesce(max(seq), 0) + 1 FROM doc_updates WHERE project_id = ?1), ?2)",
                params![doc.as_str(), update],
            )
            .map(drop)
        })
        .map_err(storage)
    }

    async fn read_all(&self, doc: &DocId) -> SyncResult<Vec<Vec<u8>>> {
        self.with(|c| {
            let mut stmt =
                c.prepare("SELECT bytes FROM doc_updates WHERE project_id = ?1 ORDER BY seq")?;
            stmt.query_map([doc.as_str()], |r| r.get(0))?.collect()
        })
        .map_err(storage)
    }

    async fn len(&self, doc: &DocId) -> SyncResult<u64> {
        self.log_len(doc.as_str())
            .map(|n| u64::try_from(n).unwrap_or(0))
            .map_err(storage)
    }
}

#[async_trait::async_trait]
impl SnapshotStore for Store {
    /// Replaces the snapshot and, in the same transaction, drops every log row
    /// the snapshot already contains.
    async fn put(&self, doc: &DocId, snapshot: Snapshot) -> SyncResult<()> {
        self.with(|c| {
            let tx = c.transaction()?;
            tx.execute(
                "INSERT INTO doc_snapshots (project_id, vv, bytes) VALUES (?1, ?2, ?3)
                 ON CONFLICT (project_id) DO UPDATE SET vv = excluded.vv, bytes = excluded.bytes",
                params![doc.as_str(), snapshot.vv.encode(), snapshot.bytes],
            )?;
            let covered: Vec<i64> = {
                let mut stmt =
                    tx.prepare("SELECT seq, bytes FROM doc_updates WHERE project_id = ?1")?;
                let rows = stmt.query_map([doc.as_str()], |r| {
                    Ok((r.get::<_, i64>(0)?, r.get::<_, Vec<u8>>(1)?))
                })?;
                let mut covered = Vec::new();
                for row in rows {
                    let (seq, bytes) = row?;
                    if let Ok(meta) = loro::LoroDoc::decode_import_blob_meta(&bytes, false)
                        && snapshot.vv.includes_vv(&meta.partial_end_vv)
                    {
                        covered.push(seq);
                    }
                }
                covered
            };
            for seq in covered {
                tx.execute(
                    "DELETE FROM doc_updates WHERE project_id = ?1 AND seq = ?2",
                    params![doc.as_str(), seq],
                )?;
            }
            tx.commit()
        })
        .map_err(storage)
    }

    async fn latest(&self, doc: &DocId) -> SyncResult<Option<Snapshot>> {
        self.with(|c| {
            c.query_row(
                "SELECT vv, bytes FROM doc_snapshots WHERE project_id = ?1",
                [doc.as_str()],
                |r| Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, Vec<u8>>(1)?)),
            )
            .optional()
        })
        .map_err(storage)?
        .map(|(vv, bytes)| {
            Ok(Snapshot {
                vv: loro::VersionVector::decode(&vv).map_err(storage)?,
                bytes,
            })
        })
        .transpose()
    }

    async fn list(&self, doc: &DocId) -> SyncResult<Vec<Snapshot>> {
        Ok(self.latest(doc).await?.into_iter().collect())
    }

    async fn drop_all(&self, doc: &DocId) -> SyncResult<()> {
        self.with(|c| {
            c.execute(
                "DELETE FROM doc_snapshots WHERE project_id = ?1",
                [doc.as_str()],
            )
            .map(drop)
        })
        .map_err(storage)
    }
}
