//! HTTP: the routes of [`crate::api::routes::ROUTES`], the sync upgrade and
//! the embedded web client, behind one set of layers.
//!
//! - Every request other than `GET` and `HEAD` must carry an `Origin` header
//!   equal to the public address ([`same_origin`]).
//! - Every answer, errors included, carries the security headers
//!   ([`security_headers`]); API answers are not cached unless they say so.
//! - Every error is an [`ApiError`] with a stable code, also when axum
//!   refuses a body, a query, a path or a method, and when a handler panics.
//! - JSON and text answers are compressed with Brotli or gzip when the client
//!   takes them.
//!
//! There is no route that writes a document body. Text changes only through
//! the sync endpoint, and every read here takes text from the same room the
//! sync endpoint writes to.

mod account;
mod files;
mod projects;
mod socket;
mod valid;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::Bytes;
use axum::extract::rejection::{BytesRejection, PathRejection, QueryRejection};
use axum::extract::{DefaultBodyLimit, FromRequest, FromRequestParts, Request, State};
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{MethodRouter, delete, get, patch, post};
use serde::de::DeserializeOwned;
use tower_http::compression::CompressionLayer;
use tower_http::compression::predicate::{Predicate as _, SizeAbove};

use crate::api::routes::{self, Body as RouteBody};
use crate::api::{ApiError, ErrorCode, Role, User};
use crate::config::{self, PublicAddress};
use crate::export::Exporter;
use crate::fonts::Fonts;
use crate::limits;
use crate::packages::Packages;
use crate::passwords::{Busy, Cost, Passwords};
use crate::store::{SessionInfo, Store, StoreError};
use crate::sync::Rooms;

/// What [`App::new`] needs beyond the store.
pub struct Options {
    pub public: PublicAddress,
    pub exporter: Exporter,
    /// The memory all unfinished multi-part updates may hold together.
    pub transfer_bytes: usize,
    pub passwords: Cost,
    pub packages: config::Packages,
}

/// The server state every route shares.
#[derive(Clone)]
pub struct App {
    inner: Arc<Inner>,
}

struct Inner {
    store: Store,
    rooms: Rooms,
    public: PublicAddress,
    exporter: Exporter,
    passwords: Passwords,
    packages: Packages,
    fonts: Fonts,
    sockets: Mutex<HashMap<String, usize>>,
    /// How many sync sockets are open, for a stop that waits for them.
    open_sockets: tokio::sync::watch::Sender<usize>,
    stopping: tokio::sync::watch::Sender<bool>,
}

impl App {
    pub fn new(store: Store, options: Options) -> Result<Self, String> {
        let passwords = Passwords::new(
            limits::PASSWORD_WORKERS,
            limits::PASSWORD_QUEUE,
            options.passwords,
        )?;
        let fonts = Fonts::load(&store.dir().join("fonts"))?;
        let packages = Packages::new(store.dir().join("packages"), options.packages);
        let (stopping, _) = tokio::sync::watch::channel(false);
        Ok(Self {
            inner: Arc::new(Inner {
                rooms: Rooms::new(store.clone(), options.transfer_bytes),
                store,
                public: options.public,
                exporter: options.exporter,
                passwords,
                packages,
                fonts,
                sockets: Mutex::default(),
                open_sockets: tokio::sync::watch::channel(0).0,
                stopping,
            }),
        })
    }

    #[must_use]
    pub fn rooms(&self) -> &Rooms {
        &self.inner.rooms
    }

    #[must_use]
    pub fn store(&self) -> &Store {
        &self.inner.store
    }

    /// The password work and sign-in limits.
    #[must_use]
    pub fn passwords(&self) -> &Passwords {
        &self.inner.passwords
    }

    #[must_use]
    pub fn fonts(&self) -> &Fonts {
        &self.inner.fonts
    }

    #[must_use]
    pub fn exporter(&self) -> &Exporter {
        &self.inner.exporter
    }

    /// Starts stopping: health answers 503, exports end, sync sockets close
    /// with 1012 after the frame they are on.
    pub fn stop(&self) {
        self.inner.stopping.send_replace(true);
        self.inner.exporter.stop();
        self.inner.rooms.stop();
    }

    /// Resolves when no sync socket is open.
    pub async fn sockets_closed(&self) {
        let mut open = self.inner.open_sockets.subscribe();
        let _ = open.wait_for(|n| *n == 0).await.map(drop);
    }

    fn is_stopping(&self) -> bool {
        *self.inner.stopping.borrow()
    }

    fn store_ref(&self) -> &Store {
        &self.inner.store
    }
}

