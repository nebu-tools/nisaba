//! Asset files: content-addressed bytes under `assets/ab/cdef…`.
//!
//! An upload writes the bytes to a temporary file, syncs it, renames it to
//! its hash and syncs the directory. Only then are the rows committed that
//! say a project holds it, so a crash can leave an unreferenced file, never
//! a reference to a missing one. [`Store::sweep_assets`] removes such files
//! at start.

use std::io::Write as _;
use std::path::PathBuf;

use rusqlite::{OptionalExtension, params};
use sha2::{Digest, Sha256};

use super::{Store, StoreResult, hex};

/// Whether `hash` looks like a SHA-256 in lower-case hex.
#[must_use]
pub fn is_hash(hash: &str) -> bool {
    hash.len() == 64 && hash.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

impl Store {
    /// Stores an asset under its SHA-256, records that `project` holds it,
    /// and returns the hash.
    pub fn put_asset(&self, project: &str, bytes: &[u8]) -> StoreResult<String> {
        let hash = self.write_asset_file(bytes)?;
        self.record_asset(project, hash, bytes)
    }

    /// Writes an asset's file, synced, under its hash, and returns the hash.
    /// No row refers to it yet.
    pub fn write_asset_file(&self, bytes: &[u8]) -> StoreResult<String> {
        let hash = hex(&Sha256::digest(bytes));
        let path = self.asset_path(&hash);
        if !path.exists() {
            let dir = path.parent().unwrap_or(&self.inner.dir).to_owned();
            std::fs::create_dir_all(&dir)?;
            let tmp = dir.join(format!(".{:016x}.tmp", rand::random::<u64>()));
            let written = (|| {
                let mut file = std::fs::File::create(&tmp)?;
                file.write_all(bytes)?;
                file.sync_all()
            })();
            if let Err(e) = written {
                let _ = std::fs::remove_file(&tmp);
                return Err(e.into());
            }
            crate::crash::point("asset:file-written");
            std::fs::rename(&tmp, &path)?;
            crate::crash::point("asset:renamed");
            std::fs::File::open(&dir)?.sync_all()?;
        }
        Ok(hash)
    }

    fn record_asset(&self, project: &str, hash: String, bytes: &[u8]) -> StoreResult<String> {
        let size = i64::try_from(bytes.len()).unwrap_or(i64::MAX);
        self.write(|c| {
            let tx = c.transaction()?;
            tx.execute(
                "INSERT OR IGNORE INTO assets (hash, size) VALUES (?1, ?2)",
                params![hash, size],
            )?;
            tx.execute(
                "INSERT OR IGNORE INTO project_assets (project_id, hash) VALUES (?1, ?2)",
                params![project, hash],
            )?;
            tx.commit()?;
            Ok(())
        })?;
        crate::crash::point("asset:after-commit");
        Ok(hash)
    }

    /// The bytes of an asset, if `project` holds it. A project holds an
    /// asset only when the bytes were uploaded to that project, so knowing a
    /// hash from elsewhere gives no access to it.
    pub fn project_asset(&self, project: &str, hash: &str) -> StoreResult<Option<Vec<u8>>> {
        if !is_hash(hash) {
            return Ok(None);
        }
        let held = self.read(|c| {
            Ok(c.query_row(
                "SELECT 1 FROM project_assets WHERE project_id = ?1 AND hash = ?2",
                params![project, hash],
                |_| Ok(()),
            )
            .optional()?)
        })?;
        if held.is_none() {
            return Ok(None);
        }
        match std::fs::read(self.asset_path(hash)) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Every asset hash the database refers to, with its size.
    pub fn asset_hashes(&self) -> StoreResult<Vec<(String, u64)>> {
        self.read(|c| {
            let mut stmt = c.prepare("SELECT hash, size FROM assets ORDER BY hash")?;
            let rows = stmt
                .query_map([], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?.cast_unsigned()))
                })?
                .collect::<Result<_, _>>()?;
            Ok(rows)
        })
    }

    pub(super) fn asset_path(&self, hash: &str) -> PathBuf {
        let (head, tail) = hash.split_at(2.min(hash.len()));
        self.inner.dir.join("assets").join(head).join(tail)
    }

    pub(super) fn remove_asset_file(&self, hash: &str) {
        if is_hash(hash) {
            let _ = std::fs::remove_file(self.asset_path(hash));
        }
    }

    /// Deletes asset files no row refers to, and temporary files a crash
    /// left. Call it before the server takes requests: an upload in flight
    /// has a file whose row is not written yet. Returns how many it deleted.
    pub fn sweep_assets(&self) -> StoreResult<usize> {
        let known: std::collections::HashSet<String> =
            self.asset_hashes()?.into_iter().map(|(h, _)| h).collect();
        let mut removed = 0;
        let root = self.inner.dir.join("assets");
        for dir in std::fs::read_dir(&root)?.flatten() {
            let head = dir.file_name().to_string_lossy().into_owned();
            if !dir.file_type().is_ok_and(|t| t.is_dir()) {
                continue;
            }
            for file in std::fs::read_dir(dir.path())?.flatten() {
                let tail = file.file_name().to_string_lossy().into_owned();
                if !known.contains(&format!("{head}{tail}")) {
                    std::fs::remove_file(file.path())?;
                    removed += 1;
                }
            }
        }
        Ok(removed)
    }
}
