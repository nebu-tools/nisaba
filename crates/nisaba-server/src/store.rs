//! Everything Nisaba keeps on disk.
//!
//! A data directory holds `nisaba.db` (an `SQLite` database in WAL mode) and `assets/`
//! (content-addressed files). The database holds accounts, sessions, projects,
//! memberships, which assets each project holds, the first-run setup code,
//! and each project's Loro update log and latest snapshot.
//!
//! The update log is written first: an update is committed with
//! `synchronous = FULL` before the server acknowledges it, so an acknowledged
//! edit survives `kill -9` and power loss. A snapshot is written in the same
//! transaction that deletes the log rows it covers, so the log and snapshot
//! never disagree about what is durable.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use loro::VersionVector;
use nisaba_sync::{RoomStore, Stored};
use rusqlite::{Connection, OptionalExtension, params};
use sha2::{Digest, Sha256};

use crate::access::{Grant, Grants};
use crate::api::{ProjectRow, User};

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
CREATE TABLE IF NOT EXISTS settings (
  key TEXT PRIMARY KEY,
  value BLOB NOT NULL
);
";

/// The `settings` key of the SHA-256 of the first-run setup code.
const SETUP_CODE: &str = "setup_code_sha256";

/// What [`Store::create_owner`] did.
#[derive(Debug, PartialEq, Eq)]
pub enum Setup {
    Created,
    /// An account exists already.
    Taken,
    /// A setup code is required and this one is missing or wrong.
    WrongCode,
}

