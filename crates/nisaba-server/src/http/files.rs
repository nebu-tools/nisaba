//! Reads of a project's files, asset uploads and downloads, the export, and
//! what the compiler loads from the server: packages and fonts.

use std::time::Instant;

use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use nisaba_compile_core::FileContents;
use nisaba_sync::SyncError;

use super::{Answer, App, Fail, Need, Path, Query, Raw, member, signed_in};
use crate::api::{AssetAdded, AssetQuery, ErrorCode, ExportQuery, Standard};
use crate::export::{ExportError, ExportRequest};
use crate::packages::PackageError;
use crate::project::{self, NewFile};
use crate::store::StoreError;

/// A room refused an update the server built.
pub fn refused_update(e: SyncError) -> Fail {
    match e {
        SyncError::Busy => Fail::new(
            ErrorCode::TryAgain,
            "a large update to this project is on its way; try again in a minute",
        )
        .retry(std::time::Duration::from_secs(60)),
        SyncError::TooLarge(_) | SyncError::ProjectFull { .. } => Fail::new(
            ErrorCode::ProjectFull,
            "the project is at its size limit for text and history",
        ),
        SyncError::Ended | SyncError::Closed => Fail::not_found("project"),
        SyncError::Forbidden => Fail::new(
            ErrorCode::Forbidden,
            "your role in this project does not allow this",
        ),
        SyncError::Storage(message) if StoreError::is_full(&message) => {
            StoreError::Full(message).into()
        }
        e => Fail::internal(e),
    }
}

/// Every file of the project, read from the live room.
pub async fn files(
    State(app): State<App>,
    headers: HeaderMap,
    Path(project): Path<String>,
) -> Answer {
    member(&app, &headers, &project, Need::Member)?;
    let room = app.rooms().get(&project).await.map_err(refused_update)?;
    Ok(axum::Json(room.read(project::files)).into_response())
}

/// Stores the body as an asset and adds its file entry, as the caller.
pub async fn upload(
    State(app): State<App>,
    headers: HeaderMap,
    Path(project): Path<String>,
    Query(q): Query<AssetQuery>,
    Raw(body): Raw,
) -> Answer {
    let (caller, _) = member(&app, &headers, &project, Need::Editor)?;
    let path = project::check_path(&q.path).map_err(|why| Fail::field("path", why))?;
    let store = app.store_ref();
    // The grant ends if the caller's access ends while the upload runs, and
    // the room then refuses the file entry.
    let grant = store
        .grant(&caller.token, &project)?
        .map_err(|_| Fail::not_found("project"))?;
    let hash = store.put_asset(&project, &body)?;
    let room = app.rooms().get(&project).await.map_err(refused_update)?;
    let snapshot = room.snapshot().map_err(refused_update)?;
    let update = project::add_files(
        Some(&snapshot),
        vec![NewFile::Asset {
            path,
            hash: hash.clone(),
        }],
    )
    .map_err(Fail::internal)?;
    room.apply(&update, &grant.access())
        .await
        .map_err(refused_update)?;
    Ok((StatusCode::CREATED, axum::Json(AssetAdded { hash })).into_response())
}

/// One byte range of a body of `len` bytes, from a `Range` header.
/// `Ok(None)` serves the whole body: no header, or one this does not read,
/// as RFC 9110 allows. `Err` is a range past the end.
fn range(headers: &HeaderMap, len: usize) -> Result<Option<(usize, usize)>, ()> {
    let Some(spec) = headers
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().strip_prefix("bytes="))
    else {
        return Ok(None);
    };
    if spec.contains(',') {
        return Ok(None);
    }
    let Some((start, end)) = spec.split_once('-') else {
        return Ok(None);
    };
    let parse = |s: &str| s.trim().parse::<usize>().ok();
    let (first, last) = match (start.trim().is_empty(), end.trim().is_empty()) {
        (true, false) => {
            let Some(n) = parse(end) else { return Ok(None) };
            if n == 0 || len == 0 {
                return Err(());
            }
            (len.saturating_sub(n), len - 1)
        }
        (false, _) => {
            let Some(first) = parse(start) else {
                return Ok(None);
            };
            let last = if end.trim().is_empty() {
                len.saturating_sub(1)
            } else {
                match parse(end) {
                    Some(last) if last >= first => last.min(len.saturating_sub(1)),
                    _ => return Ok(None),
                }
            };
            if first >= len {
                return Err(());
            }
            (first, last)
        }
        (true, true) => return Ok(None),
    };
    Ok(Some((first, last)))
}

