//! HTTP routes: first-run setup, sessions, projects, assets, reads, export,
//! the sync upgrade, and the embedded web client.
//!
//! There is no route that writes a document body. Text changes only through
//! the sync endpoint, and every read here takes text from the same room the
//! sync endpoint writes to.
//!
//! Every route under `/api/projects/{id}` and `/sync/{id}` first checks that
//! the caller is a member of project `id`, and an asset is served or
//! exported only when that project holds it.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use axum::Router;
use axum::body::Bytes;
use axum::extract::ws::{CloseFrame, Message};
use axum::extract::{DefaultBodyLimit, Path, Query, Request, State, WebSocketUpgrade};
use axum::http::{HeaderMap, Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use base64::Engine as _;
use nisaba_compile_core::FileContents;
use nisaba_sync::close;
use serde::Deserialize;

use crate::api::{AssetAdded, Created, Credentials, NewProject, Session, SetupRequest, User};
use crate::config::PublicAddress;
use crate::export::{ExportError, Exporter};
use crate::passwords::{Busy, Passwords};
use crate::project::{self, NewFile};
use crate::store::{Setup, Store};
use crate::sync::Rooms;

const SESSION_TTL: i64 = 30 * 24 * 3600;
/// Password hashes computed at once. Each takes about 19 MiB and a core.
const PASSWORD_WORKERS: usize = 4;
/// Password requests that may wait for a worker.
const PASSWORD_QUEUE: usize = 16;
/// Open sync sockets per account.
const SOCKETS_PER_ACCOUNT: usize = 32;
/// Request bodies: most routes, an asset upload, a project import.
const MAX_BODY: usize = 64 * 1024;
const MAX_ASSET: usize = 32 << 20;
const MAX_IMPORT: usize = 64 << 20;

/// The server state every route shares.
#[derive(Clone)]
pub struct App {
    store: Store,
    rooms: Rooms,
    public: Arc<PublicAddress>,
    exporter: Exporter,
    passwords: Arc<Passwords>,
    sockets: Arc<Mutex<HashMap<String, usize>>>,
}

impl App {
    #[must_use]
    /// `transfer_bytes` is the memory all unfinished multi-part updates may
    /// hold together.
    pub fn new(
        store: Store,
        public: PublicAddress,
        exporter: Exporter,
        transfer_bytes: usize,
    ) -> Self {
        Self {
            rooms: Rooms::new(store.clone(), transfer_bytes),
            store,
            public: Arc::new(public),
            exporter,
            passwords: Arc::new(Passwords::new(PASSWORD_WORKERS, PASSWORD_QUEUE)),
            sockets: Arc::default(),
        }
    }

    #[must_use]
    pub fn rooms(&self) -> &Rooms {
        &self.rooms
    }

    /// The password work and sign-in limits.
    #[must_use]
    pub fn passwords(&self) -> &Passwords {
        &self.passwords
    }
}

/// Counts one sync socket of an account while it lives.
struct SocketSlot {
    sockets: Arc<Mutex<HashMap<String, usize>>>,
    user: String,
}

impl SocketSlot {
    fn take(app: &App, user: &str) -> Option<Self> {
        let mut sockets = app.sockets.lock().expect("sockets lock");
        let n = sockets.entry(user.to_owned()).or_default();
        if *n >= SOCKETS_PER_ACCOUNT {
            return None;
        }
        *n += 1;
        Some(Self {
            sockets: Arc::clone(&app.sockets),
            user: user.to_owned(),
        })
    }
}

impl Drop for SocketSlot {
    fn drop(&mut self) {
        let mut sockets = self.sockets.lock().expect("sockets lock");
        if let Some(n) = sockets.get_mut(&self.user) {
            *n -= 1;
            if *n == 0 {
                sockets.remove(&self.user);
            }
        }
    }
}

type ApiResult = Result<Response, (StatusCode, String)>;

fn fail(status: StatusCode, msg: impl Into<String>) -> (StatusCode, String) {
    (status, msg.into())
}

fn internal(e: impl std::fmt::Display) -> (StatusCode, String) {
    fail(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}

/// A room refused an update the server built: 413 when the project is at its
/// size limit, 500 otherwise.
fn refused_update(e: nisaba_sync::SyncError) -> (StatusCode, String) {
    match e {
        nisaba_sync::SyncError::TooLarge(_) | nisaba_sync::SyncError::ProjectFull { .. } => fail(
            StatusCode::PAYLOAD_TOO_LARGE,
            "the project is at its size limit for text and history",
        ),
        e => internal(e),
    }
}

pub fn router(app: App) -> Router {
    Router::new()
        .route("/api/session", get(session))
        .route("/api/setup", post(setup))
        .route("/api/login", post(login))
        .route("/api/logout", post(logout))
        .route(
            "/api/projects",
            get(list_projects)
                .post(create_project)
                .layer(DefaultBodyLimit::max(MAX_IMPORT)),
        )
        .route("/api/projects/{id}/files", get(read_files))
        .route(
            "/api/projects/{id}/assets",
            post(upload_asset).layer(DefaultBodyLimit::max(MAX_ASSET)),
        )
        .route("/api/projects/{id}/assets/{hash}", get(asset))
        .route("/api/projects/{id}/export.pdf", get(export_pdf))
        .route("/sync/{id}", get(sync_upgrade))
        .fallback(get(crate::web::static_file))
        .layer(DefaultBodyLimit::max(MAX_BODY))
        .layer(middleware::from_fn_with_state(app.clone(), same_origin))
        .with_state(app)
}

/// Refuses every request that can change state unless it comes from a page
/// of this server. The session cookie is `SameSite=Lax`, which already keeps
/// it off cross-site requests other than top-level navigation; this check
/// does not depend on the browser's cookie rules.
async fn same_origin(State(app): State<App>, request: Request, next: Next) -> Response {
    let safe = matches!(*request.method(), Method::GET | Method::HEAD);
    if !safe && !app.public.same_origin(request.headers()) {
        return (StatusCode::FORBIDDEN, "cross-site request refused").into_response();
    }
    next.run(request).await
}

fn token(app: &App, headers: &HeaderMap) -> Option<String> {
    let name = app.public.cookie_name();
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|kv| kv.trim().split_once('='))
        .find(|(k, _)| *k == name)
        .map(|(_, v)| v.to_owned())
}

fn current_user(app: &App, headers: &HeaderMap) -> Result<User, (StatusCode, String)> {
    let token =
        token(app, headers).ok_or_else(|| fail(StatusCode::UNAUTHORIZED, "sign in first"))?;
    app.store
        .session_user(&token)
        .map_err(internal)?
        .ok_or_else(|| fail(StatusCode::UNAUTHORIZED, "session expired"))
}

/// The caller and their role in `project`. Not being a member and the
/// project not existing give the same 404.
fn member(
    app: &App,
    headers: &HeaderMap,
    project: &str,
) -> Result<(User, String), (StatusCode, String)> {
    let user = current_user(app, headers)?;
    let role = app
        .store
        .role(project, &user.id)
        .map_err(internal)?
        .ok_or_else(|| fail(StatusCode::NOT_FOUND, "no such project"))?;
    Ok((user, role))
}

fn editor(role: &str) -> Result<(), (StatusCode, String)> {
    if role == "owner" || role == "editor" {
        Ok(())
    } else {
        Err(fail(StatusCode::FORBIDDEN, "only editors can add files"))
    }
}

fn new_session(app: &App, user: &User) -> ApiResult {
    let mut raw = [0u8; 32];
    rand::Rng::fill(&mut rand::thread_rng(), &mut raw);
    let token = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(raw);
    app.store
        .create_session(&token, &user.id, SESSION_TTL)
        .map_err(internal)?;
    let cookie = app.public.session_cookie(&token, SESSION_TTL);
    Ok(([(header::SET_COOKIE, cookie)], axum::Json(user.clone())).into_response())
}

/// An Argon2id hash of `password` in PHC string form.
pub fn hash_password(password: &str) -> Result<String, String> {
    let salt = SaltString::generate(&mut argon2::password_hash::rand_core::OsRng);
    argon2::Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| e.to_string())
}

