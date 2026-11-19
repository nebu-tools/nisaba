//! The JSON bodies of the HTTP API.
//!
//! These types are the one source for both sides. The API description
//! (`api/openapi.json`) is made from their JSON Schemas, and the web client's
//! TypeScript types (`web/src/app/api.gen.ts`) are made from the description.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// `GET /api/health`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Health {
    /// `ok` while the server can read its database and is not stopping.
    pub status: HealthStatus,
    /// The server's version.
    pub version: String,
    /// Whether this binary holds the web client.
    pub web_client: bool,
    /// Whether exports run: in the sandbox, without it, or not at all.
    pub exports: ExportMode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum HealthStatus {
    Ok,
    Failing,
    Stopping,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ExportMode {
    Sandboxed,
    Unsandboxed,
    Off,
}

/// `GET /api/session`: what the client needs to pick its first screen.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Session {
    /// No account exists yet.
    pub setup_needed: bool,
    /// Setup asks for the code the server printed to its log.
    pub setup_code_needed: bool,
    /// The signed-in person, or null.
    pub user: Option<User>,
    /// When the session ends, in seconds since 1970, or null without one.
    /// The server moves it forward when this route is called, at most once
    /// a day, up to 90 days after sign-in.
    pub expires_at: Option<i64>,
}

/// A person with an account, without the password hash.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct User {
    pub id: String,
    pub email: String,
    pub name: String,
    /// Server administrators manage accounts. Being one gives no access to
    /// projects.
    pub is_admin: bool,
}

/// `POST /api/setup`: the first account, which is the administrator.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SetupRequest {
    pub email: String,
    /// At least 8 characters.
    pub password: String,
    /// The display name; the email when empty.
    #[serde(default)]
    pub name: String,
    /// The code from the server log; needed unless the server is reachable
    /// only from this machine.
    #[serde(default)]
    pub setup_code: Option<String>,
}

/// `POST /api/login`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Credentials {
    pub email: String,
    pub password: String,
}

/// `PATCH /api/account`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AccountChange {
    /// The new display name, 1 to 100 characters.
    pub name: String,
}

/// `POST /api/account/password`. Every other session of the account ends.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PasswordChange {
    pub current_password: String,
    /// At least 8 characters.
    pub new_password: String,
}

/// `POST /api/admin/users`: an account made by an administrator.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NewAccount {
    pub email: String,
    /// The display name; the email when empty.
    #[serde(default)]
    pub name: String,
    /// At least 8 characters. The person changes it after signing in.
    pub password: String,
    #[serde(default)]
    pub is_admin: bool,
}

/// A role in one project. DESIGN.md section 6 lists what each may do. A
/// commenter may not change the document until build step N4 adds the
/// check of suggestions, so today a commenter reads like a viewer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Owner,
    Editor,
    Commenter,
    Viewer,
}

impl Role {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Owner => "owner",
            Self::Editor => "editor",
            Self::Commenter => "commenter",
            Self::Viewer => "viewer",
        }
    }

    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "owner" => Some(Self::Owner),
            "editor" => Some(Self::Editor),
            "commenter" => Some(Self::Commenter),
            "viewer" => Some(Self::Viewer),
            _ => None,
        }
    }

    /// Whether this role may change the document's text and files.
    #[must_use]
    pub fn writes(self) -> bool {
        matches!(self, Self::Owner | Self::Editor)
    }

    /// The role as the sync room sees it.
    #[must_use]
    pub fn sync(self) -> nisaba_sync::Role {
        match self {
            Self::Owner | Self::Editor => nisaba_sync::Role::Editor,
            Self::Commenter => nisaba_sync::Role::Commenter,
            Self::Viewer => nisaba_sync::Role::Viewer,
        }
    }
}

/// A project as the project list shows it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Project {
    pub id: String,
    pub name: String,
    /// The caller's role in it.
    pub role: Role,
    /// Seconds since 1970.
    pub created_at: i64,
    /// The last change to its document or name, to the minute, in seconds
    /// since 1970. The list is sorted by it, newest first.
    pub updated_at: i64,
}

/// `POST /api/projects`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NewProject {
    /// 1 to 200 characters.
    pub name: String,
    /// The first files. Without any, the project gets a `main.typ`.
    #[serde(default)]
    pub files: Vec<ImportFile>,
}

/// A file in a new project: `text` for sources, `base64` for assets.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ImportFile {
    /// A relative path such as `chapters/tides.typ`.
    pub path: String,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub base64: Option<String>,
}

/// `PATCH /api/projects/{project}`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProjectChange {
    /// 1 to 200 characters.
    pub name: String,
}

/// One member of a project.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Member {
    pub user_id: String,
    pub email: String,
    pub name: String,
    pub role: Role,
}

/// `POST /api/projects/{project}/members`: adds the account with this
/// email.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NewMember {
    pub email: String,
    pub role: Role,
}

