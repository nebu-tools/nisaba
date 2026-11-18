//! Every route of the HTTP API, as data.
//!
//! [`ROUTES`] is what the API description is made from, and the router
//! registers its handlers by these entries ([`crate::http::router`]), so a
//! route cannot exist without being described. The integration test
//! `every_route_answers_as_described` walks this table against the real
//! binary.

use schemars::{JsonSchema, Schema, SchemaGenerator};

use super::types::{
    AccountChange, ApiError, AssetAdded, AssetQuery, Credentials, ErrorCode, ExportQuery,
    FileEntry, Health, Member, NewAccount, NewMember, NewProject, PasswordChange, Project,
    ProjectChange, RoleChange, Session, SetupRequest, User,
};

/// Who may call a route.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Auth {
    /// Anyone.
    Anyone,
    /// A signed-in person.
    Session,
    /// A member of the project in the path, in any role.
    Member,
    /// An owner or editor of the project in the path.
    Editor,
    /// An owner of the project in the path.
    Owner,
    /// A server administrator.
    Admin,
}

impl Auth {
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Anyone => "anyone",
            Self::Session => "session",
            Self::Member => "member",
            Self::Editor => "editor",
            Self::Owner => "owner",
            Self::Admin => "admin",
        }
    }
}

/// A named JSON type of the API.
#[derive(Clone, Copy)]
pub struct Type {
    pub name: &'static str,
    pub schema: fn(&mut SchemaGenerator) -> Schema,
}

impl std::fmt::Debug for Type {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name)
    }
}

fn schema_of<T: JsonSchema>(generator: &mut SchemaGenerator) -> Schema {
    generator.subschema_for::<T>()
}

macro_rules! ty {
    ($t:ident) => {
        Type {
            name: stringify!($t),
            schema: schema_of::<$t>,
        }
    };
}

/// What a request carries.
#[derive(Debug, Clone, Copy)]
pub enum Body {
    None,
    /// JSON of this type, at most `max` bytes.
    Json {
        ty: Type,
        max: usize,
    },
    /// Raw bytes, at most `max`.
    Bytes {
        max: usize,
    },
}

/// What a successful answer carries.
#[derive(Debug, Clone, Copy)]
pub enum Reply {
    /// No body.
    Empty,
    Json(Type),
    /// A JSON array of this type.
    List(Type),
    /// Bytes of this media type.
    Bytes(&'static str),
    /// A WebSocket upgrade.
    Upgrade,
}

/// One route.
#[derive(Debug, Clone, Copy)]
pub struct Route {
    /// A stable name, the `operationId` of the description.
    pub id: &'static str,
    pub method: &'static str,
    /// The path with `{param}` segments, as axum writes it.
    pub path: &'static str,
    pub summary: &'static str,
    pub auth: Auth,
    pub query: Option<Type>,
    pub body: Body,
    /// The success status and what it carries. Further successful answers
    /// are in `also`.
    pub ok: (u16, Reply),
    pub also: &'static [(u16, Reply)],
    /// The error codes this route sends beyond those its auth, body and
    /// method imply ([`Route::error_codes`]).
    pub errors: &'static [ErrorCode],
    /// A valid JSON body, for the description and the conformance test.
    pub example: Option<&'static str>,
}

use crate::limits::{MAX_ASSET, MAX_BODY, MAX_IMPORT};

impl Route {
    /// Whether this route changes state, so that it needs the Origin header.
    #[must_use]
    pub fn changes_state(&self) -> bool {
        !matches!(self.method, "GET" | "HEAD")
    }

