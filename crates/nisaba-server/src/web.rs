//! The embedded web client.
//!
//! The web build writes a `.br` and a `.gz` copy next to each large file. A
//! request gets the Brotli copy when it accepts `br`, the gzip copy when it
//! accepts `gzip`, and the plain file otherwise. Files under `assets/` and
//! `fonts/` carry a content hash in their name, so browsers may keep them
//! for a year; every other file is revalidated by its `ETag`, the SHA-256 of
//! its bytes.
//!
//! A server built without the client answers its pages with
//! [`ErrorCode::WebClientMissing`]; the API works without it.

use std::borrow::Cow;

use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use nisaba_compile_core::FontEntry;

use crate::api::ErrorCode;
use crate::http::Fail;
use crate::store::hex;

#[derive(rust_embed::Embed)]
#[folder = "../../web/dist-server"]
struct WebClient;

/// Whether this binary holds the web client.
#[must_use]
pub fn built() -> bool {
    WebClient::get("index.html").is_some()
}

/// Paths that are never the client's pages: a miss there is a 404.
const NOT_PAGES: &[&str] = &["api/", "sync/", "packages/", "fonts/", "assets/"];

/// Serves one file of the web client. Paths that name no file get
/// `index.html`, so the client can route them, except under [`NOT_PAGES`].
pub async fn static_file(method: Method, headers: HeaderMap, uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    if NOT_PAGES.iter().any(|p| path.starts_with(p))
        || !matches!(method, Method::GET | Method::HEAD)
    {
        return Fail::new(ErrorCode::NotFound, "nothing is at this address").into_response();
    }
    let path = if path.is_empty() || WebClient::get(path).is_none() {
        "index.html"
    } else {
        path
    };
    let Some(plain) = WebClient::get(path) else {
        return Fail::new(
            ErrorCode::WebClientMissing,
            "this server was built without the web client: run `bun run build` in web/ and \
             build the server again. The API works without it.",
        )
        .into_response();
    };
    let etag = format!("\"{}\"", &hex(&plain.metadata.sha256_hash())[..32]);
    let hashed = path.starts_with("assets/");
    if !hashed && matches_etag(&headers, &etag) {
        return (StatusCode::NOT_MODIFIED, [(header::ETAG, etag)]).into_response();
    }
    let accepts = accepted(&headers);
    let (coding, file) = [("br", "br"), ("gzip", "gz")]
        .into_iter()
        .filter(|(coding, _)| accepts.contains(coding))
        .find_map(|(coding, ext)| Some((Some(coding), WebClient::get(&format!("{path}.{ext}"))?)))
        .unwrap_or((None, plain));
    let cache = if hashed {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    };
    let mut response = (
        [
            (header::CONTENT_TYPE, mime(path)),
            (header::CACHE_CONTROL, cache),
            (header::VARY, "accept-encoding"),
        ],
        file.data.into_owned(),
    )
        .into_response();
    let h = response.headers_mut();
    if let Ok(v) = HeaderValue::from_str(&etag) {
        h.insert(header::ETAG, v);
    }
    if let Some(coding) = coding {
        h.insert(header::CONTENT_ENCODING, HeaderValue::from_static(coding));
    }
    response
}

/// Whether `If-None-Match` names `etag`.
#[must_use]
pub fn matches_etag(headers: &HeaderMap, etag: &str) -> bool {
    headers
        .get_all(header::IF_NONE_MATCH)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .any(|tag| {
            let tag = tag.trim();
            tag == "*" || tag.trim_start_matches("W/") == etag
        })
}

/// The content codings the request accepts, without those it sets to `q=0`.
fn accepted(headers: &HeaderMap) -> Vec<&str> {
    headers
        .get_all(header::ACCEPT_ENCODING)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .filter_map(|item| {
            let mut parts = item.split(';').map(str::trim);
            let coding = parts.next()?;
            let refused = parts.any(|p| {
                p.strip_prefix("q=")
                    .and_then(|q| q.parse::<f32>().ok())
                    .is_some_and(|q| q == 0.0)
            });
            (!refused).then_some(coding)
        })
        .collect()
}

#[must_use]
pub fn mime(path: &str) -> &'static str {
    match path.rsplit('.').next() {
        Some("html") => "text/html; charset=utf-8",
        Some("js") => "text/javascript",
        Some("css") => "text/css",
        Some("json") => "application/json",
        Some("wasm") => "application/wasm",
        Some("svg") => "image/svg+xml",
        Some("webp") => "image/webp",
        Some("png") => "image/png",
        Some("woff2") => "font/woff2",
        Some("otf") => "font/otf",
        Some("ttf") => "font/ttf",
        Some("ttc" | "otc") => "font/collection",
        _ => "application/octet-stream",
    }
}

/// The font catalogue the web build wrote.
#[must_use]
pub fn embedded_fonts() -> Vec<FontEntry> {
    WebClient::get("fonts/fonts.json")
        .and_then(|file| serde_json::from_slice(&file.data).ok())
        .unwrap_or_default()
}

/// One embedded font file.
#[must_use]
pub fn embedded_font(file: &str) -> Option<Cow<'static, [u8]>> {
    if file.contains('/') || file == "fonts.json" {
        return None;
    }
    WebClient::get(&format!("fonts/{file}")).map(|f| f.data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refused_codings_are_left_out() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::ACCEPT_ENCODING,
            HeaderValue::from_static("gzip;q=0.5, br;q=0, deflate"),
        );
        assert_eq!(accepted(&headers), ["gzip", "deflate"]);
    }

    #[test]
    fn an_etag_matches_with_or_without_w() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::IF_NONE_MATCH,
            HeaderValue::from_static("\"a\", W/\"b\""),
        );
        assert!(matches_etag(&headers, "\"b\""));
        assert!(!matches_etag(&headers, "\"c\""));
    }
}
