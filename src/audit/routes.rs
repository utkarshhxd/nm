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
#[derive(Serialize, ToSchema)]
pub struct AuditEntry {
    pub id: Uuid,
    pub action: String,
    pub actor_user_id: Option<Uuid>,
    pub actor_api_key_id: Option<Uuid>,
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
    let rows = sqlx::query_as!(
        AuditEntry,
        r#"select id, action, actor_user_id, actor_api_key_id, target_type, target_id,
                  ip as "ip: IpAddr", metadata, created_at
           from audit_log
           where workspace_id = $1 and ($2::uuid is null or id < $2)
           order by id desc limit $3"#,
        id,
        page.after()?,
        limit + 1,
    )
    .fetch_all(&state.pool)
    .await?;
    Ok(Json(Page::from_rows(rows, limit, |e| e.id)))
}
