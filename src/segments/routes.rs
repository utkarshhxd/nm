// SPDX-License-Identifier: AGPL-3.0-only

use axum::{Json, Router, extract::State, http::StatusCode};
use serde::{Deserialize, Deserializer};
use serde_json::{Value, json};
use utoipa::ToSchema;
use uuid::Uuid;

use super::{Segment, parse_new_rules};
use crate::{
    app::AppState,
    audit::{self, AuditEvent, actions},
    auth::{Principal, authorize},
    error::{ApiError, ApiJson, ApiPath, ApiQuery, ApiResult, FieldError},
    http::request_meta::RequestMeta,
    lists::{validate_description, validate_name},
    pagination::{Page, PageParams},
    workspaces::roles::Role,
};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/api/v1/workspaces/{workspace_id}/segments",
            axum::routing::get(list_segments).post(create_segment),
        )
        .route(
            "/api/v1/workspaces/{workspace_id}/segments/{id}",
            axum::routing::get(get_segment)
                .patch(update_segment)
                .delete(delete_segment),
        )
}

#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct NewSegment {
    /// Unique in the workspace.
    pub name: String,
    pub description: Option<String>,
    /// The whole rule document, `{match, rules}`.
    #[schema(value_type = Object)]
    pub rules: Value,
}

/// A merge patch (RFC 7386) of everything but `rules`: a field left out is left alone, `null`
/// clears `description`. `rules` is the exception: it is **replaced as a whole**, never merged
/// into the stored document. A patch carrying `rules` must carry the entire document; a partial
/// one is `422`, because merging `{"match": "any"}` into the old document would flip the
/// connective and silently keep the old rules.
#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct UpdateSegment {
    /// Cannot be `null`.
    #[serde(default, deserialize_with = "present")]
    #[schema(value_type = Option<String>)]
    pub name: Option<Option<String>>,
    /// `null` clears it to an empty string.
    #[serde(default, deserialize_with = "present")]
    #[schema(value_type = Option<String>)]
    pub description: Option<Option<String>>,
    /// The whole replacement document. `null` is refused, not ignored.
    #[serde(default, deserialize_with = "present")]
    #[schema(value_type = Option<Object>)]
    pub rules: Option<Value>,
}

/// Tells an absent key (`None`) from one present as `null` (`Some(..)`). A plain `Option<T>` maps
/// both to `None`, which would silently ignore a `"rules": null`.
fn present<'de, D, T>(d: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    T::deserialize(d).map(Some)
}

fn name_taken(e: sqlx::Error) -> ApiError {
    match e.as_database_error() {
        Some(d) if d.is_unique_violation() => ApiError::Conflict("segment-name-taken"),
        _ => e.into(),
    }
}

#[utoipa::path(
    get,
    path = "/api/v1/workspaces/{workspace_id}/segments",
    operation_id = "list_segments",
    params(("workspace_id" = Uuid, Path, description = "Workspace id"), PageParams),
    responses(
        (status = 200, description = "Newest first", body = Page<Segment>),
        (status = 404, description = "not a member, or no such workspace"),
        (status = 422, description = "a malformed cursor"),
    ),
)]
pub async fn list_segments(
    State(state): State<AppState>,
    principal: Principal,
    ApiPath(workspace_id): ApiPath<Uuid>,
    ApiQuery(page): ApiQuery<PageParams>,
) -> ApiResult<Json<Page<Segment>>> {
    authorize(&state.pool, &principal, workspace_id, Role::Viewer).await?;
    let limit = page.limit();
    let rows = sqlx::query_as!(
        Segment,
        r#"select id, name, description, rules, version, created_by, created_at, updated_at
           from segments
           where workspace_id = $1 and ($2::uuid is null or id < $2)
           order by id desc limit $3"#,
        workspace_id,
        page.after()?,
        limit + 1,
    )
    .fetch_all(&state.pool)
    .await?;
    Ok(Json(Page::from_rows(rows, limit, |s| s.id)))
}

