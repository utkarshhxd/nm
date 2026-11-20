// SPDX-License-Identifier: AGPL-3.0-only

use axum::{Json, Router, extract::State, http::StatusCode};
use serde::Deserialize;
use serde_json::json;
use sqlx::PgConnection;
use utoipa::IntoParams;
use uuid::Uuid;

use super::{NewSuppression, Reason, Scope, Suppression, create, lift, list};
use crate::{
    app::AppState,
    audit::{self, Actor, AuditEvent, actions},
    auth::{CurrentUser, Principal, authorize},
    consent,
    error::{ApiError, ApiJson, ApiPath, ApiQuery, ApiResult},
    http::request_meta::RequestMeta,
    pagination::{Page, PageParams},
    users,
    workspaces::roles::Role,
};

/// Writes the `consent_log` row for a workspace-wide opt-out an **operator** set or ended, so that
/// the log answers "is this person opted out of everything, and since when" in both directions.
///
/// Until this existed it could only answer in one. `public::unsubscribe::one_click` writes
/// `suppressed` with a null `list_id` when a subscriber opts out of everything; the identical
/// fact arriving through this API wrote an `audit_log` row and no consent row, and *ending* it —
/// here, or through the preference center — wrote nothing at all in either log. So the consent
/// log could say somebody had opted out of everything and could never say that it ended, which is
/// spec §4's "a consent log that loses the when" applied to the lift. `Resubscribed` with a null
/// `list_id` is §7.1's word for it and is already in the table's check constraint.
///
/// **No IP and no user agent.** An operator is not the subscriber, and a row carrying the
/// operator's address reads in court as the subscriber's (`0020_consent_log.sql`). `source_ref`
/// carries the suppression's id instead, which ties this row to the `audit_log` row that names
/// who did it; `audit::record` at the call site is still the record of the actor.
///
/// **What it deliberately does not record**, because `consent_log` is keyed on a contact in a
/// workspace and these are not:
///
/// * an **instance-wide** suppression (`workspace_id` is null) — it is a fact about an address in
///   every workspace at once, and there is no one contact it belongs to;
/// * a **domain**-scope suppression, for the same reason, multiplied by the domain;
/// * an address with **no contact** in the workspace — nothing to hang the row on. The
///   suppression still stands and still blocks; `audit_log` is where it is recorded.
async fn record_workspace_optout(
    tx: &mut PgConnection,
    s: &Suppression,
    action: consent::Action,
) -> ApiResult<()> {
    if s.reason != Reason::UnsubscribedAll || s.scope != Scope::Address {
        return Ok(());
    }
    let (Some(workspace_id), Some(email)) = (s.workspace_id, s.email.as_deref()) else {
        return Ok(());
    };
    // Both are normalised — `contacts::create` and `suppressions::create` apply the same
    // `normalize_email` — so this is the equality the unique index is on.
    let found = sqlx::query_scalar!(
        "select id from contacts where workspace_id = $1 and email = $2",
        workspace_id,
        email,
    )
    .fetch_optional(&mut *tx)
    .await?;
    let Some(contact_id) = found else {
        return Ok(());
    };
    consent::record(
        tx,
        workspace_id,
        contact_id,
        None,
        action,
        consent::Source::Operator,
        &consent::Context {
            source_ref: Some(s.id.to_string()),
            ..Default::default()
        },
    )
    .await
}

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
    record_workspace_optout(&mut tx, &s, consent::Action::Suppressed).await?;
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
    record_workspace_optout(&mut tx, &s, consent::Action::Resubscribed).await?;
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
