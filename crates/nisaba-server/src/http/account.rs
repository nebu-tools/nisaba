//! Health, sessions, setup, sign-in and accounts.

use std::time::Instant;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use base64::Engine as _;

use super::{Answer, App, Fail, Json, Path, admin, signed_in, token, valid};
use crate::api::{
    AccountChange, Credentials, ErrorCode, Health, HealthStatus, NewAccount, PasswordChange,
    Session, SetupRequest, User,
};
use crate::store::{Refusal, Setup, now_secs};

pub async fn health(State(app): State<App>) -> Response {
    let status = if app.is_stopping() {
        HealthStatus::Stopping
    } else if let Err(e) = app.store_ref().healthy() {
        tracing::error!(error = %e, "the health check cannot read the database");
        HealthStatus::Failing
    } else {
        HealthStatus::Ok
    };
    let code = if status == HealthStatus::Ok {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    let body = Health {
        status,
        version: env!("CARGO_PKG_VERSION").into(),
        web_client: crate::web::built(),
        exports: app.exporter().mode(),
    };
    (code, axum::Json(body)).into_response()
}

/// The cookie that holds `token` until `expires_at`.
fn cookie(app: &App, token: &str, expires_at: i64) -> String {
    app.inner
        .public
        .session_cookie(token, (expires_at - now_secs()).max(0))
}

pub async fn session(State(app): State<App>, headers: HeaderMap) -> Answer {
    let store = app.store_ref();
    let setup_needed = store.user_count()? == 0;
    let setup_code_needed = setup_needed && store.setup_code_needed()?;
    let Ok(caller) = signed_in(&app, &headers) else {
        return Ok(axum::Json(Session {
            setup_needed,
            setup_code_needed,
            user: None,
            expires_at: None,
        })
        .into_response());
    };
    let renewed = store.renew_session(&caller.token, caller.session)?;
    let expires_at = renewed.unwrap_or(caller.session.expires_at);
    let body = axum::Json(Session {
        setup_needed,
        setup_code_needed,
        user: Some(caller.user),
        expires_at: Some(expires_at),
    });
    Ok(match renewed {
        Some(end) => (
            [(header::SET_COOKIE, cookie(&app, &caller.token, end))],
            body,
        )
            .into_response(),
        None => body.into_response(),
    })
}

/// Starts a session for `user` and answers `status` with the user and the
/// cookie.
fn new_session(app: &App, user: User, status: StatusCode) -> Answer {
    let mut raw = [0u8; 32];
    rand::Rng::fill(&mut rand::thread_rng(), &mut raw);
    let token = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(raw);
    let info = app.store_ref().create_session(&token, &user.id)?;
    let cookie = cookie(app, &token, info.expires_at);
    Ok((status, [(header::SET_COOKIE, cookie)], axum::Json(user)).into_response())
}

fn setup_refusal(setup: &Setup) -> Fail {
    match setup {
        Setup::Created => Fail::internal("setup worked"),
        Setup::Taken => Fail::new(
            ErrorCode::SetupDone,
            "this server already has an administrator",
        ),
        Setup::WrongCode => Fail::new(
            ErrorCode::WrongSetupCode,
            "wrong setup code; the server printed it to its log when it started",
        ),
    }
}

/// First-run setup. The owner and the code are checked before the password
/// is hashed, so a server that has an owner does no password work here, and
/// checked again in the transaction that creates the account.
pub async fn setup(State(app): State<App>, Json(body): Json<SetupRequest>) -> Answer {
    let store = app.store_ref();
    if let Some(refused) = store.setup_refused(body.setup_code.as_deref())? {
        return Err(setup_refusal(&refused));
    }
    let email = valid::email("email", &body.email)?;
    valid::password("password", &body.password)?;
    let name = valid::name("name", &body.name, Some(&email))?;
    let hash = app
        .passwords()
        .hash(body.password)
        .await?
        .map_err(Fail::internal)?;
    let user = User {
        id: crate::store::new_id(),
        email,
        name,
        is_admin: true,
    };
    match store.create_owner(&user, &hash, Some(body.setup_code.as_deref()))? {
        Setup::Created => {
            tracing::info!(user = %user.id, "the first account was made");
            new_session(&app, user, StatusCode::CREATED)
        }
        refused => Err(setup_refusal(&refused)),
    }
}

/// Sign-in. An email that failed too often is refused with 429 before any
/// password work; see [`crate::passwords`]. A hash made with older
/// parameters is replaced.
pub async fn login(State(app): State<App>, Json(body): Json<Credentials>) -> Answer {
    let email = body.email.trim().to_lowercase();
    let passwords = app.passwords();
    if let Err(wait) = passwords.check(&email, Instant::now()) {
        return Err(Fail::new(
            ErrorCode::TooManyAttempts,
            "too many failed sign-ins for this email; wait and try again",
        )
        .retry(wait));
    }
    let found = app.store_ref().credentials(&email)?;
    let hash = found.as_ref().and_then(|(_, hash)| hash.clone());
    let matches = passwords
        .verify(body.password.clone(), hash.clone())
        .await?;
    let (Some((user, _)), true, Some(hash)) = (found, matches, hash) else {
        passwords.failed(&email, Instant::now());
        return Err(Fail::new(
            ErrorCode::WrongCredentials,
            "wrong email or password",
        ));
    };
    passwords.succeeded(&email);
    if passwords.outdated(&hash) {
        // The password is right, so a hash with today's parameters can
        // replace the old one. A failure here costs nothing but the upgrade.
        if let Ok(Ok(new)) = passwords.hash(body.password).await
            && app.store_ref().rehash_password(&user.id, &hash, &new)?
        {
            tracing::info!(user = %user.id, "stored a password hash with the current parameters");
        }
    }
    new_session(&app, user, StatusCode::OK)
}

/// The answer that clears the cookie.
fn signed_out(app: &App) -> Response {
    let cookie = app.inner.public.session_cookie("", 0);
    ([(header::SET_COOKIE, cookie)], StatusCode::NO_CONTENT).into_response()
}

pub async fn logout(State(app): State<App>, headers: HeaderMap) -> Answer {
    if let Some(token) = token(&app, &headers) {
        app.store_ref().delete_session(&token)?;
    }
    Ok(signed_out(&app))
}

pub async fn logout_everywhere(State(app): State<App>, headers: HeaderMap) -> Answer {
    let caller = signed_in(&app, &headers)?;
    app.store_ref().delete_sessions(&caller.user.id)?;
    Ok(signed_out(&app))
}

pub async fn rename(
    State(app): State<App>,
    headers: HeaderMap,
    Json(body): Json<AccountChange>,
) -> Answer {
    let caller = signed_in(&app, &headers)?;
    let name = valid::name("name", &body.name, None)?;
    let user = app
        .store_ref()
        .rename_account(&caller.user.id, &name)?
        .ok_or_else(|| Fail::new(ErrorCode::NotSignedIn, "sign in first"))?;
    Ok(axum::Json(user).into_response())
}

/// A wrong current password counts against the email like a failed
/// sign-in, so a stolen session cannot guess the password faster.
pub async fn change_password(
    State(app): State<App>,
    headers: HeaderMap,
    Json(body): Json<PasswordChange>,
) -> Answer {
    let caller = signed_in(&app, &headers)?;
    let passwords = app.passwords();
    let email = caller.user.email.clone();
    if let Err(wait) = passwords.check(&email, Instant::now()) {
        return Err(Fail::new(
            ErrorCode::TooManyAttempts,
            "too many wrong passwords for this account; wait and try again",
        )
        .retry(wait));
    }
    valid::password("new_password", &body.new_password)?;
    let hash = app.store_ref().password_hash(&caller.user.id)?;
    if !passwords.verify(body.current_password, hash).await? {
        passwords.failed(&email, Instant::now());
        let mut fail = Fail::new(ErrorCode::WrongCredentials, "the current password is wrong");
        fail.0.field = Some("current_password".into());
        return Err(fail);
    }
    passwords.succeeded(&email);
    let new = passwords
        .hash(body.new_password)
        .await?
        .map_err(Fail::internal)?;
    app.store_ref()
        .change_password(&caller.user.id, &new, &caller.token)?;
    tracing::info!(user = %caller.user.id, "password changed");
    Ok(StatusCode::NO_CONTENT.into_response())
}

pub async fn users(State(app): State<App>, headers: HeaderMap) -> Answer {
    admin(&app, &headers)?;
    Ok(axum::Json(app.store_ref().accounts()?).into_response())
}

pub async fn create_user(
    State(app): State<App>,
    headers: HeaderMap,
    Json(body): Json<NewAccount>,
) -> Answer {
    let caller = admin(&app, &headers)?;
    let email = valid::email("email", &body.email)?;
    valid::password("password", &body.password)?;
    let name = valid::name("name", &body.name, Some(&email))?;
    let hash = app
        .passwords()
        .hash(body.password)
        .await?
        .map_err(Fail::internal)?;
    let user = User {
        id: crate::store::new_id(),
        email,
        name,
        is_admin: body.is_admin,
    };
    if app.store_ref().create_account(&user, &hash)?.is_err() {
        let mut fail = Fail::new(ErrorCode::EmailTaken, "another account has this email");
        fail.0.field = Some("email".into());
        return Err(fail);
    }
    tracing::info!(by = %caller.user.id, user = %user.id, "account made");
    Ok((StatusCode::CREATED, axum::Json(user)).into_response())
}

pub async fn delete_user(
    State(app): State<App>,
    headers: HeaderMap,
    Path(user): Path<String>,
) -> Answer {
    let caller = admin(&app, &headers)?;
    match app.store_ref().delete_account(&user)? {
        Ok(()) => {
            tracing::info!(by = %caller.user.id, user, "account deleted");
            Ok(StatusCode::NO_CONTENT.into_response())
        }
        Err(Refusal::SoleOwner) => Err(Fail::new(
            ErrorCode::SoleOwner,
            "this account is the only owner of a project; delete its projects or give them \
             another owner first",
        )),
        Err(Refusal::LastAdmin) => Err(Fail::new(
            ErrorCode::LastAdmin,
            "this is the last administrator; make another one first",
        )),
        Err(_) => Err(Fail::not_found("account")),
    }
}
