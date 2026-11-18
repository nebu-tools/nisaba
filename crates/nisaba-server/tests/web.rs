//! What every answer carries, and how the web client's files are served:
//! security headers on every answer including errors, cache headers by
//! content hash, revalidation, compression, byte ranges, and errors in one
//! shape for every path and method.

#![allow(
    clippy::too_many_lines,
    reason = "an end-to-end story reads best as one test"
)]

mod common;

use common::*;
use reqwest::header::HeaderMap;

fn is_script(path: &str) -> bool {
    std::path::Path::new(path)
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("js"))
}

const SECURITY: &[(&str, &str)] = &[
    ("x-content-type-options", "nosniff"),
    ("referrer-policy", "no-referrer"),
    ("x-frame-options", "DENY"),
    ("cross-origin-opener-policy", "same-origin"),
    ("cross-origin-resource-policy", "same-origin"),
];

fn secured(headers: &HeaderMap, what: &str) {
    for (name, value) in SECURITY {
        assert_eq!(
            headers.get(*name).map(|v| v.to_str().unwrap()),
            Some(*value),
            "{what}: {name}"
        );
    }
    let csp = headers["content-security-policy"].to_str().unwrap();
    assert!(
        csp.contains("frame-ancestors 'none'") || csp == "sandbox",
        "{what}: {csp}"
    );
    assert!(headers.contains_key("permissions-policy"), "{what}");
}

/// Every answer, an error too, carries the security headers; API answers
/// are not cached; behind https the server asks for https only.
#[tokio::test]
async fn every_answer_carries_the_security_headers() {
    let dir = tempfile::tempdir().unwrap();
    let server = Server::start(dir.path());
    let base = &server.base;
    let (client, _) = sign_up(base).await;
    let project = project_of(&client, base, "#broken(").await;
    let hash = upload_dot(&client, base, &project).await;
    let anonymous = reqwest::Client::new();
    let answers = [
        (
            "page",
            client.get(format!("{base}/")).send().await.unwrap(),
            200,
        ),
        (
            "session",
            client
                .get(format!("{base}/api/session"))
                .send()
                .await
                .unwrap(),
            200,
        ),
        (
            "health",
            anonymous
                .get(format!("{base}/api/health"))
                .send()
                .await
                .unwrap(),
            200,
        ),
        (
            "unknown api",
            client
                .get(format!("{base}/api/nothing"))
                .send()
                .await
                .unwrap(),
            404,
        ),
        (
            "cross origin",
            anonymous
                .post(format!("{base}/api/login"))
                .send()
                .await
                .unwrap(),
            403,
        ),
        (
            "wrong method",
            client
                .delete(format!("{base}/api/session"))
                .send()
                .await
                .unwrap(),
            405,
        ),
        (
            "not signed in",
            anonymous
                .get(format!("{base}/api/projects"))
                .send()
                .await
                .unwrap(),
            401,
        ),
        (
            "asset",
            client
                .get(format!("{base}/api/projects/{project}/assets/{hash}"))
                .send()
                .await
                .unwrap(),
            200,
        ),
        ("failed export", export(&client, base, &project).await, 422),
        (
            "missing asset file",
            client
                .get(format!("{base}/assets/nothing.js"))
                .send()
                .await
                .unwrap(),
            404,
        ),
    ];
    for (what, res, status) in answers {
        assert_eq!(res.status(), status, "{what}");
        secured(res.headers(), what);
        let path = res.url().path().to_owned();
        if path.starts_with("/api/") && !path.contains("/assets/") {
            assert_eq!(res.headers()["cache-control"], "no-store", "{what}");
        }
        if status >= 400 {
            let body: serde_json::Value = res.json().await.unwrap();
            assert!(
                body["code"].is_string() && body["message"].is_string(),
                "{what}: {body}"
            );
        }
    }
    // The asset is a download in a sandbox, never a page of the server.
    let res = client
        .get(format!("{base}/api/projects/{project}/assets/{hash}"))
        .send()
        .await
        .unwrap();
    assert_eq!(res.headers()["content-security-policy"], "sandbox");
    assert_eq!(res.headers()["content-type"], "application/octet-stream");
    assert!(res.headers().get("strict-transport-security").is_none());

    let secure = tempfile::tempdir().unwrap();
    let origin = "https://nisaba.example.org";
    let server = Server::start_with(secure.path(), &["--public-url", origin]);
    let res = reqwest::get(format!("{}/api/health", server.base))
        .await
        .unwrap();
    assert_eq!(
        res.headers()["strict-transport-security"],
        "max-age=31536000"
    );
}

/// Files named by their content hash are kept for a year; the rest are
/// revalidated by their hash. A path that names no file gets the page, so
/// the client can route it.
#[tokio::test]
async fn files_are_cached_by_their_content_hash() {
    let dir = tempfile::tempdir().unwrap();
    let server = Server::start(dir.path());
    let base = &server.base;
    let page = reqwest::get(format!("{base}/")).await.unwrap();
    assert_eq!(page.headers()["cache-control"], "no-cache");
    let etag = page.headers()["etag"].to_str().unwrap().to_owned();
    let html = page.text().await.unwrap();
    let script = html
        .split('"')
        .find(|s| s.starts_with("/assets/") && is_script(s))
        .expect("the page loads a hashed script")
        .to_owned();
    let client = reqwest::Client::new();
    let again = client
        .get(format!("{base}/"))
        .header("if-none-match", &etag)
        .send()
        .await
        .unwrap();
    assert_eq!(again.status(), 304);
    secured(again.headers(), "not modified");
    let routed = reqwest::get(format!("{base}/projects/rock-pools"))
        .await
        .unwrap();
    assert_eq!(routed.status(), 200);
    assert_eq!(routed.text().await.unwrap(), html);
    let js = reqwest::get(format!("{base}{script}")).await.unwrap();
    assert_eq!(js.status(), 200);
    assert_eq!(
        js.headers()["cache-control"],
        "public, max-age=31536000, immutable"
    );
    assert_eq!(js.headers()["content-type"], "text/javascript");
    let fonts = reqwest::get(format!("{base}/fonts/fonts.json"))
        .await
        .unwrap();
    assert_eq!(fonts.headers()["cache-control"], "no-cache");
    let tag = fonts.headers()["etag"].to_str().unwrap().to_owned();
    let fonts = client
        .get(format!("{base}/fonts/fonts.json"))
        .header("if-none-match", tag)
        .send()
        .await
        .unwrap();
    assert_eq!(fonts.status(), 304);
}

