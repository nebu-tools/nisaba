//! `nisaba backup` while clients write, `nisaba restore` into an empty
//! directory, and the refusals of both.

#![allow(
    clippy::too_many_lines,
    reason = "an end-to-end story reads best as one test"
)]

mod common;

use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use common::*;

fn nisaba(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_nisaba"))
        .args(args)
        .output()
        .unwrap()
}

fn backup(data: &Path, dest: &Path) -> std::process::Output {
    nisaba(&[
        "backup",
        dest.to_str().unwrap(),
        "--data",
        data.to_str().unwrap(),
    ])
}

fn restore(from: &Path, data: &Path) -> std::process::Output {
    nisaba(&[
        "restore",
        from.to_str().unwrap(),
        "--data",
        data.to_str().unwrap(),
    ])
}

fn ok(out: &std::process::Output) -> String {
    assert!(
        out.status.success(),
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn failed(out: &std::process::Output) -> String {
    assert!(
        !out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// Every project as `/api/projects/{id}/files` shows it, by name.
async fn every_project(client: &reqwest::Client, base: &str) -> Vec<(String, serde_json::Value)> {
    let list: serde_json::Value = client
        .get(format!("{base}/api/projects"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let mut out = Vec::new();
    for p in list.as_array().unwrap() {
        let id = p["id"].as_str().unwrap();
        let files: serde_json::Value = client
            .get(format!("{base}/api/projects/{id}/files"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        out.push((id.to_owned(), files));
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// A backup taken while four clients write is one moment of the server:
/// restored into an empty directory, each project holds every edit
/// acknowledged before the backup began, in order, and at most those sent
/// before it ended. Its assets are there and whole. A second backup after
/// the writes stop copies only what is new, and its restore equals the
/// server in every project.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_backup_under_writes_restores_every_project() {
    let dir = tempfile::tempdir().unwrap();
    let server = Server::start(dir.path());
    let base = server.base.clone();
    let (client, cookie) = sign_up(&base).await;
    let mut projects = Vec::new();
    for i in 0..4u64 {
        let project = project_of(&client, &base, WITH_IMAGE).await;
        upload_dot(&client, &base, &project).await;
        let res = client
            .post(format!(
                "{base}/api/projects/{project}/assets?path=notes-{i}.txt"
            ))
            .body(noise(200_000, 2 * i + 1).into_bytes())
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 201);
        projects.push(project);
    }
    let stop = Arc::new(AtomicBool::new(false));
    let acked: Arc<Vec<AtomicUsize>> = Arc::new((0..4).map(|_| AtomicUsize::new(0)).collect());
    let sent: Arc<Vec<AtomicUsize>> = Arc::new((0..4).map(|_| AtomicUsize::new(0)).collect());
    let writers: Vec<_> = projects
        .iter()
        .enumerate()
        .map(|(w, project)| {
            let (base, cookie, project) = (base.clone(), cookie.clone(), project.clone());
            let (stop, acked, sent) = (stop.clone(), acked.clone(), sent.clone());
            tokio::spawn(async move {
                let mut peer = Peer::connect(&base, &cookie, &project, 200 + w as u64).await;
                let mut i = 0;
                while !stop.load(Ordering::SeqCst) {
                    sent[w].store(i + 1, Ordering::SeqCst);
                    peer.type_text(&format!(" [{i}]")).await;
                    peer.saved().await;
                    acked[w].store(i + 1, Ordering::SeqCst);
                    i += 1;
                }
            })
        })
        .collect();
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    let before: Vec<usize> = acked.iter().map(|a| a.load(Ordering::SeqCst)).collect();
    let dest = tempfile::tempdir().unwrap();
    let data = dir.path().to_owned();
    let out = {
        let dest = dest.path().to_owned();
        tokio::task::spawn_blocking(move || backup(&data, &dest))
            .await
            .unwrap()
    };
    let after: Vec<usize> = sent.iter().map(|a| a.load(Ordering::SeqCst)).collect();
    let report = ok(&out);
    assert!(report.contains("4 projects"), "{report}");
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    stop.store(true, Ordering::SeqCst);
    for w in writers {
        w.await.unwrap();
    }

    let restored = tempfile::tempdir().unwrap();
    let target = restored.path().join("data");
    ok(&restore(dest.path(), &target));
    {
        let copy = Server::start(&target);
        let (copy_client, _) = log_in(&copy.base).await;
        for (w, project) in projects.iter().enumerate() {
            let text = main_text(&copy_client, &copy.base, project).await;
            let edits = text.matches(" [").count();
            assert!(
                edits >= before[w] && edits <= after[w],
                "project {w}: {edits} edits; {} were saved before the backup, {} sent by its end",
                before[w],
                after[w]
            );
            for i in 0..edits {
                assert!(
                    text.contains(&format!(" [{i}]")),
                    "project {w} lacks edit {i}"
                );
            }
            let hash = upload_hash(&copy_client, &copy.base, project).await;
            let res = copy_client
                .get(format!(
                    "{}/api/projects/{project}/assets/{hash}",
                    copy.base
                ))
                .send()
                .await
                .unwrap();
            assert_eq!(res.status(), 200);
            assert_eq!(res.bytes().await.unwrap().as_ref(), DOT);
        }
        assert_eq!(
            export(&copy_client, &copy.base, &projects[0])
                .await
                .status(),
            200
        );
    }

    // A second backup into the same place copies only what is new.
    let report = ok(&backup(dir.path(), dest.path()));
    assert!(report.contains("0 assets copied"), "{report}");
    assert!(report.contains("5 already there"), "{report}");
    let again = tempfile::tempdir().unwrap();
    ok(&restore(dest.path(), again.path()));
    let live = every_project(&client, &base).await;
    drop(server);
    let copy = Server::start(again.path());
    let (copy_client, _) = log_in(&copy.base).await;
    assert_eq!(every_project(&copy_client, &copy.base).await, live);
}

/// The hash of `dot.png` in a project.
async fn upload_hash(client: &reqwest::Client, base: &str, project: &str) -> String {
    let files: serde_json::Value = client
        .get(format!("{base}/api/projects/{project}/files"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    files
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["path"] == "dot.png")
        .unwrap()["hash"]
        .as_str()
        .unwrap()
        .to_owned()
}

/// Restore never writes over data, refuses a damaged backup, and backup
/// does not write into the directory it copies.
#[tokio::test]
async fn restore_and_backup_refuse_what_could_lose_data() {
    let dir = tempfile::tempdir().unwrap();
    {
        let server = Server::start(dir.path());
        let (client, _) = sign_up(&server.base).await;
        let project = new_project(&client, &server.base).await;
        upload_dot(&client, &server.base, &project).await;
    }
    let dest = tempfile::tempdir().unwrap();
    ok(&backup(dir.path(), dest.path()));
    // Not over a data directory that holds anything.
    let err = failed(&restore(dest.path(), dir.path()));
    assert!(err.contains("not empty"), "{err}");
    // Not from something that is no backup.
    let empty = tempfile::tempdir().unwrap();
    let err = failed(&restore(empty.path(), &empty.path().join("data")));
    assert!(err.contains("no nisaba.db"), "{err}");
    // Not into itself.
    let err = failed(&backup(dir.path(), dir.path()));
    assert!(err.contains("somewhere else"), "{err}");
    // Not with a damaged asset, and nothing is left behind.
    let assets = dest.path().join("assets");
    let head = std::fs::read_dir(&assets)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let file = std::fs::read_dir(&head)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    std::fs::write(&file, b"not the image").unwrap();
    let target = tempfile::tempdir().unwrap();
    let err = failed(&restore(dest.path(), &target.path().join("data")));
    assert!(err.contains("damaged"), "{err}");
    assert!(!target.path().join("data").exists());
    assert_eq!(std::fs::read_dir(target.path()).unwrap().count(), 0);
    // A damaged database.
    std::fs::write(
        dest.path().join("nisaba.db"),
        b"SQLite format 3\0 and then nothing",
    )
    .unwrap();
    let err = failed(&restore(dest.path(), &target.path().join("data")));
    assert!(err.contains("database"), "{err}");
}

/// A backup killed after it copied the database, before the assets, leaves
/// a destination that the next run completes.
#[tokio::test]
async fn an_interrupted_backup_is_completed_by_the_next() {
    let dir = tempfile::tempdir().unwrap();
    {
        let server = Server::start(dir.path());
        let (client, _) = sign_up(&server.base).await;
        let project = project_of(&client, &server.base, WITH_IMAGE).await;
        upload_dot(&client, &server.base, &project).await;
    }
    let dest = tempfile::tempdir().unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_nisaba"))
        .args([
            "backup",
            dest.path().to_str().unwrap(),
            "--data",
            dir.path().to_str().unwrap(),
        ])
        .env("NISABA_CRASH_AT", "backup:database-copied")
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(
        !dest.path().join("assets").exists()
            || std::fs::read_dir(dest.path().join("assets"))
                .unwrap()
                .count()
                == 0
    );
    let report = ok(&backup(dir.path(), dest.path()));
    assert!(report.contains("1 assets copied"), "{report}");
    let target = tempfile::tempdir().unwrap();
    ok(&restore(dest.path(), &target.path().join("data")));
    let server = Server::start(&target.path().join("data"));
    let (client, _) = log_in(&server.base).await;
    let list: serde_json::Value = client
        .get(format!("{}/api/projects", server.base))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let id = list[0]["id"].as_str().unwrap();
    assert_eq!(export(&client, &server.base, id).await.status(), 200);
}
