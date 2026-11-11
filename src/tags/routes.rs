// SPDX-License-Identifier: AGPL-3.0-only

use axum::{Json, Router, extract::State, http::StatusCode};
use serde::Deserialize;
use serde_json::json;
use utoipa::ToSchema;
use uuid::Uuid;

use super::{Tag, apply, get, remove};
use crate::{
    app::AppState,
    audit::{self, AuditEvent, actions},
    auth::{Principal, authorize},
    contacts,
    error::{ApiError, ApiJson, ApiPath, ApiQuery, ApiResult},
    http::request_meta::RequestMeta,
    lists::validate_name,
    pagination::{Page, PageParams},
    workspaces::roles::Role,
};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/api/v1/workspaces/{workspace_id}/tags",
            axum::routing::get(list_tags).post(create_tag),
        )
        .route(
            "/api/v1/workspaces/{workspace_id}/tags/{id}",
            axum::routing::patch(update_tag).delete(delete_tag),
        )
        .route(
            "/api/v1/workspaces/{workspace_id}/contacts/{id}/tags/{tag_id}",
            axum::routing::put(apply_tag).delete(remove_tag),
        )
        .route(
            "/api/v1/workspaces/{workspace_id}/contacts/{id}/tags",
            axum::routing::get(list_contact_tags),
        )
}

#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct NewTag {
    /// Unique in the workspace. 1-100 characters.
    pub name: String,
}

#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct UpdateTag {
    /// Unique in the workspace. 1-100 characters.
    pub name: String,
}

#[utoipa::path(
    get,
    path = "/api/v1/workspaces/{workspace_id}/tags",
    operation_id = "list_tags",
    params(("workspace_id" = Uuid, Path, description = "Workspace id"), PageParams),
    responses(
        (status = 200, description = "Newest first", body = Page<Tag>),
        (status = 404, description = "not a member, or no such workspace"),
        (status = 422, description = "a malformed cursor"),
    ),
)]
pub async fn list_tags(
    State(state): State<AppState>,
    principal: Principal,
    ApiPath(workspace_id): ApiPath<Uuid>,
    ApiQuery(page): ApiQuery<PageParams>,
) -> ApiResult<Json<Page<Tag>>> {
    authorize(&state.pool, &principal, workspace_id, Role::Viewer).await?;
    let limit = page.limit();
    let rows = sqlx::query_as!(
        Tag,
        "select id, name, created_at from tags \
         where workspace_id = $1 and ($2::uuid is null or id < $2) \
         order by id desc limit $3",
        workspace_id,
        page.after()?,
        limit + 1,
    )
    .fetch_all(&state.pool)
    .await?;
    Ok(Json(Page::from_rows(rows, limit, |t| t.id)))
}

#[utoipa::path(
    post,
    path = "/api/v1/workspaces/{workspace_id}/tags",
    operation_id = "create_tag",
    params(("workspace_id" = Uuid, Path, description = "Workspace id")),
    request_body = NewTag,
    responses(
        (status = 201, body = Tag),
        (status = 403, description = "insufficient-role: creating a tag takes editor"),
        (status = 404, description = "not a member, or no such workspace"),
        (status = 409, description = "tag-name-taken"),
        (status = 422, description = "invalid name, or an unknown field"),
    ),
)]
pub async fn create_tag(
    State(state): State<AppState>,
    meta: RequestMeta,
    principal: Principal,
    ApiPath(workspace_id): ApiPath<Uuid>,
    ApiJson(input): ApiJson<NewTag>,
) -> ApiResult<(StatusCode, Json<Tag>)> {
    let mut tx = state.pool.begin().await?;
    authorize(&mut *tx, &principal, workspace_id, Role::Editor).await?;
    let name = validate_name(&input.name).map_err(|e| ApiError::Validation(vec![e]))?;
    let id = Uuid::now_v7();
    sqlx::query!(
        "insert into tags (id, workspace_id, name) values ($1, $2, $3)",
        id,
        workspace_id,
        name,
    )
    .execute(&mut *tx)
    .await
    // A fresh `id` can only collide on `(workspace_id, name)`.
    .map_err(|e| match e.as_database_error() {
        Some(d) if d.is_unique_violation() => ApiError::Conflict("tag-name-taken"),
        _ => e.into(),
    })?;
    audit::record(
        &mut *tx,
        &meta,
        AuditEvent {
            workspace_id: Some(workspace_id),
            actor: principal.actor(),
            action: actions::TAG_CREATED,
            target_type: Some("tag"),
            target_id: Some(id.to_string()),
            metadata: json!({ "name": name }),
        },
    )
    .await?;
    let tag = get(&mut tx, workspace_id, id).await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(tag)))
}

