//! Projects and members against the real binary: making, listing,
//! renaming and deleting projects, members and roles, and what each change
//! does to an open sync socket.

#![allow(
    clippy::too_many_lines,
    reason = "an end-to-end story reads best as one test"
)]

mod common;

use std::time::{Duration, Instant};

use common::*;
use futures_util::StreamExt as _;
use tokio_tungstenite::tungstenite::Message;

async fn json(res: reqwest::Response) -> serde_json::Value {
    res.json().await.unwrap()
}

fn db(dir: &std::path::Path) -> rusqlite::Connection {
    rusqlite::Connection::open(dir.join("nisaba.db")).unwrap()
}

/// The close code and the reason of the next close frame.
async fn close_of(ws: &mut Ws) -> (u16, String) {
    loop {
        let msg = tokio::time::timeout(Duration::from_secs(10), ws.next())
            .await
            .expect("a close within 10 s");
        match msg {
            Some(Ok(Message::Close(Some(frame)))) => {
                return (frame.code.into(), frame.reason.to_string());
            }
            Some(Ok(_)) => {}
            other => panic!("socket ended without a close frame: {other:?}"),
        }
    }
}

async fn add(
    owner: &reqwest::Client,
    base: &str,
    project: &str,
    email: &str,
    role: &str,
) -> reqwest::Response {
    owner
        .post(format!("{base}/api/projects/{project}/members"))
        .json(&serde_json::json!({"email": email, "role": role}))
        .send()
        .await
        .unwrap()
}

async fn set_role(
    owner: &reqwest::Client,
    base: &str,
    project: &str,
    user: &str,
    role: &str,
) -> reqwest::Response {
    owner
        .patch(format!("{base}/api/projects/{project}/members/{user}"))
        .json(&serde_json::json!({ "role": role }))
        .send()
        .await
        .unwrap()
}