#[utoipa::path(
    get,
    path = "/api/v1/workspaces/{workspace_id}/segments/{id}",
    operation_id = "get_segment",
    params(
        ("workspace_id" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Segment id"),
    ),
    responses(
        (status = 200, body = Segment),
        (status = 404, description = "not a member, or no such segment in this workspace"),
    ),
)]
pub async fn get_segment(
    State(state): State<AppState>,
    principal: Principal,
    ApiPath((workspace_id, id)): ApiPath<(Uuid, Uuid)>,
) -> ApiResult<Json<Segment>> {
    authorize(&state.pool, &principal, workspace_id, Role::Viewer).await?;
    let segment = sqlx::query_as!(
        Segment,
        r#"select id, name, description, rules, version, created_by, created_at, updated_at
           from segments where workspace_id = $1 and id = $2"#,
        workspace_id,
        id,
    )
    .fetch_one(&state.pool)
    .await?;
    Ok(Json(segment))
}

#[utoipa::path(
    post,
    path = "/api/v1/workspaces/{workspace_id}/segments",
    operation_id = "create_segment",
    params(("workspace_id" = Uuid, Path, description = "Workspace id")),
    request_body = NewSegment,
    responses(
        (status = 201, body = Segment),
        (status = 403, description = "insufficient-role: creating a segment takes editor"),
        (status = 404, description = "not a member, or no such workspace"),
        (status = 409, description = "segment-name-taken"),
        (status = 422, description = "invalid name or description, or a refused rule document \
            (malformed-rules, empty-rules, unknown-field, ...)"),
    ),
)]
pub async fn create_segment(
    State(state): State<AppState>,
    meta: RequestMeta,
    principal: Principal,
    ApiPath(workspace_id): ApiPath<Uuid>,
    ApiJson(input): ApiJson<NewSegment>,
) -> ApiResult<(StatusCode, Json<Segment>)> {
    let mut tx = state.pool.begin().await?;
    authorize(&mut *tx, &principal, workspace_id, Role::Editor).await?;
    let name = validate_name(&input.name);
    let description = validate_description(input.description.as_deref().unwrap_or(""));
    let (Ok(name), Ok(description)) = (&name, &description) else {
        let errors = [name.err(), description.err()].into_iter().flatten();
        return Err(ApiError::Validation(errors.collect()));
    };
    // Parsed against the live, non-archived definitions, then stored as the parsed tree's own
    // serialisation: what is stored is what was validated.
    let group = parse_new_rules(&mut tx, workspace_id, &input.rules).await?;
    let rules = serde_json::to_value(&group).map_err(|e| ApiError::Internal(e.into()))?;
    let id = Uuid::now_v7();
    let created_by = match &principal {
        Principal::User { user_id, .. } => Some(*user_id),
        Principal::ApiKey { .. } => None,
    };
    sqlx::query!(
        "insert into segments (id, workspace_id, name, description, rules, created_by) \
         values ($1, $2, $3, $4, $5, $6)",
        id,
        workspace_id,
        name,
        description,
        rules,
        created_by,
    )
    .execute(&mut *tx)
    .await
    .map_err(name_taken)?;
    audit::record(
        &mut *tx,
        &meta,
        AuditEvent {
            workspace_id: Some(workspace_id),
            actor: principal.actor(),
            action: actions::SEGMENT_CREATED,
            target_type: Some("segment"),
            target_id: Some(id.to_string()),
            metadata: json!({ "name": name }),
        },
    )
    .await?;
    let segment = sqlx::query_as!(
        Segment,
        r#"select id, name, description, rules, version, created_by, created_at, updated_at
           from segments where workspace_id = $1 and id = $2"#,
        workspace_id,
        id,
    )
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(segment)))
}

