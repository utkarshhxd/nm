// SPDX-License-Identifier: AGPL-3.0-only

//! `GET …/contacts/{id}/consent` — one contact's consent history, for the operator who has to
//! answer "did this person agree, and how?".
//!
//! It is the only reader of `consent_log` in the product. Everything else writes.

use std::net::IpAddr;

use axum::{Json, Router, extract::State, routing::get};
use chrono::{DateTime, Utc};
use serde::Serialize;
use utoipa::ToSchema;
use uuid::Uuid;

use super::{Action, Source};
use crate::{
    app::AppState,
    auth::{Principal, authorize},
    contacts,
    error::{ApiPath, ApiQuery, ApiResult},
    pagination::{Page, PageParams},
    workspaces::roles::Role,
};

pub fn routes() -> Router<AppState> {
    Router::new().route(
        "/api/v1/workspaces/{workspace_id}/contacts/{id}/consent",
        get(list_consent),
    )
}

/// One row of the log, with the list's name joined on.
///
/// **The evidence columns are the point of the row**, not decoration on it: a row that says
/// `subscribed` without saying how is what this milestone exists to prevent. `ip` and
/// `user_agent` are the subscriber's own, written only by the public routes where the request
/// really was theirs; an operator or API change carries neither and names the actor in
/// `source_ref` instead.
#[derive(Debug, Serialize, ToSchema)]
pub struct ConsentEntry {
    pub id: Uuid,
    /// `requested` means the person was **asked** and has not agreed — a double opt-in that is
    /// still pending. It is not a softer `subscribed`.
    pub action: Action,
    /// The code path the change arrived by, never who made it: a session-authenticated operator
    /// using bulk subscribe is recorded as `api`.
    pub source: Source,
    /// Null for a workspace-wide change, and for a row whose list has since been deleted —
    /// `0020_consent_log.sql` nulls the pointer and keeps the evidence.
    pub list_id: Option<Uuid>,
    /// The list's name now. Null whenever `list_id` is.
    pub list_name: Option<String>,
    /// Whatever caused the change: the form id, the import id, the user or API key id.
    pub source_ref: Option<String>,
    /// The subscriber's own address, on the public routes only.
    #[schema(value_type = Option<String>)]
    pub ip: Option<IpAddr>,
    /// The subscriber's own, on the public routes only.
    pub user_agent: Option<String>,
    /// The nonce of the public token that authorised the change: *which* link was clicked.
    pub token_jti: Option<String>,
    pub created_at: DateTime<Utc>,
}

/// One contact's consent history, newest first.
///
/// **Ordered and paged by `id`, not `created_at`.** `created_at` ties — Postgres `now()` is
/// transaction-start time, a membership write and its consent row share a transaction, and the
/// set-based writers insert a whole batch in one statement — so paging on it would skip or
/// repeat the rows that share a value. `id` is UUID v7, so `id desc` is both a total order and
/// newest-first; `0024_consent_log_paging_index.sql` is the index that serves it.
#[utoipa::path(
    get,
    path = "/api/v1/workspaces/{workspace_id}/contacts/{id}/consent",
    operation_id = "list_consent",
    params(
        ("workspace_id" = Uuid, Path, description = "Workspace id"),
        ("id" = Uuid, Path, description = "Contact id"),
        PageParams,
    ),
    responses(
        (status = 200, description = "Every consent row for this contact, newest first, paged by row id", body = Page<ConsentEntry>),
        (status = 404, description = "not a member, or no such contact in this workspace"),
        (status = 422, description = "a malformed cursor"),
    ),
)]
pub async fn list_consent(
    State(state): State<AppState>,
    principal: Principal,
    ApiPath((workspace_id, id)): ApiPath<(Uuid, Uuid)>,
    ApiQuery(page): ApiQuery<PageParams>,
) -> ApiResult<Json<Page<ConsentEntry>>> {
    let mut conn = state.pool.acquire().await?;
    authorize(&mut *conn, &principal, workspace_id, Role::Viewer).await?;
    // Another workspace's contact is a 404 here, before any row is read.
    contacts::get(&mut conn, workspace_id, id).await?;
    let limit = page.limit();
    let rows = sqlx::query_as!(
        ConsentEntry,
        r#"select c.id, c.action as "action: Action", c.source as "source: Source",
                  c.list_id, l.name as "list_name?", c.source_ref, c.ip as "ip: IpAddr",
                  c.user_agent, c.token_jti, c.created_at
           from consent_log c
           left join lists l on l.workspace_id = c.workspace_id and l.id = c.list_id
           where c.workspace_id = $1 and c.contact_id = $2 and ($3::uuid is null or c.id < $3)
           order by c.id desc limit $4"#,
        workspace_id,
        id,
        page.after()?,
        limit + 1,
    )
    .fetch_all(&mut *conn)
    .await?;
    Ok(Json(Page::from_rows(rows, limit, |r| r.id)))
}
