// SPDX-License-Identifier: AGPL-3.0-only

use axum::{Json, Router, extract::State, http::StatusCode};
use serde::Deserialize;
use serde_json::json;
use sqlx::PgConnection;
use utoipa::IntoParams;
use uuid::Uuid;

use super::{NewSuppression, Suppression, create, lift, list};
use crate::{
    app::AppState,
    audit::{self, Actor, AuditEvent, actions},
    auth::{CurrentUser, Principal, authorize},
    error::{ApiError, ApiJson, ApiPath, ApiQuery, ApiResult},
    http::request_meta::RequestMeta,
    pagination::{Page, PageParams},
    users,
    workspaces::roles::Role,
};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/api/v1/workspaces/{workspace_id}/suppressions",
            axum::routing::get(list_suppressions).post(create_suppression),
        )
        .route(
            "/api/v1/workspaces/{workspace_id}/suppressions/{id}/lift",
            axum::routing::post(lift_suppression),
        )
        // Instance-wide suppressions: not a workspace's to make or lift, so no workspace role
        // reaches them.
        .route(
            "/api/v1/admin/suppressions",
            axum::routing::get(list_instance_suppressions).post(create_instance_suppression),
        )
        .route(
            "/api/v1/admin/suppressions/{id}/lift",
            axum::routing::post(lift_instance_suppression),
        )
}

/// `?limit=&cursor=&include_lifted=`.
#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct SuppressionParams {
    /// Page size, 1 to 200 (default 50).
    pub limit: Option<u32>,
    /// Opaque cursor from the previous page's `next_cursor`.
    pub cursor: Option<String>,
    /// Also list the ones that were lifted (default: only the ones in force).
    pub include_lifted: Option<bool>,
}

impl SuppressionParams {
    fn page(&self) -> PageParams {
        PageParams {
            limit: self.limit,
            cursor: self.cursor.clone(),
        }
    }
}

/// The user behind a principal, to stamp on the row; an API key has none (the audit row names it).
fn user_of(principal: &Principal) -> Option<Uuid> {
    match principal {
        Principal::User { user_id, .. } => Some(*user_id),
        Principal::ApiKey { .. } => None,
    }
}

/// An instance admin, or `403 instance-admin-required`. Instance-wide suppressions take this and
/// not a workspace role: they block every workspace, so no one workspace's admin may touch them.
async fn require_instance_admin(db: &mut PgConnection, user: &CurrentUser) -> ApiResult<()> {
    let caller = users::find_by_id(&mut *db, user.user_id)
        .await?
        .ok_or(ApiError::Unauthorized("unauthenticated"))?;
    if !caller.is_instance_admin {
        return Err(ApiError::Forbidden("instance-admin-required"));
    }
    Ok(())
}

/// Scope and reason only. Never the address: the audit log outlives an erasure, and what was
/// blocked is already on the row `target_id` names.
async fn record(
    db: &mut PgConnection,
    meta: &RequestMeta,
    actor: Actor,
    action: &'static str,
    s: &Suppression,
) -> ApiResult<()> {
    audit::record(
        &mut *db,
        meta,
        AuditEvent {
            workspace_id: s.workspace_id,
            actor,
            action,
            target_type: Some("suppression"),
            target_id: Some(s.id.to_string()),
            metadata: json!({ "scope": s.scope, "reason": s.reason }),
        },
    )
    .await?;
    Ok(())
}

#[utoipa::path(
    get,
    path = "/api/v1/workspaces/{workspace_id}/suppressions",
    operation_id = "list_suppressions",
    params(("workspace_id" = Uuid, Path, description = "Workspace id"), SuppressionParams),
    responses(
        (status = 200, description = "This workspace's own suppressions, newest first. Instance-wide ones are not listed.", body = Page<Suppression>),
        (status = 404, description = "not a member, or no such workspace"),
        (status = 422, description = "a malformed cursor"),
    ),
)]
pub async fn list_suppressions(
    State(state): State<AppState>,
    principal: Principal,
    ApiPath(workspace_id): ApiPath<Uuid>,
    ApiQuery(params): ApiQuery<SuppressionParams>,
) -> ApiResult<Json<Page<Suppression>>> {
    let mut conn = state.pool.acquire().await?;
    authorize(&mut *conn, &principal, workspace_id, Role::Viewer).await?;
    let page = list(
        &mut conn,
        Some(workspace_id),
        params.include_lifted.unwrap_or(false),
        &params.page(),
    )
    .await?;
    Ok(Json(page))
}

#[utoipa::path(
    post,
    path = "/api/v1/workspaces/{workspace_id}/suppressions",
    operation_id = "create_suppression",
    params(("workspace_id" = Uuid, Path, description = "Workspace id")),
    request_body = NewSuppression,
    responses(
        (status = 201, body = Suppression),
        (status = 403, description = "insufficient-role: adding a suppression takes editor"),
        (status = 404, description = "not a member, or no such workspace"),
        (status = 409, description = "suppression-exists: that address or domain is already blocked"),
        (status = 422, description = "an invalid or missing field, `gdpr_erasure` as the reason, or an unknown field"),
    ),
)]
pub async fn create_suppression(
    State(state): State<AppState>,
    meta: RequestMeta,
    principal: Principal,
    ApiPath(workspace_id): ApiPath<Uuid>,
    ApiJson(input): ApiJson<NewSuppression>,
) -> ApiResult<(StatusCode, Json<Suppression>)> {
    let mut tx = state.pool.begin().await?;
    authorize(&mut *tx, &principal, workspace_id, Role::Editor).await?;
    let s = create(
        &mut tx,
        &state.secrets,
        Some(workspace_id),
        user_of(&principal),
        &input,
    )
    .await?;
    record(
        &mut tx,
        &meta,
        principal.actor(),
        actions::SUPPRESSION_CREATED,
        &s,
    )
    .await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(s)))
}

