// SPDX-License-Identifier: AGPL-3.0-only

use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sqlx::types::Json as Jsonb;
use utoipa::ToSchema;
use uuid::Uuid;

use super::{Definition, FieldKind, Row, validate_key, validate_label, validate_options};
use crate::{
    app::AppState,
    audit::{self, AuditEvent, actions},
    auth::{Principal, authorize},
    error::{ApiError, ApiJson, ApiPath, ApiQuery, ApiResult, FieldError},
    http::request_meta::RequestMeta,
    pagination::{Page, PageParams},
    workspaces::roles::Role,
};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/api/v1/workspaces/{workspace_id}/custom-fields",
            get(list_custom_fields).post(create_custom_field),
        )
        .route(
            "/api/v1/workspaces/{workspace_id}/custom-fields/{id}",
            axum::routing::patch(update_custom_field).delete(archive_custom_field),
        )
        .route(
            "/api/v1/workspaces/{workspace_id}/custom-fields/{id}/purge",
            post(purge_custom_field),
        )
}

#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct NewCustomField {
    /// `^[a-z][a-z0-9_]{0,62}$`. The JSONB key every contact's value is stored under; it cannot
    /// change afterwards.
    pub key: String,
    pub label: String,
    pub kind: FieldKind,
    /// Required (1-200 distinct values) for `select` and `multi_select`; omit for every other kind.
    pub options: Option<Vec<String>>,
}

/// `key` is immutable, so an unknown field (`key` included) is rejected, not ignored. `kind` is
/// accepted only so that a change can be refused with a message that says why.
#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct UpdateCustomField {
    pub label: Option<String>,
    /// Must equal the field's current kind: changing it is not supported yet.
    pub kind: Option<FieldKind>,
    pub options: Option<Vec<String>>,
}

#[derive(Serialize, ToSchema)]
pub struct Archived {
    pub field: Definition,
    /// States, so no client has to guess, that deleting did not delete data.
    pub note: &'static str,
}

#[derive(Serialize, ToSchema)]
pub struct Purged {
    /// Contacts whose value for the field was removed.
    pub affected: i64,
}

const ARCHIVED_NOTE: &str =
    "the field is archived; every contact keeps its value until the field is purged";

/// The field, locked until the transaction ends so update, archive and purge on one field run one
/// at a time. `404` if it is not in this workspace.
async fn load(db: &mut sqlx::PgConnection, workspace_id: Uuid, id: Uuid) -> ApiResult<Definition> {
    sqlx::query_as!(
        Row,
        r#"select id, key, label, kind as "kind: FieldKind", options as "options: Jsonb<Vec<String>>",
                  archived_at
           from custom_fields where workspace_id = $1 and id = $2 for update"#,
        workspace_id,
        id,
    )
    .fetch_one(db)
    .await
    .map(Definition::from)
    .map_err(Into::into)
}

#[utoipa::path(
    get,
    path = "/api/v1/workspaces/{workspace_id}/custom-fields",
    operation_id = "list_custom_fields",
    params(("workspace_id" = Uuid, Path, description = "Workspace id"), PageParams),
    responses(
        (status = 200, description = "Newest first, archived fields included", body = Page<Definition>),
        (status = 404, description = "not a member, or no such workspace"),
    ),
)]
pub async fn list_custom_fields(
    State(state): State<AppState>,
    principal: Principal,
    ApiPath(workspace_id): ApiPath<Uuid>,
    ApiQuery(page): ApiQuery<PageParams>,
) -> ApiResult<Json<Page<Definition>>> {
    authorize(&state.pool, &principal, workspace_id, Role::Viewer).await?;
    let limit = page.limit();
    let rows = sqlx::query_as!(
        Row,
        r#"select id, key, label, kind as "kind: FieldKind", options as "options: Jsonb<Vec<String>>",
                  archived_at
           from custom_fields
           where workspace_id = $1 and ($2::uuid is null or id < $2)
           order by id desc limit $3"#,
        workspace_id,
        page.after()?,
        limit + 1,
    )
    .fetch_all(&state.pool)
    .await?;
    let rows = rows.into_iter().map(Definition::from).collect();
    Ok(Json(Page::from_rows(rows, limit, |d| d.id)))
}