/// `PATCH /api/projects/{project}/members/{user}`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RoleChange {
    pub role: Role,
}

/// One file of `GET /api/projects/{project}/files`: `text` for sources,
/// `hash` for assets.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct FileEntry {
    /// The file's id in the document; random, never reused.
    pub id: String,
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub text: Option<String>,
    /// The SHA-256 of an asset's bytes, in hex.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub hash: Option<String>,
}

/// The content hash of an uploaded asset.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AssetAdded {
    pub hash: String,
}

/// The query of `POST /api/projects/{project}/assets`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AssetQuery {
    /// Where the file goes in the project, such as `figures/limpet.png`.
    pub path: String,
}

/// The query of `GET /api/projects/{project}/export.pdf`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ExportQuery {
    /// The caller's local date, `YYYY-MM-DD`. Used when it lies within one
    /// day of the server's UTC date; otherwise the UTC date is used.
    #[serde(default)]
    pub today: Option<String>,
    /// Which text of tracked changes to typeset. Default `written`.
    #[serde(default)]
    pub view: Option<View>,
    /// Default `pdf`.
    #[serde(default)]
    pub standard: Option<Standard>,
}

/// The text of a document with tracked changes (DESIGN.md section 5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum View {
    /// The text as it is, with suggested insertions and deletions both present.
    #[default]
    Written,
    /// Suggested deletions removed, suggested insertions kept.
    Applied,
    /// Suggested insertions removed, suggested deletions kept.
    Original,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum Standard {
    /// Typst's normal PDF.
    #[default]
    Pdf,
    /// PDF/A-2b, for archiving.
    #[serde(rename = "pdf-a-2b")]
    PdfA2b,
}

/// A compile problem in an export that failed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Problem {
    /// `error` or `warning`.
    pub severity: String,
    pub message: String,
    /// The project file the problem points into, when Typst named one. A
    /// file of a package is written `@namespace/name:version/path`.
    pub path: Option<String>,
    /// Start of the range, as a UTF-16 offset.
    pub start: Option<u32>,
    /// End of the range, as a UTF-16 offset.
    pub end: Option<u32>,
}

/// Every error the API answers has this shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ApiError {
    /// Stable: the client may branch on it.
    pub code: ErrorCode,
    /// A sentence a person can read. It may change between versions.
    pub message: String,
    /// The request field the error is about, when there is one.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub field: Option<String>,
    /// When to try again, in seconds; also sent as `Retry-After`.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub retry_after: Option<u64>,
    /// The compile problems of a failed export.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub problems: Option<Vec<Problem>>,
}

/// The stable error codes. `ERROR_STATUS` gives the HTTP status of each,
/// and the API description what each means.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    InvalidRequest,
    InvalidField,
    NotSignedIn,
    WrongCredentials,
    WrongSetupCode,
    CrossOrigin,
    Forbidden,
    NotFound,
    MethodNotAllowed,
    SetupDone,
    EmailTaken,
    AlreadyMember,
    LastOwner,
    SoleOwner,
    LastAdmin,
    PayloadTooLarge,
    ProjectFull,
    RangeNotSatisfiable,
    CompileFailed,
    ExportOutOfMemory,
    ExportOutOfTime,
    TooManyAttempts,
    Internal,
    PackageUnavailable,
    Busy,
    TryAgain,
    ExportBusy,
    ExportsOff,
    Stopping,
    WebClientMissing,
    StorageFull,
}

impl ErrorCode {
    pub const ALL: &[Self] = &[
        Self::InvalidRequest,
        Self::InvalidField,
        Self::NotSignedIn,
        Self::WrongCredentials,
        Self::WrongSetupCode,
        Self::CrossOrigin,
        Self::Forbidden,
        Self::NotFound,
        Self::MethodNotAllowed,
        Self::SetupDone,
        Self::EmailTaken,
        Self::AlreadyMember,
        Self::LastOwner,
        Self::SoleOwner,
        Self::LastAdmin,
        Self::PayloadTooLarge,
        Self::ProjectFull,
        Self::RangeNotSatisfiable,
        Self::CompileFailed,
        Self::ExportOutOfMemory,
        Self::ExportOutOfTime,
        Self::TooManyAttempts,
        Self::Internal,
        Self::PackageUnavailable,
        Self::Busy,
        Self::TryAgain,
        Self::ExportBusy,
        Self::ExportsOff,
        Self::Stopping,
        Self::WebClientMissing,
        Self::StorageFull,
    ];

