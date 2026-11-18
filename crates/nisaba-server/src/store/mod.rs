//! Everything Nisaba keeps on disk.
//!
//! A data directory holds `nisaba.db` (an `SQLite` database in WAL mode),
//! `assets/` (content-addressed files), `fonts/` (fonts the administrator
//! adds) and `packages/` (cached Typst packages). The database holds
//! accounts, sessions, projects, memberships, which assets each project
//! holds, the first-run setup code, and each project's Loro update log and
//! latest snapshot.
//!
//! The update log is written first: an update is committed with
//! `synchronous = FULL` before the server acknowledges it, so an acknowledged
//! edit survives `kill -9` and power loss. A snapshot is written in the same
//! transaction that deletes the log rows it covers, so the log and snapshot
//! never disagree about what is durable.
//!
//! Every method that takes access away (signing out, removing a member,
//! changing a role, deleting an account or a project) ends the matching sync
//! grants ([`crate::access`]) while it holds the write connection, the same
//! lock under which the room's append asks whether a grant still holds.
//!
//! One server process owns a data directory: [`Store::open`] takes an
//! exclusive lock on `nisaba.lock` and fails while another process holds it.

mod accounts;
mod assets;
mod log;
mod projects;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use rusqlite::Connection;
use sha2::{Digest, Sha256};

use crate::access::Grants;

pub use accounts::{SessionInfo, Setup, new_id};
pub use assets::is_hash;
pub use projects::Refusal;

const SCHEMA: &str = r"
CREATE TABLE IF NOT EXISTS users (
  id TEXT PRIMARY KEY,
  email TEXT NOT NULL UNIQUE,
  name TEXT NOT NULL,
  password_hash TEXT,
  is_admin INTEGER NOT NULL DEFAULT 0,
  created_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS sessions (
  token_hash BLOB PRIMARY KEY,
  user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  created_at INTEGER NOT NULL,
  expires_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS sessions_user ON sessions(user_id);
CREATE TABLE IF NOT EXISTS projects (
  id TEXT PRIMARY KEY,
  name TEXT NOT NULL,
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS members (
  project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
  user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  role TEXT NOT NULL CHECK (role IN ('owner','editor','commenter','viewer')),
  PRIMARY KEY (project_id, user_id)
);
CREATE INDEX IF NOT EXISTS members_user ON members(user_id);
CREATE TABLE IF NOT EXISTS doc_updates (
  project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
  seq INTEGER NOT NULL,
  bytes BLOB NOT NULL,
  PRIMARY KEY (project_id, seq)
);
CREATE TABLE IF NOT EXISTS doc_snapshots (
  project_id TEXT PRIMARY KEY REFERENCES projects(id) ON DELETE CASCADE,
  bytes BLOB NOT NULL
);
CREATE TABLE IF NOT EXISTS assets (
  hash TEXT PRIMARY KEY,
  size INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS project_assets (
  project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
  hash TEXT NOT NULL REFERENCES assets(hash),
  PRIMARY KEY (project_id, hash)
);
CREATE INDEX IF NOT EXISTS project_assets_hash ON project_assets(hash);
CREATE TABLE IF NOT EXISTS settings (
  key TEXT PRIMARY KEY,
  value BLOB NOT NULL
);
";

/// Why the store could not do something.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreError {
    /// The disk is full, or a file size limit was reached. Nothing of the
    /// write is kept.
    Full(String),
    /// Anything else.
    Other(String),
}

/// How a storage failure begins when it travels as text, as through the
/// rooms. [`StoreError::is_full`] reads it back.
const FULL: &str = "the server's disk is full: ";

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Full(why) => write!(f, "{FULL}{why}"),
            Self::Other(why) => f.write_str(why),
        }
    }
}

impl std::error::Error for StoreError {}

impl StoreError {
    /// Whether a storage message, as a room passes it on, says the disk is
    /// full.
    #[must_use]
    pub fn is_full(message: &str) -> bool {
        message.starts_with(FULL)
    }
}

impl From<rusqlite::Error> for StoreError {
    fn from(e: rusqlite::Error) -> Self {
        // A short write is SQLITE_FULL; a write refused outright, as past a
        // file size limit, is an I/O error on write.
        const IOERR_WRITE: i32 = 778;
        match &e {
            rusqlite::Error::SqliteFailure(f, _)
                if f.code == rusqlite::ErrorCode::DiskFull || f.extended_code == IOERR_WRITE =>
            {
                Self::Full(e.to_string())
            }
            _ => Self::Other(e.to_string()),
        }
    }
}

impl From<std::io::Error> for StoreError {
    fn from(e: std::io::Error) -> Self {
        use std::io::ErrorKind;
        match e.kind() {
            ErrorKind::StorageFull | ErrorKind::FileTooLarge | ErrorKind::QuotaExceeded => {
                Self::Full(e.to_string())
            }
            _ => Self::Other(e.to_string()),
        }
    }
}

pub type StoreResult<T> = Result<T, StoreError>;

/// The data directory: one `SQLite` database and folders of files.
#[derive(Clone)]
pub struct Store {
    inner: Arc<Inner>,
}

struct Inner {
    /// Every write, and every read that must see the writes before it in
    /// order with access changes.
    writer: Mutex<Connection>,
    /// Reads that may run beside a write: rooms that open, lists, sessions.
    reader: Mutex<Connection>,
    dir: PathBuf,
    /// The open sync grants. Changed only under the `writer` lock.
    grants: Arc<Grants>,
    _lock: Option<std::fs::File>,
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

/// Hex of `bytes`, lower case.
#[must_use]
pub fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

/// What a session token is stored as.
fn token_hash(token: &str) -> Vec<u8> {
    Sha256::digest(token.as_bytes()).to_vec()
}

impl Store {
    /// Opens (creating if needed) the data directory at `dir` and locks it
    /// for this process.
    pub fn open(dir: &Path) -> StoreResult<Self> {
        Self::open_with(dir, true)
    }