#[utoipa::path(
    patch,
    path = "/api/v1/workspaces/{workspace_id}/segments/{id}",
    operation_id = "update_segment",
    params(
        ("workspace_id" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Segment id"),
    ),
    request_body = UpdateSegment,
    responses(
        (status = 200, body = Segment),
        (status = 403, description = "insufficient-role: editing a segment takes editor"),
        (status = 404, description = "not a member, or no such segment in this workspace"),
        (status = 409, description = "segment-name-taken"),
        (status = 422, description = "invalid name or description, an unknown field, or a rule \
            document that is not a whole, valid one"),
    ),
)]
pub async fn update_segment(
    State(state): State<AppState>,
    meta: RequestMeta,
    principal: Principal,
    ApiPath((workspace_id, id)): ApiPath<(Uuid, Uuid)>,
    ApiJson(input): ApiJson<UpdateSegment>,
) -> ApiResult<Json<Segment>> {
    let mut tx = state.pool.begin().await?;
    authorize(&mut *tx, &principal, workspace_id, Role::Editor).await?;
    // `null` clears a description; a name has no empty value to clear to.
    let name = match &input.name {
        Some(None) => Err(FieldError::new("name", "cannot be null")),
        Some(Some(n)) => validate_name(n).map(Some),
        None => Ok(None),
    };
    let description = match &input.description {
        Some(d) => validate_description(d.as_deref().unwrap_or("")).map(Some),
        None => Ok(None),
    };
    let (Ok(name), Ok(description)) = (&name, &description) else {
        let errors = [name.err(), description.err()].into_iter().flatten();
        return Err(ApiError::Validation(errors.collect()));
    };
    // Whatever is sent must be a whole, valid document: it is parsed on its own and never merged
    // with the stored one.
    let rules = match &input.rules {
        Some(doc) => {
            let group = parse_new_rules(&mut tx, workspace_id, doc).await?;
            Some(serde_json::to_value(&group).map_err(|e| ApiError::Internal(e.into()))?)
        }
        None => None,
    };
    // `is distinct from` on `jsonb`, not on text: `jsonb` reorders keys, so a text comparison
    // would bump the version for a save that changed nothing. Every right-hand side reads the
    // OLD row, so `rules` here is the stored document.
    let version = sqlx::query_scalar!(
        "update segments set name = coalesce($3, name), description = coalesce($4, description), \
         version = case when $5::jsonb is not null and $5::jsonb is distinct from rules \
                        then version + 1 else version end, \
         rules = coalesce($5::jsonb, rules), updated_at = now() \
         where workspace_id = $1 and id = $2 returning version",
        workspace_id,
        id,
        name.as_deref(),
        description.as_deref(),
        rules,
    )
    .fetch_optional(&mut *tx)
    .await
    .map_err(name_taken)?
    .ok_or(ApiError::NotFound)?;
    let fields: Vec<&str> = [
        ("name", input.name.is_some()),
        ("description", input.description.is_some()),
        ("rules", input.rules.is_some()),
    ]
    .into_iter()
    .filter(|(_, sent)| *sent)
    .map(|(field, _)| field)
    .collect();
    audit::record(
        &mut *tx,
        &meta,
        AuditEvent {
            workspace_id: Some(workspace_id),
            actor: principal.actor(),
            action: actions::SEGMENT_UPDATED,
            target_type: Some("segment"),
            target_id: Some(id.to_string()),
            metadata: json!({ "fields": fields, "version": version }),
        },
    )
    .await?;
    let segment = sqlx::query_as!(
        Segment,
        r#"select id, name, description, rules, version, created_by, created_at, updated_at
           from segments where workspace_id = $1 and id = $2"#,
        workspace_id,
        id,
    )
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Json(segment))
}

/// A hard delete. A segment is only a saved question: no contact is touched.
#[utoipa::path(
    delete,
    path = "/api/v1/workspaces/{workspace_id}/segments/{id}",
    operation_id = "delete_segment",
    params(
        ("workspace_id" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Segment id"),
    ),
    responses(
        (status = 204, description = "Deleted"),
        (status = 403, description = "insufficient-role: deleting a segment takes editor"),
        (status = 404, description = "not a member, or no such segment in this workspace"),
    ),
)]
pub async fn delete_segment(
    State(state): State<AppState>,
    meta: RequestMeta,
    principal: Principal,
    ApiPath((workspace_id, id)): ApiPath<(Uuid, Uuid)>,
) -> ApiResult<StatusCode> {
    let mut tx = state.pool.begin().await?;
    authorize(&mut *tx, &principal, workspace_id, Role::Editor).await?;
    let deleted = sqlx::query!(
        "delete from segments where workspace_id = $1 and id = $2",
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
            action: actions::SEGMENT_DELETED,
            target_type: Some("segment"),
            target_id: Some(id.to_string()),
            metadata: json!({}),
        },
    )
    .await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}
