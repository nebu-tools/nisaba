//! The store under failure: the server stopped at each point of a write, a
//! write-ahead log cut in the middle of its last transaction, a disk that
//! fills up, and compaction while several clients write.
//!
//! The rule in each case: nothing the server acknowledged is lost, and
//! nothing half written is ever read back.

#![allow(
    clippy::too_many_lines,
    reason = "an end-to-end story reads best as one test"
)]

mod common;

use std::time::Duration;

use common::*;
use futures_util::{SinkExt as _, StreamExt as _};
use loro::{ExportMode, VersionVector};
use nisaba_sync::{Frame, PART, RoomStore as _, split};
use tokio_tungstenite::tungstenite::Message;

/// What came of one update sent to a server that may die.
enum Sent {
    Acked,
    /// The socket ended first.
    Lost,
}

/// Types `word` at the end of main.typ and waits for its Ack, or for the
/// socket to end.
async fn type_and_wait(peer: &mut Peer, word: &str) -> Sent {
    let text = peer.main_text();
    text.insert(text.len_unicode(), word).unwrap();
    peer.doc.commit();
    let update = peer.doc.export(ExportMode::updates(&peer.acked)).unwrap();
    let local = peer.doc.oplog_vv();
    for message in split(&Frame::Update(update), PART) {
        if peer.ws.send(Message::Binary(message.into())).await.is_err() {
            return Sent::Lost;
        }
    }
    loop {
        let Ok(Some(Ok(msg))) = tokio::time::timeout(Duration::from_secs(20), peer.ws.next()).await
        else {
            return Sent::Lost;
        };
        let Message::Binary(bytes) = msg else {
            if matches!(msg, Message::Close(_)) {
                return Sent::Lost;
            }
            continue;
        };
        if let Ok(Frame::Ack(vv)) = Frame::decode(&bytes, PART) {
            let vv = VersionVector::decode(&vv).unwrap();
            peer.acked = vv.clone();
            if vv.includes_vv(&local) {
                return Sent::Acked;
            }
        }
    }
}

/// Starts a server that will die at `point`, makes a project before that,
/// and returns the data directory, the project and the setup's cookie.
async fn prepared() -> (tempfile::TempDir, String) {
    let dir = tempfile::tempdir().unwrap();
    let server = Server::start(dir.path());
    let (client, _) = sign_up(&server.base).await;
    let project = new_project(&client, &server.base).await;
    server.kill9();
    (dir, project)
}

/// Every file under assets/ has a row and every row a file.
fn assets_agree(dir: &std::path::Path) {
    let store = nisaba_server::store::Store::open_shared(dir).unwrap();
    let rows: std::collections::BTreeSet<String> = store
        .asset_hashes()
        .unwrap()
        .into_iter()
        .map(|(h, _)| h)
        .collect();
    let mut files = std::collections::BTreeSet::new();
    for head in std::fs::read_dir(dir.join("assets")).unwrap().flatten() {
        for tail in std::fs::read_dir(head.path()).unwrap().flatten() {
            files.insert(format!(
                "{}{}",
                head.file_name().to_string_lossy(),
                tail.file_name().to_string_lossy()
            ));
        }
    }
    assert_eq!(rows, files, "asset rows and files disagree");
}

/// The server is killed at each point of an update's append and of a
/// compaction. After a restart, every acknowledged edit is there, the one
/// that was on its way is there once or not at all, and the client's
/// resend makes the server equal to the client.
#[tokio::test]
async fn a_crash_at_any_point_of_a_write_loses_nothing_acknowledged() {
    for point in [
        "append:before-commit",
        "append:after-commit",
        "compact:snapshot-written",
        "compact:before-commit",
        "compact:after-commit",
    ] {
        let (dir, project) = prepared().await;
        let server = Server::launch(dir.path(), &[], &[("NISABA_CRASH_AT", point)]);
        let (_, cookie) = log_in(&server.base).await;
        let mut peer = Peer::connect(&server.base, &cookie, &project, 51).await;
        let base_text = peer.main_text().to_string();
        let mut acked = base_text.clone();
        let mut last = None;
        for i in 0..200 {
            // A compaction is due after 8 MiB of log: the second edit is
            // that large, so the room compacts after it.
            let word = if point.starts_with("compact") && i == 1 {
                noise(8 << 20, 6)
            } else {
                format!(" w{i}")
            };
            match type_and_wait(&mut peer, &word).await {
                Sent::Acked => acked.push_str(&word),
                Sent::Lost => {
                    last = Some(word);
                    break;
                }
            }
        }
        let last = last.unwrap_or_else(|| panic!("{point}: the server never stopped"));
        let status = server.wait_for_exit(10);
        assert!(!status.success(), "{point}: {status:?}");

        let server = Server::start(dir.path());
        let (client, cookie) = log_in(&server.base).await;
        let saved = main_text(&client, &server.base, &project).await;
        let with_last = format!("{acked}{last}");
        assert!(
            saved == acked || saved == with_last,
            "{point}: the server holds {} characters, {} were acknowledged",
            saved.len(),
            acked.len()
        );
        // The client sends what the server lacks; then both agree.
        let mut again = Peer::connect(&server.base, &cookie, &project, 52).await;
        again
            .doc
            .import(&peer.doc.export(ExportMode::all_updates()).unwrap())
            .unwrap();
        again.push_saved().await;
        assert_eq!(
            main_text(&client, &server.base, &project).await,
            with_last,
            "{point}"
        );
        let store = nisaba_server::store::Store::open_shared(dir.path()).unwrap();
        let stored = store.load(&project).unwrap();
        if point == "compact:after-commit" {
            assert!(stored.snapshot.is_some(), "{point}");
        }
        drop(server);
    }
}