    /// Every error code this route may answer with.
    #[must_use]
    pub fn error_codes(&self) -> Vec<ErrorCode> {
        let mut codes = vec![ErrorCode::Internal];
        if self.changes_state() {
            codes.push(ErrorCode::CrossOrigin);
        }
        if !matches!(self.body, Body::None) || self.query.is_some() || self.path.contains('{') {
            codes.push(ErrorCode::InvalidRequest);
        }
        if !matches!(self.body, Body::None) {
            codes.push(ErrorCode::PayloadTooLarge);
        }
        match self.auth {
            Auth::Anyone => {}
            Auth::Session => codes.push(ErrorCode::NotSignedIn),
            Auth::Member => codes.extend([ErrorCode::NotSignedIn, ErrorCode::NotFound]),
            Auth::Editor | Auth::Owner | Auth::Admin => codes.extend([
                ErrorCode::NotSignedIn,
                ErrorCode::NotFound,
                ErrorCode::Forbidden,
            ]),
        }
        for code in self.errors {
            if !codes.contains(code) {
                codes.push(*code);
            }
        }
        codes
    }
}

use Auth::{Admin, Anyone, Editor, Member as M, Owner};
use ErrorCode as E;

/// Every route of the server.
pub const ROUTES: &[Route] = &[
    Route {
        id: "health",
        method: "GET",
        path: "/api/health",
        summary: "Whether the server can read its database. Answers 503 with the same body \
                  while it cannot, and while it stops.",
        auth: Anyone,
        query: None,
        body: Body::None,
        ok: (200, Reply::Json(ty!(Health))),
        also: &[(503, Reply::Json(ty!(Health)))],
        errors: &[],
        example: None,
    },
    Route {
        id: "session",
        method: "GET",
        path: "/api/session",
        summary: "The signed-in person and whether setup is needed. Moves the session's end \
                  forward, at most once a day, and sends the cookie again when it does. The \
                  client calls this when it starts and at least once a day while it stays open.",
        auth: Anyone,
        query: None,
        body: Body::None,
        ok: (200, Reply::Json(ty!(Session))),
        also: &[],
        errors: &[],
        example: None,
    },
    Route {
        id: "setup",
        method: "POST",
        path: "/api/setup",
        summary: "First-run setup: makes the first account, the administrator, and signs it in.",
        auth: Anyone,
        query: None,
        body: Body::Json {
            ty: ty!(SetupRequest),
            max: MAX_BODY,
        },
        ok: (201, Reply::Json(ty!(User))),
        also: &[],
        errors: &[E::SetupDone, E::WrongSetupCode, E::InvalidField, E::Busy],
        example: Some(
            r#"{"email":"curator@example.org","password":"barnacle tide","name":"Curator"}"#,
        ),
    },
    Route {
        id: "login",
        method: "POST",
        path: "/api/login",
        summary: "Signs in and sets the session cookie.",
        auth: Anyone,
        query: None,
        body: Body::Json {
            ty: ty!(Credentials),
            max: MAX_BODY,
        },
        ok: (200, Reply::Json(ty!(User))),
        also: &[],
        errors: &[E::WrongCredentials, E::TooManyAttempts, E::Busy],
        example: Some(r#"{"email":"curator@example.org","password":"barnacle tide"}"#),
    },
    Route {
        id: "logout",
        method: "POST",
        path: "/api/logout",
        summary: "Ends this session and closes its sync sockets with 4401. Works without a \
                  session too.",
        auth: Anyone,
        query: None,
        body: Body::None,
        ok: (204, Reply::Empty),
        also: &[],
        errors: &[],
        example: None,
    },
    Route {
        id: "logout_everywhere",
        method: "POST",
        path: "/api/logout-everywhere",
        summary: "Ends every session of the account, this one too, and closes their sync \
                  sockets with 4401.",
        auth: Auth::Session,
        query: None,
        body: Body::None,
        ok: (204, Reply::Empty),
        also: &[],
        errors: &[],
        example: None,
    },
    Route {
        id: "account_update",
        method: "PATCH",
        path: "/api/account",
        summary: "Changes the display name.",
        auth: Auth::Session,
        query: None,
        body: Body::Json {
            ty: ty!(AccountChange),
            max: MAX_BODY,
        },
        ok: (200, Reply::Json(ty!(User))),
        also: &[],
        errors: &[E::InvalidField],
        example: Some(r#"{"name":"Tide curator"}"#),
    },
    Route {
        id: "password_change",
        method: "POST",
        path: "/api/account/password",
        summary: "Changes the password. Every other session of the account ends and its sync \
                  sockets close with 4401; this session stays. A wrong current password counts \
                  as a failed sign-in.",
        auth: Auth::Session,
        query: None,
        body: Body::Json {
            ty: ty!(PasswordChange),
            max: MAX_BODY,
        },
        ok: (204, Reply::Empty),
        also: &[],
        errors: &[
            E::WrongCredentials,
            E::TooManyAttempts,
            E::InvalidField,
            E::Busy,
        ],
        example: Some(r#"{"current_password":"barnacle tide","new_password":"barnacle tide"}"#),
    },
    Route {
        id: "users_list",
        method: "GET",
        path: "/api/admin/users",
        summary: "Every account, sorted by email.",
        auth: Admin,
        query: None,
        body: Body::None,
        ok: (200, Reply::List(ty!(User))),
        also: &[],
        errors: &[],
        example: None,
    },
    Route {
        id: "user_create",
        method: "POST",
        path: "/api/admin/users",
        summary: "Makes an account with a first password.",
        auth: Admin,
        query: None,
        body: Body::Json {
            ty: ty!(NewAccount),
            max: MAX_BODY,
        },
        ok: (201, Reply::Json(ty!(User))),
        also: &[],
        errors: &[E::EmailTaken, E::InvalidField, E::Busy],
        example: Some(
            r#"{"email":"warden@example.org","name":"Warden","password":"anemone pool"}"#,
        ),
    },
    Route {
        id: "user_delete",
        method: "DELETE",
        path: "/api/admin/users/{user}",
        summary: "Deletes an account, its sessions and its memberships. Its sync sockets close \
                  with 4401. Refused while it is the only owner of a project, or the last \
                  administrator.",
        auth: Admin,
        query: None,
        body: Body::None,
        ok: (204, Reply::Empty),
        also: &[],
        errors: &[E::SoleOwner, E::LastAdmin],
        example: None,
    },
    Route {
        id: "projects_list",
        method: "GET",
        path: "/api/projects",
        summary: "The projects the caller is a member of, most recently changed first.",
        auth: Auth::Session,
        query: None,
        body: Body::None,
        ok: (200, Reply::List(ty!(Project))),
        also: &[],
        errors: &[],
        example: None,
    },
    Route {
        id: "project_create",
        method: "POST",
        path: "/api/projects",
        summary: "Makes a project with the caller as its owner.",
        auth: Auth::Session,
        query: None,
        body: Body::Json {
            ty: ty!(NewProject),
            max: MAX_IMPORT,
        },
        ok: (201, Reply::Json(ty!(Project))),
        also: &[],
        errors: &[E::InvalidField, E::ProjectFull, E::StorageFull],
        example: Some(
            r#"{"name":"Rock pools","files":[{"path":"main.typ","text":"= Rock pools\n"}]}"#,
        ),
    },
    Route {
        id: "project_get",
        method: "GET",
        path: "/api/projects/{project}",
        summary: "One project.",
        auth: M,
        query: None,
        body: Body::None,
        ok: (200, Reply::Json(ty!(Project))),
        also: &[],
        errors: &[],
        example: None,
    },
    Route {
        id: "project_update",
        method: "PATCH",
        path: "/api/projects/{project}",
        summary: "Renames a project.",
        auth: Owner,
        query: None,
        body: Body::Json {
            ty: ty!(ProjectChange),
            max: MAX_BODY,
        },
        ok: (200, Reply::Json(ty!(Project))),
        also: &[],
        errors: &[E::InvalidField],
        example: Some(r#"{"name":"Rock pools of the north shore"}"#),
    },
    Route {
        id: "project_delete",
        method: "DELETE",
        path: "/api/projects/{project}",
        summary: "Deletes a project: its document, history, memberships and the asset files no \
                  other project holds. Its sync sockets close with 4004.",
        auth: Owner,
        query: None,
        body: Body::None,
        ok: (204, Reply::Empty),
        also: &[],
        errors: &[],
        example: None,
    },
    Route {
        id: "members_list",
        method: "GET",
        path: "/api/projects/{project}/members",
        summary: "The members of a project, sorted by name.",
        auth: M,
        query: None,
        body: Body::None,
        ok: (200, Reply::List(ty!(Member))),
        also: &[],
        errors: &[],
        example: None,
    },
    Route {
        id: "member_add",
        method: "POST",
        path: "/api/projects/{project}/members",
        summary: "Adds the account with this email to the project. An email with no account \
                  is an invalid field.",
        auth: Owner,
        query: None,
        body: Body::Json {
            ty: ty!(NewMember),
            max: MAX_BODY,
        },
        ok: (201, Reply::Json(ty!(Member))),
        also: &[],
        errors: &[E::AlreadyMember, E::InvalidField],
        example: Some(r#"{"email":"warden@example.org","role":"viewer"}"#),
    },
    Route {
        id: "member_update",
        method: "PATCH",
        path: "/api/projects/{project}/members/{user}",
        summary: "Changes a member's role. A member who can no longer write has their sync \
                  sockets closed with 4003; one who now may write can do so at once.",
        auth: Owner,
        query: None,
        body: Body::Json {
            ty: ty!(RoleChange),
            max: MAX_BODY,
        },
        ok: (200, Reply::Json(ty!(Member))),
        also: &[],
        errors: &[E::LastOwner],
        example: Some(r#"{"role":"editor"}"#),
    },
    Route {
        id: "member_remove",
        method: "DELETE",
        path: "/api/projects/{project}/members/{user}",
        summary: "Removes a member: an owner removes anyone, any member removes themself. Their \
                  sync sockets to the project close with 4004.",
        auth: M,
        query: None,
        body: Body::None,
        ok: (204, Reply::Empty),
        also: &[],
        errors: &[E::Forbidden, E::LastOwner],
        example: None,
    },
    Route {
        id: "files",
        method: "GET",
        path: "/api/projects/{project}/files",
        summary: "Every file of the project as the server holds it now, sorted by path.",
        auth: M,
        query: None,
        body: Body::None,
        ok: (200, Reply::List(ty!(FileEntry))),
        also: &[],
        errors: &[],
        example: None,
    },
    Route {
        id: "asset_upload",
        method: "POST",
        path: "/api/projects/{project}/assets",
        summary: "Stores the body as an asset of the project and adds a file entry for it at \
                  `path`.",
        auth: Editor,
        query: Some(ty!(AssetQuery)),
        body: Body::Bytes { max: MAX_ASSET },
        ok: (201, Reply::Json(ty!(AssetAdded))),
        also: &[],
        errors: &[E::InvalidField, E::ProjectFull, E::TryAgain, E::StorageFull],
        example: None,
    },
    Route {
        id: "asset_get",
        method: "GET",
        path: "/api/projects/{project}/assets/{hash}",
        summary: "The bytes of an asset the project holds. Takes one byte range (`Range: \
                  bytes=a-b`) and answers 206.",
        auth: M,
        query: None,
        body: Body::None,
        ok: (200, Reply::Bytes("application/octet-stream")),
        also: &[(206, Reply::Bytes("application/octet-stream"))],
        errors: &[E::RangeNotSatisfiable],
        example: None,
    },
    Route {
        id: "export",
        method: "GET",
        path: "/api/projects/{project}/export.pdf",
        summary: "Typesets the project as the server holds it now and answers the PDF.",
        auth: M,
        query: Some(ty!(ExportQuery)),
        body: Body::None,
        ok: (200, Reply::Bytes("application/pdf")),
        also: &[],
        errors: &[
            E::CompileFailed,
            E::ExportOutOfMemory,
            E::ExportOutOfTime,
            E::ExportBusy,
            E::ExportsOff,
            E::Stopping,
        ],
        example: None,
    },
    Route {
        id: "package",
        method: "GET",
        path: "/packages/{namespace}/{name}/{archive}",
        summary: "A Typst package as `.tar.gz`; `archive` is `{version}.tar.gz`. Served from the \
                  server's cache, which downloads a missing package from the registry when the \
                  server allows that.",
        auth: Auth::Session,
        query: None,
        body: Body::None,
        ok: (200, Reply::Bytes("application/gzip")),
        also: &[],
        errors: &[E::PackageUnavailable],
        example: None,
    },
    Route {
        id: "fonts",
        method: "GET",
        path: "/fonts/fonts.json",
        summary: "The catalogue of every font the server holds: the embedded ones and those in \
                  the data directory's `fonts/`. Revalidated on each visit.",
        auth: Anyone,
        query: None,
        body: Body::None,
        ok: (200, Reply::List(ty!(FontCatalogueEntry))),
        also: &[],
        errors: &[],
        example: None,
    },
    Route {
        id: "font_file",
        method: "GET",
        path: "/fonts/{file}",
        summary: "One font file of the catalogue, named by its hash; cached for a year.",
        auth: Anyone,
        query: None,
        body: Body::None,
        ok: (200, Reply::Bytes("font/ttf")),
        also: &[],
        errors: &[],
        example: None,
    },
    Route {
        id: "sync",
        method: "GET",
        path: "/sync/{project}",
        summary: "The sync socket (`x-nisaba-sync`). A handshake whose Origin is not the public \
                  address gets 403. Every other refusal is a close code after the upgrade.",
        auth: Anyone,
        query: None,
        body: Body::None,
        ok: (101, Reply::Upgrade),
        also: &[],
        errors: &[E::CrossOrigin],
        example: None,
    },
    Route {
        id: "web_client",
        method: "GET",
        path: "/",
        summary: "The web client. Every path no other route takes gets its `index.html`, except \
                  under `/assets/`, `/fonts/`, `/api/`, `/packages/` and `/sync/`.",
        auth: Anyone,
        query: None,
        body: Body::None,
        ok: (200, Reply::Bytes("text/html; charset=utf-8")),
        also: &[],
        errors: &[E::WebClientMissing],
        example: None,
    },
];

/// One entry of `fonts.json`.
#[derive(serde::Serialize, serde::Deserialize, JsonSchema)]
pub struct FontCatalogueEntry {
    /// The font file, relative to `/fonts/`.
    pub file: String,
    /// The face's index inside the file.
    pub index: u32,
    /// Typst's description of the face: family, variant, coverage.
    pub info: serde_json::Value,
}

/// Every named type the routes use, also those only inside other types,
/// so the description lists each one.
#[must_use]
pub fn extra_types() -> Vec<Type> {
    vec![ty!(ApiError)]
}

/// The route with this id. Panics on an unknown id: a programming error
/// the router's test catches.
#[must_use]
pub fn route(id: &str) -> &'static Route {
    ROUTES
        .iter()
        .find(|r| r.id == id)
        .unwrap_or_else(|| panic!("no route {id}"))
}
