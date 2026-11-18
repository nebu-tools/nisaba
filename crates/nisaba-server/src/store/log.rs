//! Each project's update log and snapshot: what the rooms load and write.

use loro::VersionVector;
use nisaba_sync::{LogSize, RoomStore, Stored};
use rusqlite::{OptionalExtension, params};

use super::{Store, StoreResult, now_secs};

/// `updated_at` of a project moves at most this often, so a busy project
/// does not rewrite its row on every keystroke.
const UPDATED_EVERY: i64 = 60;

impl Store {
    /// Number of log rows for a project.
    pub fn log_len(&self, project_id: &str) -> StoreResult<i64> {
        self.read(|c| {
            Ok(c.query_row(
                "SELECT count(*) FROM doc_updates WHERE project_id = ?1",
                [project_id],
                |r| r.get(0),
            )?)
        })
    }

    /// The bytes a project stores: snapshot and log.
    pub fn stored_bytes(&self, project_id: &str) -> StoreResult<i64> {
        self.read(|c| {
            Ok(c.query_row(
                "SELECT coalesce((SELECT length(bytes) FROM doc_snapshots WHERE project_id = ?1), 0)
                      + coalesce((SELECT sum(length(bytes)) FROM doc_updates WHERE project_id = ?1), 0)",
                [project_id],
                |r| r.get(0),
            )?)
        })
    }
}

impl RoomStore for Store {
    fn load(&self, project: &str) -> Result<Stored, String> {
        Ok(self.read(|c| {
            // One read transaction, so the snapshot and the log are of the
            // same moment even while a compaction commits.
            c.execute_batch("BEGIN")?;
            let loaded = (|| {
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
                Ok::<_, rusqlite::Error>(Stored { snapshot, log })
            })();
            c.execute_batch("COMMIT")?;
            Ok(loaded?)
        })?)
    }

    /// Checks `admit` under the write lock, the lock every sign-out,
    /// removal and role change ends its grants under, so access ends either
    /// before the append (and nothing is written) or after it. A project
    /// that was deleted takes nothing either.
    fn append(
        &self,
        project: &str,
        update: &[u8],
        admit: &dyn Fn() -> bool,
    ) -> Result<bool, String> {
        Ok(self.write(|c| {
            if !admit() {
                return Ok(false);
            }
            let tx = c.transaction()?;
            let exists = tx
                .query_row(
                    "SELECT 1 FROM projects WHERE id = ?1",
                    [project],
                    |_| Ok(()),
                )
                .optional()?
                .is_some();
            if !exists {
                return Ok(false);
            }
            tx.execute(
                "INSERT INTO doc_updates (project_id, seq, bytes) VALUES (?1,
                   (SELECT coalesce(max(seq), 0) + 1 FROM doc_updates WHERE project_id = ?1), ?2)",
                params![project, update],
            )?;
            let now = now_secs();
            tx.execute(
                "UPDATE projects SET updated_at = ?2 WHERE id = ?1 AND updated_at <= ?3",
                params![project, now, now - UPDATED_EVERY],
            )?;
            crate::crash::point("append:before-commit");
            tx.commit()?;
            crate::crash::point("append:after-commit");
            Ok(true)
        })?)
    }

    /// Replaces the snapshot and, in the same transaction, drops every log
    /// row the snapshot already contains. Returns what is left in the log.
    fn compact(
        &self,
        project: &str,
        covers: &VersionVector,
        snapshot: &[u8],
    ) -> Result<LogSize, String> {
        Ok(self.write(|c| {
            let tx = c.transaction()?;
            tx.execute(
                "INSERT INTO doc_snapshots (project_id, bytes) VALUES (?1, ?2)
                 ON CONFLICT (project_id) DO UPDATE SET bytes = excluded.bytes",
                params![project, snapshot],
            )?;
            crate::crash::point("compact:snapshot-written");
            let mut left = LogSize::default();
            let covered: Vec<i64> = {
                let mut stmt =
                    tx.prepare("SELECT seq, bytes FROM doc_updates WHERE project_id = ?1")?;
                let rows = stmt.query_map([project], |r| {
                    Ok((r.get::<_, i64>(0)?, r.get::<_, Vec<u8>>(1)?))
                })?;
                let mut covered = Vec::new();
                for row in rows {
                    let (seq, bytes) = row?;
                    match loro::LoroDoc::decode_import_blob_meta(&bytes, false) {
                        Ok(meta) if covers.includes_vv(&meta.partial_end_vv) => covered.push(seq),
                        _ => {
                            left.entries += 1;
                            left.bytes += bytes.len();
                        }
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
            crate::crash::point("compact:before-commit");
            tx.commit()?;
            crate::crash::point("compact:after-commit");
            Ok(left)
        })?)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};

    use nisaba_sync::RoomStore as _;

    use super::*;
    use crate::api::User;

    /// The room asks whether a grant still holds while the write lock is
    /// held, and sign-outs and removals end grants under that lock, so no
    /// update is written after access ended.
    #[test]
    fn an_append_asks_for_access_under_the_lock_that_ends_it() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let user = User {
            id: "u".into(),
            email: "u@example.org".into(),
            name: "U".into(),
            is_admin: false,
        };
        store.create_account(&user, "x").unwrap().unwrap();
        store.create_session("t", "u").unwrap();
        store.create_project("p", "P", "u").unwrap();
        let grant = store.grant("t", "p").unwrap().unwrap();
        let access = grant.access();
        let locked = AtomicBool::new(false);
        let asked = || {
            locked.store(store.inner.writer.try_lock().is_err(), Ordering::SeqCst);
            access().is_some()
        };
        assert!(store.append("p", b"one", &asked).unwrap());
        assert!(locked.load(Ordering::SeqCst), "asked outside the lock");
        store.delete_session("t").unwrap();
        assert!(!store.append("p", b"two", &asked).unwrap());
        assert_eq!(store.log_len("p").unwrap(), 1);
        // A deleted project takes nothing, whoever asks.
        store.delete_project("p").unwrap();
        assert!(!store.append("p", b"three", &|| true).unwrap());
        assert_eq!(store.log_len("p").unwrap(), 0);
    }
}
