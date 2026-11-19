//! Export from the HTTP API end to end: a project with an administrator's
//! font, a font of its own, packages from a registry, images and a
//! bibliography; PDF/A; the views of tracked changes; and the package
//! cache that the browser and the export share.

#![allow(
    clippy::too_many_lines,
    reason = "an end-to-end story reads best as one test"
)]

mod common;

use std::fmt::Write as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use common::*;
use nisaba_compile_core::FileContents;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

/// A package registry on a free port, serving `files` by path and counting
/// the requests.
struct Registry {
    url: String,
    hits: Arc<AtomicUsize>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Registry {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn registry(files: Vec<(String, Vec<u8>)>) -> Registry {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let hits = Arc::new(AtomicUsize::new(0));
    let files = Arc::new(files);
    let counted = Arc::clone(&hits);
    let task = tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let (files, hits) = (Arc::clone(&files), Arc::clone(&counted));
            tokio::spawn(async move {
                let mut request = Vec::new();
                let mut buf = [0u8; 1024];
                while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                    match socket.read(&mut buf).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => request.extend_from_slice(&buf[..n]),
                    }
                }
                let line = String::from_utf8_lossy(&request);
                let path = line.split_whitespace().nth(1).unwrap_or("").to_owned();
                hits.fetch_add(1, Ordering::SeqCst);
                let found = files
                    .iter()
                    .find(|(p, _)| *p == path)
                    .map(|(_, b)| b.clone());
                let head = match &found {
                    Some(body) => format!(
                        "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                        body.len()
                    ),
                    None => {
                        "HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
                            .into()
                    }
                };
                let _ = socket.write_all(head.as_bytes()).await;
                if let Some(body) = found {
                    let _ = socket.write_all(&body).await;
                }
                let _ = socket.shutdown().await;
            });
        }
    });
    Registry { url, hits, task }
}

/// A `.tar.gz` of `files`.
fn package(files: &[(&str, &str)]) -> Vec<u8> {
    use std::io::Write as _;
    let mut tar = tar::Builder::new(Vec::new());
    for (path, data) in files {
        let mut header = tar::Header::new_gnu();
        header.set_size(data.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        tar.append_data(&mut header, path, data.as_bytes()).unwrap();
    }
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    gz.write_all(&tar.into_inner().unwrap()).unwrap();
    gz.finish().unwrap()
}

fn manifest(name: &str, version: &str) -> String {
    format!("[package]\nname = \"{name}\"\nversion = \"{version}\"\nentrypoint = \"lib.typ\"\n")
}

/// The two packages of the registry: `shells` imports `sand`.
fn packages() -> Vec<(String, Vec<u8>)> {
    let shells_toml = manifest("shells", "0.1.0");
    let sand_toml = manifest("sand", "0.2.0");
    vec![
        (
            "/preview/shells-0.1.0.tar.gz".into(),
            package(&[
                ("typst.toml", &shells_toml),
                (
                    "lib.typ",
                    "#import \"@preview/sand:0.2.0\": grain\n#let whelk = [A whelk on #grain]",
                ),
            ]),
        ),
        (
            "/preview/sand-0.2.0.tar.gz".into(),
            package(&[
                ("typst.toml", &sand_toml),
                ("lib.typ", "#let grain = [sand]"),
            ]),
        ),
    ]
}

/// `DejaVu Sans Mono` from the web build, with its family renamed to `name`,
/// which must have 16 characters: a font no bundled family matches.
fn renamed_mono(name: &str) -> Vec<u8> {
    assert_eq!(name.len(), 16);
    let fonts = concat!(env!("CARGO_MANIFEST_DIR"), "/../../web/dist-server/fonts");
    let catalogue: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(format!("{fonts}/fonts.json")).unwrap())
            .unwrap();
    let file = catalogue
        .as_array()
        .unwrap()
        .iter()
        .find(|e| {
            e["info"]["family"] == "DejaVu Sans Mono"
                && e["info"]["variant"]["weight"] == 400
                && e["info"]["variant"]["style"] == "normal"
        })
        .unwrap()["file"]
        .as_str()
        .unwrap()
        .to_owned();
    let mut data = std::fs::read(format!("{fonts}/{file}")).unwrap();
    let old = "DejaVu Sans Mono";
    let replace = |data: &mut Vec<u8>, from: &[u8], to: &[u8]| {
        let mut i = 0;
        while let Some(at) = data[i..].windows(from.len()).position(|w| w == from) {
            data[i + at..i + at + from.len()].copy_from_slice(to);
            i += at + from.len();
        }
    };
    replace(&mut data, old.as_bytes(), name.as_bytes());
    let utf16 = |s: &str| {
        s.encode_utf16()
            .flat_map(u16::to_be_bytes)
            .collect::<Vec<u8>>()
    };
    replace(&mut data, &utf16(old), &utf16(name));
    data
}