/// The server is killed while it stores an asset, and while it makes or
/// deletes a project. After a restart, asset files and rows agree, and a
/// project is there whole or not at all.
#[tokio::test]
async fn a_crash_while_storing_files_and_projects_leaves_no_half() {
    for point in ["asset:file-written", "asset:renamed", "asset:after-commit"] {
        let (dir, project) = prepared().await;
        let server = Server::launch(dir.path(), &[], &[("NISABA_CRASH_AT", point)]);
        let (client, _) = log_in(&server.base).await;
        let res = client
            .post(format!(
                "{}/api/projects/{project}/assets?path=dot.png",
                server.base
            ))
            .body(DOT)
            .send()
            .await;
        assert!(res.is_err(), "{point}: the upload was answered");
        server.wait_for_exit(10);
        let server = Server::start(dir.path());
        assets_agree(dir.path());
        let (client, _) = log_in(&server.base).await;
        let files = client
            .get(format!("{}/api/projects/{project}/files", server.base))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert!(!files.contains("dot.png"), "{point}");
        // The upload works when sent again, and adds one entry.
        upload_dot(&client, &server.base, &project).await;
        let files: serde_json::Value = client
            .get(format!("{}/api/projects/{project}/files", server.base))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let dots = files
            .as_array()
            .unwrap()
            .iter()
            .filter(|f| f["path"] == "dot.png")
            .count();
        assert_eq!(dots, 1, "{point}");
        assets_agree(dir.path());
    }

    // A project is made whole: its first document is in the same commit.
    let (dir, _) = prepared().await;
    let server = Server::launch(dir.path(), &[], &[("NISABA_CRASH_AT", "project:created")]);
    let (client, _) = log_in(&server.base).await;
    let made = client
        .post(format!("{}/api/projects", server.base))
        .json(&serde_json::json!({"name": "Kelp forest", "files": [
            {"path": "main.typ", "text": "= Kelp forest\n"},
            {"path": "dot.png", "base64": "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP4z8DwHwAFAAH/iZk9HQAAAABJRU5ErkJggg=="},
        ]}))
        .send()
        .await;
    assert!(made.is_err());
    server.wait_for_exit(10);
    let server = Server::start(dir.path());
    let (client, _) = log_in(&server.base).await;
    let list: serde_json::Value = client
        .get(format!("{}/api/projects", server.base))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let kelp = list
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == "Kelp forest")
        .expect("the project is there");
    let id = kelp["id"].as_str().unwrap();
    assert!(
        main_text(&client, &server.base, id)
            .await
            .contains("Kelp forest")
    );
    assert_eq!(export(&client, &server.base, id).await.status(), 200);
    assets_agree(dir.path());

    // A deleted project is gone after a crash before its files went, and
    // the next start removes them.
    let doomed = project_of(&client, &server.base, WITH_IMAGE).await;
    let hash = upload_dot(&client, &server.base, &doomed).await;
    let other = client
        .post(format!(
            "{}/api/projects/{doomed}/assets?path=only.bin",
            server.base
        ))
        .body(b"only here".to_vec())
        .send()
        .await
        .unwrap();
    assert_eq!(other.status(), 201);
    drop(server);
    let server = Server::launch(dir.path(), &[], &[("NISABA_CRASH_AT", "project:deleted")]);
    let (client, _) = log_in(&server.base).await;
    assert!(
        client
            .delete(format!("{}/api/projects/{doomed}", server.base))
            .send()
            .await
            .is_err()
    );
    server.wait_for_exit(10);
    let server = Server::start(dir.path());
    let (client, _) = log_in(&server.base).await;
    let res = client
        .get(format!("{}/api/projects/{doomed}", server.base))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 404);
    assets_agree(dir.path());
    let kept = dir.path().join("assets").join(&hash[..2]).join(&hash[2..]);
    assert!(kept.exists(), "the image another project holds stays");
}

