//! HTTP routes: first-run setup, sessions, projects, assets, reads, export,
//! the sync upgrade, and the embedded web client.
//!
//! There is no route that writes a document body. Text changes only through
//! the sync endpoint, and every read here takes text from the same room the
//! sync endpoint writes to.

use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use axum::Router;
use axum::body::Bytes;
use axum::extract::{Path, Query, State, WebSocketUpgrade};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use base64::Engine as _;
use nisaba_compile_core::FileContents;
use nisaba_sync::Role;
use serde::Deserialize;

use crate::api::{AssetAdded, Created, Credentials, NewProject, Session, User};
use crate::project::{self, NewFile};
use crate::store::Store;
use crate::sync::Rooms;

const COOKIE: &str = "nisaba_session";
const SESSION_TTL: i64 = 30 * 24 * 3600;

#[derive(Clone)]
pub struct App {
    pub store: Store,
    pub rooms: Rooms,
}

type ApiResult = Result<Response, (StatusCode, String)>;

fn fail(status: StatusCode, msg: impl Into<String>) -> (StatusCode, String) {
    (status, msg.into())
}

fn internal(e: impl std::fmt::Display) -> (StatusCode, String) {
    fail(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}

pub fn router(app: App) -> Router {
    Router::new()
        .route("/api/session", get(session))
        .route("/api/setup", post(setup))
        .route("/api/login", post(login))
        .route("/api/logout", post(logout))
        .route("/api/projects", get(list_projects).post(create_project))
        .route("/api/projects/{id}/files", get(read_files))
        .route("/api/projects/{id}/assets", post(upload_asset))
        .route("/api/projects/{id}/export.pdf", get(export_pdf))
        .route("/api/assets/{hash}", get(asset))
        .route("/sync/{id}", get(sync_upgrade))
        .fallback(get(crate::web::static_file))
        .with_state(app)
}

fn token(headers: &HeaderMap) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|kv| kv.trim().split_once('='))
        .find(|(k, _)| *k == COOKIE)
        .map(|(_, v)| v.to_owned())
}

fn current_user(app: &App, headers: &HeaderMap) -> Result<User, (StatusCode, String)> {
    let token = token(headers).ok_or_else(|| fail(StatusCode::UNAUTHORIZED, "sign in first"))?;
    app.store
        .session_user(&token)
        .map_err(internal)?
        .ok_or_else(|| fail(StatusCode::UNAUTHORIZED, "session expired"))
}

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

fn new_session(app: &App, user: &User) -> ApiResult {
    let mut raw = [0u8; 32];
    rand::Rng::fill(&mut rand::thread_rng(), &mut raw);
    let token = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(raw);
    app.store
        .create_session(&token, &user.id, SESSION_TTL)
        .map_err(internal)?;
    let cookie = format!("{COOKIE}={token}; HttpOnly; SameSite=Lax; Path=/; Max-Age={SESSION_TTL}");
    Ok(([(header::SET_COOKIE, cookie)], axum::Json(user.clone())).into_response())
}

async fn session(State(app): State<App>, headers: HeaderMap) -> ApiResult {
    let setup_needed = app.store.user_count().map_err(internal)? == 0;
    let user = current_user(&app, &headers).ok();
    Ok(axum::Json(Session { setup_needed, user }).into_response())
}

async fn setup(State(app): State<App>, axum::Json(body): axum::Json<Credentials>) -> ApiResult {
    if app.store.user_count().map_err(internal)? > 0 {
        return Err(fail(
            StatusCode::CONFLICT,
            "this server already has an owner",
        ));
    }
    if body.password.len() < 8 {
        return Err(fail(StatusCode::BAD_REQUEST, "use at least 8 characters"));
    }
    let salt = SaltString::generate(&mut argon2::password_hash::rand_core::OsRng);
    let hash = argon2::Argon2::default()
        .hash_password(body.password.as_bytes(), &salt)
        .map_err(internal)?
        .to_string();
    let user = User {
        id: format!("{:016x}", rand::random::<u64>()),
        email: body.email.trim().to_lowercase(),
        name: if body.name.trim().is_empty() {
            body.email.clone()
        } else {
            body.name.trim().to_owned()
        },
    };
    app.store
        .create_user(&user.id, &user.email, &user.name, &hash)
        .map_err(internal)?;
    new_session(&app, &user)
}

