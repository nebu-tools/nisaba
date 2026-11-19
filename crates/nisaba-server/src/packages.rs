//! The cache of Typst packages in the data directory's `packages/`.
//!
//! A package such as `@preview/cetz:0.3.4` is kept as
//! `packages/preview/cetz-0.3.4.tar.gz`. On a miss, and when the server is
//! allowed to download, it fetches the archive from the registry, checks
//! that it is no larger than [`PACKAGE_MAX`] and unpacks within
//! [`PACKAGE_UNPACKED_MAX`] and [`PACKAGE_FILES_MAX`] files, and stores it
//! (written to a temporary file, synced and renamed). Two requests for one
//! missing package share one download.
//!
//! The browser asks for packages by `GET /packages/...`; an export collects
//! the packages its files import, and theirs, by [`Packages::for_files`].

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{Read as _, Write as _};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use nisaba_compile_core::FileContents;

use crate::config;
use crate::limits::{
    PACKAGE_DOWNLOAD, PACKAGE_FILES_MAX, PACKAGE_MAX, PACKAGE_UNPACKED_MAX, PACKAGES_PER_EXPORT,
};
use crate::store::StoreError;

/// A package: namespace, name and version.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Spec {
    namespace: String,
    name: String,
    version: String,
}

fn word(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
        && !s.starts_with('-')
}

impl Spec {
    /// A package spec from its parts, or `None` when a part is not a
    /// lower-case name or the version is not `major.minor.patch`.
    #[must_use]
    pub fn new(namespace: &str, name: &str, version: &str) -> Option<Self> {
        let numbers: Vec<_> = version.split('.').collect();
        let version_ok = numbers.len() == 3
            && numbers
                .iter()
                .all(|n| !n.is_empty() && n.len() <= 9 && n.bytes().all(|b| b.is_ascii_digit()));
        (word(namespace) && word(name) && version_ok).then(|| Self {
            namespace: namespace.to_owned(),
            name: name.to_owned(),
            version: version.to_owned(),
        })
    }

    /// Reads `@namespace/name:version`.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        let rest = s.strip_prefix('@')?;
        let (namespace, rest) = rest.split_once('/')?;
        let (name, version) = rest.split_once(':')?;
        Self::new(namespace, name, version)
    }

    fn file(&self) -> PathBuf {
        PathBuf::from(&self.namespace).join(format!("{}-{}.tar.gz", self.name, self.version))
    }
}

impl std::fmt::Display for Spec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "@{}/{}:{}", self.namespace, self.name, self.version)
    }
}

/// Why a package could not be served.
#[derive(Debug)]
pub enum PackageError {
    /// Not in the cache, and not downloaded: downloads are off, or the
    /// registry does not have it.
    NotFound(String),
    /// The registry could not be reached or sent something unusable.
    Unavailable(String),
    Store(StoreError),
}

pub struct Packages {
    dir: PathBuf,
    settings: config::Packages,
    client: std::sync::OnceLock<reqwest::Client>,
    downloads: Mutex<HashMap<Spec, Arc<tokio::sync::Mutex<()>>>>,
}

/// The files of one package, for an export: its spec and its files.
pub type Unpacked = (String, Vec<(String, Vec<u8>)>);

impl Packages {
    #[must_use]
    pub fn new(dir: PathBuf, settings: config::Packages) -> Self {
        Self {
            dir,
            settings,
            client: std::sync::OnceLock::new(),
            downloads: Mutex::default(),
        }
    }

    /// The archive of `spec`, from the cache or downloaded into it.
    pub async fn archive(&self, spec: &Spec) -> Result<Vec<u8>, PackageError> {
        let path = self.dir.join(spec.file());
        if let Some(bytes) = read(&path).await? {
            return Ok(bytes);
        }
        if !self.settings.download {
            return Err(PackageError::NotFound(format!(
                "{spec} is not in this server's package cache, and this server does not \
                 download packages"
            )));
        }
        let gate = Arc::clone(
            self.downloads
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .entry(spec.clone())
                .or_default(),
        );
        let _one = gate.lock().await;
        let result = async {
            if let Some(bytes) = read(&path).await? {
                return Ok(bytes);
            }
            let bytes = self.download(spec).await?;
            unpack(&bytes).map_err(|why| {
                PackageError::Unavailable(format!("the registry's {spec} is unusable: {why}"))
            })?;
            let store = path.clone();
            let stored = bytes.clone();
            tokio::task::spawn_blocking(move || write_atomically(&store, &stored))
                .await
                .map_err(|e| PackageError::Store(StoreError::Other(e.to_string())))?
                .map_err(PackageError::Store)?;
            tracing::info!(package = %spec, bytes = bytes.len(), "cached a package");
            Ok(bytes)
        }
        .await;
        self.downloads
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(spec);
        result
    }

