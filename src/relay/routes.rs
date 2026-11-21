// SPDX-License-Identifier: AGPL-3.0-only

use axum::{Json, Router, extract::State, http::StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::PgConnection;
use utoipa::ToSchema;
use uuid::Uuid;

use super::Security;
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
            "/api/v1/workspaces/{workspace_id}/relays",
            axum::routing::get(list_relays).post(create_relay),
        )
        .route(
            "/api/v1/workspaces/{workspace_id}/relays/{id}",
            axum::routing::get(get_relay)
                .patch(update_relay)
                .delete(delete_relay),
        )
}

/// A relay as the API shows it. There is no password field, on purpose: `has_password` is all an
/// operator may learn about it.
#[derive(Serialize, ToSchema)]
pub struct RelayView {
    pub id: Uuid,
    pub workspace_id: Uuid,
    /// Unique in the workspace.
    pub name: String,
    pub host: String,
    pub port: i32,
    pub security: Security,
    pub username: Option<String>,
    /// Whether a password is stored. The password itself is never returned.
    pub has_password: bool,
    pub max_connections: i32,
    pub rate_limit_per_minute: i32,
    pub enabled: bool,
    pub failover_order: i32,
}

// Neither request type derives `Debug`, so a `{:?}` of one cannot put the password in a log.
#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct NewRelay {
    /// Unique in the workspace.
    pub name: String,
    pub host: String,
    /// 1 to 65535.
    pub port: i32,
    pub security: Security,
    pub username: Option<String>,
    /// Stored encrypted and never returned.
    pub password: Option<String>,
    /// Concurrent connections one sender process may hold open. Default 4.
    pub max_connections: Option<i32>,
    /// What the relay's operator says it allows. At least 1.
    pub rate_limit_per_minute: i32,
    /// Default `true`.
    pub enabled: Option<bool>,
    /// Lower is tried first. Default 0.
    pub failover_order: Option<i32>,
}

/// A field left out is left alone. `username` and `password` may be `null`, which clears them; on
/// any other field `null` is `422`, because there is nothing to clear it to.
#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct UpdateRelay {
    #[serde(default, deserialize_with = "present")]
    #[schema(value_type = String, required = false)]
    pub name: Option<Option<String>>,
    #[serde(default, deserialize_with = "present")]
    #[schema(value_type = String, required = false)]
    pub host: Option<Option<String>>,
    #[serde(default, deserialize_with = "present")]
    #[schema(value_type = i32, required = false)]
    pub port: Option<Option<i32>>,
    #[serde(default, deserialize_with = "present")]
    #[schema(value_type = Security, required = false)]
    pub security: Option<Option<Security>>,
    /// `null` clears it.
    #[serde(default, deserialize_with = "present")]
    #[schema(value_type = Option<String>)]
    pub username: Option<Option<String>>,
    /// Absent keeps the stored password; `null` removes it; a string replaces it.
    #[serde(default, deserialize_with = "present")]
    #[schema(value_type = Option<String>)]
    pub password: Option<Option<String>>,
    #[serde(default, deserialize_with = "present")]
    #[schema(value_type = i32, required = false)]
    pub max_connections: Option<Option<i32>>,
    #[serde(default, deserialize_with = "present")]
    #[schema(value_type = i32, required = false)]
    pub rate_limit_per_minute: Option<Option<i32>>,
    #[serde(default, deserialize_with = "present")]
    #[schema(value_type = bool, required = false)]
    pub enabled: Option<Option<bool>>,
    #[serde(default, deserialize_with = "present")]
    #[schema(value_type = i32, required = false)]
    pub failover_order: Option<Option<i32>>,
}

/// Tells a key that is absent (`None`) from one present as `null` (`Some(None)`). A plain
/// `Option<T>` maps both to `None`, which here would turn `"password": null` into "keep it".
/// The same helper `lists`, `contacts`, `forms` and `segments` each carry.
fn present<'de, D, T>(d: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    T::deserialize(d).map(Some)
}

/// Collects a field's refusal instead of returning at the first, so one `422` names them all.
fn keep<T>(r: Result<T, FieldError>, errors: &mut Vec<FieldError>) -> Option<T> {
    r.map_err(|e| errors.push(e)).ok()
}

/// A key sent as `null` where null means nothing.
fn not_null<T>(
    field: &'static str,
    v: Option<Option<T>>,
    errors: &mut Vec<FieldError>,
) -> Option<T> {
    if matches!(v, Some(None)) {
        errors.push(FieldError::new(field, "cannot be null"));
    }
    v.flatten()
}

