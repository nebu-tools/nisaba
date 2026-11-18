//! The embedded web client: static files and the fonts the compiler loads.
//!
//! The web build writes a `.br` and a `.gz` copy next to each large file. A
//! request gets the Brotli copy when it accepts `br`, the gzip copy when it
//! accepts `gzip`, and the plain file otherwise. Files under `assets/` and
//! `fonts/` carry a content hash in their name, so browsers may keep them for
//! a year; every other file is revalidated.

use std::borrow::Cow;
use std::sync::LazyLock;

use axum::http::{HeaderMap, HeaderValue, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use nisaba_compile_core::{FileContents, FontEntry, LiveCompiler};

#[derive(rust_embed::Embed)]
#[folder = "../../web/dist-server"]
struct WebClient;

/// Serves one file of the web client. Paths that name no file get
/// `index.html`, so the client can route them, except under the hashed
/// directories, where a missing file is a 404.
pub async fn static_file(headers: HeaderMap, uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    let hashed = path.starts_with("assets/") || path.starts_with("fonts/");
    let path = if path.is_empty() || (!hashed && WebClient::get(path).is_none()) {
        "index.html"
    } else {
        path
    };
    let accepts = accepted(&headers);
    let found = [("br", "br"), ("gzip", "gz")]
        .into_iter()
        .filter(|(coding, _)| accepts.contains(coding))
        .find_map(|(coding, ext)| Some((Some(coding), WebClient::get(&format!("{path}.{ext}"))?)))
        .or_else(|| Some((None, WebClient::get(path)?)));
    let Some((coding, file)) = found else {
        let message = if path == "index.html" {
            "web client not built"
        } else {
            "not found"
        };
        return (StatusCode::NOT_FOUND, message).into_response();
    };
    let cache = if hashed && path != "fonts/fonts.json" {
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
    if let Some(coding) = coding {
        response
            .headers_mut()
            .insert(header::CONTENT_ENCODING, HeaderValue::from_static(coding));
    }
    response
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

fn mime(path: &str) -> &'static str {
    match path.rsplit('.').next() {
        Some("html") => "text/html; charset=utf-8",
        Some("js") => "text/javascript",
        Some("css") => "text/css",
        Some("json") => "application/json",
        Some("wasm") => "application/wasm",
        Some("svg") => "image/svg+xml",
        Some("webp") => "image/webp",
        Some("woff2") => "font/woff2",
        Some("otf") => "font/otf",
        Some("ttf") => "font/ttf",
        _ => "application/octet-stream",
    }
}

/// The font catalogue the web build wrote, read once from the embedded client.
static FONTS: LazyLock<Vec<FontEntry>> = LazyLock::new(|| {
    WebClient::get("fonts/fonts.json")
        .and_then(|file| serde_json::from_slice(&file.data).ok())
        .unwrap_or_default()
});

fn font_data(entry: &FontEntry) -> Option<Cow<'static, [u8]>> {
    WebClient::get(&format!("fonts/{}", entry.file)).map(|file| file.data)
}

/// Compiles a project to PDF with the same fonts the browser loads. The
/// server holds every font already, so it adds them all and compiles once.
pub fn pdf(files: Vec<(String, FileContents)>) -> Result<Vec<u8>, String> {
    let mut compiler = LiveCompiler::new("main.typ", &FONTS)?;
    for (index, entry) in FONTS.iter().enumerate() {
        let data = font_data(entry).ok_or("font file missing from the build")?;
        compiler.add_font(index, data.into_owned())?;
    }
    let (year, month, day) = today();
    compiler.set_today(year, month, day);
    for (path, contents) in files {
        compiler.write(&path, contents);
    }
    let result = compiler.compile();
    if !result.ok {
        let messages: Vec<_> = result.problems.iter().map(|p| p.message.clone()).collect();
        return Err(messages.join("; "));
    }
    compiler.pdf()
}

/// Today's date in UTC, from the system clock.
fn today() -> (i32, u8, u8) {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    civil_from_days(i64::try_from(secs / 86_400).unwrap_or(0))
}

/// Converts days since 1970-01-01 to a calendar date (Howard Hinnant's
/// `civil_from_days`).
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn civil_from_days(days: i64) -> (i32, u8, u8) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u8;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u8;
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year as i32, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_from_days() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(11_016), (2000, 2, 29));
        assert_eq!(civil_from_days(20_819), (2027, 1, 1));
    }

    #[test]
    fn refused_codings_are_left_out() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::ACCEPT_ENCODING,
            HeaderValue::from_static("gzip;q=0.5, br;q=0, deflate"),
        );
        assert_eq!(accepted(&headers), ["gzip", "deflate"]);
    }
}