async fn login(State(app): State<App>, axum::Json(body): axum::Json<Credentials>) -> ApiResult {
    let wrong = || fail(StatusCode::UNAUTHORIZED, "wrong email or password");
    let (user, hash) = app
        .store
        .user_by_email(&body.email.trim().to_lowercase())
        .map_err(internal)?
        .ok_or_else(wrong)?;
    let parsed = PasswordHash::new(&hash).map_err(internal)?;
    argon2::Argon2::default()
        .verify_password(body.password.as_bytes(), &parsed)
        .map_err(|_| wrong())?;
    new_session(&app, &user)
}

async fn logout(State(app): State<App>, headers: HeaderMap) -> ApiResult {
    if let Some(token) = token(&headers) {
        app.store.delete_session(&token).map_err(internal)?;
    }
    let cookie = format!("{COOKIE}=; HttpOnly; SameSite=Lax; Path=/; Max-Age=0");
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
        } else if let Some(b64) = file.base64 {
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(b64)
                .map_err(|e| fail(StatusCode::BAD_REQUEST, e.to_string()))?;
            let hash = app.store.put_asset(&bytes).map_err(internal)?;
            files.push(NewFile::Asset {
                path: file.path,
                hash,
            });
        }
    }
    if files.is_empty() {
        files.push(NewFile::Text {
            path: "main.typ".into(),
            text: STARTER.into(),
        });
    }
    let update = project::add_files(None, files).map_err(internal)?;
    let room = app.rooms.get(&id).await.map_err(internal)?;
    room.apply(&update).await.map_err(internal)?;
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
    if role != "owner" && role != "editor" {
        return Err(fail(StatusCode::FORBIDDEN, "only editors can add files"));
    }
    let hash = app.store.put_asset(&body).map_err(internal)?;
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
    room.apply(&update).await.map_err(internal)?;
    Ok(axum::Json(AssetAdded { hash }).into_response())
}

async fn asset(State(app): State<App>, headers: HeaderMap, Path(hash): Path<String>) -> ApiResult {
    current_user(&app, &headers)?;
    let bytes = app
        .store
        .get_asset(&hash)
        .ok_or_else(|| fail(StatusCode::NOT_FOUND, "no such asset"))?;
    Ok((
        [(
            header::CACHE_CONTROL,
            "private, max-age=31536000, immutable",
        )],
        bytes,
    )
        .into_response())
}

/// Compiles the project on the server from the room's current state.
async fn export_pdf(
    State(app): State<App>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> ApiResult {
    member(&app, &headers, &id)?;
    let room = app.rooms.get(&id).await.map_err(internal)?;
    let files = room.read(project::files);
    let store = app.store.clone();
    let started = std::time::Instant::now();
    let pdf = tokio::task::spawn_blocking(move || {
        let contents = files
            .into_iter()
            .filter_map(|file| {
                let contents = match (file.text, file.hash) {
                    (Some(text), _) => FileContents::Text(text),
                    (None, Some(hash)) => FileContents::Bytes(store.get_asset(&hash)?),
                    (None, None) => return None,
                };
                Some((file.path, contents))
            })
            .collect();
        crate::web::pdf(contents)
    })
    .await
    .map_err(internal)?
    .map_err(|e| fail(StatusCode::UNPROCESSABLE_ENTITY, e))?;
    tracing::info!(project = %id, ms = started.elapsed().as_millis(), bytes = pdf.len(), "exported PDF");
    Ok(([(header::CONTENT_TYPE, "application/pdf")], pdf).into_response())
}

async fn sync_upgrade(
    State(app): State<App>,
    headers: HeaderMap,
    Path(id): Path<String>,
    ws: WebSocketUpgrade,
) -> ApiResult {
    // A cross-site page must not open a socket with the user's cookie.
    let host = headers
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("");
    if let Some(origin) = headers.get(header::ORIGIN).and_then(|o| o.to_str().ok())
        && !origin.ends_with(&format!("//{host}"))
    {
        return Err(fail(StatusCode::FORBIDDEN, "cross-origin socket refused"));
    }
    let (_, role) = member(&app, &headers, &id)?;
    let role = match role.as_str() {
        "owner" | "editor" => Role::Editor,
        "commenter" => Role::Commenter,
        _ => Role::Viewer,
    };
    let rooms = app.rooms.clone();
    Ok(ws.on_upgrade(move |socket| crate::sync::run(socket, rooms, id, role)))
}