#[tokio::test]
async fn projects_are_made_listed_renamed_and_read() {
    let dir = tempfile::tempdir().unwrap();
    accounts(dir.path(), &["a@example.org", "b@example.org"]);
    let server = Server::start(dir.path());
    let base = &server.base;
    let (alice, _) = log_in(base).await;
    let (bob, _) = log_in_as(base, "b@example.org").await;
    let res = alice
        .post(format!("{base}/api/projects"))
        .json(&serde_json::json!({"name": "  Rock pools  "}))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 201);
    let first = json(res).await;
    assert_eq!(first["name"], "Rock pools");
    assert_eq!(first["role"], "owner");
    let first = first["id"].as_str().unwrap().to_owned();
    // Without files, a project starts with main.typ.
    let files = json(
        alice
            .get(format!("{base}/api/projects/{first}/files"))
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(files[0]["path"], "main.typ");
    let second = new_project(&alice, base).await;
    // The list is sorted by the last change, newest first.
    db(dir.path())
        .execute(
            "UPDATE projects SET updated_at = updated_at - 600 WHERE id = ?1",
            [&second],
        )
        .unwrap();
    let list = json(
        alice
            .get(format!("{base}/api/projects"))
            .send()
            .await
            .unwrap(),
    )
    .await;
    let ids: Vec<_> = list
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["id"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(ids, [first.clone(), second.clone()]);
    // An edit moves a project up, at most once a minute.
    let (_, cookie) = log_in(base).await;
    let mut peer = Peer::connect(base, &cookie, &second, 11).await;
    peer.type_text(" Wrack").await;
    peer.saved().await;
    let list = json(
        alice
            .get(format!("{base}/api/projects"))
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(list[0]["id"], second.as_str());

    let rename = |client: &reqwest::Client, name: &str| {
        let (client, base, first) = (client.clone(), base.clone(), first.clone());
        let body = serde_json::json!({ "name": name });
        async move {
            client
                .patch(format!("{base}/api/projects/{first}"))
                .json(&body)
                .send()
                .await
                .unwrap()
        }
    };
    let res = rename(&alice, "Rock pools of the north shore").await;
    assert_eq!(res.status(), 200);
    assert_eq!(json(res).await["name"], "Rock pools of the north shore");
    for bad in ["", "   ", &"x".repeat(201)] {
        assert_eq!(rename(&alice, bad).await.status(), 400, "{bad:?}");
    }
    assert_eq!(rename(&alice, &"x".repeat(200)).await.status(), 200);
    // Bob is not a member: the project does not exist for him.
    for res in [
        rename(&bob, "Mine now").await,
        bob.get(format!("{base}/api/projects/{first}"))
            .send()
            .await
            .unwrap(),
        bob.delete(format!("{base}/api/projects/{first}"))
            .send()
            .await
            .unwrap(),
    ] {
        assert_eq!(res.status(), 404);
        assert_eq!(json(res).await["code"], "not_found");
    }
    assert_eq!(
        json(
            bob.get(format!("{base}/api/projects"))
                .send()
                .await
                .unwrap()
        )
        .await,
        serde_json::json!([])
    );
}

/// The files of a new project are checked before anything is stored.
#[tokio::test]
async fn a_new_project_checks_its_files() {
    let dir = tempfile::tempdir().unwrap();
    let server = Server::start(dir.path());
    let base = &server.base;
    let (client, _) = sign_up(base).await;
    let make = |files: serde_json::Value| {
        let (client, base) = (client.clone(), base.clone());
        async move {
            client
                .post(format!("{base}/api/projects"))
                .json(&serde_json::json!({"name": "Kelp", "files": files}))
                .send()
                .await
                .unwrap()
        }
    };
    for (files, field) in [
        (
            serde_json::json!([{"path": "../main.typ", "text": ""}]),
            "files[0].path",
        ),
        (
            serde_json::json!([{"path": "/main.typ", "text": ""}]),
            "files[0].path",
        ),
        (
            serde_json::json!([{"path": "a.typ", "text": ""}, {"path": "a.typ", "text": ""}]),
            "files[1].path",
        ),
        (
            serde_json::json!([{"path": "a.typ", "text": "", "base64": ""}]),
            "files[0]",
        ),
        (serde_json::json!([{"path": "a.typ"}]), "files[0]"),
        (
            serde_json::json!([{"path": "a.png", "base64": "!!"}]),
            "files[0].base64",
        ),
    ] {
        let res = make(files.clone()).await;
        assert_eq!(res.status(), 400, "{files}");
        assert_eq!(json(res).await["field"], field, "{files}");
    }
    // Text past the 32 MiB a project may hold.
    let big = "x".repeat(33 << 20);
    let res = make(serde_json::json!([{"path": "main.typ", "text": big}])).await;
    assert_eq!(res.status(), 413);
    assert_eq!(json(res).await["code"], "project_full");
    // A body past the 64 MiB an import may be.
    let huge = "x".repeat((64 << 20) + 1);
    let res = make(serde_json::json!([{"path": "main.typ", "text": huge}])).await;
    assert_eq!(res.status(), 413);
    assert_eq!(json(res).await["code"], "payload_too_large");
    // Nothing of the refused projects is stored.
    assert_eq!(
        json(
            client
                .get(format!("{base}/api/projects"))
                .send()
                .await
                .unwrap()
        )
        .await,
        serde_json::json!([])
    );
    let assets = std::fs::read_dir(dir.path().join("assets"))
        .unwrap()
        .count();
    assert_eq!(assets, 0);
}

#[tokio::test]
async fn members_are_added_changed_and_removed() {
    let dir = tempfile::tempdir().unwrap();
    accounts(
        dir.path(),
        &["a@example.org", "b@example.org", "c@example.org"],
    );
    let server = Server::start(dir.path());
    let base = &server.base;
    let (owner, _) = log_in(base).await;
    let (bob, _) = log_in_as(base, "b@example.org").await;
    let (cat, _) = log_in_as(base, "c@example.org").await;
    let project = new_project(&owner, base).await;
    let res = add(&owner, base, &project, "nobody@example.org", "viewer").await;
    assert_eq!(res.status(), 400);
    assert_eq!(json(res).await["field"], "email");
    let res = add(&owner, base, &project, "B@example.org", "viewer").await;
    assert_eq!(res.status(), 201);
    let member = json(res).await;
    assert_eq!(member["role"], "viewer");
    let bob_id = member["user_id"].as_str().unwrap().to_owned();
    let res = add(&owner, base, &project, "b@example.org", "editor").await;
    assert_eq!(res.status(), 409);
    assert_eq!(json(res).await["code"], "already_member");
    let res = owner
        .post(format!("{base}/api/projects/{project}/members"))
        .json(&serde_json::json!({"email": "c@example.org", "role": "admiral"}))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 400);
    assert_eq!(json(res).await["code"], "invalid_request");

    // A viewer reads, and may neither upload nor manage members.
    assert_eq!(
        bob.get(format!("{base}/api/projects/{project}/files"))
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    let res = bob
        .post(format!("{base}/api/projects/{project}/assets?path=dot.png"))
        .body(DOT)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 403);
    assert_eq!(
        add(&bob, base, &project, "c@example.org", "viewer")
            .await
            .status(),
        403
    );
    let members = json(
        bob.get(format!("{base}/api/projects/{project}/members"))
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(members.as_array().unwrap().len(), 2);

    // The only owner can neither leave nor step down.
    let owner_id = members
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["role"] == "owner")
        .unwrap()["user_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let res = set_role(&owner, base, &project, &owner_id, "editor").await;
    assert_eq!(res.status(), 409);
    assert_eq!(json(res).await["code"], "last_owner");
    let res = owner
        .delete(format!("{base}/api/projects/{project}/members/{owner_id}"))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 409);
    // With a second owner, the first may step down.
    let res = set_role(&owner, base, &project, &bob_id, "owner").await;
    assert_eq!(res.status(), 200);
    assert_eq!(json(res).await["role"], "owner");
    assert_eq!(
        set_role(&owner, base, &project, &owner_id, "editor")
            .await
            .status(),
        200
    );
    assert_eq!(
        set_role(&owner, base, &project, "nobody", "editor")
            .await
            .status(),
        403,
        "an editor now"
    );
    assert_eq!(
        set_role(&bob, base, &project, "nobody", "editor")
            .await
            .status(),
        404
    );

    // A member who is not an owner may remove only themself.
    add(&bob, base, &project, "c@example.org", "editor").await;
    let cat_id =
        json(cat.get(format!("{base}/api/session")).send().await.unwrap()).await["user"]["id"]
            .as_str()
            .unwrap()
            .to_owned();
    let res = cat
        .delete(format!("{base}/api/projects/{project}/members/{owner_id}"))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 403);
    let res = cat
        .delete(format!("{base}/api/projects/{project}/members/{cat_id}"))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 204);
    assert_eq!(
        cat.get(format!("{base}/api/projects/{project}"))
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
}