/// Checked against when an email has no account, so a wrong email takes as
/// long as a wrong password.
static DUMMY_HASH: LazyLock<String> =
    LazyLock::new(|| hash_password("no account has this password").expect("hashing works"));

async fn session(State(app): State<App>, headers: HeaderMap) -> ApiResult {
    let setup_needed = app.store.user_count().map_err(internal)? == 0;
    let setup_code_needed = setup_needed && app.store.setup_code_needed().map_err(internal)?;
    let user = current_user(&app, &headers).ok();
    Ok(axum::Json(Session {
        setup_needed,
        setup_code_needed,
        user,
    })
    .into_response())
}

/// A refusal that tells the client when to try again.
fn try_later(status: StatusCode, wait: Duration, msg: &str) -> Response {
    let secs = wait.as_secs_f64().ceil().max(1.0);
    (
        status,
        [(header::RETRY_AFTER, format!("{secs:.0}"))],
        msg.to_owned(),
    )
        .into_response()
}

fn busy(Busy: Busy) -> Response {
    try_later(
        StatusCode::SERVICE_UNAVAILABLE,
        Duration::from_secs(1),
        "the server is busy; try again in a moment",
    )
}

fn setup_refusal(setup: &Setup) -> (StatusCode, String) {
    match setup {
        Setup::Created => internal("setup worked"),
        Setup::Taken => fail(StatusCode::CONFLICT, "this server already has an owner"),
        Setup::WrongCode => fail(
            StatusCode::FORBIDDEN,
            "wrong setup code; the server printed it to its log when it started",
        ),
    }
}