/// Answers `bytes`, or the one range the request asks for.
pub fn ranged(
    headers: &HeaderMap,
    bytes: Vec<u8>,
    fixed: &[(header::HeaderName, &'static str)],
) -> Response {
    let len = bytes.len();
    let mut response = match range(headers, len) {
        Ok(None) => bytes.into_response(),
        Ok(Some((first, last))) => {
            let mut r = (StatusCode::PARTIAL_CONTENT, bytes[first..=last].to_vec()).into_response();
            if let Ok(v) = HeaderValue::from_str(&format!("bytes {first}-{last}/{len}")) {
                r.headers_mut().insert(header::CONTENT_RANGE, v);
            }
            r
        }
        Err(()) => {
            let mut r = Fail::new(
                ErrorCode::RangeNotSatisfiable,
                format!("the file has {len} bytes"),
            )
            .into_response();
            if let Ok(v) = HeaderValue::from_str(&format!("bytes */{len}")) {
                r.headers_mut().insert(header::CONTENT_RANGE, v);
            }
            return r;
        }
    };
    let headers = response.headers_mut();
    headers.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    for (name, value) in fixed {
        headers.insert(name.clone(), HeaderValue::from_static(value));
    }
    response
}

/// An asset of a project. The bytes are served as an opaque download in a
/// sandbox, so an uploaded HTML or SVG file cannot run as a page of this
/// server.
pub async fn asset(
    State(app): State<App>,
    headers: HeaderMap,
    Path((project, hash)): Path<(String, String)>,
) -> Answer {
    member(&app, &headers, &project, Need::Member)?;
    let bytes = app
        .store_ref()
        .project_asset(&project, &hash)?
        .ok_or_else(|| Fail::not_found("asset"))?;
    Ok(ranged(
        &headers,
        bytes,
        &[
            (header::CONTENT_TYPE, "application/octet-stream"),
            (
                header::CACHE_CONTROL,
                "private, max-age=31536000, immutable",
            ),
            (header::CONTENT_SECURITY_POLICY, "sandbox"),
        ],
    ))
}

/// Compiles the project from the room's current state, in a child process.
pub async fn export(
    State(app): State<App>,
    headers: HeaderMap,
    Path(project): Path<String>,
    Query(q): Query<ExportQuery>,
) -> Answer {
    member(&app, &headers, &project, Need::Member)?;
    let room = app.rooms().get(&project).await.map_err(refused_update)?;
    let view = q.view.unwrap_or_default();
    let files = room.read(|doc| project::files_in_view(doc, view));
    let store = app.store_ref();
    let mut contents = Vec::new();
    for file in files {
        match (file.text, file.hash) {
            (Some(text), _) => contents.push((file.path, FileContents::Text(text))),
            // An asset the project does not hold is left out, and the
            // compiler reports the missing file.
            (None, Some(hash)) => {
                if let Some(bytes) = store.project_asset(&project, &hash)? {
                    contents.push((file.path, FileContents::Bytes(bytes)));
                }
            }
            (None, None) => {}
        }
    }
    let packages = app.inner.packages.for_files(&contents).await;
    let request = ExportRequest {
        today: crate::dates::export_date(q.today.as_deref(), crate::dates::utc_days()),
        files: contents,
        packages,
        fonts: app.fonts().extra(),
        archival: q.standard.unwrap_or_default() == Standard::PdfA2b,
    };
    let started = Instant::now();
    let pdf = app
        .exporter()
        .pdf(request)
        .await
        .map_err(|e| export_fail(&project, e))?;
    tracing::info!(
        project,
        ms = started.elapsed().as_millis(),
        bytes = pdf.len(),
        "exported a PDF"
    );
    Ok(([(header::CONTENT_TYPE, "application/pdf")], pdf).into_response())
}

fn export_fail(project: &str, e: ExportError) -> Fail {
    let message = e.to_string();
    match e {
        ExportError::Busy => {
            Fail::new(ErrorCode::ExportBusy, message).retry(std::time::Duration::from_secs(30))
        }
        ExportError::Unsandboxed(_) => Fail::new(ErrorCode::ExportsOff, message),
        ExportError::Stopping => Fail::new(ErrorCode::Stopping, message),
        ExportError::Compile(problems) => {
            let mut fail = Fail::new(ErrorCode::CompileFailed, message);
            fail.0.problems = Some(problems);
            fail
        }
        ExportError::OutOfMemory { .. } => Fail::new(ErrorCode::ExportOutOfMemory, message),
        ExportError::OutOfTime { .. } => Fail::new(ErrorCode::ExportOutOfTime, message),
        ExportError::Failed(_) => {
            // What the child wrote may quote the document, so only the
            // kind of failure goes to the log.
            tracing::warn!(project, "an export child failed");
            Fail::new(ErrorCode::Internal, message)
        }
    }
}

/// A package from the cache, downloaded first when the cache lacks it and
/// the server allows downloads.
pub async fn package(
    State(app): State<App>,
    headers: HeaderMap,
    Path((namespace, name, archive)): Path<(String, String, String)>,
) -> Answer {
    signed_in(&app, &headers)?;
    let version = archive
        .strip_suffix(".tar.gz")
        .ok_or_else(|| Fail::not_found("package"))?;
    let spec = crate::packages::Spec::new(&namespace, &name, version)
        .ok_or_else(|| Fail::not_found("package"))?;
    match app.inner.packages.archive(&spec).await {
        Ok(bytes) => Ok((
            [
                (header::CONTENT_TYPE, "application/gzip"),
                (
                    header::CACHE_CONTROL,
                    "private, max-age=31536000, immutable",
                ),
            ],
            bytes,
        )
            .into_response()),
        Err(PackageError::NotFound(why)) => Err(Fail::new(ErrorCode::NotFound, why)),
        Err(PackageError::Unavailable(why)) => Err(Fail::new(ErrorCode::PackageUnavailable, why)),
        Err(PackageError::Store(e)) => Err(e.into()),
    }
}

/// `fonts.json`, revalidated by its hash on every visit.
pub async fn font_catalogue(State(app): State<App>, headers: HeaderMap) -> Response {
    let fonts = app.fonts();
    let etag = fonts.catalogue_etag();
    if crate::web::matches_etag(&headers, etag) {
        return (StatusCode::NOT_MODIFIED, [(header::ETAG, etag.to_owned())]).into_response();
    }
    (
        [
            (header::CONTENT_TYPE, "application/json".to_owned()),
            (header::CACHE_CONTROL, "no-cache".to_owned()),
            (header::ETAG, etag.to_owned()),
        ],
        fonts.catalogue().to_owned(),
    )
        .into_response()
}

/// One font file, named by its hash, so browsers keep it for a year.
pub async fn font_file(
    State(app): State<App>,
    headers: HeaderMap,
    Path(file): Path<String>,
) -> Answer {
    let bytes = app
        .fonts()
        .file(&file)
        .ok_or_else(|| Fail::not_found("font"))?;
    Ok(ranged(
        &headers,
        bytes,
        &[
            (header::CONTENT_TYPE, crate::web::mime(&file)),
            (header::CACHE_CONTROL, "public, max-age=31536000, immutable"),
        ],
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with(range: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(header::RANGE, HeaderValue::from_str(range).unwrap());
        h
    }

    #[test]
    fn ranges_are_read_as_rfc_9110_says() {
        assert_eq!(range(&HeaderMap::new(), 10), Ok(None));
        assert_eq!(range(&with("bytes=0-3"), 10), Ok(Some((0, 3))));
        assert_eq!(range(&with("bytes=4-"), 10), Ok(Some((4, 9))));
        assert_eq!(range(&with("bytes=-3"), 10), Ok(Some((7, 9))));
        assert_eq!(range(&with("bytes=-30"), 10), Ok(Some((0, 9))));
        assert_eq!(range(&with("bytes=5-100"), 10), Ok(Some((5, 9))));
        assert_eq!(range(&with("bytes=10-"), 10), Err(()));
        assert_eq!(range(&with("bytes=-0"), 10), Err(()));
        for ignored in [
            "bytes=0-1,3-4",
            "items=0-1",
            "bytes=3-1",
            "bytes=x-1",
            "bytes=-",
        ] {
            assert_eq!(range(&with(ignored), 10), Ok(None), "{ignored}");
        }
    }

    #[test]
    fn a_full_project_refuses_an_upload_with_413() {
        let full = refused_update(SyncError::ProjectFull { need: 10, left: 1 });
        assert_eq!(full.0.code, ErrorCode::ProjectFull);
        assert_eq!(refused_update(SyncError::Busy).0.code, ErrorCode::TryAgain);
        let disk = refused_update(SyncError::Storage(
            StoreError::Full("no space".into()).to_string(),
        ));
        assert_eq!(disk.0.code, ErrorCode::StorageFull);
    }
}
