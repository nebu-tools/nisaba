//! Accounts and sessions against the real binary: cookies, renewal,
//! sign-out everywhere, password changes, rehashing, and the
//! administrator's account routes.

#![allow(
    clippy::too_many_lines,
    reason = "an end-to-end story reads best as one test"
)]

mod common;

use std::time::Duration;

use common::*;
use futures_util::SinkExt as _;
use nisaba_sync::Frame;
use tokio_tungstenite::tungstenite::Message;

fn now() -> i64 {
    nisaba_server::store::now_secs()
}

fn db(dir: &std::path::Path) -> rusqlite::Connection {
    rusqlite::Connection::open(dir.join("nisaba.db")).unwrap()
}

fn max_age(cookie: &str) -> i64 {
    cookie
        .split(';')
        .find_map(|p| p.trim().strip_prefix("Max-Age="))
        .unwrap()
        .parse()
        .unwrap()
}

async fn json(res: reqwest::Response) -> serde_json::Value {
    res.json().await.unwrap()
}

/// Behind plain http the cookie has every attribute but `Secure`, and lasts
/// 30 days.
#[tokio::test]
async fn the_cookie_has_the_attributes_design_names() {
    let dir = tempfile::tempdir().unwrap();
    let server = Server::start(dir.path());
    let res = browser(&server.base)
        .post(format!("{}/api/setup", server.base))
        .json(&serde_json::json!({"email": "a@example.org", "password": PASSWORD}))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 201);
    let cookie = res.headers()["set-cookie"].to_str().unwrap().to_owned();
    assert!(cookie.starts_with("nisaba_session="), "{cookie}");
    for part in ["; HttpOnly", "; SameSite=Lax", "; Path=/"] {
        assert!(cookie.contains(part), "{cookie} lacks {part}");
    }
    assert!(!cookie.contains("Secure"), "{cookie}");
    assert!((max_age(&cookie) - 30 * 86_400).abs() <= 2, "{cookie}");
    let user = json(res).await;
    assert_eq!(user["is_admin"], true);
}

/// `GET /api/session` moves the end of a session that was last renewed
/// more than a day ago to 30 days from now, never past 90 days after
/// sign-in, and sends the cookie again. An expired session is gone.
#[tokio::test]
async fn a_session_is_renewed_when_used_and_ends_when_it_expires() {
    let dir = tempfile::tempdir().unwrap();
    let server = Server::start(dir.path());
    let base = &server.base;
    let (client, _) = sign_up(base).await;
    let session = |client: &reqwest::Client| {
        let client = client.clone();
        let base = base.clone();
        async move {
            client
                .get(format!("{base}/api/session"))
                .send()
                .await
                .unwrap()
        }
    };
    // Just made: no renewal, no cookie.
    let res = session(&client).await;
    assert!(res.headers().get("set-cookie").is_none());
    let body = json(res).await;
    assert!((body["expires_at"].as_i64().unwrap() - (now() + 30 * 86_400)).abs() <= 2);
    // As if it was renewed three days ago.
    let three_days = 3 * 86_400;
    db(dir.path())
        .execute(
            "UPDATE sessions SET created_at = created_at - ?1, expires_at = expires_at - ?1",
            [three_days],
        )
        .unwrap();
    let res = session(&client).await;
    let cookie = res.headers()["set-cookie"].to_str().unwrap().to_owned();
    assert!((max_age(&cookie) - 30 * 86_400).abs() <= 2, "{cookie}");
    let body = json(res).await;
    assert!((body["expires_at"].as_i64().unwrap() - (now() + 30 * 86_400)).abs() <= 2);
    // Signed in 89 days ago: the end stops at 90 days after sign-in.
    db(dir.path())
        .execute(
            "UPDATE sessions SET created_at = ?1, expires_at = ?2",
            [now() - 89 * 86_400, now() + 3600],
        )
        .unwrap();
    let body = json(session(&client).await).await;
    let end = body["expires_at"].as_i64().unwrap();
    assert!((end - (now() + 86_400)).abs() <= 2, "{end}");
    // Expired: no user, and every route that needs one says so.
    db(dir.path())
        .execute("UPDATE sessions SET expires_at = ?1", [now() - 1])
        .unwrap();
    let body = json(session(&client).await).await;
    assert_eq!(body["user"], serde_json::Value::Null);
    let res = client
        .get(format!("{base}/api/projects"))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 401);
    assert_eq!(json(res).await["code"], "not_signed_in");
}