/// An error answer: an [`ApiError`] and its status.
#[derive(Debug)]
pub struct Fail(pub ApiError);

impl Fail {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self(ApiError {
            code,
            message: message.into(),
            field: None,
            retry_after: None,
            problems: None,
        })
    }

    /// An error about one field of the request.
    pub fn field(field: &str, message: impl Into<String>) -> Self {
        let mut fail = Self::new(ErrorCode::InvalidField, message);
        fail.0.field = Some(field.to_owned());
        fail
    }

    /// Asks the client to try again after `wait`.
    #[must_use]
    pub fn retry(mut self, wait: Duration) -> Self {
        let secs = wait.as_secs() + u64::from(wait.subsec_nanos() > 0);
        self.0.retry_after = Some(secs.max(1));
        self
    }

    pub fn internal(e: impl std::fmt::Display) -> Self {
        tracing::error!(error = %e, "request failed");
        Self::new(ErrorCode::Internal, "the server failed; its log says why")
    }

    fn not_found(what: &str) -> Self {
        Self::new(ErrorCode::NotFound, format!("no such {what}"))
    }
}

impl From<StoreError> for Fail {
    fn from(e: StoreError) -> Self {
        match e {
            StoreError::Full(_) => {
                tracing::error!(error = %e, "storage is full");
                Self::new(
                    ErrorCode::StorageFull,
                    "the server's disk is full; nothing was written",
                )
            }
            StoreError::Other(_) => Self::internal(e),
        }
    }
}

impl From<Busy> for Fail {
    fn from(Busy: Busy) -> Self {
        Self::new(ErrorCode::Busy, "the server is busy; try again in a moment")
            .retry(Duration::from_secs(1))
    }
}

impl IntoResponse for Fail {
    fn into_response(self) -> Response {
        let status =
            StatusCode::from_u16(self.0.code.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        let mut response = (status, axum::Json(&self.0)).into_response();
        if let Some(secs) = self.0.retry_after
            && let Ok(value) = HeaderValue::from_str(&secs.to_string())
        {
            response.headers_mut().insert(header::RETRY_AFTER, value);
        }
        response
    }
}

pub type Answer = Result<Response, Fail>;

/// A JSON body, refused with [`ErrorCode::InvalidRequest`] or
/// [`ErrorCode::PayloadTooLarge`] in the error shape.
pub struct Json<T>(pub T);

impl<S: Send + Sync, T: DeserializeOwned> FromRequest<S> for Json<T> {
    type Rejection = Fail;

    async fn from_request(req: Request, state: &S) -> Result<Self, Fail> {
        let bytes = Bytes::from_request(req, state)
            .await
            .map_err(|e| body_fail(&e))?;
        serde_json::from_slice(&bytes).map(Json).map_err(|e| {
            Fail::new(
                ErrorCode::InvalidRequest,
                format!("the body is not what this route takes: {e}"),
            )
        })
    }
}

/// Raw body bytes, refused in the error shape.
pub struct Raw(pub Bytes);

impl<S: Send + Sync> FromRequest<S> for Raw {
    type Rejection = Fail;

    async fn from_request(req: Request, state: &S) -> Result<Self, Fail> {
        Bytes::from_request(req, state)
            .await
            .map(Raw)
            .map_err(|e| body_fail(&e))
    }
}

fn body_fail(e: &BytesRejection) -> Fail {
    if e.status() == StatusCode::PAYLOAD_TOO_LARGE {
        Fail::new(
            ErrorCode::PayloadTooLarge,
            "the request body is larger than this route takes",
        )
    } else {
        Fail::new(ErrorCode::InvalidRequest, e.body_text())
    }
}

/// A query string, refused in the error shape.
pub struct Query<T>(pub T);

impl<S: Send + Sync, T: DeserializeOwned> FromRequestParts<S> for Query<T> {
    type Rejection = Fail;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Fail> {
        axum::extract::Query::<T>::from_request_parts(parts, state)
            .await
            .map(|q| Query(q.0))
            .map_err(|e: QueryRejection| Fail::new(ErrorCode::InvalidRequest, e.body_text()))
    }
}

/// Path parameters, refused in the error shape.
pub struct Path<T>(pub T);

impl<S: Send + Sync, T: DeserializeOwned + Send> FromRequestParts<S> for Path<T> {
    type Rejection = Fail;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Fail> {
        axum::extract::Path::<T>::from_request_parts(parts, state)
            .await
            .map(|p| Path(p.0))
            .map_err(|e: PathRejection| Fail::new(ErrorCode::InvalidRequest, e.body_text()))
    }
}

/// The session cookie's value, if the request has one.
fn token(app: &App, headers: &HeaderMap) -> Option<String> {
    let name = app.inner.public.cookie_name();
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|kv| kv.trim().split_once('='))
        .find(|(k, _)| *k == name)
        .map(|(_, v)| v.to_owned())
}

