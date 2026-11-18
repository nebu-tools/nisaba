//! Running the server: its command line, settings from a file and the
//! environment, the health check, logs that hold no private data, a stop
//! that finishes the writes it has, and one server per data directory.

mod common;

use std::io::Write as _;
use std::process::{Command, Stdio};
use std::time::Duration;

use common::*;
use futures_util::{SinkExt as _, StreamExt as _};
use loro::{ExportMode, VersionVector};
use nisaba_sync::{Frame, PART};
use tokio_tungstenite::tungstenite::Message;

fn nisaba(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_nisaba"))
        .args(args)
        .output()
        .unwrap()
}

fn text(out: &std::process::Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

#[test]
fn the_command_line_says_what_it_takes() {
    let out = nisaba(&[]);
    assert_eq!(out.status.code(), Some(2));
    assert!(text(&out).contains("usage: nisaba COMMAND"));
    for help in ["help", "--help"] {
        let out = nisaba(&[help]);
        assert!(out.status.success());
        for word in [
            "serve",
            "check",
            "setup",
            "backup",
            "restore",
            "packages add",
            "NISABA_EXPORT_MEMORY_MB",
        ] {
            assert!(text(&out).contains(word), "{help}: {word}");
        }
    }
    let out = nisaba(&["version"]);
    assert!(text(&out).starts_with("nisaba "));
    let out = nisaba(&["sail"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(text(&out).contains("unknown command sail"));
    let out = nisaba(&["serve", "--colour", "blue"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(text(&out).contains("unknown argument --colour"));
    // `nisaba api` prints the committed description.
    let out = nisaba(&["api"]);
    let committed = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../api/openapi.json"
    ))
    .unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout), committed);
}

/// `nisaba check` prints every setting and fails on one the server would
/// refuse.
#[test]
fn check_prints_the_settings_and_finds_problems() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().to_str().unwrap();
    let out = nisaba(&["check", "--data", data, "--export-seconds", "9"]);
    assert!(out.status.success(), "{}", text(&out));
    let said = text(&out);
    for line in [
        "export.seconds = 9",
        "listen = 127.0.0.1:8080",
        "the settings are fine",
        "exports are",
    ] {
        assert!(said.contains(line), "{line}: {said}");
    }
    assert!(
        !dir.path().join("nisaba.db").exists(),
        "check makes no database"
    );
    let out = nisaba(&["check", "--data", data, "--listen", "0.0.0.0:8080"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(text(&out).contains("--public-url"), "{}", text(&out));
    let config = dir.path().join("nisaba.toml");
    std::fs::write(&config, "[export]\nmemory = 512\n").unwrap();
    let out = nisaba(&["check", "--config", config.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        text(&out).contains("unknown setting export.memory"),
        "{}",
        text(&out)
    );
    let out = Command::new(env!("CARGO_BIN_EXE_nisaba"))
        .args(["check", "--data", data])
        .env("NISABA_EXPORT_MEMORY_MB", "12")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(
        text(&out).contains("NISABA_EXPORT_MEMORY_MB"),
        "{}",
        text(&out)
    );
}

/// A file and the environment set the server: here the log format and a
/// public https address, under which the server asks for a setup code.
#[test]
fn the_server_reads_its_settings_from_a_file_and_the_environment() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("nisaba.toml");
    std::fs::write(&config, "log_format = \"json\"\n[export]\nqueue = 0\n").unwrap();
    let data = dir.path().join("data");
    let server = Server::launch(
        &data,
        &[],
        &[
            ("NISABA_CONFIG", config.to_str().unwrap()),
            ("NISABA_PUBLIC_URL", "https://nisaba.example.org"),
        ],
    );
    assert!(server.setup_code.is_some(), "{:?}", server.start);
    assert!(
        server
            .start
            .iter()
            .any(|l| l == "public address https://nisaba.example.org")
    );
    drop(server);
}

/// `nisaba setup` makes the administrator from the terminal, once.
#[tokio::test]
async fn setup_from_the_command_line_makes_the_first_account() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().to_str().unwrap();
    let setup = |password: &str| {
        let mut child = Command::new(env!("CARGO_BIN_EXE_nisaba"))
            .args([
                "setup",
                "--email",
                "Curator@Example.org",
                "--name",
                "Curator",
                "--data",
                data,
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        writeln!(child.stdin.take().unwrap(), "{password}").unwrap();
        child.wait_with_output().unwrap()
    };
    let out = setup("short");
    assert_eq!(out.status.code(), Some(1));
    assert!(text(&out).contains("at least 8"));
    let out = setup(PASSWORD);
    assert!(out.status.success(), "{}", text(&out));
    let out = setup(PASSWORD);
    assert_eq!(out.status.code(), Some(1));
    assert!(text(&out).contains("already has an administrator"));
    let server = Server::start(dir.path());
    let res = try_log_in(&server.base, "curator@example.org", PASSWORD).await;
    assert_eq!(res.status(), 200);
    let user: serde_json::Value = res.json().await.unwrap();
    assert_eq!(user["is_admin"], true);
    assert_eq!(user["name"], "Curator");
    // With an account, the web setup is closed too.
    let res = browser(&server.base)
        .post(format!("{}/api/setup", server.base))
        .json(&serde_json::json!({"email": "b@example.org", "password": PASSWORD}))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 409);
}

/// Two servers never share a data directory.
#[test]
fn a_second_server_on_one_data_directory_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let _first = Server::start(dir.path());
    let out = nisaba(&[
        "serve",
        "--data",
        dir.path().to_str().unwrap(),
        "--listen",
        "127.0.0.1:0",
    ]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        text(&out).contains("another nisaba process"),
        "{}",
        text(&out)
    );
}

#[tokio::test]
async fn the_health_check_answers() {
    let dir = tempfile::tempdir().unwrap();
    let server = Server::start(dir.path());
    let res = reqwest::get(format!("{}/api/health", server.base))
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    let body: serde_json::Value = res.json().await.unwrap();
    assert_eq!(body["status"], "ok");
    assert_eq!(body["web_client"], true);
    assert!(["sandboxed", "unsandboxed", "off"].contains(&body["exports"].as_str().unwrap()));
    assert_eq!(body["version"], env!("CARGO_PKG_VERSION"));
}

/// The logs, at the most detailed level, hold no password, no session
/// token, no email address and no document text, in either format.
#[tokio::test]
async fn the_logs_hold_no_private_data() {
    for format in ["text", "json"] {
        let dir = tempfile::tempdir().unwrap();
        let server = Server::launch(
            dir.path(),
            &["--log-format", format],
            &[("RUST_LOG", "debug")],
        );
        let base = &server.base;
        let secret = "Tidepool-secret-paragraph";
        let (client, cookie) = sign_up(base).await;
        assert_eq!(
            try_log_in(base, "a@example.org", "wrong password")
                .await
                .status(),
            401
        );
        let project = project_of(&client, base, &format!("= {secret}\n#broken(")).await;
        let mut peer = Peer::connect(base, &cookie, &project, 91).await;
        peer.type_text(" another-secret-sentence").await;
        peer.saved().await;
        assert_eq!(export(&client, base, &project).await.status(), 422);
        client
            .post(format!(
                "{base}/api/projects/{project}/assets?path=private-name.png"
            ))
            .body(DOT)
            .send()
            .await
            .unwrap();
        client
            .post(format!("{base}/api/logout"))
            .send()
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
        let logs = server.logs();
        assert!(
            logs.contains("request"),
            "{format}: no request was logged: {logs}"
        );
        let token = cookie.split_once('=').unwrap().1;
        for private in [
            PASSWORD,
            "wrong password",
            token,
            "a@example.org",
            secret,
            "another-secret-sentence",
            "private-name",
        ] {
            assert!(
                !logs.contains(private),
                "{format}: the log holds {private:?}"
            );
        }
        if format == "json" {
            for line in logs.lines().filter(|l| !l.trim().is_empty()) {
                serde_json::from_str::<serde_json::Value>(line)
                    .unwrap_or_else(|e| panic!("not a JSON line ({e}): {line}"));
            }
        }
    }
}

/// On SIGTERM the server closes each socket with 1012 after the update it
/// is on: every update it acknowledged before is saved, it exits with 0
/// within the grace time, and the write-ahead log is folded into the
/// database.
#[tokio::test]
async fn stopping_finishes_the_writes_it_has() {
    let dir = tempfile::tempdir().unwrap();
    let server = Server::start(dir.path());
    let (client, cookie) = sign_up(&server.base).await;
    let project = new_project(&client, &server.base).await;
    let mut peer = Peer::connect(&server.base, &cookie, &project, 95).await;
    // Updates go out without waiting for their Acks.
    for i in 0..40 {
        let text = peer.main_text();
        text.insert(text.len_unicode(), &format!(" [{i}]")).unwrap();
        peer.doc.commit();
        let update = peer.doc.export(ExportMode::updates(&peer.acked)).unwrap();
        peer.ws
            .send(Message::Binary(Frame::Update(update).encode().into()))
            .await
            .unwrap();
        peer.acked = peer.doc.oplog_vv();
    }
    let pid = server.child.id().to_string();
    assert!(
        Command::new("kill")
            .args(["-TERM", &pid])
            .status()
            .unwrap()
            .success()
    );
    let mut acked = VersionVector::default();
    let code = loop {
        match tokio::time::timeout(Duration::from_secs(10), peer.ws.next())
            .await
            .unwrap()
        {
            Some(Ok(Message::Binary(bytes))) => {
                if let Ok(Frame::Ack(vv)) = Frame::decode(&bytes, PART) {
                    acked = VersionVector::decode(&vv).unwrap();
                }
            }
            Some(Ok(Message::Close(frame))) => break frame.map(|f| u16::from(f.code)),
            Some(Ok(_)) => {}
            other => panic!("the socket ended without a close: {other:?}"),
        }
    };
    assert_eq!(code, Some(1012));
    let status = server.terminate(10);
    assert!(status.success(), "{status:?}");
    let wal = dir.path().join("nisaba.db-wal");
    assert!(
        std::fs::metadata(&wal).map_or(0, |m| m.len()) == 0,
        "the log was not folded in"
    );
    let server = Server::start(dir.path());
    let (client, _) = log_in(&server.base).await;
    let saved = main_text(&client, &server.base, &project).await;
    // Every acknowledged edit is saved: the saved text holds each edit up
    // to the last one the Acks covered.
    let shown = peer.doc.fork_at(&peer.doc.vv_to_frontiers(&acked)).unwrap();
    let acked_text = nisaba_server::project::files(&shown)
        .into_iter()
        .find(|f| f.path == "main.typ")
        .and_then(|f| f.text)
        .unwrap();
    assert!(
        saved.starts_with(&acked_text),
        "{saved:?} lacks {acked_text:?}"
    );
}