fn name(v: &str) -> Result<String, FieldError> {
    let v = v.trim();
    if v.is_empty() || v.chars().count() > 100 {
        return Err(FieldError::new("name", "must be 1 to 100 characters"));
    }
    Ok(v.to_owned())
}

fn host(v: &str) -> Result<String, FieldError> {
    let v = v.trim();
    if v.is_empty() || v.len() > 253 || v.contains(char::is_whitespace) {
        return Err(FieldError::new(
            "host",
            "must be 1 to 253 characters, with no whitespace",
        ));
    }
    Ok(v.to_owned())
}

fn port(v: i32) -> Result<i32, FieldError> {
    if !(1..=65535).contains(&v) {
        return Err(FieldError::new("port", "must be 1 to 65535"));
    }
    Ok(v)
}

fn positive(field: &'static str, v: i32) -> Result<i32, FieldError> {
    if v < 1 {
        return Err(FieldError::new(field, "must be at least 1"));
    }
    Ok(v)
}

/// A username or password that is present must have something in it; `null` is how to say none.
fn not_empty(field: &'static str, v: &str) -> Result<(), FieldError> {
    if v.is_empty() {
        return Err(FieldError::new(
            field,
            "cannot be empty; send null to clear",
        ));
    }
    Ok(())
}

fn name_taken(e: sqlx::Error) -> ApiError {
    match e.as_database_error() {
        Some(d) if d.is_unique_violation() => ApiError::Conflict("relay-name-taken"),
        _ => e.into(),
    }
}

async fn get(db: &mut PgConnection, workspace_id: Uuid, id: Uuid) -> ApiResult<RelayView> {
    Ok(sqlx::query_as!(
        RelayView,
        r#"select id, workspace_id, name, host, port, security as "security: Security", username,
                  password_encrypted is not null as "has_password!", max_connections,
                  rate_limit_per_minute, enabled, failover_order
           from relays where workspace_id = $1 and id = $2"#,
        workspace_id,
        id,
    )
    .fetch_one(db)
    .await?)
}

/// Names the relay and the host. Never a credential: the audit log is read by admins and kept.
async fn record(
    db: &mut PgConnection,
    meta: &RequestMeta,
    principal: &Principal,
    action: &'static str,
    (workspace_id, id): (Uuid, Uuid),
    metadata: serde_json::Value,
) -> ApiResult<()> {
    audit::record(
        db,
        meta,
        AuditEvent {
            workspace_id: Some(workspace_id),
            actor: principal.actor(),
            action,
            target_type: Some("relay"),
            target_id: Some(id.to_string()),
            metadata,
        },
    )
    .await?;
    Ok(())
}

#[utoipa::path(
    get,
    path = "/api/v1/workspaces/{workspace_id}/relays",
    operation_id = "list_relays",
    params(("workspace_id" = Uuid, Path, description = "Workspace id"), PageParams),
    responses(
        (status = 200, description = "Newest first. Never carries a password.", body = Page<RelayView>),
        (status = 404, description = "not a member, or no such workspace"),
        (status = 422, description = "a malformed cursor"),
    ),
)]
pub async fn list_relays(
    State(state): State<AppState>,
    principal: Principal,
    ApiPath(workspace_id): ApiPath<Uuid>,
    ApiQuery(page): ApiQuery<PageParams>,
) -> ApiResult<Json<Page<RelayView>>> {
    authorize(&state.pool, &principal, workspace_id, Role::Viewer).await?;
    let limit = page.limit();
    let rows = sqlx::query_as!(
        RelayView,
        r#"select id, workspace_id, name, host, port, security as "security: Security", username,
                  password_encrypted is not null as "has_password!", max_connections,
                  rate_limit_per_minute, enabled, failover_order
           from relays
           where workspace_id = $1 and ($2::uuid is null or id < $2)
           order by id desc limit $3"#,
        workspace_id,
        page.after()?,
        limit + 1,
    )
    .fetch_all(&state.pool)
    .await?;
    Ok(Json(Page::from_rows(rows, limit, |r| r.id)))
}

#[utoipa::path(
    get,
    path = "/api/v1/workspaces/{workspace_id}/relays/{id}",
    operation_id = "get_relay",
    params(
        ("workspace_id" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Relay id"),
    ),
    responses(
        (status = 200, body = RelayView),
        (status = 404, description = "not a member, or no such relay in this workspace"),
    ),
)]
pub async fn get_relay(
    State(state): State<AppState>,
    principal: Principal,
    ApiPath((workspace_id, id)): ApiPath<(Uuid, Uuid)>,
) -> ApiResult<Json<RelayView>> {
    let mut conn = state.pool.acquire().await?;
    authorize(&mut *conn, &principal, workspace_id, Role::Viewer).await?;
    Ok(Json(get(&mut conn, workspace_id, id).await?))
}