/// The write-ahead log is cut at every length inside its last transaction,
/// as by a crash in the middle of writing it. Each cut database opens, and
/// holds every update before that transaction and the last one whole or
/// not at all.
#[tokio::test]
async fn a_torn_log_is_never_read_as_an_update() {
    let (dir, project) = prepared().await;
    let wal = dir.path().join("nisaba.db-wal");
    // Some acknowledged edits, then one more whose commit the crash ends.
    let server = Server::start(dir.path());
    let (_, cookie) = log_in(&server.base).await;
    let mut peer = Peer::connect(&server.base, &cookie, &project, 61).await;
    for i in 0..5 {
        assert!(matches!(
            type_and_wait(&mut peer, &format!(" t{i}")).await,
            Sent::Acked
        ));
    }
    let acked = peer.main_text().to_string();
    server.kill9();
    let before = std::fs::metadata(&wal).unwrap().len();
    let server = Server::launch(
        dir.path(),
        &[],
        &[("NISABA_CRASH_AT", "append:after-commit")],
    );
    let (_, cookie) = log_in(&server.base).await;
    let mut peer = Peer::connect(&server.base, &cookie, &project, 62).await;
    let last = noise(20_000, 4);
    assert!(matches!(type_and_wait(&mut peer, &last).await, Sent::Lost));
    server.wait_for_exit(10);
    let after = std::fs::metadata(&wal).unwrap().len();
    assert!(after > before + 20_000, "{before} {after}");
    let full = std::fs::read(&wal).unwrap();
    let step = ((after - before) / 40).max(1);
    let mut cuts: Vec<u64> = (before..=after)
        .step_by(usize::try_from(step).unwrap())
        .collect();
    cuts.extend([after - 1, after - 8, before + 1, before + 32]);
    let mut whole = 0;
    for cut in cuts {
        let copy = tempfile::tempdir().unwrap();
        std::fs::copy(dir.path().join("nisaba.db"), copy.path().join("nisaba.db")).unwrap();
        std::fs::write(
            copy.path().join("nisaba.db-wal"),
            &full[..usize::try_from(cut).unwrap()],
        )
        .unwrap();
        let store = nisaba_server::store::Store::open(copy.path()).unwrap();
        let stored = store.load(&project).unwrap();
        let doc = loro::LoroDoc::new();
        if let Some(s) = &stored.snapshot {
            doc.import(s).unwrap();
        }
        for update in &stored.log {
            doc.import(update).expect("every stored update is whole");
        }
        let text = main_of(&doc);
        if text == format!("{acked}{last}") {
            whole += 1;
        } else {
            assert_eq!(text, acked, "cut at {cut} of {before}..{after}");
        }
    }
    assert!(whole >= 1, "the uncut log holds the last update");
}

fn main_of(doc: &loro::LoroDoc) -> String {
    let files = nisaba_server::project::files(doc);
    files
        .into_iter()
        .find(|f| f.path == "main.typ")
        .and_then(|f| f.text)
        .unwrap_or_default()
}

