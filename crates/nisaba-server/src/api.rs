//! The JSON bodies of the HTTP API.
//!
//! These types are the one source for both sides. The web client imports
//! TypeScript declarations generated from them (`web/src/app/api.gen.ts`).
//! The test below fails when that file is stale; run it with
//! `NISABA_WRITE_API=1` to write the file.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// `GET /api/session`.
#[derive(Debug, Serialize, TS)]
pub struct Session {
    pub setup_needed: bool,
    pub user: Option<User>,
}

/// A signed-in user, without the password hash.
#[derive(Clone, Debug, Serialize, TS)]
pub struct User {
    pub id: String,
    pub email: String,
    pub name: String,
}

/// `POST /api/setup` and `POST /api/login`. Login ignores `name`.
#[derive(Debug, Deserialize, TS)]
pub struct Credentials {
    pub email: String,
    pub password: String,
    #[serde(default)]
    pub name: String,
}

/// A project as the project list shows it.
#[derive(Clone, Debug, Serialize, TS)]
pub struct ProjectRow {
    pub id: String,
    pub name: String,
    pub role: String,
}

/// `POST /api/projects`.
#[derive(Debug, Deserialize, TS)]
pub struct NewProject {
    pub name: String,
    #[serde(default)]
    pub files: Vec<ImportFile>,
}

/// A file in a project import: `text` for sources, `base64` for assets.
#[derive(Debug, Deserialize, TS)]
pub struct ImportFile {
    pub path: String,
    #[ts(optional)]
    pub text: Option<String>,
    #[ts(optional)]
    pub base64: Option<String>,
}

/// The id of a new project.
#[derive(Debug, Serialize, TS)]
pub struct Created {
    pub id: String,
}

/// One file of `GET /api/projects/{id}/files`: `text` for sources, `hash`
/// for assets.
#[derive(Debug, Clone, Serialize, TS)]
pub struct FileEntry {
    pub id: String,
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub hash: Option<String>,
}

/// The content hash of an uploaded asset.
#[derive(Debug, Serialize, TS)]
pub struct AssetAdded {
    pub hash: String,
}

/// The TypeScript declarations of every type above.
#[must_use]
pub fn typescript() -> String {
    let cfg = ts_rs::Config::default();
    let decls = [
        Session::decl(&cfg),
        User::decl(&cfg),
        Credentials::decl(&cfg),
        ProjectRow::decl(&cfg),
        NewProject::decl(&cfg),
        ImportFile::decl(&cfg),
        Created::decl(&cfg),
        FileEntry::decl(&cfg),
        AssetAdded::decl(&cfg),
    ];
    let mut out = String::from(
        "// Generated from crates/nisaba-server/src/api.rs. Do not edit.\n\
         // Regenerate: NISABA_WRITE_API=1 cargo test -p nisaba-server api\n",
    );
    for decl in decls {
        out.push_str("\nexport ");
        out.push_str(&decl);
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn api_types_are_current() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../web/src/app/api.gen.ts");
        let want = super::typescript();
        if std::env::var_os("NISABA_WRITE_API").is_some() {
            std::fs::write(path, &want).unwrap();
            return;
        }
        let have = std::fs::read_to_string(path).unwrap_or_default();
        assert!(
            have == want,
            "web/src/app/api.gen.ts is stale; run NISABA_WRITE_API=1 cargo test -p nisaba-server api"
        );
    }
}