/// The server sends the precompressed copy of a file and compresses JSON,
/// in the coding the request takes, and nothing the request does not take.
#[tokio::test]
async fn answers_are_compressed_as_the_request_takes_them() {
    let dir = tempfile::tempdir().unwrap();
    let server = Server::start(dir.path());
    let base = &server.base;
    let (client, _) = sign_up(base).await;
    let big = noise(50_000, 3);
    let project = project_of(&client, base, &big).await;
    let page = reqwest::get(format!("{base}/"))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let script = page
        .split('"')
        .find(|s| s.starts_with("/assets/") && is_script(s))
        .unwrap()
        .to_owned();
    let plain = reqwest::Client::new();
    for (accept, coding) in [
        ("br", Some("br")),
        ("gzip", Some("gzip")),
        ("identity", None),
        ("br;q=0, gzip", Some("gzip")),
    ] {
        let res = plain
            .get(format!("{base}{script}"))
            .header("accept-encoding", accept)
            .send()
            .await
            .unwrap();
        assert_eq!(
            res.headers()
                .get("content-encoding")
                .map(|v| v.to_str().unwrap()),
            coding,
            "{accept}"
        );
        assert_eq!(res.headers()["vary"], "accept-encoding");
    }
    let files = client
        .get(format!("{base}/api/projects/{project}/files"))
        .header("accept-encoding", "gzip")
        .send()
        .await
        .unwrap();
    assert_eq!(files.headers()["content-encoding"], "gzip");
    let files = client
        .get(format!("{base}/api/projects/{project}/files"))
        .send()
        .await
        .unwrap();
    assert!(files.headers().get("content-encoding").is_none());
    assert!(files.text().await.unwrap().contains(&big));
}

/// An asset answers one byte range with 206, a range past its end with 416.
#[tokio::test]
async fn an_asset_answers_byte_ranges() {
    let dir = tempfile::tempdir().unwrap();
    let server = Server::start(dir.path());
    let base = &server.base;
    let (client, _) = sign_up(base).await;
    let project = new_project(&client, base).await;
    let hash = upload_dot(&client, base, &project).await;
    let url = format!("{base}/api/projects/{project}/assets/{hash}");
    let res = client
        .get(&url)
        .header("range", "bytes=1-3")
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 206);
    assert_eq!(
        res.headers()["content-range"],
        format!("bytes 1-3/{}", DOT.len())
    );
    assert_eq!(res.headers()["accept-ranges"], "bytes");
    assert_eq!(res.bytes().await.unwrap().as_ref(), &DOT[1..=3]);
    let res = client
        .get(&url)
        .header("range", "bytes=-4")
        .send()
        .await
        .unwrap();
    assert_eq!(res.bytes().await.unwrap().as_ref(), &DOT[DOT.len() - 4..]);
    let res = client
        .get(&url)
        .header("range", "bytes=1000-")
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 416);
    assert_eq!(
        res.headers()["content-range"],
        format!("bytes */{}", DOT.len())
    );
    let body: serde_json::Value = res.json().await.unwrap();
    assert_eq!(body["code"], "range_not_satisfiable");
    let res = client
        .get(&url)
        .header("range", "lines=1-2")
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    let res = client
        .get(format!(
            "{base}/api/projects/{project}/assets/{}",
            "0".repeat(64)
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 404);
}

/// A body the server cannot read, too large, or of the wrong shape gets the
/// error shape with a stable code.
#[tokio::test]
async fn bad_requests_answer_in_the_error_shape() {
    let dir = tempfile::tempdir().unwrap();
    let server = Server::start(dir.path());
    let base = &server.base;
    let (client, _) = sign_up(base).await;
    let login = format!("{base}/api/login");
    for (body, code, status) in [
        ("not json".to_owned(), "invalid_request", 400),
        (
            r#"{"email": "a@example.org"}"#.to_owned(),
            "invalid_request",
            400,
        ),
        (
            r#"{"email": 1, "password": "x"}"#.to_owned(),
            "invalid_request",
            400,
        ),
        (
            r#"{"email": "a@example.org", "password": "x", "admin": true}"#.to_owned(),
            "invalid_request",
            400,
        ),
        (
            format!(
                r#"{{"email": "{}", "password": "x"}}"#,
                "a".repeat(70 * 1024)
            ),
            "payload_too_large",
            413,
        ),
    ] {
        let res = client
            .post(&login)
            .header("content-type", "application/json")
            .body(body)
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), status);
        let answer: serde_json::Value = res.json().await.unwrap();
        assert_eq!(answer["code"], code);
        assert!(!answer["message"].as_str().unwrap().is_empty());
    }
    // A sync path that is not a WebSocket handshake.
    let project = new_project(&client, base).await;
    let res = client
        .get(format!("{base}/sync/{project}"))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 400);
    let answer: serde_json::Value = res.json().await.unwrap();
    assert_eq!(answer["code"], "invalid_request");
}