    async fn download(&self, spec: &Spec) -> Result<Vec<u8>, PackageError> {
        let unavailable = |e: &dyn std::fmt::Display| {
            PackageError::Unavailable(format!("cannot download {spec}: {e}"))
        };
        let client = if let Some(c) = self.client.get() {
            c
        } else {
            let c = reqwest::Client::builder()
                .timeout(PACKAGE_DOWNLOAD)
                .user_agent(concat!("nisaba/", env!("CARGO_PKG_VERSION")))
                .build()
                .map_err(|e| unavailable(&e))?;
            self.client.get_or_init(|| c)
        };
        let url = format!(
            "{}/{}/{}-{}.tar.gz",
            self.settings.registry, spec.namespace, spec.name, spec.version
        );
        let mut response = client.get(&url).send().await.map_err(|e| unavailable(&e))?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Err(PackageError::NotFound(format!(
                "the package registry has no {spec}"
            )));
        }
        if !response.status().is_success() {
            return Err(unavailable(&response.status()));
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|e| unavailable(&e))? {
            if bytes.len() + chunk.len() > PACKAGE_MAX {
                return Err(unavailable(&format!(
                    "it is larger than {} MiB",
                    PACKAGE_MAX >> 20
                )));
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok(bytes)
    }

    /// The packages `files` import, and the packages those import, at most
    /// [`PACKAGES_PER_EXPORT`], unpacked. A package that cannot be had is
    /// left out, and the compiler names it.
    pub async fn for_files(&self, files: &[(String, FileContents)]) -> Vec<Unpacked> {
        let mut queue: VecDeque<Spec> = files
            .iter()
            .filter(|(path, _)| is_typst(path))
            .filter_map(|(_, c)| match c {
                FileContents::Text(t) => Some(scan(t)),
                FileContents::Bytes(_) => None,
            })
            .flatten()
            .collect();
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        while let Some(spec) = queue.pop_front() {
            if seen.len() >= PACKAGES_PER_EXPORT || !seen.insert(spec.clone()) {
                continue;
            }
            let Ok(bytes) = self.archive(&spec).await else {
                continue;
            };
            let Ok(files) = unpack(&bytes) else { continue };
            for (path, data) in &files {
                if is_typst(path)
                    && let Ok(text) = std::str::from_utf8(data)
                {
                    queue.extend(scan(text));
                }
            }
            out.push((spec.to_string(), files));
        }
        out
    }
}

fn is_typst(path: &str) -> bool {
    std::path::Path::new(path)
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("typ"))
}

async fn read(path: &std::path::Path) -> Result<Option<Vec<u8>>, PackageError> {
    match tokio::fs::read(path).await {
        Ok(bytes) => Ok(Some(bytes)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(PackageError::Store(e.into())),
    }
}

fn write_atomically(path: &std::path::Path, bytes: &[u8]) -> Result<(), StoreError> {
    let dir = path
        .parent()
        .ok_or_else(|| StoreError::Other("no directory".into()))?;
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(".{:016x}.tmp", rand::random::<u64>()));
    let written = (|| {
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        std::fs::rename(&tmp, path)?;
        std::fs::File::open(dir)?.sync_all()
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    Ok(written?)
}

/// The files of a package archive, with their paths inside the package.
/// Refuses an archive that unpacks to more than [`PACKAGE_UNPACKED_MAX`]
/// bytes or [`PACKAGE_FILES_MAX`] files, or holds a path that leaves the
/// package.
pub fn unpack(bytes: &[u8]) -> Result<Vec<(String, Vec<u8>)>, String> {
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(bytes));
    let mut files = Vec::new();
    let mut total = 0u64;
    for entry in archive.entries().map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        if !entry.header().entry_type().is_file() {
            continue;
        }
        if files.len() >= PACKAGE_FILES_MAX {
            return Err(format!("it holds more than {PACKAGE_FILES_MAX} files"));
        }
        let path = entry.path().map_err(|e| e.to_string())?;
        let mut parts = Vec::new();
        for c in path.components() {
            match c {
                std::path::Component::Normal(p) => parts.push(p.to_string_lossy().into_owned()),
                std::path::Component::CurDir => {}
                _ => return Err(format!("{} leaves the package", path.display())),
            }
        }
        if parts.is_empty() {
            continue;
        }
        let mut data = Vec::new();
        let left = PACKAGE_UNPACKED_MAX - total;
        entry
            .take(left + 1)
            .read_to_end(&mut data)
            .map_err(|e| e.to_string())?;
        total += data.len() as u64;
        if total > PACKAGE_UNPACKED_MAX {
            return Err(format!(
                "it unpacks to more than {} MiB",
                PACKAGE_UNPACKED_MAX >> 20
            ));
        }
        files.push((parts.join("/"), data));
    }
    if !files.iter().any(|(p, _)| p == "typst.toml") {
        return Err("it has no typst.toml".into());
    }
    Ok(files)
}