/// The signed-in caller: who, the session token and the session.
struct Caller {
    user: User,
    token: String,
    session: SessionInfo,
}

fn signed_in(app: &App, headers: &HeaderMap) -> Result<Caller, Fail> {
    let not_signed_in = || Fail::new(ErrorCode::NotSignedIn, "sign in first");
    let token = token(app, headers).ok_or_else(not_signed_in)?;
    let (user, session) = app.store_ref().session(&token)?.ok_or_else(not_signed_in)?;
    Ok(Caller {
        user,
        token,
        session,
    })
}

fn admin(app: &App, headers: &HeaderMap) -> Result<Caller, Fail> {
    let caller = signed_in(app, headers)?;
    if !caller.user.is_admin {
        return Err(Fail::new(
            ErrorCode::Forbidden,
            "only a server administrator may do this",
        ));
    }
    Ok(caller)
}

/// The caller and their role in `project`, which must be at least `need`.
/// Not being a member and the project not existing give the same 404.
fn member(
    app: &App,
    headers: &HeaderMap,
    project: &str,
    need: Need,
) -> Result<(Caller, Role), Fail> {
    let caller = signed_in(app, headers)?;
    let role = app
        .store_ref()
        .role(project, &caller.user.id)?
        .ok_or_else(|| Fail::not_found("project"))?;
    let allowed = match need {
        Need::Member => true,
        Need::Editor => role.writes(),
        Need::Owner => role == Role::Owner,
    };
    if !allowed {
        let what = match need {
            Need::Owner => "only an owner of the project may do this",
            _ => "your role in this project does not allow this",
        };
        return Err(Fail::new(ErrorCode::Forbidden, what));
    }
    Ok((caller, role))
}

#[derive(Clone, Copy)]
enum Need {
    Member,
    Editor,
    Owner,
}

/// The handler of every route of [`ROUTES`] but the web client, which is
/// the router's fallback.
fn handlers() -> Vec<(&'static str, MethodRouter<App>)> {
    vec![
        ("health", get(account::health)),
        ("session", get(account::session)),
        ("setup", post(account::setup)),
        ("login", post(account::login)),
        ("logout", post(account::logout)),
        ("logout_everywhere", post(account::logout_everywhere)),
        ("account_update", patch(account::rename)),
        ("password_change", post(account::change_password)),
        ("users_list", get(account::users)),
        ("user_create", post(account::create_user)),
        ("user_delete", delete(account::delete_user)),
        ("projects_list", get(projects::list)),
        ("project_create", post(projects::create)),
        ("project_get", get(projects::get)),
        ("project_update", patch(projects::rename)),
        ("project_delete", delete(projects::delete)),
        ("members_list", get(projects::members)),
        ("member_add", post(projects::add_member)),
        ("member_update", patch(projects::set_role)),
        ("member_remove", delete(projects::remove_member)),
        ("files", get(files::files)),
        ("asset_upload", post(files::upload)),
        ("asset_get", get(files::asset)),
        ("export", get(files::export)),
        ("package", get(files::package)),
        ("fonts", get(files::font_catalogue)),
        ("font_file", get(files::font_file)),
        ("sync", get(socket::upgrade)),
    ]
}