/// Where mail leaves through, and with what credential, so writing it takes admin.
#[utoipa::path(
    post,
    path = "/api/v1/workspaces/{workspace_id}/relays",
    operation_id = "create_relay",
    params(("workspace_id" = Uuid, Path, description = "Workspace id")),
    request_body = NewRelay,
    responses(
        (status = 201, body = RelayView),
        (status = 403, description = "insufficient-role: configuring a relay takes admin"),
        (status = 404, description = "not a member, or no such workspace"),
        (status = 409, description = "relay-name-taken"),
        (status = 422, description = "an invalid or missing field, or an unknown field"),
    ),
)]
pub async fn create_relay(
    State(state): State<AppState>,
    meta: RequestMeta,
    principal: Principal,
    ApiPath(workspace_id): ApiPath<Uuid>,
    ApiJson(input): ApiJson<NewRelay>,
) -> ApiResult<(StatusCode, Json<RelayView>)> {
    let mut tx = state.pool.begin().await?;
    authorize(&mut *tx, &principal, workspace_id, Role::Admin).await?;
    let mut errors = Vec::new();
    let name = keep(name(&input.name), &mut errors);
    let host = keep(host(&input.host), &mut errors);
    let port = keep(port(input.port), &mut errors);
    let rate = keep(
        positive("rate_limit_per_minute", input.rate_limit_per_minute),
        &mut errors,
    );
    let max_connections = keep(
        positive("max_connections", input.max_connections.unwrap_or(4)),
        &mut errors,
    );
    for (field, v) in [("username", &input.username), ("password", &input.password)] {
        if let Some(v) = v {
            keep(not_empty(field, v), &mut errors);
        }
    }
    let (Some(name), Some(host), Some(port), Some(rate), Some(max_connections), true) =
        (name, host, port, rate, max_connections, errors.is_empty())
    else {
        return Err(ApiError::Validation(errors));
    };
    let id = Uuid::now_v7();
    let encrypted = input
        .password
        .as_deref()
        .map(|p| state.secrets.encrypt(p.as_bytes()));
    sqlx::query!(
        "insert into relays (id, workspace_id, name, host, port, security, username, \
         password_encrypted, max_connections, rate_limit_per_minute, enabled, failover_order) \
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, coalesce($11, true), coalesce($12, 0))",
        id,
        workspace_id,
        name,
        host,
        port,
        input.security as Security,
        input.username,
        encrypted,
        max_connections,
        rate,
        input.enabled,
        input.failover_order,
    )
    .execute(&mut *tx)
    .await
    .map_err(name_taken)?;
    record(
        &mut tx,
        &meta,
        &principal,
        actions::RELAY_CREATED,
        (workspace_id, id),
        json!({ "name": name, "host": host }),
    )
    .await?;
    let relay = get(&mut tx, workspace_id, id).await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(relay)))
}

