// SPDX-License-Identifier: AGPL-3.0-only

use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    routing::{get, patch},
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::{PgConnection, PgExecutor, PgPool, Postgres, Transaction};
use utoipa::ToSchema;
use uuid::Uuid;

use super::roles::{Role, can_assign};
use crate::{
    app::AppState,
    audit::{self, AuditEvent, actions},
    auth::{CurrentUser, Principal, WorkspaceAccess, authorize},
    error::{ApiError, ApiJson, ApiPath, ApiQuery, ApiResult},
    http::request_meta::RequestMeta,
    pagination::{Page, PageParams},
};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/v1/workspaces/{id}/members", get(list))
        .route(
            "/api/v1/workspaces/{id}/members/{user_id}",
            patch(change_role).delete(remove),
        )
}

#[derive(Serialize, ToSchema)]
pub struct Member {
    pub user_id: Uuid,
    pub email: String,
    pub display_name: String,
    pub role: Role,
    pub joined_at: DateTime<Utc>,
}

#[derive(Deserialize, ToSchema)]
pub struct ChangeRole {
    pub role: Role,
}

async fn find_member(
    db: impl PgExecutor<'_>,
    workspace_id: Uuid,
    user_id: Uuid,
) -> ApiResult<Member> {
    Ok(sqlx::query_as!(
        Member,
        r#"select u.id as user_id, u.email, u.display_name, m.role as "role: Role",
                  m.created_at as joined_at
           from memberships m join users u on u.id = m.user_id
           where m.workspace_id = $1 and m.user_id = $2"#,
        workspace_id,
        user_id,
    )
    .fetch_one(db)
    .await?)
}

/// Opens the transaction for a write to a workspace's memberships and returns the caller's access
/// and the workspace's owners, whose rows it holds locked until commit.
///
/// Every membership write takes the owner-row lock first, so within a workspace they run one at a
/// time. That is what makes the last-owner check sound: of two requests that each step down a
/// different owner, the second waits here, then finds the first has committed, sees one owner
/// left, and gets `last-owner`. It also means the caller's own role cannot change between the
/// check below and the write. Owners are locked in `user_id` order so two writers cannot deadlock.
///
/// `authorize` runs twice on purpose: once before the lock, so a non-member (404) or an
/// under-privileged member (403) never queues behind or holds a lock on a workspace they have
/// no standing in, and once after, to re-check against the state the lock waited for.
async fn begin_membership_write(
    pool: &PgPool,
    principal: &Principal,
    workspace_id: Uuid,
    min: Role,
) -> ApiResult<(Transaction<'static, Postgres>, WorkspaceAccess, Vec<Uuid>)> {
    let mut tx = pool.begin().await?;
    authorize(&mut *tx, principal, workspace_id, min).await?;
    let owners = sqlx::query_scalar!(
        "select user_id from memberships where workspace_id = $1 and role = 'owner' \
         order by user_id for update",
        workspace_id,
    )
    .fetch_all(&mut *tx)
    .await?;
    let access = authorize(&mut *tx, principal, workspace_id, min).await?;
    Ok((tx, access, owners))
}

/// `404` if `user_id` is not a member of the workspace.
async fn current_role(db: &mut PgConnection, workspace_id: Uuid, user_id: Uuid) -> ApiResult<Role> {
    sqlx::query_scalar!(
        r#"select role as "role: Role" from memberships where workspace_id = $1 and user_id = $2"#,
        workspace_id,
        user_id,
    )
    .fetch_optional(db)
    .await?
    .ok_or(ApiError::NotFound)
}

#[utoipa::path(
    get,
    path = "/api/v1/workspaces/{id}/members",
    operation_id = "list_members",
    params(("id" = Uuid, Path, description = "Workspace id"), PageParams),
    responses(
        (status = 200, body = Page<Member>),
        (status = 404, description = "not a member, or no such workspace"),
    ),
)]
pub async fn list(
    State(state): State<AppState>,
    principal: Principal,
    ApiPath(id): ApiPath<Uuid>,
    ApiQuery(page): ApiQuery<PageParams>,
) -> ApiResult<Json<Page<Member>>> {
    authorize(&state.pool, &principal, id, Role::Viewer).await?;
    let limit = page.limit();
    let rows = sqlx::query_as!(
        Member,
        r#"select u.id as user_id, u.email, u.display_name, m.role as "role: Role",
                  m.created_at as joined_at
           from memberships m join users u on u.id = m.user_id
           where m.workspace_id = $1 and ($2::uuid is null or m.user_id < $2)
           order by m.user_id desc limit $3"#,
        id,
        page.after()?,
        limit + 1,
    )
    .fetch_all(&state.pool)
    .await?;
    Ok(Json(Page::from_rows(rows, limit, |m| m.user_id)))
}