/// The router of every described route: each handler at its route's path,
/// with that route's body limit.
fn api_router() -> Router<App> {
    let mut by_path: Vec<(&'static str, MethodRouter<App>)> = Vec::new();
    for (id, handler) in handlers() {
        let route = routes::route(id);
        let limit = match route.body {
            RouteBody::None => limits::MAX_BODY,
            RouteBody::Json { max, .. } | RouteBody::Bytes { max } => max,
        };
        let handler = handler.layer(DefaultBodyLimit::max(limit));
        match by_path.iter_mut().find(|(path, _)| *path == route.path) {
            Some((_, existing)) => {
                let merged = std::mem::take(existing).merge(handler);
                *existing = merged;
            }
            None => by_path.push((route.path, handler)),
        }
    }
    by_path
        .into_iter()
        .fold(Router::new(), |router, (path, handler)| {
            router.route(path, handler)
        })
}

pub fn router(app: App) -> Router {
    let compress = SizeAbove::new(1024).and(
        |_: StatusCode, _: axum::http::Version, headers: &HeaderMap, _: &axum::http::Extensions| {
            headers
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .is_some_and(|t| {
                    t.starts_with("application/json")
                        || t.starts_with("text/")
                        || t.starts_with("image/svg")
                })
        },
    );
    api_router()
        .fallback(crate::web::static_file)
        .method_not_allowed_fallback(method_not_allowed)
        .layer(CompressionLayer::new().compress_when(compress))
        .layer(middleware::from_fn_with_state(app.clone(), same_origin))
        .layer(tower_http::catch_panic::CatchPanicLayer::custom(panicked))
        .layer(middleware::from_fn_with_state(
            app.clone(),
            security_headers,
        ))
        .layer(middleware::from_fn(log_request))
        .with_state(app)
}

async fn method_not_allowed() -> Fail {
    Fail::new(
        ErrorCode::MethodNotAllowed,
        "this path does not take this method",
    )
}

fn panicked(_: Box<dyn std::any::Any + Send + 'static>) -> Response {
    tracing::error!("a request handler panicked");
    Fail::new(ErrorCode::Internal, "the server failed; its log says why").into_response()
}

/// Refuses every request that can change state unless it comes from a page
/// of this server. The session cookie is `SameSite=Lax`, which already keeps
/// it off cross-site requests other than top-level navigation; this check
/// does not depend on the browser's cookie rules.
async fn same_origin(State(app): State<App>, request: Request, next: Next) -> Response {
    let safe = matches!(*request.method(), Method::GET | Method::HEAD);
    if !safe && !app.inner.public.same_origin(request.headers()) {
        return Fail::new(
            ErrorCode::CrossOrigin,
            "a request that changes something must come from a page of this server",
        )
        .into_response();
    }
    next.run(request).await
}

/// The page the web client runs in may load scripts, styles, workers, fonts
/// and WebAssembly only from this server, talk only to this server, and
/// never be framed.
const CSP: &str = "default-src 'self'; script-src 'self' 'wasm-unsafe-eval'; \
                   style-src 'self' 'unsafe-inline'; img-src 'self' data: blob:; \
                   font-src 'self' data:; connect-src 'self'; worker-src 'self' blob:; \
                   object-src 'none'; base-uri 'none'; form-action 'self'; \
                   frame-ancestors 'none'";

/// Adds the security headers to every answer, errors included. An answer
/// that set its own `Content-Security-Policy` (an asset: `sandbox`) keeps
/// it. API answers get `Cache-Control: no-store` unless they set one.
async fn security_headers(State(app): State<App>, request: Request, next: Next) -> Response {
    let api = request.uri().path().starts_with("/api/");
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    let fixed = [
        (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        (header::REFERRER_POLICY, "no-referrer"),
        (header::X_FRAME_OPTIONS, "DENY"),
        (
            header::HeaderName::from_static("cross-origin-opener-policy"),
            "same-origin",
        ),
        (
            header::HeaderName::from_static("cross-origin-resource-policy"),
            "same-origin",
        ),
        (
            header::HeaderName::from_static("permissions-policy"),
            "camera=(), microphone=(), geolocation=(), payment=(), usb=()",
        ),
    ];
    for (name, value) in fixed {
        headers.insert(name, HeaderValue::from_static(value));
    }
    headers
        .entry(header::CONTENT_SECURITY_POLICY)
        .or_insert(HeaderValue::from_static(CSP));
    if api {
        headers
            .entry(header::CACHE_CONTROL)
            .or_insert(HeaderValue::from_static("no-store"));
    }
    if app.inner.public.is_https() {
        headers.insert(
            header::STRICT_TRANSPORT_SECURITY,
            HeaderValue::from_static("max-age=31536000"),
        );
    }
    response
}

/// Logs each request: method, path without its query, status and time. The
/// query is left out because it may hold a file name. Failures of the
/// server are logged as warnings, the rest at debug level.
async fn log_request(request: Request, next: Next) -> Response {
    let method = request.method().clone();
    let path = request.uri().path().to_owned();
    let started = Instant::now();
    let response = next.run(request).await;
    let status = response.status().as_u16();
    let ms = started.elapsed().as_millis();
    if status >= 500 {
        tracing::warn!(%method, path, status, ms, "request");
    } else {
        tracing::debug!(%method, path, status, ms, "request");
    }
    response
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_router_serves_every_described_route() {
        let mut registered: Vec<_> = super::handlers().into_iter().map(|(id, _)| id).collect();
        registered.push("web_client");
        registered.sort_unstable();
        let mut described: Vec<_> = crate::api::routes::ROUTES.iter().map(|r| r.id).collect();
        described.sort_unstable();
        assert_eq!(registered, described);
    }
}
