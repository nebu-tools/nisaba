//! `nisaba backup` and `nisaba restore`.
//!
//! A backup is a data directory. `backup` copies the database first, with
//! `SQLite`'s online backup in one read transaction, so the copy is one
//! moment of the database while the server goes on writing. Then it copies
//! every asset the copied database refers to, and the font and package
//! files, that the destination does not hold yet. Every asset the copy
//! refers to was on disk before the row that refers to it was committed,
//! so the copy is complete. A second run into the same destination copies
//! only what is new. Each file is written to a temporary name, synced and
//! renamed, and each asset's bytes are checked against its hash.
//!
//! `restore` copies a backup into a data directory that does not exist or is
//! empty, never over data. It checks the database (`PRAGMA
//! integrity_check`), every asset against its hash, and that every
//! project's document loads, before it puts the directory in place.

use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};

use rusqlite::Connection;
use sha2::{Digest, Sha256};

use crate::store::hex;

/// What a backup or restore copied.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Report {
    pub projects: usize,
    pub assets_copied: usize,
    pub assets_kept: usize,
    pub files_copied: usize,
    pub bytes: u64,
}

impl std::fmt::Display for Report {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} projects; {} assets copied, {} already there; {} font and package files copied; \
             {} bytes",
            self.projects, self.assets_copied, self.assets_kept, self.files_copied, self.bytes
        )
    }
}

fn io(what: &str, path: &Path) -> impl Fn(std::io::Error) -> String {
    let path = path.display().to_string();
    let what = what.to_owned();
    move |e| format!("cannot {what} {path}: {e}")
}

#[expect(clippy::needless_pass_by_value, reason = "used as map_err(sql)")]
fn sql(e: rusqlite::Error) -> String {
    format!("database: {e}")
}

/// Copies the data directory `data` into `dest` while a server may run on
/// it.
pub fn backup(data: &Path, dest: &Path) -> Result<Report, String> {
    let source = data.join("nisaba.db");
    if !source.is_file() {
        return Err(format!("{} holds no nisaba.db", data.display()));
    }
    if same_dir(data, dest) {
        return Err("the backup must go somewhere else than the data directory".into());
    }
    std::fs::create_dir_all(dest).map_err(io("make", dest))?;
    let partial = dest.join(format!(".nisaba.db.{:016x}.tmp", rand::random::<u64>()));
    let copied = (|| {
        let from = Connection::open_with_flags(&source, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(sql)?;
        from.busy_timeout(std::time::Duration::from_secs(30))
            .map_err(sql)?;
        let mut to = Connection::open(&partial).map_err(sql)?;
        {
            let backup = rusqlite::backup::Backup::new(&from, &mut to).map_err(sql)?;
            // All pages in one step: one read transaction, one moment.
            backup.step(-1).map_err(sql)?;
        }
        to.execute_batch("PRAGMA journal_mode = DELETE;")
            .map_err(sql)?;
        drop(to);
        sync_file(&partial)
    })();
    if let Err(e) = copied {
        let _ = std::fs::remove_file(&partial);
        return Err(e);
    }
    let target = dest.join("nisaba.db");
    std::fs::rename(&partial, &target).map_err(io("rename", &partial))?;
    for stale in ["nisaba.db-wal", "nisaba.db-shm", "nisaba.db-journal"] {
        let _ = std::fs::remove_file(dest.join(stale));
    }
    sync_dir(dest)?;
    crate::crash::point("backup:database-copied");
    let mut report = copy_assets(&target, data, dest, false)?;
    for dir in ["fonts", "packages"] {
        copy_tree(&data.join(dir), &dest.join(dir), &mut report)?;
    }
    Ok(report)
}

/// Copies the backup `backup` into the data directory `data`, which must
/// not exist or be empty.
pub fn restore(backup: &Path, data: &Path) -> Result<Report, String> {
    let source = backup.join("nisaba.db");
    if !source.is_file() {
        return Err(format!("{} holds no nisaba.db", backup.display()));
    }
    if data.exists()
        && std::fs::read_dir(data)
            .map_err(io("read", data))?
            .next()
            .is_some()
    {
        return Err(format!(
            "{} is not empty; restore only into an empty or new data directory",
            data.display()
        ));
    }
    {
        let conn = Connection::open_with_flags(&source, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(sql)?;
        let check: String = conn
            .query_row("PRAGMA integrity_check", [], |r| r.get(0))
            .map_err(sql)?;
        if check != "ok" {
            return Err(format!("the backup's database is damaged: {check}"));
        }
    }
    let parent = data
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent).map_err(io("make", parent))?;
    let staging = parent.join(format!(
        ".{}.restore-{:016x}",
        data.file_name()
            .map_or("data".into(), |n| n.to_string_lossy().into_owned()),
        rand::random::<u64>()
    ));
    let result = (|| {
        std::fs::create_dir_all(&staging).map_err(io("make", &staging))?;
        let target = staging.join("nisaba.db");
        copy_file(&source, &target)?;
        let mut report = copy_assets(&target, backup, &staging, true)?;
        for dir in ["fonts", "packages"] {
            copy_tree(&backup.join(dir), &staging.join(dir), &mut report)?;
        }
        report.projects = check_projects(&staging)?;
        sync_dir(&staging)?;
        Ok(report)
    })();
    let report = match result {
        Ok(report) => report,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&staging);
            return Err(e);
        }
    };
    if data.exists() {
        std::fs::remove_dir(data).map_err(io("replace", data))?;
    }
    std::fs::rename(&staging, data).map_err(io("move the restored data to", data))?;
    sync_dir(parent)?;
    Ok(report)
}

