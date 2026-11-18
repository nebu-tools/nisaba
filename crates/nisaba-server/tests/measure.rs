//! Measurements for the pull request text, not checks. Run them on a
//! release build:
//!
//! ```sh
//! cargo test --release -p nisaba-server --test measure -- --ignored --nocapture
//! ```
//!
//! Each prints its numbers with the machine's load average at the time.

#![allow(
    clippy::too_many_lines,
    reason = "an end-to-end story reads best as one test"
)]

mod common;

use std::time::{Duration, Instant};

use common::*;
use loro::ExportMode;

fn load() -> String {
    std::fs::read_to_string("/proc/loadavg")
        .unwrap_or_default()
        .trim()
        .to_owned()
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

/// Median and 95th percentile, in milliseconds.
fn spread(mut times: Vec<Duration>) -> (f64, f64) {
    times.sort();
    let at = |percent: usize| ms(times[(times.len() - 1) * percent / 100]);
    (at(50), at(95))
}

fn rss_kib() -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .unwrap_or_default()
        .lines()
        .find_map(|l| l.strip_prefix("VmRSS:"))
        .and_then(|v| v.trim().trim_end_matches("kB").trim().parse().ok())
        .unwrap_or(0)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "a measurement; run it by hand on a release build"]
async fn measure() {
    let dir = tempfile::tempdir().unwrap();
    let server = InProcess::start(dir.path()).await;
    let base = &server.base;
    let (client, cookie) = sign_up(base).await;
    println!("load average at start: {}", load());

    // Memory of an idle room with 20 KB of text, measured first, before
    // the allocator holds memory that later steps freed. Making a project
    // opens no room.
    let mut projects = Vec::new();
    for i in 0..200u64 {
        projects.push(project_of(&client, base, &noise(20_000, 2 * i + 1)).await);
    }
    let _ = server.app.rooms().sweep();
    let before = rss_kib();
    let mut open = Vec::new();
    for p in &projects {
        open.push(server.app.rooms().get(p).await.unwrap());
    }
    let after = rss_kib();
    println!(
        "idle rooms: {} KiB for {} rooms of 20 KB of text, {} KiB each",
        after.saturating_sub(before),
        open.len(),
        after.saturating_sub(before) / open.len() as u64
    );
    // Sign-in: one request after another, the Argon2id hash included.
    let mut times = Vec::new();
    for _ in 0..30 {
        let started = Instant::now();
        let res = try_log_in(base, "a@example.org", PASSWORD).await;
        times.push(started.elapsed());
        assert_eq!(res.status(), 200);
    }
    let (median, p95) = spread(times);
    println!("sign-in: median {median:.1} ms, 95th percentile {p95:.1} ms (30 sign-ins)");

    // Opening a room with 1 MB of text: the room loads from the store, and
    // a client connects and gets the whole document in its Welcome.
    let text = noise(1 << 20, 5);
    let project = project_of(&client, base, &text).await;
    let mut loads = Vec::new();
    let mut connects = Vec::new();
    for _ in 0..10 {
        server.app.rooms().evict(&project).await;
        let started = Instant::now();
        let room = server.app.rooms().get(&project).await.unwrap();
        loads.push(started.elapsed());
        drop(room);
        server.app.rooms().evict(&project).await;
        let started = Instant::now();
        let peer = Peer::connect(base, &cookie, &project, 7).await;
        connects.push(started.elapsed());
        assert_eq!(peer.main_text().len_unicode(), text.len());
    }
    let (load_median, load_p95) = spread(loads);
    let (connect_median, connect_p95) = spread(connects);
    println!(
        "room with 1 MiB of text: load {load_median:.1} ms median ({load_p95:.1} ms 95th), \
         connect to Welcome {connect_median:.1} ms median ({connect_p95:.1} ms 95th)"
    );

    // A snapshot plus a long log: the 1 MiB project above gets 499 more
    // edits, one fewer than makes a compaction due, after one compaction.
    let mut peer = Peer::connect(base, &cookie, &project, 9).await;
    for i in 0..520 {
        let t = peer.main_text();
        t.insert(0, &format!("{i} ")).unwrap();
        peer.push().await;
    }
    peer.saved().await;
    drop(peer);
    let store = &server.store;
    assert!(
        within(20, || store.log_len(&project).unwrap() < 500).await,
        "no compaction"
    );
    let mut peer = Peer::connect(base, &cookie, &project, 10).await;
    while store.log_len(&project).unwrap() < 499 {
        let t = peer.main_text();
        t.insert(0, "x").unwrap();
        peer.push_saved().await;
    }
    drop(peer);
    let rows = store.log_len(&project).unwrap();
    let mut opens = Vec::new();
    for _ in 0..10 {
        server.app.rooms().evict(&project).await;
        let started = Instant::now();
        let room = server.app.rooms().get(&project).await.unwrap();
        opens.push(started.elapsed());
        drop(room);
    }
    let (open_median, open_p95) = spread(opens);
    println!(
        "room with a snapshot of 1 MiB of text and {rows} log entries: open {open_median:.1} ms \
         median ({open_p95:.1} ms 95th)"
    );

    // Update to Ack over loopback, one edit at a time.
    let latency = new_project(&client, base).await;
    let mut peer = Peer::connect(base, &cookie, &latency, 11).await;
    let mut times = Vec::new();
    for i in 0..300 {
        let started = Instant::now();
        peer.type_text(&format!(" {i}")).await;
        peer.saved().await;
        times.push(started.elapsed());
    }
    let (ack_median, ack_p95) = spread(times);
    println!(
        "update to Ack: median {ack_median:.1} ms, 95th percentile {ack_p95:.1} ms (300 edits)"
    );

    // Updates one room takes: one client sends 2000 small edits without
    // waiting, then waits for the Ack of the last; then four clients at once.
    let busy = new_project(&client, base).await;
    let mut peer = Peer::connect(base, &cookie, &busy, 8).await;
    let started = Instant::now();
    for i in 0..2000 {
        let t = peer.main_text();
        t.insert(t.len_unicode(), &format!(" {i}")).unwrap();
        peer.push().await;
    }
    peer.saved().await;
    let one = 2000.0 / started.elapsed().as_secs_f64();
    let started = Instant::now();
    let tasks: Vec<_> = (0..4u64)
        .map(|w| {
            let (base, cookie, busy) = (base.clone(), cookie.clone(), busy.clone());
            tokio::spawn(async move {
                let mut peer = Peer::connect(&base, &cookie, &busy, 20 + w).await;
                for i in 0..500 {
                    let t = peer.main_text();
                    t.insert(0, &format!("{w}.{i} ")).unwrap();
                    peer.doc.commit();
                    let update = peer.doc.export(ExportMode::updates(&peer.acked)).unwrap();
                    peer.acked = peer.doc.oplog_vv();
                    send(&mut peer.ws, &nisaba_sync::Frame::Update(update)).await;
                    if i % 50 == 49 {
                        peer.acked = loro::VersionVector::default();
                        peer.saved().await;
                    }
                }
                peer.saved().await;
            })
        })
        .collect();
    for t in tasks {
        t.await.unwrap();
    }
    let four = 2000.0 / started.elapsed().as_secs_f64();
    println!("one room: {one:.0} updates a second from one client, {four:.0} from four at once");

    println!("load average at the end: {}", load());
}