/// Re-enables mailing someone who opted out, bounced or complained, so it takes admin. The row
/// stays, with `lifted_at` set, and the audit row names the reason it was lifted from.
#[utoipa::path(
    post,
    path = "/api/v1/workspaces/{workspace_id}/suppressions/{id}/lift",
    operation_id = "lift_suppression",
    params(
        ("workspace_id" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Suppression id"),
    ),
    responses(
        (status = 200, description = "The suppression, now lifted", body = Suppression),
        (status = 403, description = "insufficient-role: lifting takes admin; or erasure-not-liftable: nobody lifts an erasure"),
        (status = 404, description = "not a member, or no such suppression in this workspace"),
        (status = 409, description = "suppression-already-lifted"),
    ),
)]
pub async fn lift_suppression(
    State(state): State<AppState>,
    meta: RequestMeta,
    principal: Principal,
    ApiPath((workspace_id, id)): ApiPath<(Uuid, Uuid)>,
) -> ApiResult<Json<Suppression>> {
    let mut tx = state.pool.begin().await?;
    authorize(&mut *tx, &principal, workspace_id, Role::Admin).await?;
    let s = lift(&mut tx, Some(workspace_id), id, user_of(&principal)).await?;
    record(
        &mut tx,
        &meta,
        principal.actor(),
        actions::SUPPRESSION_LIFTED,
        &s,
    )
    .await?;
    tx.commit().await?;
    Ok(Json(s))
}

#[utoipa::path(
    get,
    path = "/api/v1/admin/suppressions",
    operation_id = "list_instance_suppressions",
    params(SuppressionParams),
    responses(
        (status = 200, description = "Instance-wide suppressions, newest first", body = Page<Suppression>),
        (status = 403, description = "instance-admin-required"),
        (status = 422, description = "a malformed cursor"),
    ),
)]
pub async fn list_instance_suppressions(
    State(state): State<AppState>,
    user: CurrentUser,
    ApiQuery(params): ApiQuery<SuppressionParams>,
) -> ApiResult<Json<Page<Suppression>>> {
    let mut conn = state.pool.acquire().await?;
    require_instance_admin(&mut conn, &user).await?;
    let page = list(
        &mut conn,
        None,
        params.include_lifted.unwrap_or(false),
        &params.page(),
    )
    .await?;
    Ok(Json(page))
}

#[utoipa::path(
    post,
    path = "/api/v1/admin/suppressions",
    operation_id = "create_instance_suppression",
    request_body = NewSuppression,
    responses(
        (status = 201, description = "An instance-wide suppression: it blocks in every workspace", body = Suppression),
        (status = 403, description = "instance-admin-required"),
        (status = 409, description = "suppression-exists: that address or domain is already blocked instance-wide"),
        (status = 422, description = "an invalid or missing field, `gdpr_erasure` as the reason, or an unknown field"),
    ),
)]
pub async fn create_instance_suppression(
    State(state): State<AppState>,
    meta: RequestMeta,
    user: CurrentUser,
    ApiJson(input): ApiJson<NewSuppression>,
) -> ApiResult<(StatusCode, Json<Suppression>)> {
    let mut tx = state.pool.begin().await?;
    require_instance_admin(&mut tx, &user).await?;
    let s = create(&mut tx, &state.secrets, None, Some(user.user_id), &input).await?;
    record(
        &mut tx,
        &meta,
        Actor::User(user.user_id),
        actions::SUPPRESSION_CREATED,
        &s,
    )
    .await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(s)))
}

#[utoipa::path(
    post,
    path = "/api/v1/admin/suppressions/{id}/lift",
    operation_id = "lift_instance_suppression",
    params(("id" = Uuid, Path, description = "Suppression id")),
    responses(
        (status = 200, description = "The suppression, now lifted", body = Suppression),
        (status = 403, description = "instance-admin-required; or erasure-not-liftable: nobody lifts an erasure"),
        (status = 404, description = "no such instance-wide suppression"),
        (status = 409, description = "suppression-already-lifted"),
    ),
)]
pub async fn lift_instance_suppression(
    State(state): State<AppState>,
    meta: RequestMeta,
    user: CurrentUser,
    ApiPath(id): ApiPath<Uuid>,
) -> ApiResult<Json<Suppression>> {
    let mut tx = state.pool.begin().await?;
    require_instance_admin(&mut tx, &user).await?;
    let s = lift(&mut tx, None, id, Some(user.user_id)).await?;
    record(
        &mut tx,
        &meta,
        Actor::User(user.user_id),
        actions::SUPPRESSION_LIFTED,
        &s,
    )
    .await?;
    tx.commit().await?;
    Ok(Json(s))
}