/// The data directory: one `SQLite` database and a folder of assets.
#[derive(Clone)]
pub struct Store {
    conn: Arc<Mutex<Connection>>,
    assets: PathBuf,
    /// The open sync sockets. Changed only under the `conn` lock.
    grants: Arc<Grants>,
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
            grants: Arc::default(),
        })
    }

    fn with<T>(&self, f: impl FnOnce(&mut Connection) -> rusqlite::Result<T>) -> Result<T, String> {
        let mut conn = self.conn.lock().map_err(|_| "database lock poisoned")?;
        f(&mut conn).map_err(|e| e.to_string())
    }

    pub fn user_count(&self) -> Result<i64, String> {
        self.with(|c| c.query_row("SELECT count(*) FROM users", [], |r| r.get(0)))
    }

    /// Adds an account. Only for setting up a data directory in tests and
    /// tools; the server itself creates its one account with
    /// [`Store::create_owner`].
    pub fn create_user(&self, id: &str, email: &str, name: &str, hash: &str) -> Result<(), String> {
        self.with(|c| insert_user(c, id, email, name, hash))
    }

    /// Creates the first account, in one write transaction that also checks
    /// that no account exists and, when a setup code is stored, that `code`
    /// matches it. The code is compared in constant time and deleted with the
    /// same commit, so it works once.
    pub fn create_owner(
        &self,
        user: &User,
        password_hash: &str,
        code: Option<&str>,
    ) -> Result<Setup, String> {
        self.with(|c| {
            let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            if let Some(refused) = setup_refused(&tx, code)? {
                return Ok(refused);
            }
            insert_user(&tx, &user.id, &user.email, &user.name, password_hash)?;
            tx.execute("DELETE FROM settings WHERE key = ?1", [SETUP_CODE])?;
            tx.commit()?;
            Ok(Setup::Created)
        })
    }

    /// Says, before any password work, why setup with `code` would fail:
    /// [`Setup::Taken`] or [`Setup::WrongCode`]. `None` means it may work.
    /// [`Store::create_owner`] checks again in its transaction.
    pub fn setup_refused(&self, code: Option<&str>) -> Result<Option<Setup>, String> {
        self.with(|c| setup_refused(c, code))
    }

    /// Prepares first-run setup at start. While no account exists and
    /// `needed` is true, makes a new setup code, keeps only its SHA-256 and
    /// returns the code, replacing any code of an earlier start. Otherwise
    /// drops any stored code and returns `None`.
    pub fn prepare_setup(&self, needed: bool) -> Result<Option<String>, String> {
        let raw: [u8; 16] = rand::random();
        let code = hex(&raw)
            .as_bytes()
            .chunks(4)
            .map(|c| std::str::from_utf8(c).expect("hex is ASCII"))
            .collect::<Vec<_>>()
            .join("-");
        let hash = Sha256::digest(normalize_code(&code).as_bytes()).to_vec();
        self.with(|c| {
            let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            let users: i64 = tx.query_row("SELECT count(*) FROM users", [], |r| r.get(0))?;
            tx.execute("DELETE FROM settings WHERE key = ?1", [SETUP_CODE])?;
            let code = if users == 0 && needed {
                tx.execute(
                    "INSERT INTO settings (key, value) VALUES (?1, ?2)",
                    params![SETUP_CODE, hash],
                )?;
                Some(code)
            } else {
                None
            };
            tx.commit()?;
            Ok(code)
        })
    }

    /// Whether first-run setup asks for a code.
    pub fn setup_code_needed(&self) -> Result<bool, String> {
        self.with(|c| {
            c.query_row(
                "SELECT 1 FROM settings WHERE key = ?1",
                [SETUP_CODE],
                |_| Ok(()),
            )
            .optional()
            .map(|row| row.is_some())
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

    /// Adds a session and drops every expired one.
    pub fn create_session(&self, token: &str, user_id: &str, ttl_secs: i64) -> Result<(), String> {
        let hash = Sha256::digest(token.as_bytes()).to_vec();
        self.with(|c| {
            let now = now_secs();
            c.execute("DELETE FROM sessions WHERE expires_at <= ?1", [now])?;
            c.execute(
                "INSERT INTO sessions (token_hash, user_id, expires_at) VALUES (?1, ?2, ?3)",
                params![hash, user_id, now + ttl_secs],
            )
            .map(drop)
        })
    }

    /// Signs a session out. Its sync sockets close with 4401.
    pub fn delete_session(&self, token: &str) -> Result<(), String> {
        let hash = Sha256::digest(token.as_bytes()).to_vec();
        self.with(|c| {
            c.execute("DELETE FROM sessions WHERE token_hash = ?1", [&hash])?;
            self.grants.signed_out(&hash);
            Ok(())
        })
    }

    /// Deletes an account with its sessions and memberships. Its sync
    /// sockets close with 4401.
    pub fn delete_user(&self, user_id: &str) -> Result<(), String> {
        self.with(|c| {
            c.execute("DELETE FROM users WHERE id = ?1", [user_id])?;
            self.grants.user_removed(user_id);
            Ok(())
        })
    }

    /// Takes `user_id` out of a project. Their sockets to it close with 4004.
    pub fn remove_member(&self, project_id: &str, user_id: &str) -> Result<(), String> {
        self.with(|c| {
            c.execute(
                "DELETE FROM members WHERE project_id = ?1 AND user_id = ?2",
                params![project_id, user_id],
            )?;
            self.grants.member_removed(project_id, user_id);
            Ok(())
        })
    }

    /// Lets the session `token` hold a sync socket to `project`. Refuses
    /// with 4401 when the session is missing or expired, and with 4004 when
    /// the user is not a member. The grant is registered under the database
    /// lock, so a sign-out or removal either happens before (and the grant is
    /// refused) or after (and ends it).
    pub fn grant(&self, token: &str, project: &str) -> Result<Result<Grant, u16>, String> {
        let hash = Sha256::digest(token.as_bytes()).to_vec();
        self.with(|c| {
            let found = c
                .query_row(
                    "SELECT u.id, u.email, u.name, s.expires_at FROM sessions s
                     JOIN users u ON u.id = s.user_id
                     WHERE s.token_hash = ?1 AND s.expires_at > ?2",
                    params![hash, now_secs()],
                    |r| {
                        Ok((
                            User {
                                id: r.get(0)?,
                                email: r.get(1)?,
                                name: r.get(2)?,
                            },
                            r.get::<_, i64>(3)?,
                        ))
                    },
                )
                .optional()?;
            let Some((user, expires_at)) = found else {
                return Ok(Err(nisaba_sync::close::SIGNED_OUT));
            };
            let role: Option<String> = c
                .query_row(
                    "SELECT role FROM members WHERE project_id = ?1 AND user_id = ?2",
                    params![project, user.id],
                    |r| r.get(0),
                )
                .optional()?;
            let role = match role.as_deref() {
                None => return Ok(Err(nisaba_sync::close::NOT_FOUND)),
                Some("owner" | "editor") => nisaba_sync::Role::Editor,
                Some("commenter") => nisaba_sync::Role::Commenter,
                Some(_) => nisaba_sync::Role::Viewer,
            };
            Ok(Ok(self.grants.add(hash, user, project, role, expires_at)))
        })
    }

    /// The open sync socket grants.
    #[must_use]
    pub fn grants(&self) -> &Grants {
        &self.grants
    }

    /// Adds `user_id` to a project. Only for tests and tools until
    /// invitations exist.
    pub fn add_member(&self, project_id: &str, user_id: &str, role: &str) -> Result<(), String> {
        self.with(|c| {
            c.execute(
                "INSERT INTO members (project_id, user_id, role) VALUES (?1, ?2, ?3)",
                params![project_id, user_id, role],
            )
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

    /// Stores an asset under its SHA-256, records that `project` holds it,
    /// and returns the hash. The file is written and synced before its rows
    /// are committed.
    pub fn put_asset(&self, project: &str, bytes: &[u8]) -> Result<String, String> {
        let hash = hex(&Sha256::digest(bytes));
        let path = self.asset_path(&hash);
        if !path.exists() {
            std::fs::create_dir_all(path.parent().expect("asset path has a parent"))
                .map_err(|e| e.to_string())?;
            let tmp = path.with_extension(format!("{:016x}.tmp", rand::random::<u64>()));
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
            let tx = c.transaction()?;
            tx.execute(
                "INSERT OR IGNORE INTO assets (hash, size) VALUES (?1, ?2)",
                params![hash, size],
            )?;
            tx.execute(
                "INSERT OR IGNORE INTO project_assets (project_id, hash) VALUES (?1, ?2)",
                params![project, hash],
            )?;
            tx.commit()
        })?;
        Ok(hash)
    }

    /// The bytes of an asset, if `project` holds it. A project holds an
    /// asset only when the bytes were uploaded to that project, so knowing a
    /// hash from elsewhere gives no access to it.
    pub fn project_asset(&self, project: &str, hash: &str) -> Result<Option<Vec<u8>>, String> {
        if hash.len() != 64 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Ok(None);
        }
        let held = self.with(|c| {
            c.query_row(
                "SELECT 1 FROM project_assets WHERE project_id = ?1 AND hash = ?2",
                params![project, hash],
                |_| Ok(()),
            )
            .optional()
        })?;
        Ok(held.and_then(|()| std::fs::read(self.asset_path(hash)).ok()))
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

fn insert_user(
    c: &Connection,
    id: &str,
    email: &str,
    name: &str,
    hash: &str,
) -> rusqlite::Result<()> {
    c.execute(
        "INSERT INTO users (id, email, name, password_hash, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![id, email, name, hash, now_secs()],
    )
    .map(drop)
}

/// Why setup with `code` cannot work now, if it cannot. The code is compared
/// by SHA-256 in constant time.
fn setup_refused(c: &Connection, code: Option<&str>) -> rusqlite::Result<Option<Setup>> {
    let users: i64 = c.query_row("SELECT count(*) FROM users", [], |r| r.get(0))?;
    if users > 0 {
        return Ok(Some(Setup::Taken));
    }
    let stored: Option<Vec<u8>> = c
        .query_row(
            "SELECT value FROM settings WHERE key = ?1",
            [SETUP_CODE],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(stored) = stored {
        use subtle::ConstantTimeEq as _;
        let given = Sha256::digest(normalize_code(code.unwrap_or("")).as_bytes());
        if !bool::from(given.as_slice().ct_eq(&stored)) {
            return Ok(Some(Setup::WrongCode));
        }
    }
    Ok(None)
}

/// A setup code as typed: hex digits only, lower case. People may copy it
/// with or without the dashes and in either case.
fn normalize_code(code: &str) -> String {
    code.chars()
        .filter(char::is_ascii_hexdigit)
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

impl RoomStore for Store {
    fn load(&self, project: &str) -> Result<Stored, String> {
        self.with(|c| {
            let snapshot = c
                .query_row(
                    "SELECT bytes FROM doc_snapshots WHERE project_id = ?1",
                    [project],
                    |r| r.get(0),
                )
                .optional()?;
            let mut stmt =
                c.prepare("SELECT bytes FROM doc_updates WHERE project_id = ?1 ORDER BY seq")?;
            let log = stmt
                .query_map([project], |r| r.get(0))?
                .collect::<Result<_, _>>()?;
            Ok(Stored { snapshot, log })
        })
    }

    /// Checks `admit` under the database lock, the lock every sign-out and
    /// removal ends its grants under, so access ends either before the
    /// append (and nothing is written) or after it.
    fn append(
        &self,
        project: &str,
        update: &[u8],
        admit: &dyn Fn() -> bool,
    ) -> Result<bool, String> {
        self.with(|c| {
            if !admit() {
                return Ok(false);
            }
            c.execute(
                "INSERT INTO doc_updates (project_id, seq, bytes) VALUES (?1,
                   (SELECT coalesce(max(seq), 0) + 1 FROM doc_updates WHERE project_id = ?1), ?2)",
                params![project, update],
            )?;
            Ok(true)
        })
    }

    /// Replaces the snapshot and, in the same transaction, drops every log row
    /// the snapshot already contains.
    fn compact(
        &self,
        project: &str,
        covers: &VersionVector,
        snapshot: &[u8],
    ) -> Result<(), String> {
        self.with(|c| {
            let tx = c.transaction()?;
            tx.execute(
                "INSERT INTO doc_snapshots (project_id, bytes) VALUES (?1, ?2)
                 ON CONFLICT (project_id) DO UPDATE SET bytes = excluded.bytes",
                params![project, snapshot],
            )?;
            let covered: Vec<i64> = {
                let mut stmt =
                    tx.prepare("SELECT seq, bytes FROM doc_updates WHERE project_id = ?1")?;
                let rows = stmt.query_map([project], |r| {
                    Ok((r.get::<_, i64>(0)?, r.get::<_, Vec<u8>>(1)?))
                })?;
                let mut covered = Vec::new();
                for row in rows {
                    let (seq, bytes) = row?;
                    if let Ok(meta) = loro::LoroDoc::decode_import_blob_meta(&bytes, false)
                        && covers.includes_vv(&meta.partial_end_vv)
                    {
                        covered.push(seq);
                    }
                }
                covered
            };
            for seq in covered {
                tx.execute(
                    "DELETE FROM doc_updates WHERE project_id = ?1 AND seq = ?2",
                    params![project, seq],
                )?;
            }
            tx.commit()
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};

    use nisaba_sync::RoomStore as _;

    use super::*;

    /// The room asks whether a grant still holds while the database lock is
    /// held, and sign-outs and removals end grants under that lock, so no
    /// update is written after access ended.
    #[test]
    fn an_append_asks_for_access_under_the_lock_that_ends_it() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        store.create_user("u", "u@example.org", "U", "x").unwrap();
        store.create_session("t", "u", 86_400).unwrap();
        store.create_project("p", "P", "u").unwrap();
        let grant = store.grant("t", "p").unwrap().unwrap();
        let admits = grant.admits();
        let locked = AtomicBool::new(false);
        let asked = |admit: &nisaba_sync::Admit| {
            locked.store(store.conn.try_lock().is_err(), Ordering::SeqCst);
            admit()
        };
        assert!(store.append("p", b"one", &|| asked(&admits)).unwrap());
        assert!(locked.load(Ordering::SeqCst), "asked outside the lock");
        store.delete_session("t").unwrap();
        assert!(!store.append("p", b"two", &|| asked(&admits)).unwrap());
        assert_eq!(store.log_len("p").unwrap(), 1);
    }
}
