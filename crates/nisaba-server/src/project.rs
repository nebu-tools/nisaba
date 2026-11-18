//! How a project lives inside its Loro document.
//!
//! One Loro document holds a whole project. Its `files` map is keyed by a
//! random file id; each entry is a map with a `path` and either a `text`
//! container (a source file) or a `hash` (an asset whose bytes are on disk).
//! Random ids mean two people who create a file at once get two entries,
//! never one entry that silently drops the other's text.

use loro::{
    ExportMode, LoroDoc, LoroMap, LoroText, LoroValue, TextDelta, ValueOrContainer, VersionVector,
};

use crate::api::{FileEntry, View};
use crate::limits::PATH_MAX;

pub const FILES: &str = "files";

/// A file to add to a project.
pub enum NewFile {
    Text { path: String, text: String },
    Asset { path: String, hash: String },
}

/// Reads every file from the document, sorted by path.
#[must_use]
pub fn files(doc: &LoroDoc) -> Vec<FileEntry> {
    files_in_view(doc, View::Written)
}

/// Reads every file from the document with its text in `view`: as written,
/// with suggestions applied, or as it was before them (DESIGN.md section 5).
#[must_use]
pub fn files_in_view(doc: &LoroDoc, view: View) -> Vec<FileEntry> {
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
            Some(ValueOrContainer::Container(loro::Container::Text(t))) => {
                Some(text_in_view(&t, view))
            }
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

/// The mark of a suggested insertion.
pub const INSERTED: &str = "ins";
/// The mark of a suggested deletion.
pub const DELETED: &str = "del";

/// One text in one view. "As written" is the text as it is. "With
/// suggestions applied" drops text marked `del`. "Original" drops text
/// marked `ins`. The client has the same function, and the fixture
/// `tests/fixtures/views.json` keeps the two equal.
#[must_use]
pub fn text_in_view(text: &LoroText, view: View) -> String {
    let drop = match view {
        View::Written => return text.to_string(),
        View::Applied => DELETED,
        View::Original => INSERTED,
    };
    let mut out = String::new();
    for delta in text.to_delta() {
        if let TextDelta::Insert { insert, attributes } = delta {
            let marked = attributes
                .as_ref()
                .and_then(|a| a.get(drop))
                .is_some_and(|v| !matches!(v, LoroValue::Null));
            if !marked {
                out.push_str(&insert);
            }
        }
    }
    out
}

/// Checks a file path of a project: relative, with `/` between its parts,
/// no empty part, no `.` or `..`, no backslash or control character, at
/// most [`PATH_MAX`] bytes. Returns it unchanged, or why it is refused.
pub fn check_path(path: &str) -> Result<String, String> {
    if path.is_empty() || path.len() > PATH_MAX {
        return Err(format!("a path has 1 to {PATH_MAX} bytes"));
    }
    if path.starts_with('/') {
        return Err("a path is relative to the project and does not start with /".into());
    }
    if path.chars().any(|c| c == '\\' || c.is_control()) {
        return Err("a path has no backslash and no control character".into());
    }
    if path
        .split('/')
        .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return Err("a path has no empty part, no . and no ..".into());
    }
    Ok(path.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_stay_inside_the_project() {
        for good in [
            "main.typ",
            "chapters/tides.typ",
            "figures/limpet 2.png",
            "a/b/c.bib",
        ] {
            assert_eq!(check_path(good).as_deref(), Ok(good));
        }
        let long = "x".repeat(PATH_MAX + 1);
        for bad in [
            "",
            "/etc/passwd",
            "../up.typ",
            "a/../b",
            "a//b",
            "./a",
            "a/",
            "a\\b",
            "a\u{0}b",
            long.as_str(),
        ] {
            assert!(check_path(bad).is_err(), "{bad:?}");
        }
    }

    /// The shared fixture: each case gives text in segments with their
    /// marks, and the text of each view.
    #[test]
    fn views_follow_the_shared_fixture() {
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("../tests/fixtures/views.json")).unwrap();
        for case in fixture["cases"].as_array().unwrap() {
            let doc = LoroDoc::new();
            let mut styles = loro::StyleConfigMap::new();
            styles.insert(
                INSERTED.into(),
                loro::StyleConfig {
                    expand: loro::ExpandType::After,
                },
            );
            styles.insert(
                DELETED.into(),
                loro::StyleConfig {
                    expand: loro::ExpandType::None,
                },
            );
            doc.config_text_style(styles);
            let text = doc.get_text("t");
            // The whole text first, then the marks: an `ins` mark grows at
            // its end, so text typed after it would join it.
            let mut marks = Vec::new();
            for segment in case["segments"].as_array().unwrap() {
                let start = text.len_unicode();
                text.insert(start, segment["text"].as_str().unwrap())
                    .unwrap();
                if let Some(mark) = segment["mark"].as_str() {
                    marks.push((start..text.len_unicode(), mark));
                }
            }
            let value = serde_json::json!({"id": "s1", "author": "u", "time": 0}).to_string();
            for (range, mark) in marks {
                text.mark(range, mark, value.clone()).unwrap();
            }
            doc.commit();
            let name = case["name"].as_str().unwrap();
            for (view, key) in [
                (View::Written, "written"),
                (View::Applied, "applied"),
                (View::Original, "original"),
            ] {
                assert_eq!(
                    text_in_view(&text, view),
                    case[key].as_str().unwrap(),
                    "{name}: {key}"
                );
            }
        }
    }
}