/// First-run setup. The owner and the code are checked before the password
/// is hashed, so a server that has an owner does no password work here, and
/// checked again in the transaction that creates the account.
async fn setup(State(app): State<App>, axum::Json(body): axum::Json<SetupRequest>) -> ApiResult {
    if let Some(refused) = app
        .store
        .setup_refused(body.setup_code.as_deref())
        .map_err(internal)?
    {
        return Err(setup_refusal(&refused));
    }
    if body.password.len() < 8 {
        return Err(fail(StatusCode::BAD_REQUEST, "use at least 8 characters"));
    }
    let password = body.password.clone();
    let hash = match app.passwords.run(move || hash_password(&password)).await {
        Ok(hash) => hash.map_err(internal)?,
        Err(b) => return Ok(busy(b)),
    };
    let email = body.email.trim().to_lowercase();
    let user = User {
        id: format!("{:016x}", rand::random::<u64>()),
        name: if body.name.trim().is_empty() {
            email.clone()
        } else {
            body.name.trim().to_owned()
        },
        email,
    };
    match app
        .store
        .create_owner(&user, &hash, body.setup_code.as_deref())
        .map_err(internal)?
    {
        Setup::Created => new_session(&app, &user),
        refused => Err(setup_refusal(&refused)),
    }
}

/// Sign-in. An email that failed too often is refused with 429 before any
/// password work; see [`crate::passwords`].
async fn login(State(app): State<App>, axum::Json(body): axum::Json<Credentials>) -> ApiResult {
    let email = body.email.trim().to_lowercase();
    if let Err(wait) = app.passwords.check(&email, Instant::now()) {
        return Ok(try_later(
            StatusCode::TOO_MANY_REQUESTS,
            wait,
            "too many failed sign-ins for this email; wait and try again",
        ));
    }
    let found = app.store.user_by_email(&email).map_err(internal)?;
    let hash = found
        .as_ref()
        .map_or_else(|| DUMMY_HASH.clone(), |(_, hash)| hash.clone());
    let password = body.password;
    let matches = app
        .passwords
        .run(move || {
            PasswordHash::new(&hash).is_ok_and(|parsed| {
                argon2::Argon2::default()
                    .verify_password(password.as_bytes(), &parsed)
                    .is_ok()
            })
        })
        .await;
    match (found, matches) {
        (_, Err(b)) => Ok(busy(b)),
        (Some((user, _)), Ok(true)) => {
            app.passwords.succeeded(&email);
            new_session(&app, &user)
        }
        _ => {
            app.passwords.failed(&email, Instant::now());
            Err(fail(StatusCode::UNAUTHORIZED, "wrong email or password"))
        }
    }
}

async fn logout(State(app): State<App>, headers: HeaderMap) -> ApiResult {
    if let Some(token) = token(&app, &headers) {
        app.store.delete_session(&token).map_err(internal)?;
    }
    let cookie = app.public.session_cookie("", 0);
    Ok(([(header::SET_COOKIE, cookie)], StatusCode::NO_CONTENT).into_response())
}

