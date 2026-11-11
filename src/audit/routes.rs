// SPDX-License-Identifier: AGPL-3.0-only

use std::net::IpAddr;

use axum::{Json, Router, extract::State, routing::get};
use chrono::{DateTime, Utc};
use serde::Serialize;
use utoipa::ToSchema;
use uuid::Uuid;

use crate::{
    app::AppState,
    auth::{Principal, authorize},
    error::{ApiPath, ApiQuery, ApiResult},
    pagination::{Page, PageParams},
    workspaces::roles::Role,
};

pub fn routes() -> Router<AppState> {
    Router::new().route("/api/v1/workspaces/{id}/audit-log", get(list))
}

/// One audit row, as stored. `metadata` is returned verbatim, which is why writers must never put
/// a secret in it.
///
/// `actor_email` and `actor_api_key_prefix` name the actor the ids point at, resolved by the two
/// LEFT JOINs in `list`. They are a convenience for display, not a second identity: the ids stay
/// authoritative, and either name is `None` once the user or key behind it has been deleted —
/// `audit_log` has no foreign keys on purpose, so its rows outlive both.
#[derive(Serialize, ToSchema)]
pub struct AuditEntry {
    pub id: Uuid,
    pub action: String,
    pub actor_user_id: Option<Uuid>,
    pub actor_api_key_id: Option<Uuid>,
    pub actor_email: Option<String>,
    /// The key's public prefix, without the `nm_` scheme: a key is shown as `nm_{prefix}`.
    pub actor_api_key_prefix: Option<String>,
    pub target_type: Option<String>,
    pub target_id: Option<String>,
    #[schema(value_type = Option<String>)]
    pub ip: Option<IpAddr>,
    #[schema(value_type = Object)]
    pub metadata: serde_json::Value,
    pub created_at: DateTime<Utc>,
}

#[utoipa::path(
    get,
    path = "/api/v1/workspaces/{id}/audit-log",
    operation_id = "list_audit_log",
    params(("id" = Uuid, Path, description = "Workspace id"), PageParams),
    responses(
        (status = 200, description = "Newest first", body = Page<AuditEntry>),
        (status = 403, description = "insufficient-role: below admin"),
        (status = 404, description = "not a member, or no such workspace"),
    ),
)]
pub async fn list(
    State(state): State<AppState>,
    principal: Principal,
    ApiPath(id): ApiPath<Uuid>,
    ApiQuery(page): ApiQuery<PageParams>,
) -> ApiResult<Json<Page<AuditEntry>>> {
    authorize(&state.pool, &principal, id, Role::Admin).await?;
    let limit = page.limit();
    // Audit ids are UUIDv7, so `id desc` is newest first.
    // LEFT JOINs: the predicate stays `a.workspace_id = $1`, and an entry whose actor has since
    // been deleted — or which has no actor at all — must still be returned, unnamed.
    //
    // Both joins are UNSCOPED — no `workspace_id` of their own — and are safe only because the
    // actor ids come from rows this workspace owns, and nothing writes a foreign actor into a
    // workspace-scoped audit row. The day something does, this join hands that actor's email to
    // a different tenant's admins. A new audit writer either keeps that invariant or these joins
    // need their own workspace predicate.
    let rows = sqlx::query_as!(
        AuditEntry,
        r#"select a.id, a.action, a.actor_user_id, a.actor_api_key_id,
                  u.email as "actor_email?", k.prefix as "actor_api_key_prefix?",
                  a.target_type, a.target_id, a.ip as "ip: IpAddr", a.metadata, a.created_at
           from audit_log a
           left join users u on u.id = a.actor_user_id
           left join api_keys k on k.id = a.actor_api_key_id
           where a.workspace_id = $1 and ($2::uuid is null or a.id < $2)
           order by a.id desc limit $3"#,
        id,
        page.after()?,
        limit + 1,
    )
    .fetch_all(&state.pool)
    .await?;
    Ok(Json(Page::from_rows(rows, limit, |e| e.id)))
}