/// Every `@namespace/name:version` in a text. A comment or a string that
/// names a package counts too; loading one package more than needed is
/// harmless.
#[must_use]
pub fn scan(text: &str) -> Vec<Spec> {
    let mut out = Vec::new();
    for (i, _) in text.match_indices('@') {
        let rest = &text[i + 1..];
        let end = rest
            .find(|c: char| {
                !(c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '/' | ':' | '.'))
            })
            .unwrap_or(rest.len());
        let candidate = rest[..end].trim_end_matches('.');
        if let Some(spec) = Spec::parse(&format!("@{candidate}")) {
            out.push(spec);
        }
    }
    out
}

#[cfg(test)]
pub mod tests {
    use super::*;

    /// A `.tar.gz` of `files`.
    #[must_use]
    pub fn archive(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut tar = tar::Builder::new(Vec::new());
        for (path, data) in files {
            let mut header = tar::Header::new_gnu();
            header.set_size(data.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            tar.append_data(&mut header, path, *data).unwrap();
        }
        let tar = tar.into_inner().unwrap();
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        gz.write_all(&tar).unwrap();
        gz.finish().unwrap()
    }

    #[test]
    fn specs_are_read_and_checked() {
        let spec = Spec::parse("@preview/cetz:0.3.4").unwrap();
        assert_eq!(spec.to_string(), "@preview/cetz:0.3.4");
        assert_eq!(spec.file(), PathBuf::from("preview/cetz-0.3.4.tar.gz"));
        for bad in [
            "@preview/cetz:0.3",
            "@preview/../x:1.0.0",
            "@Preview/cetz:0.3.4",
            "preview/cetz:0.3.4",
            "@preview/cetz:0.3.4.5",
            "@preview/-x:1.0.0",
        ] {
            assert!(Spec::parse(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn imports_are_found_in_text() {
        let text = "#import \"@preview/cetz:0.3.4\": canvas\n// see @preview/fletcher:0.5.1.\n@me";
        let found: Vec<String> = scan(text).iter().map(ToString::to_string).collect();
        assert_eq!(found, ["@preview/cetz:0.3.4", "@preview/fletcher:0.5.1"]);
    }

    #[test]
    fn unpacking_stays_inside_the_package() {
        let ok = archive(&[("typst.toml", b"[package]"), ("src/lib.typ", b"#let x = 1")]);
        let files = unpack(&ok).unwrap();
        assert_eq!(files.len(), 2);
        assert!(
            unpack(&archive(&[("lib.typ", b"")])).is_err(),
            "no typst.toml"
        );
        assert!(unpack(b"not gzip").is_err());
        // The tar crate refuses to write `..`, so build that header by hand.
        let mut header = tar::Header::new_gnu();
        header.set_size(1);
        header.set_mode(0o644);
        {
            let name = &mut header.as_gnu_mut().unwrap().name;
            name[..9].copy_from_slice(b"../escape");
        }
        header.set_cksum();
        let mut tar = tar::Builder::new(Vec::new());
        tar.append(&header, &b"x"[..]).unwrap();
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        gz.write_all(&tar.into_inner().unwrap()).unwrap();
        let err = unpack(&gz.finish().unwrap()).unwrap_err();
        assert!(err.contains("leaves the package"), "{err}");
    }

    #[test]
    fn a_package_of_too_many_files_is_refused() {
        let names: Vec<String> = (0..PACKAGE_FILES_MAX)
            .map(|i| format!("f{i}.typ"))
            .collect();
        let mut files: Vec<(&str, &[u8])> = vec![("typst.toml", b"")];
        files.extend(names.iter().map(|n| (n.as_str(), &b""[..])));
        let err = unpack(&archive(&files)).unwrap_err();
        assert!(err.contains("more than 4096 files"), "{err}");
        files.pop();
        assert_eq!(unpack(&archive(&files)).unwrap().len(), PACKAGE_FILES_MAX);
    }

    #[test]
    fn a_package_too_large_unpacked_is_refused() {
        let big = vec![b'a'; usize::try_from(PACKAGE_UNPACKED_MAX).unwrap() + 1];
        let err = unpack(&archive(&[("typst.toml", b""), ("big.txt", &big)])).unwrap_err();
        assert!(err.contains("more than"), "{err}");
    }
}
