//! The sync upgrade.

use std::sync::Arc;

use axum::extract::{State, WebSocketUpgrade};
use axum::http::HeaderMap;
use axum::response::Response;
use nisaba_sync::close;

use super::{Answer, App, Fail, Path, token};
use crate::api::ErrorCode;
use crate::limits::SOCKETS_PER_ACCOUNT;

/// Counts one sync socket of an account while it lives.
struct SocketSlot {
    app: App,
    user: String,
}

impl SocketSlot {
    fn take(app: &App, user: &str) -> Option<Self> {
        let mut sockets = app
            .inner
            .sockets
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let n = sockets.entry(user.to_owned()).or_default();
        if *n >= SOCKETS_PER_ACCOUNT {
            return None;
        }
        *n += 1;
        let total = sockets.values().sum();
        app.inner.open_sockets.send_replace(total);
        Some(Self {
            app: app.clone(),
            user: user.to_owned(),
        })
    }
}

impl Drop for SocketSlot {
    fn drop(&mut self) {
        let mut sockets = self
            .app
            .inner
            .sockets
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(n) = sockets.get_mut(&self.user) {
            *n -= 1;
            if *n == 0 {
                sockets.remove(&self.user);
            }
        }
        let total = sockets.values().sum();
        self.app.inner.open_sockets.send_replace(total);
    }
}

/// Opens a sync socket. A handshake from another origin gets a plain 403. A
/// handshake from this server's pages is upgraded even when the caller may
/// not open the project, and the socket is then closed with a code the
/// client acts on: signed out (4401), no such project (4004), too many
/// sockets (4029) or the server stopping (1012). The two refusals of access
/// look alike for a project that does not exist and one the caller is not a
/// member of. The socket holds a grant from the session (`access.rs`).
pub async fn upgrade(
    State(app): State<App>,
    headers: HeaderMap,
    Path(project): Path<String>,
    ws: Result<WebSocketUpgrade, axum::extract::ws::rejection::WebSocketUpgradeRejection>,
) -> Answer {
    if !app.inner.public.same_origin(&headers) {
        return Err(Fail::new(
            ErrorCode::CrossOrigin,
            "a sync socket must come from a page of this server",
        ));
    }
    let ws = ws.map_err(|e| Fail::new(ErrorCode::InvalidRequest, e.to_string()))?;
    // A message is at most one part and its header.
    let max = nisaba_sync::PART + 64;
    let ws = ws.max_message_size(max).max_frame_size(max);
    if app.is_stopping() {
        return Ok(refuse(ws, close::RESTARTING, "the server is stopping"));
    }
    let Some(token) = token(&app, &headers) else {
        return Ok(refuse(ws, close::SIGNED_OUT, "sign in first"));
    };
    let grant = match app.store_ref().grant(&token, &project)? {
        Ok(grant) => grant,
        Err(code) if code == close::NOT_FOUND => {
            return Ok(refuse(ws, code, "no such project"));
        }
        Err(code) => return Ok(refuse(ws, code, "sign in again")),
    };
    let Some(slot) = SocketSlot::take(&app, &grant.user.id) else {
        return Ok(refuse(
            ws,
            close::FULL,
            "too many open connections for this account",
        ));
    };
    let rooms = app.rooms().clone();
    let slot = Arc::new(slot);
    Ok(ws.on_upgrade(move |socket| async move {
        crate::sync::run(socket, rooms, project, grant).await;
        drop(slot);
    }))
}

/// Upgrades and at once closes with `code`.
fn refuse(ws: WebSocketUpgrade, code: u16, reason: &'static str) -> Response {
    ws.on_upgrade(move |mut socket| async move {
        crate::sync::close_with(&mut socket, code, reason).await;
    })
}