async fn list_projects(State(app): State<App>, headers: HeaderMap) -> ApiResult {
    let user = current_user(&app, &headers)?;
    Ok(axum::Json(app.store.projects_for(&user.id).map_err(internal)?).into_response())
}

const STARTER: &str = "= Untitled\n\nStart writing here.\n";

async fn create_project(
    State(app): State<App>,
    headers: HeaderMap,
    axum::Json(body): axum::Json<NewProject>,
) -> ApiResult {
    let user = current_user(&app, &headers)?;
    let id = format!("{:032x}", rand::random::<u128>());
    let mut assets = Vec::new();
    for file in &body.files {
        if file.text.is_none()
            && let Some(b64) = &file.base64
        {
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(b64)
                .map_err(|e| fail(StatusCode::BAD_REQUEST, e.to_string()))?;
            assets.push((file.path.clone(), bytes));
        }
    }
    app.store
        .create_project(&id, body.name.trim(), &user.id)
        .map_err(internal)?;
    let mut files = Vec::new();
    for file in body.files {
        if let Some(text) = file.text {
            files.push(NewFile::Text {
                path: file.path,
                text,
            });
        }
    }
    for (path, bytes) in assets {
        let hash = app.store.put_asset(&id, &bytes).map_err(internal)?;
        files.push(NewFile::Asset { path, hash });
    }
    if files.is_empty() {
        files.push(NewFile::Text {
            path: "main.typ".into(),
            text: STARTER.into(),
        });
    }
    let update = project::add_files(None, files).map_err(internal)?;
    let room = app.rooms.get(&id).await.map_err(internal)?;
    room.apply(&update).await.map_err(refused_update)?;
    Ok((StatusCode::CREATED, axum::Json(Created { id })).into_response())
}