/// Past a file size limit every write fails, as on a full disk. The server
/// refuses the update that does not fit (4500, no Ack) and the upload (507),
/// stays up, and keeps everything it acknowledged.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_full_disk_refuses_the_write_and_keeps_what_was_saved() {
    let (dir, project) = prepared().await;
    // `ulimit -f` counts blocks of 1 KiB in bash: 6 MiB per file.
    let script = format!(
        "ulimit -f 6144 && exec {} \"$@\"",
        env!("CARGO_BIN_EXE_nisaba")
    );
    let mut child = std::process::Command::new("bash")
        .args(["-c", &script, "nisaba", "serve", "--data"])
        .arg(dir.path())
        .args(["--listen", "127.0.0.1:0"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let base = {
        use std::io::BufRead as _;
        let mut lines = std::io::BufReader::new(child.stdout.take().unwrap()).lines();
        let base = lines
            .by_ref()
            .map_while(Result::ok)
            .find_map(|l| {
                l.split_whitespace()
                    .find(|w| w.starts_with("http://"))
                    .map(str::to_owned)
            })
            .unwrap();
        std::thread::spawn(move || for _ in lines {});
        base
    };
    let (client, cookie) = log_in(&base).await;
    let mut peer = Peer::connect(&base, &cookie, &project, 71).await;
    let mut acked = peer.main_text().to_string();
    let mut refused = None;
    for i in 0..40u64 {
        let word = noise(400_000, i);
        match type_and_wait(&mut peer, &word).await {
            Sent::Acked => acked.push_str(&word),
            Sent::Lost => {
                refused = Some(word);
                break;
            }
        }
    }
    let refused = refused.expect("the limit was never reached");
    assert!(
        acked.len() > 1_000_000,
        "{} bytes acknowledged",
        acked.len()
    );
    // The server is up and says the disk is full for an upload.
    let health = client
        .get(format!("{base}/api/health"))
        .send()
        .await
        .unwrap();
    assert_eq!(health.status(), 200);
    let res = client
        .post(format!("{base}/api/projects/{project}/assets?path=big.bin"))
        .body(vec![7u8; 7 << 20])
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 507);
    let body: serde_json::Value = res.json().await.unwrap();
    assert_eq!(body["code"], "storage_full");
    let text = main_text(&client, &base, &project).await;
    assert_eq!(text, acked, "the refused update is not there");
    child.kill().unwrap();
    child.wait().unwrap();
    // Without the limit, everything acknowledged is there, and the refused
    // update can be sent again.
    let server = Server::start(dir.path());
    let (client, cookie) = log_in(&server.base).await;
    assert_eq!(main_text(&client, &server.base, &project).await, acked);
    let mut again = Peer::connect(&server.base, &cookie, &project, 72).await;
    again
        .doc
        .import(&peer.doc.export(ExportMode::all_updates()).unwrap())
        .unwrap();
    again.push_saved().await;
    assert_eq!(
        main_text(&client, &server.base, &project).await,
        format!("{acked}{refused}")
    );
    assets_agree(dir.path());
}

/// Four clients type 150 edits each into one project at once, and one of
/// them sends a 9 MiB update in the middle, so the room compacts by bytes
/// and by entries while edits keep coming. Every edit is acknowledged,
/// saved once, and survives a restart; the log was compacted.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn compaction_under_load_keeps_every_acknowledged_update() {
    let dir = tempfile::tempdir().unwrap();
    let server = Server::start(dir.path());
    let (client, cookie) = sign_up(&server.base).await;
    let project = new_project(&client, &server.base).await;
    let big = noise(9 << 20, 8);
    let writers: Vec<_> = (0..4u64)
        .map(|w| {
            let (base, cookie, project) = (server.base.clone(), cookie.clone(), project.clone());
            let big = big.clone();
            tokio::spawn(async move {
                let mut peer = Peer::connect(&base, &cookie, &project, 100 + w).await;
                for i in 0..150 {
                    let text = peer.main_text();
                    text.insert(0, &format!("<{w}:{i}>")).unwrap();
                    if w == 0 && i == 40 {
                        text.insert(text.len_unicode(), &big).unwrap();
                        peer.push_reading().await;
                        continue;
                    }
                    peer.push().await;
                    // A client reads while it writes; waiting for the Acks
                    // now and then also takes in what the others sent.
                    if i % 5 == 4 {
                        peer.saved().await;
                    }
                }
                peer.saved().await;
                peer
            })
        })
        .collect();
    let mut peers = Vec::new();
    for w in writers {
        peers.push(w.await.unwrap());
    }
    let text = main_text(&client, &server.base, &project).await;
    for w in 0..4 {
        for i in 0..150 {
            assert_eq!(text.matches(&format!("<{w}:{i}>")).count(), 1, "<{w}:{i}>");
        }
    }
    assert_eq!(text.matches(&big).count(), 1);
    // The compaction of the last batch runs after its Ack; wait for it.
    let store = nisaba_server::store::Store::open_shared(dir.path()).unwrap();
    assert!(
        within(20, || store.log_len(&project).unwrap() < 500).await,
        "the log was never compacted: {} rows",
        store.log_len(&project).unwrap()
    );
    assert!(store.load(&project).unwrap().snapshot.is_some());
    server.kill9();
    let server = Server::start(dir.path());
    let (client, _) = log_in(&server.base).await;
    assert_eq!(main_text(&client, &server.base, &project).await, text);
}