/// Signing out everywhere ends every session of the account and closes
/// every one of its sync sockets with 4401; another account keeps its own.
#[tokio::test]
async fn signing_out_everywhere_ends_every_session() {
    let dir = tempfile::tempdir().unwrap();
    accounts(dir.path(), &["a@example.org", "b@example.org"]);
    let server = Server::start(dir.path());
    let base = &server.base;
    let (one, one_cookie) = log_in(base).await;
    let (two, two_cookie) = log_in(base).await;
    let (other, other_cookie) = log_in_as(base, "b@example.org").await;
    let project = new_project(&one, base).await;
    let theirs = new_project(&other, base).await;
    let mut a = Peer::connect(base, &one_cookie, &project, 11).await;
    let mut b = Peer::connect(base, &two_cookie, &project, 12).await;
    let mut c = Peer::connect(base, &other_cookie, &theirs, 13).await;
    let res = two
        .post(format!("{base}/api/logout-everywhere"))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 204);
    assert_eq!(closed_with(&mut a.ws).await, 4401);
    assert_eq!(closed_with(&mut b.ws).await, 4401);
    for client in [&one, &two] {
        let res = client
            .get(format!("{base}/api/projects"))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 401);
    }
    c.type_text(" still here").await;
    c.saved().await;
    // Without a session there is nothing to sign out of.
    let res = one
        .post(format!("{base}/api/logout-everywhere"))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 401);
}

/// A password change needs the current password, counts a wrong one as a
/// failed sign-in, and ends every other session of the account.
#[tokio::test]
async fn a_password_change_ends_the_other_sessions() {
    let dir = tempfile::tempdir().unwrap();
    let server = Server::start(dir.path());
    let base = &server.base;
    sign_up(base).await;
    let (here, here_cookie) = log_in(base).await;
    let (_, there_cookie) = log_in(base).await;
    let project = new_project(&here, base).await;
    let mut mine = Peer::connect(base, &here_cookie, &project, 11).await;
    let mut theirs = Peer::connect(base, &there_cookie, &project, 12).await;
    let change = |current: &str, new: &str| {
        let (here, base) = (here.clone(), base.clone());
        let body = serde_json::json!({"current_password": current, "new_password": new});
        async move {
            here.post(format!("{base}/api/account/password"))
                .json(&body)
                .send()
                .await
                .unwrap()
        }
    };
    let res = change("wrong one", "periwinkle shell").await;
    assert_eq!(res.status(), 401);
    let body = json(res).await;
    assert_eq!(body["code"], "wrong_credentials");
    assert_eq!(body["field"], "current_password");
    let res = change(PASSWORD, "short").await;
    assert_eq!(res.status(), 400);
    assert_eq!(json(res).await["field"], "new_password");
    assert_eq!(change(PASSWORD, "periwinkle shell").await.status(), 204);
    assert_eq!(closed_with(&mut theirs.ws).await, 4401);
    // This session and its socket go on.
    mine.type_text(" Sea lettuce").await;
    mine.saved().await;
    assert_eq!(
        here.get(format!("{base}/api/projects"))
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    assert_eq!(
        try_log_in(base, "a@example.org", PASSWORD).await.status(),
        401
    );
    assert_eq!(
        try_log_in(base, "a@example.org", "periwinkle shell")
            .await
            .status(),
        200
    );
}

/// Wrong current passwords slow down like failed sign-ins: after three, the
/// next attempt waits, even with the right password.
#[tokio::test]
async fn wrong_current_passwords_are_slowed_down() {
    let dir = tempfile::tempdir().unwrap();
    let server = Server::start(dir.path());
    let base = &server.base;
    let (client, _) = sign_up(base).await;
    let body =
        serde_json::json!({"current_password": "nope nope", "new_password": "periwinkle shell"});
    for _ in 0..3 {
        let res = client
            .post(format!("{base}/api/account/password"))
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 401);
    }
    let right =
        serde_json::json!({"current_password": PASSWORD, "new_password": "periwinkle shell"});
    let res = client
        .post(format!("{base}/api/account/password"))
        .json(&right)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 429);
    assert!(res.headers().contains_key("retry-after"));
    assert_eq!(json(res).await["code"], "too_many_attempts");
}