/// Every file of the project, read from the live room.
async fn read_files(
    State(app): State<App>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> ApiResult {
    member(&app, &headers, &id)?;
    let room = app.rooms.get(&id).await.map_err(internal)?;
    Ok(axum::Json(room.read(project::files)).into_response())
}

#[derive(Deserialize)]
struct AssetQuery {
    path: String,
}

async fn upload_asset(
    State(app): State<App>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(q): Query<AssetQuery>,
    body: Bytes,
) -> ApiResult {
    let (_, role) = member(&app, &headers, &id)?;
    editor(&role)?;
    let hash = app.store.put_asset(&id, &body).map_err(internal)?;
    let room = app.rooms.get(&id).await.map_err(internal)?;
    let snapshot = room.snapshot().map_err(internal)?;
    let update = project::add_files(
        Some(&snapshot),
        vec![NewFile::Asset {
            path: q.path,
            hash: hash.clone(),
        }],
    )
    .map_err(internal)?;
    room.apply(&update).await.map_err(refused_update)?;
    Ok(axum::Json(AssetAdded { hash }).into_response())
}

/// An asset of a project. The bytes are served as an opaque download in a
/// sandbox, so an uploaded HTML or SVG file cannot run as a page of this
/// server.
async fn asset(
    State(app): State<App>,
    headers: HeaderMap,
    Path((id, hash)): Path<(String, String)>,
) -> ApiResult {
    member(&app, &headers, &id)?;
    let bytes = app
        .store
        .project_asset(&id, &hash)
        .map_err(internal)?
        .ok_or_else(|| fail(StatusCode::NOT_FOUND, "no such asset"))?;
    Ok((
        [
            (header::CONTENT_TYPE, "application/octet-stream"),
            (
                header::CACHE_CONTROL,
                "private, max-age=31536000, immutable",
            ),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            (header::CONTENT_SECURITY_POLICY, "sandbox"),
        ],
        bytes,
    )
        .into_response())
}

#[derive(Deserialize)]
struct ExportQuery {
    /// The caller's local date, `YYYY-MM-DD`.
    today: Option<String>,
}

/// Compiles the project from the room's current state, in a child process.
async fn export_pdf(
    State(app): State<App>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(q): Query<ExportQuery>,
) -> ApiResult {
    member(&app, &headers, &id)?;
    let room = app.rooms.get(&id).await.map_err(internal)?;
    let files = room.read(project::files);
    let mut contents = Vec::new();
    for file in files {
        match (file.text, file.hash) {
            (Some(text), _) => contents.push((file.path, FileContents::Text(text))),
            // An asset the project does not hold is left out, and the
            // compiler reports the missing file.
            (None, Some(hash)) => {
                if let Some(bytes) = app.store.project_asset(&id, &hash).map_err(internal)? {
                    contents.push((file.path, FileContents::Bytes(bytes)));
                }
            }
            (None, None) => {}
        }
    }
    let today = crate::web::export_date(q.today.as_deref(), crate::web::utc_days());
    let started = Instant::now();
    let pdf = match app.exporter.pdf(contents, today).await {
        Ok(pdf) => pdf,
        Err(ExportError::Busy) => {
            return Ok((
                StatusCode::SERVICE_UNAVAILABLE,
                [(header::RETRY_AFTER, "30")],
                ExportError::Busy.to_string(),
            )
                .into_response());
        }
        Err(e @ (ExportError::Unsandboxed(_) | ExportError::Stopping)) => {
            return Err(fail(StatusCode::SERVICE_UNAVAILABLE, e.to_string()));
        }
        Err(e @ ExportError::Failed(_)) => {
            tracing::warn!(project = %id, error = %e, "export failed");
            return Err(internal(e));
        }
        Err(e) => return Err(fail(StatusCode::UNPROCESSABLE_ENTITY, e.to_string())),
    };
    tracing::info!(project = %id, ms = started.elapsed().as_millis(), bytes = pdf.len(), "exported PDF");
    Ok(([(header::CONTENT_TYPE, "application/pdf")], pdf).into_response())
}

/// Opens a sync socket. A handshake from another origin gets a plain 403. A
/// handshake from this server's pages is upgraded even when the caller may
/// not open the project, and the socket is then closed with a code the
/// client acts on: signed out (4401), no such project (4004) or too many
/// sockets (4029). The socket holds a grant from the session; it closes with
/// 4401 when the session ends or expires and 4004 when the membership goes,
/// and no frame after that is applied (`access.rs`).
async fn sync_upgrade(
    State(app): State<App>,
    headers: HeaderMap,
    Path(id): Path<String>,
    ws: WebSocketUpgrade,
) -> ApiResult {
    if !app.public.same_origin(&headers) {
        return Err(fail(StatusCode::FORBIDDEN, "cross-origin socket refused"));
    }
    // A message is at most one part and its header.
    let max = nisaba_sync::PART + 64;
    let ws = ws.max_message_size(max).max_frame_size(max);
    let Some(token) = token(&app, &headers) else {
        return Ok(refuse_socket(ws, close::SIGNED_OUT, "sign in again"));
    };
    let grant = match app.store.grant(&token, &id).map_err(internal)? {
        Ok(grant) => grant,
        Err(code) if code == close::NOT_FOUND => {
            return Ok(refuse_socket(ws, code, "no such project"));
        }
        Err(code) => return Ok(refuse_socket(ws, code, "sign in again")),
    };
    let Some(slot) = SocketSlot::take(&app, &grant.user.id) else {
        return Ok(refuse_socket(
            ws,
            close::FULL,
            "too many open connections for this account",
        ));
    };
    let rooms = app.rooms.clone();
    Ok(ws.on_upgrade(move |socket| async move {
        crate::sync::run(socket, rooms, id, grant).await;
        drop(slot);
    }))
}

/// Upgrades and at once closes with `code`.
fn refuse_socket(ws: WebSocketUpgrade, code: u16, reason: &'static str) -> Response {
    ws.on_upgrade(move |mut socket| async move {
        let _ = socket
            .send(Message::Close(Some(CloseFrame {
                code,
                reason: reason.into(),
            })))
            .await;
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_full_project_refuses_an_upload_with_413() {
        let full = nisaba_sync::SyncError::ProjectFull { need: 10, left: 1 };
        assert_eq!(refused_update(full).0, StatusCode::PAYLOAD_TOO_LARGE);
        let big = nisaba_sync::SyncError::TooLarge(10);
        assert_eq!(refused_update(big).0, StatusCode::PAYLOAD_TOO_LARGE);
    }
}