#[utoipa::path(
    patch,
    path = "/api/v1/workspaces/{id}/members/{user_id}",
    params(
        ("id" = Uuid, Path, description = "Workspace id"),
        ("user_id" = Uuid, Path, description = "Member's user id"),
    ),
    request_body = ChangeRole,
    responses(
        (status = 200, body = Member),
        (status = 403, description = "insufficient-role: below admin, or the change is not the caller's to make"),
        (status = 404, description = "not a member of the workspace, or the target is not"),
        (status = 409, description = "last-owner"),
    ),
)]
pub async fn change_role(
    State(state): State<AppState>,
    meta: RequestMeta,
    user: CurrentUser,
    ApiPath((id, target)): ApiPath<(Uuid, Uuid)>,
    ApiJson(body): ApiJson<ChangeRole>,
) -> ApiResult<Json<Member>> {
    let principal = user.principal();
    let (mut tx, access, owners) =
        begin_membership_write(&state.pool, &principal, id, Role::Admin).await?;
    let from = current_role(&mut tx, id, target).await?;
    if !can_assign(access.role, Some(from), body.role) {
        return Err(ApiError::Forbidden("insufficient-role"));
    }
    if from == Role::Owner && body.role != Role::Owner && owners.len() <= 1 {
        return Err(ApiError::Conflict("last-owner"));
    }
    sqlx::query!(
        "update memberships set role = $3 where workspace_id = $1 and user_id = $2",
        id,
        target,
        body.role as Role,
    )
    .execute(&mut *tx)
    .await?;
    audit::record(
        &mut *tx,
        &meta,
        AuditEvent {
            workspace_id: Some(id),
            actor: principal.actor(),
            action: actions::MEMBER_ROLE_CHANGED,
            target_type: Some("user"),
            target_id: Some(target.to_string()),
            metadata: json!({ "from": from, "to": body.role }),
        },
    )
    .await?;
    let member = find_member(&mut *tx, id, target).await?;
    tx.commit().await?;
    Ok(Json(member))
}

#[utoipa::path(
    delete,
    path = "/api/v1/workspaces/{id}/members/{user_id}",
    operation_id = "remove_member",
    params(
        ("id" = Uuid, Path, description = "Workspace id"),
        ("user_id" = Uuid, Path, description = "Member's user id"),
    ),
    responses(
        (status = 204, description = "Removed"),
        (status = 403, description = "insufficient-role: removing someone else needs admin, and an owner needs an owner"),
        (status = 404, description = "not a member of the workspace, or the target is not"),
        (status = 409, description = "last-owner"),
    ),
)]
pub async fn remove(
    State(state): State<AppState>,
    meta: RequestMeta,
    user: CurrentUser,
    ApiPath((id, target)): ApiPath<(Uuid, Uuid)>,
) -> ApiResult<StatusCode> {
    let principal = user.principal();
    // Anyone may remove themselves; removing someone else takes admin.
    let leaving = user.user_id == target;
    let min = if leaving { Role::Viewer } else { Role::Admin };
    let (mut tx, access, owners) = begin_membership_write(&state.pool, &principal, id, min).await?;
    let role = current_role(&mut tx, id, target).await?;
    // An admin may not remove an owner, as they may not demote one.
    if role == Role::Owner && access.role < Role::Owner {
        return Err(ApiError::Forbidden("insufficient-role"));
    }
    if role == Role::Owner && owners.len() <= 1 {
        return Err(ApiError::Conflict("last-owner"));
    }
    sqlx::query!(
        "delete from memberships where workspace_id = $1 and user_id = $2",
        id,
        target,
    )
    .execute(&mut *tx)
    .await?;
    audit::record(
        &mut *tx,
        &meta,
        AuditEvent {
            workspace_id: Some(id),
            actor: principal.actor(),
            action: actions::MEMBER_REMOVED,
            target_type: Some("user"),
            target_id: Some(target.to_string()),
            metadata: json!({ "role": role }),
        },
    )
    .await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}