/// A hash stored today still verifies after the server is started with
/// stronger parameters, and that sign-in replaces it with a hash made with
/// the new parameters.
#[tokio::test]
async fn a_hash_from_today_is_rehashed_when_the_parameters_rise() {
    let dir = tempfile::tempdir().unwrap();
    let stored = |dir: &std::path::Path| -> String {
        db(dir)
            .query_row("SELECT password_hash FROM users", [], |r| r.get(0))
            .unwrap()
    };
    {
        let server = Server::start(dir.path());
        sign_up(&server.base).await;
    }
    let today = stored(dir.path());
    assert!(today.contains("m=19456,t=2,p=1"), "{today}");
    let server = Server::start_with(
        dir.path(),
        &[
            "--password-iterations",
            "3",
            "--password-memory-kib",
            "32768",
        ],
    );
    let (client, _) = log_in(&server.base).await;
    let rehashed = stored(dir.path());
    assert!(rehashed.starts_with("$argon2id$"), "{rehashed}");
    assert!(rehashed.contains("m=32768,t=3,p=1"), "{rehashed}");
    assert_ne!(rehashed, today);
    // The new hash works, and the next sign-in leaves it alone.
    log_in(&server.base).await;
    assert_eq!(stored(dir.path()), rehashed);
    let res = client
        .get(format!("{}/api/session", server.base))
        .send()
        .await
        .unwrap();
    assert_eq!(json(res).await["user"]["email"], "a@example.org");
    // Weaker parameters than the defaults are refused at start.
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_nisaba"))
        .args([
            "check",
            "--data",
            dir.path().to_str().unwrap(),
            "--password-iterations",
            "1",
        ])
        .output()
        .unwrap();
    assert!(!out.status.success());
}

/// An administrator makes, lists and deletes accounts. Nobody else may, and
/// the last administrator and a project's only owner stay.
#[tokio::test]
async fn administrators_manage_accounts() {
    let dir = tempfile::tempdir().unwrap();
    let server = Server::start(dir.path());
    let base = &server.base;
    let (admin, _) = sign_up(base).await;
    let users = format!("{base}/api/admin/users");
    let make = |email: &str, password: &str| {
        let (admin, users) = (admin.clone(), users.clone());
        let body = serde_json::json!({"email": email, "name": "Warden", "password": password});
        async move { admin.post(users).json(&body).send().await.unwrap() }
    };
    let res = make("Warden@Example.org", PASSWORD).await;
    assert_eq!(res.status(), 201);
    let warden = json(res).await;
    assert_eq!(warden["email"], "warden@example.org");
    assert_eq!(warden["is_admin"], false);
    let res = make("warden@example.org", PASSWORD).await;
    assert_eq!(res.status(), 409);
    assert_eq!(json(res).await["code"], "email_taken");
    let long = format!("{}@example.org", "w".repeat(254 - 12 + 1));
    let longest = format!("{}@example.org", "w".repeat(254 - 12));
    assert_eq!(make(&longest, PASSWORD).await.status(), 201);
    for (email, password, field) in [
        ("no at sign", PASSWORD, "email"),
        (long.as_str(), PASSWORD, "email"),
        ("w2@example.org", "short", "password"),
    ] {
        let res = make(email, password).await;
        assert_eq!(res.status(), 400, "{email}");
        assert_eq!(json(res).await["field"], field);
    }
    let list = json(admin.get(&users).send().await.unwrap()).await;
    let emails: Vec<_> = list
        .as_array()
        .unwrap()
        .iter()
        .map(|u| u["email"].clone())
        .collect();
    assert_eq!(
        emails,
        ["a@example.org", "warden@example.org", longest.as_str()]
    );

    let (warden_client, warden_cookie) = log_in_as(base, "warden@example.org").await;
    for res in [
        warden_client.get(&users).send().await.unwrap(),
        warden_client
            .post(&users)
            .json(&serde_json::json!({"email": "x@example.org", "password": PASSWORD}))
            .send()
            .await
            .unwrap(),
    ] {
        assert_eq!(res.status(), 403);
        assert_eq!(json(res).await["code"], "forbidden");
    }
    // The only owner of a project stays until the project goes.
    let project = new_project(&warden_client, base).await;
    let mut socket = Peer::connect(base, &warden_cookie, &project, 11).await;
    let id = warden["id"].as_str().unwrap();
    let res = admin.delete(format!("{users}/{id}")).send().await.unwrap();
    assert_eq!(res.status(), 409);
    assert_eq!(json(res).await["code"], "sole_owner");
    let res = warden_client
        .delete(format!("{base}/api/projects/{project}"))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 204);
    assert_eq!(closed_with(&mut socket.ws).await, 4004);
    let other = new_project(&admin, base).await;
    admin
        .post(format!("{base}/api/projects/{other}/members"))
        .json(&serde_json::json!({"email": "warden@example.org", "role": "editor"}))
        .send()
        .await
        .unwrap();
    let mut socket = Peer::connect(base, &warden_cookie, &other, 12).await;
    assert_eq!(
        admin
            .delete(format!("{users}/{id}"))
            .send()
            .await
            .unwrap()
            .status(),
        204
    );
    assert_eq!(closed_with(&mut socket.ws).await, 4401);
    assert_eq!(
        try_log_in(base, "warden@example.org", PASSWORD)
            .await
            .status(),
        401
    );
    let res = admin.delete(format!("{users}/{id}")).send().await.unwrap();
    assert_eq!(res.status(), 404);
    // The last administrator cannot delete themself, once no project
    // holds them as its only owner.
    let res = admin
        .delete(format!("{base}/api/projects/{other}"))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 204);
    let me = json(
        admin
            .get(format!("{base}/api/session"))
            .send()
            .await
            .unwrap(),
    )
    .await;
    let me = me["user"]["id"].as_str().unwrap().to_owned();
    let res = admin.delete(format!("{users}/{me}")).send().await.unwrap();
    assert_eq!(res.status(), 409);
    assert_eq!(json(res).await["code"], "last_admin");
}

