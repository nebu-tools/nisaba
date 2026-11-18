//! How a project lives inside its Loro document.
//!
//! One Loro document holds a whole project. Its `files` map is keyed by a
//! random file id; each entry is a map with a `path` and either a `text`
//! container (a source file) or a `hash` (an asset whose bytes are on disk).
//! Random ids mean two people who create a file at once get two entries,
//! never one entry that silently drops the other's text.

use loro::{ExportMode, LoroDoc, LoroMap, LoroText, LoroValue, ValueOrContainer, VersionVector};

pub const FILES: &str = "files";

/// One file as the server reads it from the Loro document.
#[derive(Debug, Clone, serde::Serialize)]
pub struct FileEntry {
    pub id: String,
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hash: Option<String>,
}

/// A file to add to a project.
pub enum NewFile {
    Text { path: String, text: String },
    Asset { path: String, hash: String },
}

/// Reads every file from the document, sorted by path.
#[must_use]
pub fn files(doc: &LoroDoc) -> Vec<FileEntry> {
    let mut out = Vec::new();
    doc.get_map(FILES).for_each(|id, value| {
        let ValueOrContainer::Container(loro::Container::Map(entry)) = value else {
            return;
        };
        let path = match entry.get("path") {
            Some(ValueOrContainer::Value(LoroValue::String(p))) => p.to_string(),
            _ => return,
        };
        let text = match entry.get("text") {
            Some(ValueOrContainer::Container(loro::Container::Text(t))) => Some(t.to_string()),
            _ => None,
        };
        let hash = match entry.get("hash") {
            Some(ValueOrContainer::Value(LoroValue::String(h))) => Some(h.to_string()),
            _ => None,
        };
        out.push(FileEntry {
            id: id.to_string(),
            path,
            text,
            hash,
        });
    });
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

/// Builds a Loro update that adds `new` files on top of the document state in
/// `snapshot` (empty for a new project). The update is what the server feeds
/// through the room, exactly like an update from a browser.
pub fn add_files(snapshot: Option<&[u8]>, new: Vec<NewFile>) -> Result<Vec<u8>, String> {
    let doc = LoroDoc::new();
    if let Some(bytes) = snapshot {
        doc.import(bytes).map_err(|e| e.to_string())?;
    }
    doc.set_peer_id(rand::random::<u64>() | 1)
        .map_err(|e| e.to_string())?;
    let before: VersionVector = doc.oplog_vv();
    let files = doc.get_map(FILES);
    for file in new {
        let id = format!("{:016x}", rand::random::<u64>());
        let entry = files
            .insert_container(&id, LoroMap::new())
            .map_err(|e| e.to_string())?;
        match file {
            NewFile::Text { path, text } => {
                entry.insert("path", path).map_err(|e| e.to_string())?;
                let body = entry
                    .insert_container("text", LoroText::new())
                    .map_err(|e| e.to_string())?;
                body.insert(0, &text).map_err(|e| e.to_string())?;
            }
            NewFile::Asset { path, hash } => {
                entry.insert("path", path).map_err(|e| e.to_string())?;
                entry.insert("hash", hash).map_err(|e| e.to_string())?;
            }
        }
    }
    doc.commit();
    doc.export(ExportMode::updates(&before))
        .map_err(|e| e.to_string())
}