/// Typesetting fails unless `font` is a monospace font the compiler has.
fn needs(font: &str) -> String {
    format!(
        "#text(font: \"{font}\")[#context assert(measure[iiii].width == measure[mmmm].width, \
         message: \"{font} is missing\")]\n"
    )
}

const FIGURE: &str = r#"<svg xmlns="http://www.w3.org/2000/svg" width="10" height="10"><circle cx="5" cy="5" r="4"/></svg>"#;
const REFS: &str = "@book{carson1955, title={The Edge of the Sea}, author={Carson, Rachel}, \
                    year={1955}, publisher={Houghton Mifflin}}";

async fn upload(client: &reqwest::Client, base: &str, project: &str, path: &str, bytes: Vec<u8>) {
    let res = client
        .post(format!("{base}/api/projects/{project}/assets?path={path}"))
        .body(bytes)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 201, "{path}");
}

#[tokio::test]
async fn an_export_with_fonts_packages_images_and_a_bibliography_works() {
    let registry = registry(packages()).await;
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("fonts")).unwrap();
    std::fs::write(
        dir.path().join("fonts/tidepool.ttf"),
        renamed_mono("Tidepool Mono AA"),
    )
    .unwrap();
    std::fs::write(dir.path().join("fonts/readme.txt"), "not a font").unwrap();
    let server = Server::start_with(dir.path(), &["--package-registry", &registry.url]);
    let base = &server.base;
    let (client, _) = sign_up(base).await;

    // The catalogue offers the administrator's font to the browser.
    let catalogue: serde_json::Value = client
        .get(format!("{base}/fonts/fonts.json"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let added = catalogue
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["info"]["family"] == "Tidepool Mono AA")
        .expect("the added font is in the catalogue");
    let file = added["file"].as_str().unwrap();
    let res = client
        .get(format!("{base}/fonts/{file}"))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    assert!(
        res.headers()["cache-control"]
            .to_str()
            .unwrap()
            .contains("immutable")
    );

    let main = format!(
        "#import \"@preview/shells:0.1.0\": whelk\n= Rock pools\n#whelk\n\
         #image(\"figures/dot.png\", width: 1cm)\n#image(\"figures/circle.svg\", width: 1cm)\n\
         {}{}See @carson1955.\n#bibliography(\"refs.bib\")\n",
        needs("Tidepool Mono AA"),
        needs("Tidepool Mono BB"),
    );
    let project = project_with(
        &client,
        base,
        serde_json::json!([
            {"path": "main.typ", "text": main},
            {"path": "refs.bib", "text": REFS},
        ]),
    )
    .await;
    upload(&client, base, &project, "figures/dot.png", DOT.to_vec()).await;
    upload(
        &client,
        base,
        &project,
        "figures/circle.svg",
        FIGURE.as_bytes().to_vec(),
    )
    .await;
    upload(
        &client,
        base,
        &project,
        "fonts/tide.ttf",
        renamed_mono("Tidepool Mono BB"),
    )
    .await;
    let res = export(&client, base, &project).await;
    assert_eq!(res.status(), 200, "{}", res.text().await.unwrap());
    let pdf = res.bytes().await.unwrap();
    assert!(pdf.starts_with(b"%PDF"));
    assert_eq!(
        registry.hits.load(Ordering::SeqCst),
        2,
        "shells and the sand it imports"
    );
    // The second export uses the cache.
    assert_eq!(export(&client, base, &project).await.status(), 200);
    assert_eq!(registry.hits.load(Ordering::SeqCst), 2);

    // PDF/A-2b, and refusals of a bad query.
    let res = client
        .get(format!(
            "{base}/api/projects/{project}/export.pdf?standard=pdf-a-2b"
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200, "{}", res.text().await.unwrap());
    assert!(String::from_utf8_lossy(&res.bytes().await.unwrap()).contains("pdfaid:part"));
    for query in ["standard=pdf-x", "view=draft", "colour=blue"] {
        let res = client
            .get(format!("{base}/api/projects/{project}/export.pdf?{query}"))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 400, "{query}");
    }

    // A font nobody has: the compile problem names it.
    let missing = project_of(&client, base, &needs("Tidepool Mono CC")).await;
    let res = export(&client, base, &missing).await;
    assert_eq!(res.status(), 422);
    let body: serde_json::Value = res.json().await.unwrap();
    assert_eq!(body["code"], "compile_failed");
    let problems = body["problems"].as_array().unwrap();
    assert!(
        problems.iter().any(|p| p["message"]
            .as_str()
            .unwrap()
            .contains("Tidepool Mono CC is missing")),
        "{problems:?}"
    );
    assert_eq!(problems[0]["path"], "main.typ");

    // The browser gets the same packages from the cache.
    let shells = client
        .get(format!("{base}/packages/preview/shells/0.1.0.tar.gz"))
        .send()
        .await
        .unwrap();
    assert_eq!(shells.status(), 200);
    assert_eq!(shells.headers()["content-type"], "application/gzip");
    assert_eq!(shells.bytes().await.unwrap().to_vec(), packages()[0].1);
    assert_eq!(registry.hits.load(Ordering::SeqCst), 2);
    let anonymous = reqwest::get(format!("{base}/packages/preview/shells/0.1.0.tar.gz"))
        .await
        .unwrap();
    assert_eq!(anonymous.status(), 401);
    for (path, status) in [
        ("preview/shells/0.1.tar.gz", 404),
        ("preview/shells/0.1.0.zip", 404),
        ("preview/kelp/1.0.0.tar.gz", 404),
    ] {
        let res = client
            .get(format!("{base}/packages/{path}"))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), status, "{path}");
    }
    drop(server);

    // Offline, what the cache holds still works and what it lacks is named.
    let server = Server::start_with(dir.path(), &["--no-package-downloads"]);
    let (client, _) = log_in(&server.base).await;
    assert_eq!(export(&client, &server.base, &project).await.status(), 200);
    let kelp = project_of(
        &client,
        &server.base,
        "#import \"@preview/kelp:1.0.0\": frond\n#frond",
    )
    .await;
    let res = export(&client, &server.base, &kelp).await;
    assert_eq!(res.status(), 422);
    let text = res.text().await.unwrap();
    assert!(text.contains("@preview/kelp:1.0.0"), "{text}");
    let res = client
        .get(format!(
            "{}/packages/preview/kelp/1.0.0.tar.gz",
            server.base
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 404);
    drop(server);

    // A registry that does not answer is a gateway failure, not a miss.
    let server = Server::start_with(dir.path(), &["--package-registry", "http://127.0.0.1:9"]);
    let (client, _) = log_in(&server.base).await;
    let res = client
        .get(format!(
            "{}/packages/preview/kelp/1.0.0.tar.gz",
            server.base
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 502);
    let body: serde_json::Value = res.json().await.unwrap();
    assert_eq!(body["code"], "package_unavailable");
}

/// The three views of a text with tracked changes typeset what DESIGN
/// section 5 says: as written, with suggestions applied, and the original.
#[tokio::test]
async fn an_export_typesets_each_view_of_tracked_changes() {
    let dir = tempfile::tempdir().unwrap();
    let server = Server::start(dir.path());
    let base = &server.base;
    let (client, cookie) = sign_up(base).await;
    let project = project_of(&client, base, "The crab hunts at night.").await;
    let mut peer = Peer::connect(base, &cookie, &project, 81).await;
    let mut styles = loro::StyleConfigMap::new();
    styles.insert(
        "ins".into(),
        loro::StyleConfig {
            expand: loro::ExpandType::After,
        },
    );
    styles.insert(
        "del".into(),
        loro::StyleConfig {
            expand: loro::ExpandType::None,
        },
    );
    peer.doc.config_text_style(styles);
    let text = peer.main_text();
    // "The crab hunts at night." becomes "The crabwhelk hunts at night.",
    // with "crab" marked deleted and "whelk" marked inserted.
    text.insert(8, "whelk").unwrap();
    let mark = serde_json::json!({"id": "s1", "author": "u", "time": 0}).to_string();
    text.mark(4..8, "del", mark.clone()).unwrap();
    text.mark(8..13, "ins", mark).unwrap();
    peer.push_saved().await;
    let today = nisaba_server::dates::export_date(None, nisaba_server::dates::utc_days());
    let date = format!("{:04}-{:02}-{:02}", today.0, today.1, today.2);
    for (view, expected) in [
        ("written", "The crabwhelk hunts at night."),
        ("applied", "The whelk hunts at night."),
        ("original", "The crab hunts at night."),
    ] {
        let res = client
            .get(format!(
                "{base}/api/projects/{project}/export.pdf?today={date}&view={view}"
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200, "{view}");
        let got = res.bytes().await.unwrap().to_vec();
        let want = nisaba_server::export::compile_here(nisaba_server::export::ExportRequest {
            today,
            files: vec![("main.typ".into(), FileContents::Text(expected.into()))],
            packages: Vec::new(),
            fonts: Vec::new(),
            archival: false,
        })
        .unwrap();
        assert!(got == want, "{view}: the PDF is not that of {expected:?}");
    }
}

/// A package archive over 16 MiB is refused as unusable, and one export
/// loads at most 64 packages.
#[tokio::test]
async fn package_downloads_are_limited() {
    let mut files: Vec<(String, Vec<u8>)> = Vec::new();
    let big = {
        use std::io::Write as _;
        // Random bytes do not compress, so the archive stays over 16 MiB.
        let mut x = 0x2545_f491_4f6c_dd1d_u64;
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::none());
        let noise: Vec<u8> = (0..(17 << 20))
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                x.to_le_bytes()[0]
            })
            .collect();
        gz.write_all(&noise).unwrap();
        gz.finish().unwrap()
    };
    files.push(("/preview/boulder-1.0.0.tar.gz".into(), big));
    let mut imports = String::new();
    for i in 0..70 {
        let toml = manifest(&format!("pebble{i}"), "1.0.0");
        files.push((
            format!("/preview/pebble{i}-1.0.0.tar.gz"),
            package(&[("typst.toml", &toml), ("lib.typ", "#let size = 1")]),
        ));
        let _ = writeln!(imports, "#import \"@preview/pebble{i}:1.0.0\"");
    }
    let registry = registry(files).await;
    let dir = tempfile::tempdir().unwrap();
    let server = Server::start_with(dir.path(), &["--package-registry", &registry.url]);
    let (client, _) = sign_up(&server.base).await;
    let res = client
        .get(format!(
            "{}/packages/preview/boulder/1.0.0.tar.gz",
            server.base
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 502);
    let body: serde_json::Value = res.json().await.unwrap();
    assert!(
        body["message"]
            .as_str()
            .unwrap()
            .contains("larger than 16 MiB"),
        "{body}"
    );
    let before = registry.hits.load(Ordering::SeqCst);
    let project = project_of(&client, &server.base, &imports).await;
    let res = export(&client, &server.base, &project).await;
    assert_eq!(res.status(), 422, "six packages are left out");
    assert_eq!(registry.hits.load(Ordering::SeqCst) - before, 64);
}