#[tokio::test]
async fn a_name_can_change_within_its_limits() {
    let dir = tempfile::tempdir().unwrap();
    let server = Server::start(dir.path());
    let base = &server.base;
    let (client, _) = sign_up(base).await;
    let rename = |name: String| {
        let (client, base) = (client.clone(), base.clone());
        async move {
            client
                .patch(format!("{base}/api/account"))
                .json(&serde_json::json!({ "name": name }))
                .send()
                .await
                .unwrap()
        }
    };
    let res = rename("  Tide curator ".into()).await;
    assert_eq!(res.status(), 200);
    assert_eq!(json(res).await["name"], "Tide curator");
    for bad in [String::new(), "x".repeat(101), "a\u{7}b".into()] {
        let res = rename(bad).await;
        assert_eq!(res.status(), 400);
        assert_eq!(json(res).await["code"], "invalid_field");
    }
    assert_eq!(rename("y".repeat(100)).await.status(), 200);
}

/// Setup checks its fields after it checks that no account exists.
#[tokio::test]
async fn setup_checks_its_fields() {
    let dir = tempfile::tempdir().unwrap();
    let server = Server::start(dir.path());
    let base = &server.base;
    for (body, field) in [
        (
            serde_json::json!({"email": "a@example.org", "password": "seven c"}),
            "password",
        ),
        (
            serde_json::json!({"email": "nobody", "password": PASSWORD}),
            "email",
        ),
        (
            serde_json::json!({"email": "a@example.org", "password": "x".repeat(1025)}),
            "password",
        ),
    ] {
        let res = browser(base)
            .post(format!("{base}/api/setup"))
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 400);
        assert_eq!(json(res).await["field"], field);
    }
    // Exactly eight characters is enough.
    let res = browser(base)
        .post(format!("{base}/api/setup"))
        .json(&serde_json::json!({"email": "a@example.org", "password": "eight ch"}))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 201);
}

/// A socket from a signed-out session is told so, and a heartbeat keeps a
/// live one answering.
#[tokio::test]
async fn a_heartbeat_is_answered() {
    let dir = tempfile::tempdir().unwrap();
    let server = Server::start(dir.path());
    let base = &server.base;
    let (client, cookie) = sign_up(base).await;
    let project = new_project(&client, base).await;
    let mut peer = Peer::connect(base, &cookie, &project, 11).await;
    peer.ws
        .send(Message::Binary(Frame::Heartbeat.encode().into()))
        .await
        .unwrap();
    let got = tokio::time::timeout(Duration::from_secs(5), peer.recv())
        .await
        .unwrap();
    assert!(matches!(got, In::Frame(Frame::Heartbeat)));
}