#[utoipa::path(
    post,
    path = "/api/v1/workspaces/{workspace_id}/custom-fields",
    operation_id = "create_custom_field",
    params(("workspace_id" = Uuid, Path, description = "Workspace id")),
    request_body = NewCustomField,
    responses(
        (status = 201, body = Definition),
        (status = 403, description = "insufficient-role: creating a field takes admin"),
        (status = 404, description = "not a member, or no such workspace"),
        (status = 409, description = "key-taken, even by an archived field"),
        (status = 422, description = "invalid key, label or options"),
    ),
)]
pub async fn create_custom_field(
    State(state): State<AppState>,
    meta: RequestMeta,
    principal: Principal,
    ApiPath(workspace_id): ApiPath<Uuid>,
    ApiJson(input): ApiJson<NewCustomField>,
) -> ApiResult<(StatusCode, Json<Definition>)> {
    let mut tx = state.pool.begin().await?;
    authorize(&mut *tx, &principal, workspace_id, Role::Admin).await?;

    let key = validate_key(&input.key);
    let label = validate_label(&input.label);
    let options = validate_options(input.kind, input.options.as_deref().unwrap_or_default());
    let (key, label, options) = match (key, label, options) {
        (Ok(k), Ok(l), Ok(o)) => (k, l, o),
        (k, l, o) => {
            let errors = [k.err(), l.err(), o.err()].into_iter().flatten().collect();
            return Err(ApiError::Validation(errors));
        }
    };

    let id = Uuid::now_v7();
    // The only unique constraint a fresh `id` can hit is `(workspace_id, key)`.
    sqlx::query!(
        "insert into custom_fields (id, workspace_id, key, label, kind, options) \
         values ($1, $2, $3, $4, $5, $6)",
        id,
        workspace_id,
        key,
        label,
        input.kind as FieldKind,
        Jsonb(&options) as Jsonb<&Vec<String>>,
    )
    .execute(&mut *tx)
    .await
    .map_err(|e| match e.as_database_error() {
        Some(d) if d.is_unique_violation() => ApiError::Conflict("key-taken"),
        _ => e.into(),
    })?;
    audit::record(
        &mut *tx,
        &meta,
        AuditEvent {
            workspace_id: Some(workspace_id),
            actor: principal.actor(),
            action: actions::CUSTOM_FIELD_CREATED,
            target_type: Some("custom_field"),
            target_id: Some(id.to_string()),
            metadata: json!({ "key": key, "label": label, "kind": input.kind }),
        },
    )
    .await?;
    tx.commit().await?;
    let field = Definition {
        id,
        key,
        label,
        kind: input.kind,
        options,
        archived_at: None,
    };
    Ok((StatusCode::CREATED, Json(field)))
}

#[utoipa::path(
    patch,
    path = "/api/v1/workspaces/{workspace_id}/custom-fields/{id}",
    operation_id = "update_custom_field",
    params(
        ("workspace_id" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Custom field id"),
    ),
    request_body = UpdateCustomField,
    responses(
        (status = 200, body = Definition),
        (status = 403, description = "insufficient-role"),
        (status = 404, description = "not a member, or no such field in this workspace"),
        (status = 422, description = "invalid label or options, an unknown field such as `key`, or a kind change"),
    ),
)]
pub async fn update_custom_field(
    State(state): State<AppState>,
    meta: RequestMeta,
    principal: Principal,
    ApiPath((workspace_id, id)): ApiPath<(Uuid, Uuid)>,
    ApiJson(input): ApiJson<UpdateCustomField>,
) -> ApiResult<Json<Definition>> {
    let mut tx = state.pool.begin().await?;
    authorize(&mut *tx, &principal, workspace_id, Role::Admin).await?;
    let current = load(&mut tx, workspace_id, id).await?;

    // Converting every contact's stored value needs a dry run and a background job (M1b).
    let kind = match input.kind {
        Some(k) if k != current.kind => Some(FieldError::new(
            "kind",
            "changing a field's type is not supported yet",
        )),
        _ => None,
    };
    let label = input.label.as_deref().map(validate_label).transpose();
    let options = input
        .options
        .as_deref()
        .map(|o| validate_options(current.kind, o))
        .transpose();
    let (label, options) = match (kind, label, options) {
        (None, Ok(l), Ok(o)) => (l, o),
        (k, l, o) => {
            let errors = [k, l.err(), o.err()].into_iter().flatten().collect();
            return Err(ApiError::Validation(errors));
        }
    };

    let row = sqlx::query_as!(
        Row,
        r#"update custom_fields
           set label = coalesce($3, label), options = coalesce($4, options), updated_at = now()
           where workspace_id = $1 and id = $2
           returning id, key, label, kind as "kind: FieldKind",
                     options as "options: Jsonb<Vec<String>>", archived_at"#,
        workspace_id,
        id,
        label,
        options.as_ref().map(Jsonb) as Option<Jsonb<&Vec<String>>>,
    )
    .fetch_one(&mut *tx)
    .await?;
    // The key, and only the fields that were sent, as their new values.
    let mut changed = Map::new();
    changed.insert("key".into(), Value::from(current.key));
    if let Some(l) = label {
        changed.insert("label".into(), Value::from(l));
    }
    if let Some(o) = options {
        changed.insert("options".into(), Value::from(o));
    }
    audit::record(
        &mut *tx,
        &meta,
        AuditEvent {
            workspace_id: Some(workspace_id),
            actor: principal.actor(),
            action: actions::CUSTOM_FIELD_UPDATED,
            target_type: Some("custom_field"),
            target_id: Some(id.to_string()),
            metadata: Value::Object(changed),
        },
    )
    .await?;
    tx.commit().await?;
    Ok(Json(row.into()))
}