#[utoipa::path(
    patch,
    path = "/api/v1/workspaces/{workspace_id}/tags/{id}",
    operation_id = "update_tag",
    params(
        ("workspace_id" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Tag id"),
    ),
    request_body = UpdateTag,
    responses(
        (status = 200, body = Tag),
        (status = 403, description = "insufficient-role: renaming a tag takes editor"),
        (status = 404, description = "not a member, or no such tag in this workspace"),
        (status = 409, description = "tag-name-taken"),
        (status = 422, description = "invalid name, or an unknown field"),
    ),
)]
pub async fn update_tag(
    State(state): State<AppState>,
    meta: RequestMeta,
    principal: Principal,
    ApiPath((workspace_id, id)): ApiPath<(Uuid, Uuid)>,
    ApiJson(input): ApiJson<UpdateTag>,
) -> ApiResult<Json<Tag>> {
    let mut tx = state.pool.begin().await?;
    authorize(&mut *tx, &principal, workspace_id, Role::Editor).await?;
    let name = validate_name(&input.name).map_err(|e| ApiError::Validation(vec![e]))?;
    let updated = sqlx::query!(
        "update tags set name = $3 where workspace_id = $1 and id = $2",
        workspace_id,
        id,
        name,
    )
    .execute(&mut *tx)
    .await
    .map_err(|e| match e.as_database_error() {
        Some(d) if d.is_unique_violation() => ApiError::Conflict("tag-name-taken"),
        _ => e.into(),
    })?;
    if updated.rows_affected() == 0 {
        return Err(ApiError::NotFound);
    }
    audit::record(
        &mut *tx,
        &meta,
        AuditEvent {
            workspace_id: Some(workspace_id),
            actor: principal.actor(),
            action: actions::TAG_UPDATED,
            target_type: Some("tag"),
            target_id: Some(id.to_string()),
            metadata: json!({ "name": name }),
        },
    )
    .await?;
    let tag = get(&mut tx, workspace_id, id).await?;
    tx.commit().await?;
    Ok(Json(tag))
}

