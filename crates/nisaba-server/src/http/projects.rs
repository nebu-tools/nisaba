//! Projects and members.

use std::collections::HashSet;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use base64::Engine as _;
use sha2::{Digest, Sha256};

use super::{Answer, App, Fail, Json, Need, Path, member, signed_in, valid};
use crate::api::{ErrorCode, NewMember, NewProject, ProjectChange, Role, RoleChange};
use crate::project::{self, NewFile};
use crate::store::{Refusal, hex};

pub async fn list(State(app): State<App>, headers: HeaderMap) -> Answer {
    let caller = signed_in(&app, &headers)?;
    Ok(axum::Json(app.store_ref().projects_for(&caller.user.id)?).into_response())
}

const STARTER: &str = "= Untitled\n\nStart writing here.\n";

/// Makes a project. The first document is built and checked against the
/// project's size limit before anything is stored, and stored in the same
/// transaction as the project.
pub async fn create(
    State(app): State<App>,
    headers: HeaderMap,
    Json(body): Json<NewProject>,
) -> Answer {
    let caller = signed_in(&app, &headers)?;
    let name = valid::project_name(&body.name)?;
    let mut seen = HashSet::new();
    let mut files = Vec::new();
    let mut assets = Vec::new();
    for (i, file) in body.files.into_iter().enumerate() {
        let field = format!("files[{i}]");
        let path = project::check_path(&file.path)
            .map_err(|why| Fail::field(&format!("{field}.path"), why))?;
        if !seen.insert(path.clone()) {
            return Err(Fail::field(
                &format!("{field}.path"),
                format!("{path} appears twice"),
            ));
        }
        match (file.text, file.base64) {
            (Some(text), None) => files.push(NewFile::Text { path, text }),
            (None, Some(b64)) => {
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(b64)
                    .map_err(|e| Fail::field(&format!("{field}.base64"), e.to_string()))?;
                let hash = hex(&Sha256::digest(&bytes));
                files.push(NewFile::Asset {
                    path,
                    hash: hash.clone(),
                });
                assets.push(bytes);
            }
            _ => {
                return Err(Fail::field(
                    &field,
                    "give a file either text or base64, not both or neither",
                ));
            }
        }
    }
    if files.is_empty() {
        files.push(NewFile::Text {
            path: "main.typ".into(),
            text: STARTER.into(),
        });
    }
    // Built like any update and checked the way the room checks one, then
    // stored as the first entry of the log.
    let update = project::add_files(None, files).map_err(Fail::internal)?;
    loro::LoroDoc::new()
        .import(&update)
        .map_err(Fail::internal)?;
    if update.len() > crate::limits::room().max_document {
        return Err(Fail::new(
            ErrorCode::ProjectFull,
            "these files are larger than a project's 32 MiB limit for text and history",
        ));
    }
    let store = app.store_ref();
    let mut held = Vec::new();
    for bytes in &assets {
        held.push((store.write_asset_file(bytes)?, bytes.len() as u64));
    }
    let id = format!("{:032x}", rand::random::<u128>());
    store.create_project_with(&id, &name, &caller.user.id, Some(&update), &held)?;
    tracing::info!(project = %id, user = %caller.user.id, "project made");
    let project = store
        .project_for(&id, &caller.user.id)?
        .ok_or_else(|| Fail::internal("the new project is gone"))?;
    Ok((StatusCode::CREATED, axum::Json(project)).into_response())
}

pub async fn get(
    State(app): State<App>,
    headers: HeaderMap,
    Path(project): Path<String>,
) -> Answer {
    let (caller, _) = member(&app, &headers, &project, Need::Member)?;
    let found = app
        .store_ref()
        .project_for(&project, &caller.user.id)?
        .ok_or_else(|| Fail::not_found("project"))?;
    Ok(axum::Json(found).into_response())
}

pub async fn rename(
    State(app): State<App>,
    headers: HeaderMap,
    Path(project): Path<String>,
    Json(body): Json<ProjectChange>,
) -> Answer {
    let (caller, _) = member(&app, &headers, &project, Need::Owner)?;
    let name = valid::project_name(&body.name)?;
    app.store_ref().rename_project(&project, &name)?;
    let found = app
        .store_ref()
        .project_for(&project, &caller.user.id)?
        .ok_or_else(|| Fail::not_found("project"))?;
    Ok(axum::Json(found).into_response())
}

/// Deletes the project. The store ends every grant of it (4004) in the
/// step that deletes it; then the room is closed and forgotten.
pub async fn delete(
    State(app): State<App>,
    headers: HeaderMap,
    Path(project): Path<String>,
) -> Answer {
    let (caller, _) = member(&app, &headers, &project, Need::Owner)?;
    app.store_ref().delete_project(&project)?;
    app.rooms().evict(&project).await;
    tracing::info!(project, user = %caller.user.id, "project deleted");
    Ok(StatusCode::NO_CONTENT.into_response())
}

pub async fn members(
    State(app): State<App>,
    headers: HeaderMap,
    Path(project): Path<String>,
) -> Answer {
    member(&app, &headers, &project, Need::Member)?;
    Ok(axum::Json(app.store_ref().members(&project)?).into_response())
}

pub async fn add_member(
    State(app): State<App>,
    headers: HeaderMap,
    Path(project): Path<String>,
    Json(body): Json<NewMember>,
) -> Answer {
    member(&app, &headers, &project, Need::Owner)?;
    let email = valid::email("email", &body.email)?;
    match app.store_ref().add_member(&project, &email, body.role)? {
        Ok(added) => Ok((StatusCode::CREATED, axum::Json(added)).into_response()),
        Err(Refusal::AlreadyMember) => Err(Fail::new(
            ErrorCode::AlreadyMember,
            "this account is already a member of the project",
        )),
        Err(_) => Err(Fail::field("email", "no account has this email")),
    }
}

fn member_refusal(refusal: Refusal) -> Fail {
    match refusal {
        Refusal::LastOwner => Fail::new(
            ErrorCode::LastOwner,
            "the project needs an owner; make another member an owner first",
        ),
        _ => Fail::not_found("member"),
    }
}

pub async fn set_role(
    State(app): State<App>,
    headers: HeaderMap,
    Path((project, user)): Path<(String, String)>,
    Json(body): Json<RoleChange>,
) -> Answer {
    member(&app, &headers, &project, Need::Owner)?;
    match app.store_ref().set_role(&project, &user, body.role)? {
        Ok(changed) => Ok(axum::Json(changed).into_response()),
        Err(refusal) => Err(member_refusal(refusal)),
    }
}

/// An owner removes anyone; any member may leave.
pub async fn remove_member(
    State(app): State<App>,
    headers: HeaderMap,
    Path((project, user)): Path<(String, String)>,
) -> Answer {
    let (caller, role) = member(&app, &headers, &project, Need::Member)?;
    if role != Role::Owner && caller.user.id != user {
        return Err(Fail::new(
            ErrorCode::Forbidden,
            "only an owner may remove another member",
        ));
    }
    match app.store_ref().remove_member(&project, &user)? {
        Ok(()) => Ok(StatusCode::NO_CONTENT.into_response()),
        Err(refusal) => Err(member_refusal(refusal)),
    }
}