/// A role change reaches an open socket at once: an editor who becomes a
/// viewer is closed with 4003, and a viewer who becomes an editor may write
/// on the socket they have.
#[tokio::test]
async fn a_role_change_reaches_an_open_socket_at_once() {
    let dir = tempfile::tempdir().unwrap();
    accounts(dir.path(), &["a@example.org", "b@example.org"]);
    let server = Server::start(dir.path());
    let base = &server.base;
    let (owner, _) = log_in(base).await;
    let (_, bob_cookie) = log_in_as(base, "b@example.org").await;
    let project = new_project(&owner, base).await;
    let bob = json(add(&owner, base, &project, "b@example.org", "editor").await).await;
    let bob_id = bob["user_id"].as_str().unwrap();

    let mut editor = Peer::connect(base, &bob_cookie, &project, 21).await;
    editor.type_text(" Barnacles").await;
    editor.saved().await;
    let changed = Instant::now();
    assert_eq!(
        set_role(&owner, base, &project, bob_id, "viewer")
            .await
            .status(),
        200
    );
    let (code, reason) = close_of(&mut editor.ws).await;
    assert_eq!(code, 4003, "{reason}");
    assert!(reason.contains("role"), "{reason}");
    assert!(changed.elapsed() < Duration::from_secs(2));

    // As a viewer, an update is refused with 4003 and not stored.
    let mut viewer = Peer::connect(base, &bob_cookie, &project, 22).await;
    viewer.type_text(" Refused").await;
    assert_eq!(closed_with(&mut viewer.ws).await, 4003);
    assert!(!main_text(&owner, base, &project).await.contains("Refused"));

    // A viewer's socket stays open through a promotion and may then write.
    let mut promoted = Peer::connect(base, &bob_cookie, &project, 23).await;
    assert_eq!(
        set_role(&owner, base, &project, bob_id, "editor")
            .await
            .status(),
        200
    );
    promoted.type_text(" Mussels").await;
    promoted.saved().await;
    assert!(main_text(&owner, base, &project).await.contains("Mussels"));
    // A commenter may not change the document yet (build step N4).
    assert_eq!(
        set_role(&owner, base, &project, bob_id, "commenter")
            .await
            .status(),
        200
    );
    assert_eq!(close_of(&mut promoted.ws).await.0, 4003);
}