#[utoipa::path(
    delete,
    path = "/api/v1/workspaces/{workspace_id}/custom-fields/{id}",
    operation_id = "archive_custom_field",
    params(
        ("workspace_id" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Custom field id"),
    ),
    responses(
        (status = 200, description = "Archived. Every contact keeps its value", body = Archived),
        (status = 403, description = "insufficient-role: archiving a field takes admin"),
        (status = 404, description = "not a member, or no such field in this workspace"),
    ),
)]
pub async fn archive_custom_field(
    State(state): State<AppState>,
    meta: RequestMeta,
    principal: Principal,
    ApiPath((workspace_id, id)): ApiPath<(Uuid, Uuid)>,
) -> ApiResult<Json<Archived>> {
    let mut tx = state.pool.begin().await?;
    authorize(&mut *tx, &principal, workspace_id, Role::Admin).await?;
    // Archiving twice keeps the first `archived_at`. The update's own row lock serialises it.
    let row = sqlx::query_as!(
        Row,
        r#"update custom_fields set archived_at = coalesce(archived_at, now()), updated_at = now()
           where workspace_id = $1 and id = $2
           returning id, key, label, kind as "kind: FieldKind",
                     options as "options: Jsonb<Vec<String>>", archived_at"#,
        workspace_id,
        id,
    )
    .fetch_one(&mut *tx)
    .await?;
    audit::record(
        &mut *tx,
        &meta,
        AuditEvent {
            workspace_id: Some(workspace_id),
            actor: principal.actor(),
            action: actions::CUSTOM_FIELD_ARCHIVED,
            target_type: Some("custom_field"),
            target_id: Some(id.to_string()),
            metadata: json!({ "key": row.key }),
        },
    )
    .await?;
    tx.commit().await?;
    Ok(Json(Archived {
        field: row.into(),
        note: ARCHIVED_NOTE,
    }))
}

#[utoipa::path(
    post,
    path = "/api/v1/workspaces/{workspace_id}/custom-fields/{id}/purge",
    operation_id = "purge_custom_field",
    params(
        ("workspace_id" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Custom field id"),
    ),
    responses(
        (status = 200, description = "Every contact's value for the field was removed", body = Purged),
        (status = 403, description = "insufficient-role: purging takes admin"),
        (status = 404, description = "not a member, or no such field in this workspace"),
        (status = 409, description = "field-not-archived"),
    ),
)]
pub async fn purge_custom_field(
    State(state): State<AppState>,
    meta: RequestMeta,
    principal: Principal,
    ApiPath((workspace_id, id)): ApiPath<(Uuid, Uuid)>,
) -> ApiResult<Json<Purged>> {
    let mut tx = state.pool.begin().await?;
    authorize(&mut *tx, &principal, workspace_id, Role::Admin).await?;
    let field = load(&mut tx, workspace_id, id).await?;
    if field.archived_at.is_none() {
        return Err(ApiError::Conflict("field-not-archived"));
    }
    // Unchecked `sqlx::query`, because `contacts` does not exist until the next task; the macro
    // cannot compile against it. That task MUST turn this into `query!` and add a
    // `purge_strips_the_key` test to `tests/contacts.rs`.
    // shortcut: one statement in one transaction, holding a row lock per contact until it
    // commits; batch it if a single workspace ever holds enough contacts for that to hurt.
    let affected = sqlx::query(
        "update contacts set attributes = attributes - $2::text, updated_at = now() \
         where workspace_id = $1 and attributes ? $2",
    )
    .bind(workspace_id)
    .bind(&field.key)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    let affected = i64::try_from(affected).unwrap_or(i64::MAX);
    audit::record(
        &mut *tx,
        &meta,
        AuditEvent {
            workspace_id: Some(workspace_id),
            actor: principal.actor(),
            action: actions::CUSTOM_FIELD_PURGED,
            target_type: Some("custom_field"),
            target_id: Some(id.to_string()),
            metadata: json!({ "key": field.key, "affected": affected }),
        },
    )
    .await?;
    tx.commit().await?;
    Ok(Json(Purged { affected }))
}