/// Every project's document loads: its snapshot and log import into Loro.
fn check_projects(dir: &Path) -> Result<usize, String> {
    use nisaba_sync::RoomStore as _;
    let store = crate::store::Store::open(dir).map_err(|e| e.to_string())?;
    let ids = store.project_ids().map_err(|e| e.to_string())?;
    for id in &ids {
        let stored = store.load(id)?;
        let doc = loro::LoroDoc::new();
        if let Some(snapshot) = &stored.snapshot {
            doc.import(snapshot)
                .map_err(|e| format!("project {id}: its snapshot does not load: {e}"))?;
        }
        for update in &stored.log {
            doc.import(update)
                .map_err(|e| format!("project {id}: an update does not load: {e}"))?;
        }
    }
    store.checkpoint().map_err(|e| e.to_string())?;
    Ok(ids.len())
}

/// Copies every asset the database at `db` refers to from `from` to `to`,
/// checking each against its hash. With `all`, a missing asset is an error;
/// otherwise too, since the database refers to it.
fn copy_assets(db: &Path, from: &Path, to: &Path, all: bool) -> Result<Report, String> {
    let conn =
        Connection::open_with_flags(db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).map_err(sql)?;
    let mut report = Report {
        projects: conn
            .query_row("SELECT count(*) FROM projects", [], |r| r.get::<_, i64>(0))
            .map_err(sql)?
            .try_into()
            .unwrap_or(0),
        ..Report::default()
    };
    let mut stmt = conn
        .prepare("SELECT hash, size FROM assets ORDER BY hash")
        .map_err(sql)?;
    let rows: Vec<(String, i64)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .map_err(sql)?
        .collect::<Result<_, _>>()
        .map_err(sql)?;
    for (hash, size) in rows {
        if !crate::store::is_hash(&hash) {
            return Err(format!(
                "the database names an asset {hash:?} that is not a hash"
            ));
        }
        let rel = PathBuf::from("assets").join(&hash[..2]).join(&hash[2..]);
        let target = to.join(&rel);
        let size = u64::try_from(size).unwrap_or(0);
        if !all && std::fs::metadata(&target).is_ok_and(|m| m.len() == size) {
            report.assets_kept += 1;
            continue;
        }
        let source = from.join(&rel);
        let bytes = std::fs::read(&source).map_err(io("read the asset", &source))?;
        if hex(&Sha256::digest(&bytes)) != hash {
            return Err(format!(
                "the asset {} is damaged: its bytes do not match its hash",
                source.display()
            ));
        }
        write_file(&target, &bytes)?;
        report.assets_copied += 1;
        report.bytes += bytes.len() as u64;
    }
    Ok(report)
}

/// Copies the files under `from` that `to` does not hold with the same size.
fn copy_tree(from: &Path, to: &Path, report: &mut Report) -> Result<(), String> {
    let Ok(entries) = std::fs::read_dir(from) else {
        return Ok(());
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        if name.to_string_lossy().starts_with('.') {
            continue;
        }
        let target = to.join(&name);
        let kind = entry.file_type().map_err(io("read", &path))?;
        if kind.is_dir() {
            copy_tree(&path, &target, report)?;
        } else if kind.is_file() {
            let len = entry.metadata().map_err(io("read", &path))?.len();
            if std::fs::metadata(&target).is_ok_and(|m| m.len() == len) {
                continue;
            }
            let bytes = std::fs::read(&path).map_err(io("read", &path))?;
            write_file(&target, &bytes)?;
            report.files_copied += 1;
            report.bytes += bytes.len() as u64;
        }
    }
    Ok(())
}

fn copy_file(from: &Path, to: &Path) -> Result<(), String> {
    let mut source = std::fs::File::open(from).map_err(io("read", from))?;
    let mut bytes = Vec::new();
    source.read_to_end(&mut bytes).map_err(io("read", from))?;
    write_file(to, &bytes)
}

/// Writes `bytes` to a temporary name beside `path`, syncs it and renames it.
fn write_file(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let dir = path.parent().ok_or("a file has no directory")?;
    std::fs::create_dir_all(dir).map_err(io("make", dir))?;
    let tmp = dir.join(format!(".{:016x}.tmp", rand::random::<u64>()));
    let written = (|| {
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        std::fs::rename(&tmp, path)
    })();
    if let Err(e) = written {
        let _ = std::fs::remove_file(&tmp);
        return Err(io("write", path)(e));
    }
    sync_dir(dir)
}

fn sync_file(path: &Path) -> Result<(), String> {
    std::fs::File::open(path)
        .and_then(|f| f.sync_all())
        .map_err(io("sync", path))
}

fn sync_dir(path: &Path) -> Result<(), String> {
    std::fs::File::open(path)
        .and_then(|f| f.sync_all())
        .map_err(io("sync", path))
}

fn same_dir(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}