#[utoipa::path(
    patch,
    path = "/api/v1/workspaces/{workspace_id}/relays/{id}",
    operation_id = "update_relay",
    params(
        ("workspace_id" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Relay id"),
    ),
    request_body = UpdateRelay,
    responses(
        (status = 200, body = RelayView),
        (status = 403, description = "insufficient-role: configuring a relay takes admin"),
        (status = 404, description = "not a member, or no such relay in this workspace"),
        (status = 409, description = "relay-name-taken"),
        (status = 422, description = "an invalid field, an explicit null for a field that cannot \
            be cleared, or an unknown field"),
    ),
)]
pub async fn update_relay(
    State(state): State<AppState>,
    meta: RequestMeta,
    principal: Principal,
    ApiPath((workspace_id, id)): ApiPath<(Uuid, Uuid)>,
    ApiJson(input): ApiJson<UpdateRelay>,
) -> ApiResult<Json<RelayView>> {
    let mut tx = state.pool.begin().await?;
    authorize(&mut *tx, &principal, workspace_id, Role::Admin).await?;
    // Which keys were sent, by name. `password` here is the key and says that it changed, never
    // what it changed to.
    let fields: Vec<&str> = [
        ("name", input.name.is_some()),
        ("host", input.host.is_some()),
        ("port", input.port.is_some()),
        ("security", input.security.is_some()),
        ("username", input.username.is_some()),
        ("password", input.password.is_some()),
        ("max_connections", input.max_connections.is_some()),
        (
            "rate_limit_per_minute",
            input.rate_limit_per_minute.is_some(),
        ),
        ("enabled", input.enabled.is_some()),
        ("failover_order", input.failover_order.is_some()),
    ]
    .into_iter()
    .filter(|(_, sent)| *sent)
    .map(|(field, _)| field)
    .collect();
    let UpdateRelay {
        name: new_name,
        host: new_host,
        port: new_port,
        security,
        username,
        password,
        max_connections,
        rate_limit_per_minute,
        enabled,
        failover_order,
    } = input;
    let mut errors = Vec::new();
    let new_name =
        not_null("name", new_name, &mut errors).and_then(|v| keep(name(&v), &mut errors));
    let new_host =
        not_null("host", new_host, &mut errors).and_then(|v| keep(host(&v), &mut errors));
    let new_port = not_null("port", new_port, &mut errors).and_then(|v| keep(port(v), &mut errors));
    let max_connections = not_null("max_connections", max_connections, &mut errors)
        .and_then(|v| keep(positive("max_connections", v), &mut errors));
    let rate = not_null("rate_limit_per_minute", rate_limit_per_minute, &mut errors)
        .and_then(|v| keep(positive("rate_limit_per_minute", v), &mut errors));
    let enabled = not_null("enabled", enabled, &mut errors);
    let failover_order = not_null("failover_order", failover_order, &mut errors);
    let security = not_null("security", security, &mut errors);
    for (field, v) in [("username", &username), ("password", &password)] {
        if let Some(Some(v)) = v {
            keep(not_empty(field, v), &mut errors);
        }
    }
    if !errors.is_empty() {
        return Err(ApiError::Validation(errors));
    }
    // `$11` and `$13` say whether the key was sent; the value beside each is what to store, and
    // `null` there is what clears.
    let encrypted = password
        .iter()
        .flatten()
        .next()
        .map(|p| state.secrets.encrypt(p.as_bytes()));
    let changed = sqlx::query!(
        "update relays set name = coalesce($3, name), host = coalesce($4, host), \
         port = coalesce($5, port), security = coalesce($6, security), \
         max_connections = coalesce($7, max_connections), \
         rate_limit_per_minute = coalesce($8, rate_limit_per_minute), \
         enabled = coalesce($9, enabled), failover_order = coalesce($10, failover_order), \
         username = case when $11 then $12 else username end, \
         password_encrypted = case when $13 then $14 else password_encrypted end, \
         updated_at = now() \
         where workspace_id = $1 and id = $2 returning name, host",
        workspace_id,
        id,
        new_name,
        new_host,
        new_port,
        security as Option<Security>,
        max_connections,
        rate,
        enabled,
        failover_order,
        username.is_some(),
        username.flatten(),
        password.is_some(),
        encrypted,
    )
    .fetch_optional(&mut *tx)
    .await
    .map_err(name_taken)?
    .ok_or(ApiError::NotFound)?;
    record(
        &mut tx,
        &meta,
        &principal,
        actions::RELAY_UPDATED,
        (workspace_id, id),
        json!({ "name": changed.name, "host": changed.host, "fields": fields }),
    )
    .await?;
    let relay = get(&mut tx, workspace_id, id).await?;
    tx.commit().await?;
    Ok(Json(relay))
}

/// A hard delete. The relay's token bucket goes with it.
#[utoipa::path(
    delete,
    path = "/api/v1/workspaces/{workspace_id}/relays/{id}",
    operation_id = "delete_relay",
    params(
        ("workspace_id" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Relay id"),
    ),
    responses(
        (status = 204, description = "Deleted"),
        (status = 403, description = "insufficient-role: configuring a relay takes admin"),
        (status = 404, description = "not a member, or no such relay in this workspace"),
    ),
)]
pub async fn delete_relay(
    State(state): State<AppState>,
    meta: RequestMeta,
    principal: Principal,
    ApiPath((workspace_id, id)): ApiPath<(Uuid, Uuid)>,
) -> ApiResult<StatusCode> {
    let mut tx = state.pool.begin().await?;
    authorize(&mut *tx, &principal, workspace_id, Role::Admin).await?;
    let gone = sqlx::query!(
        "delete from relays where workspace_id = $1 and id = $2 returning name, host",
        workspace_id,
        id,
    )
    .fetch_one(&mut *tx)
    .await?;
    record(
        &mut tx,
        &meta,
        &principal,
        actions::RELAY_DELETED,
        (workspace_id, id),
        json!({ "name": gone.name, "host": gone.host }),
    )
    .await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}