    /// The code as the JSON writes it.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::InvalidRequest => "invalid_request",
            Self::InvalidField => "invalid_field",
            Self::NotSignedIn => "not_signed_in",
            Self::WrongCredentials => "wrong_credentials",
            Self::WrongSetupCode => "wrong_setup_code",
            Self::CrossOrigin => "cross_origin",
            Self::Forbidden => "forbidden",
            Self::NotFound => "not_found",
            Self::MethodNotAllowed => "method_not_allowed",
            Self::SetupDone => "setup_done",
            Self::EmailTaken => "email_taken",
            Self::AlreadyMember => "already_member",
            Self::LastOwner => "last_owner",
            Self::SoleOwner => "sole_owner",
            Self::LastAdmin => "last_admin",
            Self::PayloadTooLarge => "payload_too_large",
            Self::ProjectFull => "project_full",
            Self::RangeNotSatisfiable => "range_not_satisfiable",
            Self::CompileFailed => "compile_failed",
            Self::ExportOutOfMemory => "export_out_of_memory",
            Self::ExportOutOfTime => "export_out_of_time",
            Self::TooManyAttempts => "too_many_attempts",
            Self::Internal => "internal",
            Self::PackageUnavailable => "package_unavailable",
            Self::Busy => "busy",
            Self::TryAgain => "try_again",
            Self::ExportBusy => "export_busy",
            Self::ExportsOff => "exports_off",
            Self::Stopping => "stopping",
            Self::WebClientMissing => "web_client_missing",
            Self::StorageFull => "storage_full",
        }
    }

    /// The HTTP status this code is sent with.
    #[must_use]
    pub fn status(self) -> u16 {
        match self {
            Self::InvalidRequest | Self::InvalidField => 400,
            Self::NotSignedIn | Self::WrongCredentials => 401,
            Self::WrongSetupCode | Self::CrossOrigin | Self::Forbidden => 403,
            Self::NotFound => 404,
            Self::MethodNotAllowed => 405,
            Self::SetupDone
            | Self::EmailTaken
            | Self::AlreadyMember
            | Self::LastOwner
            | Self::SoleOwner
            | Self::LastAdmin => 409,
            Self::PayloadTooLarge | Self::ProjectFull => 413,
            Self::RangeNotSatisfiable => 416,
            Self::CompileFailed | Self::ExportOutOfMemory | Self::ExportOutOfTime => 422,
            Self::TooManyAttempts => 429,
            Self::Internal => 500,
            Self::PackageUnavailable => 502,
            Self::Busy
            | Self::TryAgain
            | Self::ExportBusy
            | Self::ExportsOff
            | Self::Stopping
            | Self::WebClientMissing => 503,
            Self::StorageFull => 507,
        }
    }

    /// What the code means, for the API description.
    #[must_use]
    pub fn meaning(self) -> &'static str {
        match self {
            Self::InvalidRequest => {
                "The request could not be read: not JSON, a missing or unknown field, or a bad \
                 query or path."
            }
            Self::InvalidField => {
                "A field has a value the server does not accept; `field` names it."
            }
            Self::NotSignedIn => "No valid session: sign in first.",
            Self::WrongCredentials => "Wrong email or password, or wrong current password.",
            Self::WrongSetupCode => "The setup code is missing or wrong.",
            Self::CrossOrigin => {
                "The request changes state and its Origin header is not this server's public \
                 address."
            }
            Self::Forbidden => "The caller's role or account does not allow this.",
            Self::NotFound => {
                "No such thing, or the caller may not see it. A project the caller is not a \
                 member of is not found."
            }
            Self::MethodNotAllowed => "The path exists, but not with this method.",
            Self::SetupDone => "The server already has an administrator.",
            Self::EmailTaken => "Another account has this email.",
            Self::AlreadyMember => "The account is already a member of the project.",
            Self::LastOwner => "The project would have no owner left.",
            Self::SoleOwner => {
                "The account is the only owner of a project; delete the projects or give them \
                 another owner first."
            }
            Self::LastAdmin => "The server would have no administrator left.",
            Self::PayloadTooLarge => "The request body is larger than this route takes.",
            Self::ProjectFull => "The project is at its 32 MiB limit for text and history.",
            Self::RangeNotSatisfiable => "The byte range lies outside the file.",
            Self::CompileFailed => "The document has errors; `problems` lists them.",
            Self::ExportOutOfMemory => "The export needed more memory than its limit.",
            Self::ExportOutOfTime => "The export took longer than its limit.",
            Self::TooManyAttempts => "Too many failed sign-ins for this email; wait `retry_after`.",
            Self::Internal => "The server failed. The log says why.",
            Self::PackageUnavailable => {
                "The package registry could not be reached or sent something unusable."
            }
            Self::Busy => "All password workers are busy; try again after `retry_after`.",
            Self::TryAgain => {
                "A large update to this project is on its way and holds the space; try again \
                 in a minute."
            }
            Self::ExportBusy => "All export slots and the queue are full; try again in a minute.",
            Self::ExportsOff => {
                "This server cannot sandbox exports and was not allowed to run them without."
            }
            Self::Stopping => "The server is stopping; try again in a minute.",
            Self::WebClientMissing => "This server was built without the web client.",
            Self::StorageFull => "The server's disk is full; nothing was written.",
        }
    }
}
