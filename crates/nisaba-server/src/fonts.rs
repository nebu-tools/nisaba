//! The fonts the server holds: those embedded with the web client, and
//! those the administrator puts in the data directory's `fonts/`.
//!
//! At start the server reads every `.ttf`, `.otf`, `.ttc` and `.otc` file in
//! `fonts/`, names it by a hash of its bytes, and adds its faces to the
//! embedded catalogue. The browser loads the merged `fonts.json` and fetches
//! only the faces a compile asks for; an export gets every added font. A
//! file Typst cannot read as a font is left out, with a warning in the log.
//! Fonts added while the server runs count from its next start.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use nisaba_compile_core::FontEntry;
use sha2::{Digest, Sha256};

use crate::store::hex;

pub struct Fonts {
    catalogue: String,
    etag: String,
    /// The administrator's fonts, by their name in the catalogue.
    added: HashMap<String, Arc<Vec<u8>>>,
}

const EXTENSIONS: &[&str] = &["ttf", "otf", "ttc", "otc"];

impl Fonts {
    /// The embedded catalogue and every font file in `dir`.
    pub fn load(dir: &Path) -> Result<Self, String> {
        let mut entries = crate::web::embedded_fonts();
        let mut added = HashMap::new();
        let mut files: Vec<_> = match std::fs::read_dir(dir) {
            Ok(read) => read.flatten().map(|e| e.path()).collect(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(format!("cannot read {}: {e}", dir.display())),
        };
        files.sort();
        for path in files {
            let Some(ext) = path
                .extension()
                .and_then(|e| e.to_str())
                .map(str::to_ascii_lowercase)
                .filter(|e| EXTENSIONS.contains(&e.as_str()))
            else {
                continue;
            };
            let bytes =
                std::fs::read(&path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
            let name = format!("{}.{ext}", &hex(&Sha256::digest(&bytes))[..32]);
            let faces = FontEntry::read(&name, &bytes);
            if faces.is_empty() {
                tracing::warn!(file = %path.display(), "not a font Typst can read; left out");
                continue;
            }
            if added.insert(name, Arc::new(bytes)).is_none() {
                entries.extend(faces);
            }
        }
        let catalogue = serde_json::to_string(&entries).map_err(|e| e.to_string())?;
        let etag = format!("\"{}\"", &hex(&Sha256::digest(catalogue.as_bytes()))[..32]);
        Ok(Self {
            catalogue,
            etag,
            added,
        })
    }

    /// `fonts.json`.
    #[must_use]
    pub fn catalogue(&self) -> &str {
        &self.catalogue
    }

    #[must_use]
    pub fn catalogue_etag(&self) -> &str {
        &self.etag
    }

    /// One font file of the catalogue.
    #[must_use]
    pub fn file(&self, name: &str) -> Option<Vec<u8>> {
        if let Some(bytes) = self.added.get(name) {
            return Some(bytes.as_ref().clone());
        }
        crate::web::embedded_font(name).map(std::borrow::Cow::into_owned)
    }

    /// The administrator's fonts, for an export.
    #[must_use]
    pub fn extra(&self) -> Vec<Arc<Vec<u8>>> {
        let mut names: Vec<_> = self.added.keys().collect();
        names.sort();
        names
            .into_iter()
            .map(|n| Arc::clone(&self.added[n]))
            .collect()
    }
}