/// A hard delete. The tag comes off every contact that had it; the contacts stay.
#[utoipa::path(
    delete,
    path = "/api/v1/workspaces/{workspace_id}/tags/{id}",
    operation_id = "delete_tag",
    params(
        ("workspace_id" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Tag id"),
    ),
    responses(
        (status = 204, description = "Deleted, and removed from every contact"),
        (status = 403, description = "insufficient-role: deleting a tag takes editor"),
        (status = 404, description = "not a member, or no such tag in this workspace"),
    ),
)]
pub async fn delete_tag(
    State(state): State<AppState>,
    meta: RequestMeta,
    principal: Principal,
    ApiPath((workspace_id, id)): ApiPath<(Uuid, Uuid)>,
) -> ApiResult<StatusCode> {
    let mut tx = state.pool.begin().await?;
    authorize(&mut *tx, &principal, workspace_id, Role::Editor).await?;
    let deleted = sqlx::query!(
        "delete from tags where workspace_id = $1 and id = $2",
        workspace_id,
        id,
    )
    .execute(&mut *tx)
    .await?;
    if deleted.rows_affected() == 0 {
        return Err(ApiError::NotFound);
    }
    audit::record(
        &mut *tx,
        &meta,
        AuditEvent {
            workspace_id: Some(workspace_id),
            actor: principal.actor(),
            action: actions::TAG_DELETED,
            target_type: Some("tag"),
            target_id: Some(id.to_string()),
            metadata: json!({}),
        },
    )
    .await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Idempotent: applying a tag the contact already has is a `204` and changes nothing.
#[utoipa::path(
    put,
    path = "/api/v1/workspaces/{workspace_id}/contacts/{id}/tags/{tag_id}",
    operation_id = "apply_tag",
    params(
        ("workspace_id" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Contact id"),
        ("tag_id" = Uuid, Path, description = "Tag id"),
    ),
    responses(
        (status = 204, description = "The contact has the tag"),
        (status = 403, description = "insufficient-role: tagging a contact takes editor"),
        (status = 404, description = "not a member, or no such contact or tag in this workspace"),
    ),
)]
pub async fn apply_tag(
    State(state): State<AppState>,
    meta: RequestMeta,
    principal: Principal,
    ApiPath((workspace_id, id, tag_id)): ApiPath<(Uuid, Uuid, Uuid)>,
) -> ApiResult<StatusCode> {
    let mut tx = state.pool.begin().await?;
    authorize(&mut *tx, &principal, workspace_id, Role::Editor).await?;
    // Only a tag that was not there is an event; a repeat leaves no audit row.
    if apply(&mut tx, workspace_id, id, tag_id).await? {
        audit::record(
            &mut *tx,
            &meta,
            AuditEvent {
                workspace_id: Some(workspace_id),
                actor: principal.actor(),
                action: actions::TAG_APPLIED,
                target_type: Some("tag"),
                target_id: Some(tag_id.to_string()),
                metadata: json!({ "contact_id": id }),
            },
        )
        .await?;
    }
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

#[utoipa::path(
    delete,
    path = "/api/v1/workspaces/{workspace_id}/contacts/{id}/tags/{tag_id}",
    operation_id = "remove_tag",
    params(
        ("workspace_id" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Contact id"),
        ("tag_id" = Uuid, Path, description = "Tag id"),
    ),
    responses(
        (status = 204, description = "The tag is off the contact"),
        (status = 403, description = "insufficient-role: untagging a contact takes editor"),
        (status = 404, description = "not a member, or the contact did not have this tag"),
    ),
)]
pub async fn remove_tag(
    State(state): State<AppState>,
    meta: RequestMeta,
    principal: Principal,
    ApiPath((workspace_id, id, tag_id)): ApiPath<(Uuid, Uuid, Uuid)>,
) -> ApiResult<StatusCode> {
    let mut tx = state.pool.begin().await?;
    authorize(&mut *tx, &principal, workspace_id, Role::Editor).await?;
    remove(&mut tx, workspace_id, id, tag_id).await?;
    audit::record(
        &mut *tx,
        &meta,
        AuditEvent {
            workspace_id: Some(workspace_id),
            actor: principal.actor(),
            action: actions::TAG_REMOVED,
            target_type: Some("tag"),
            target_id: Some(tag_id.to_string()),
            metadata: json!({ "contact_id": id }),
        },
    )
    .await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

#[utoipa::path(
    get,
    path = "/api/v1/workspaces/{workspace_id}/contacts/{id}/tags",
    operation_id = "list_contact_tags",
    params(
        ("workspace_id" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Contact id"),
        PageParams,
    ),
    responses(
        (status = 200, description = "The tags on the contact, newest tag first", body = Page<Tag>),
        (status = 404, description = "not a member, or no such contact in this workspace"),
        (status = 422, description = "a malformed cursor"),
    ),
)]
pub async fn list_contact_tags(
    State(state): State<AppState>,
    principal: Principal,
    ApiPath((workspace_id, id)): ApiPath<(Uuid, Uuid)>,
    ApiQuery(page): ApiQuery<PageParams>,
) -> ApiResult<Json<Page<Tag>>> {
    let mut conn = state.pool.acquire().await?;
    authorize(&mut *conn, &principal, workspace_id, Role::Viewer).await?;
    contacts::get(&mut conn, workspace_id, id).await?;
    let limit = page.limit();
    let rows = sqlx::query_as!(
        Tag,
        "select t.id, t.name, t.created_at from contact_tags ct \
         join tags t on t.workspace_id = ct.workspace_id and t.id = ct.tag_id \
         where ct.workspace_id = $1 and ct.contact_id = $2 and ($3::uuid is null or t.id < $3) \
         order by t.id desc limit $4",
        workspace_id,
        id,
        page.after()?,
        limit + 1,
    )
    .fetch_all(&mut *conn)
    .await?;
    Ok(Json(Page::from_rows(rows, limit, |t| t.id)))
}
