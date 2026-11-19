//! Many rooms opened and closed: each closes when nothing holds it, and the
//! server's memory does not grow from one round to the next. This file is
//! its own test process, so the memory it reads is the server's alone.

mod common;

use common::*;

/// The resident memory of this process, in KiB.
#[cfg(target_os = "linux")]
fn rss_kib() -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .unwrap()
        .lines()
        .find_map(|l| l.strip_prefix("VmRSS:"))
        .and_then(|v| v.trim().trim_end_matches("kB").trim().parse().ok())
        .unwrap()
}

#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rooms_close_and_their_memory_returns() {
    let dir = tempfile::tempdir().unwrap();
    let server = InProcess::start(dir.path()).await;
    let base = &server.base;
    let (client, cookie) = sign_up(base).await;
    let mut projects = Vec::new();
    for i in 0..120u64 {
        projects.push(project_of(&client, base, &noise(20_000, 2 * i + 1)).await);
    }
    let rooms = server.app.rooms();
    let _ = rooms.sweep();
    assert_eq!(rooms.len(), 0);
    let mut rounds = Vec::new();
    for round in 0..6 {
        // Every room opens: some by a read, some by a socket that edits.
        for (i, project) in projects.iter().enumerate() {
            if i % 4 == 0 {
                let mut peer = Peer::connect(base, &cookie, project, 1000 + i as u64).await;
                peer.type_text(&format!(" r{round}")).await;
                peer.saved().await;
            } else {
                main_text(&client, base, project).await;
            }
        }
        assert!(
            rooms.len() >= projects.len() * 3 / 4,
            "{} rooms open",
            rooms.len()
        );
        // When every socket is gone, a sweep closes every room.
        assert!(
            within(10, || {
                let _ = rooms.sweep();
                rooms.is_empty()
            })
            .await,
            "{} rooms stay open",
            rooms.len()
        );
        assert!(within(10, || server.store.grants().is_empty()).await);
        rounds.push(rss_kib());
    }
    // The first round grows the allocator's pools; after that, memory
    // stays where it was.
    let settled = rounds[1];
    let last = *rounds.last().unwrap();
    eprintln!("resident memory after each round, KiB: {rounds:?}");
    assert!(
        last <= settled + settled / 5 + 8 * 1024,
        "memory grew from {settled} KiB to {last} KiB over the rounds: {rounds:?}"
    );

    // Memory per idle room, for the measurements in the pull request.
    let before = rss_kib();
    let mut open = Vec::new();
    for project in &projects {
        open.push(rooms.get(project).await.unwrap());
    }
    let held = rss_kib();
    eprintln!(
        "{} idle rooms of about 20 KB of text hold {} KiB, {} KiB each",
        open.len(),
        held.saturating_sub(before),
        held.saturating_sub(before) / open.len() as u64
    );
    drop(open);
    let _ = rooms.sweep();
    assert!(rooms.is_empty());
}