/// Issue 61: an expired session and a lost membership close a socket with
/// different codes and reasons: 4401 for the session, 4004 for the
/// project. No close code 4091 exists.
#[tokio::test]
async fn an_expired_session_and_a_lost_membership_close_differently() {
    let dir = tempfile::tempdir().unwrap();
    let server = InProcess::start(dir.path()).await;
    let base = &server.base;
    let (owner, _) = sign_up(base).await;
    let project = new_project(&owner, base).await;
    let bea = account(dir.path(), "b@example.org", false);
    add(&owner, base, &project, "b@example.org", "editor").await;
    // The owner's session expires; Bea loses her membership.
    let owner_id = server.user_id("a@example.org");
    server
        .store
        .create_session_for("short-lived", &owner_id, 2)
        .unwrap();
    let mut expiring = Peer::connect(base, "nisaba_session=short-lived", &project, 31).await;
    let (_, cookie) = log_in_as(base, "b@example.org").await;
    let mut removed = Peer::connect(base, &cookie, &project, 32).await;
    let res = owner
        .delete(format!("{base}/api/projects/{project}/members/{bea}"))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 204);
    let (code, reason) = close_of(&mut removed.ws).await;
    assert_eq!(code, 4004);
    assert!(reason.contains("no longer a member"), "{reason}");
    let (code, reason) = close_of(&mut expiring.ws).await;
    assert_eq!(code, 4401);
    assert!(reason.contains("expired"), "{reason}");
    assert!(!nisaba_sync::close::ALL.iter().any(|c| c.code == 4091));
}

/// Deleting a project closes its sockets with 4004, removes its rows, and
/// removes the asset files no other project holds.
#[tokio::test]
async fn deleting_a_project_closes_its_sockets_and_removes_its_data() {
    let dir = tempfile::tempdir().unwrap();
    accounts(dir.path(), &["a@example.org", "b@example.org"]);
    let server = Server::start(dir.path());
    let base = &server.base;
    let (owner, cookie) = log_in(base).await;
    let (bob, _) = log_in_as(base, "b@example.org").await;
    let doomed = project_of(&owner, base, WITH_IMAGE).await;
    let kept = project_of(&owner, base, WITH_IMAGE).await;
    let shared = upload_dot(&owner, base, &doomed).await;
    assert_eq!(upload_dot(&owner, base, &kept).await, shared);
    let own = owner
        .post(format!("{base}/api/projects/{doomed}/assets?path=only.bin"))
        .body(b"only in the doomed project".to_vec())
        .send()
        .await
        .unwrap();
    let only = json(own).await["hash"].as_str().unwrap().to_owned();
    let file = |hash: &str| dir.path().join("assets").join(&hash[..2]).join(&hash[2..]);
    assert!(file(&only).exists() && file(&shared).exists());
    add(&owner, base, &doomed, "b@example.org", "editor").await;
    let mut socket = Peer::connect(base, &cookie, &doomed, 41).await;
    socket.type_text(" Doomed").await;
    socket.saved().await;
    // Only an owner may delete.
    let res = bob
        .delete(format!("{base}/api/projects/{doomed}"))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 403);
    let res = owner
        .delete(format!("{base}/api/projects/{doomed}"))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 204);
    let (code, reason) = close_of(&mut socket.ws).await;
    assert_eq!(code, 4004);
    assert!(reason.contains("deleted"), "{reason}");
    for path in ["", "/files", "/members"] {
        let res = owner
            .get(format!("{base}/api/projects/{doomed}{path}"))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 404, "{path}");
    }
    let mut again = socket_to(base, &cookie, &doomed).await;
    assert_eq!(close_code(&mut again).await, 4004);
    let conn = db(dir.path());
    for table in ["members", "doc_updates", "doc_snapshots", "project_assets"] {
        let n: i64 = conn
            .query_row(
                &format!("SELECT count(*) FROM {table} WHERE project_id = ?1"),
                [&doomed],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 0, "{table}");
    }
    assert!(!file(&only).exists(), "the asset only it held is gone");
    assert!(
        file(&shared).exists(),
        "the asset another project holds stays"
    );
    let res = owner
        .get(format!("{base}/api/projects/{kept}/assets/{shared}"))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    let res = export(&owner, base, &kept).await;
    assert_eq!(res.status(), 200);
}

async fn socket_to(base: &str, cookie: &str, project: &str) -> Ws {
    socket(base, cookie, project).await.unwrap()
}