    /// Opens the data directory without taking its lock, for a tool that
    /// works beside a running server (`nisaba setup`) and for tests.
    pub fn open_shared(dir: &Path) -> StoreResult<Self> {
        Self::open_with(dir, false)
    }

    fn open_with(dir: &Path, exclusive: bool) -> StoreResult<Self> {
        for sub in ["assets", "fonts", "packages"] {
            std::fs::create_dir_all(dir.join(sub))?;
        }
        let lock = if exclusive { Some(lock(dir)?) } else { None };
        let path = dir.join("nisaba.db");
        let writer = Connection::open(&path)?;
        // Incremental vacuum must be chosen before the first table exists;
        // on an existing database this does nothing.
        writer.execute_batch(
            "PRAGMA auto_vacuum = INCREMENTAL; PRAGMA journal_mode = WAL; \
             PRAGMA synchronous = FULL; PRAGMA foreign_keys = ON; PRAGMA busy_timeout = 5000;",
        )?;
        writer.execute_batch(SCHEMA)?;
        let reader = Connection::open_with_flags(
            &path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        reader.execute_batch("PRAGMA foreign_keys = ON; PRAGMA busy_timeout = 5000;")?;
        Ok(Self {
            inner: Arc::new(Inner {
                writer: Mutex::new(writer),
                reader: Mutex::new(reader),
                dir: dir.to_owned(),
                grants: Arc::default(),
                _lock: lock,
            }),
        })
    }

    /// The data directory.
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.inner.dir
    }

    /// Runs `f` on the write connection.
    fn write<T>(&self, f: impl FnOnce(&mut Connection) -> StoreResult<T>) -> StoreResult<T> {
        f(&mut self.writer())
    }

    fn writer(&self) -> MutexGuard<'_, Connection> {
        self.inner
            .writer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Runs `f` on the read connection.
    fn read<T>(&self, f: impl FnOnce(&Connection) -> StoreResult<T>) -> StoreResult<T> {
        let conn = self
            .inner
            .reader
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        f(&conn)
    }

    /// The open sync grants.
    #[must_use]
    pub fn grants(&self) -> &Grants {
        &self.inner.grants
    }

    /// Whether the database answers, for the health check.
    pub fn healthy(&self) -> StoreResult<()> {
        self.read(|c| {
            c.query_row("SELECT count(*) FROM settings", [], |_| Ok(()))
                .map_err(Into::into)
        })
    }

    /// Writes the write-ahead log into the database file and empties it, for
    /// a clean stop. Nothing is lost without it; the next start replays the
    /// log.
    pub fn checkpoint(&self) -> StoreResult<()> {
        self.write(|c| {
            c.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))
                .map_err(Into::into)
        })
    }

    /// The path of the database file.
    #[must_use]
    pub fn database_path(&self) -> PathBuf {
        self.inner.dir.join("nisaba.db")
    }

    /// Copies the database, as one consistent read, to `dest`, with
    /// `SQLite`'s online backup. Writers go on meanwhile.
    pub fn backup_database(&self, dest: &Path) -> StoreResult<()> {
        let conn = Connection::open_with_flags(
            self.database_path(),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )?;
        let mut out = Connection::open(dest)?;
        {
            let backup = rusqlite::backup::Backup::new(&conn, &mut out)?;
            // All pages in one step: one read transaction, so one moment.
            backup.step(-1)?;
        }
        out.execute_batch("PRAGMA journal_mode = DELETE;")?;
        Ok(())
    }

    /// Which project ids exist, for backup checks and tools.
    pub fn project_ids(&self) -> StoreResult<Vec<String>> {
        self.read(|c| {
            let mut stmt = c.prepare("SELECT id FROM projects ORDER BY id")?;
            let ids = stmt
                .query_map([], |r| r.get(0))?
                .collect::<Result<_, _>>()?;
            Ok(ids)
        })
    }
}

/// Takes the data directory's lock, or says which process holds it.
#[cfg(target_os = "linux")]
fn lock(dir: &Path) -> StoreResult<std::fs::File> {
    use rustix::fs::{FlockOperation, flock};
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join("nisaba.lock"))?;
    flock(&file, FlockOperation::NonBlockingLockExclusive).map_err(|_| {
        StoreError::Other(format!(
            "another nisaba process uses the data directory {}",
            dir.display()
        ))
    })?;
    Ok(file)
}

#[cfg(not(target_os = "linux"))]
fn lock(dir: &Path) -> StoreResult<std::fs::File> {
    Ok(std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join("nisaba.lock"))?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_data_directory_has_one_server() {
        let dir = tempfile::tempdir().unwrap();
        let first = Store::open(dir.path()).unwrap();
        let second = Store::open(dir.path()).err().expect("refused");
        assert!(
            second.to_string().contains("another nisaba process"),
            "{second}"
        );
        assert!(Store::open_shared(dir.path()).is_ok());
        drop(first);
        assert!(Store::open(dir.path()).is_ok());
    }

    #[test]
    fn a_full_disk_is_told_apart() {
        let e = StoreError::from(std::io::Error::from(std::io::ErrorKind::StorageFull));
        assert!(matches!(e, StoreError::Full(_)));
        assert!(StoreError::is_full(&e.to_string()));
        let e = StoreError::from(std::io::Error::from(std::io::ErrorKind::NotFound));
        assert!(!StoreError::is_full(&e.to_string()));
    }
}
