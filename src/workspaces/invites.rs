// SPDX-License-Identifier: AGPL-3.0-only

//! Invites by link. An invite token is a bearer credential: only its sha256 is stored, it is shown
//! once (in `invite_url`, at creation) and never logged or audited. Three endpoints are
//! workspace-scoped and go through `authorize`; the two public ones authorise by token instead,
//! and a token that is unknown, expired, used or revoked is one and the same `404`.

use axum::{
    Json, Router,
    extract::State,
    http::{StatusCode, header},
    routing::{delete, get, post},
};
use axum_extra::extract::CookieJar;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::json;
use utoipa::ToSchema;
use uuid::Uuid;

use super::roles::{Role, can_assign};
use crate::{
    app::AppState,
    audit::{self, Actor, AuditEvent, actions},
    auth::{CurrentUser, Principal, authorize, sessions},
    crypto::tokens::{random_token, sha256},
    error::{ApiError, ApiJson, ApiPath, ApiQuery, ApiResult, FieldError},
    http::request_meta::RequestMeta,
    pagination::{Page, PageParams},
    users,
};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/v1/workspaces/{id}/invites", get(list).post(create))
        .route(
            "/api/v1/workspaces/{id}/invites/{invite_id}",
            delete(revoke),
        )
        .route("/api/v1/invites/{token}", get(preview))
        .route("/api/v1/invites/{token}/accept", post(accept))
}

#[derive(Deserialize, ToSchema)]
pub struct NewInvite {
    pub email: String,
    pub role: Role,
}

/// The one response that carries the token, inside `invite_url`.
#[derive(Serialize, ToSchema)]
pub struct CreatedInvite {
    pub id: Uuid,
    pub email: String,
    pub role: Role,
    pub invite_url: String,
    pub expires_at: DateTime<Utc>,
}

/// An open invite as the workspace's admins see it: no token, no URL.
#[derive(Serialize, ToSchema)]
pub struct InviteSummary {
    pub id: Uuid,
    pub email: String,
    pub role: Role,
    pub invited_by: Option<Uuid>,
    pub expires_at: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
}

/// What the invitee sees before accepting.
#[derive(Serialize, ToSchema)]
pub struct InvitePreview {
    pub workspace_name: String,
    pub email: String,
    pub role: Role,
    /// Whether to show a sign-in or a set-a-password form.
    pub account_exists: bool,
}

#[derive(Deserialize, ToSchema)]
pub struct AcceptInvite {
    /// Required, 12 to 256 characters, when no account exists for the invited address.
    pub password: Option<String>,
    /// Used only when this accept creates the account.
    pub display_name: Option<String>,
}

#[derive(Serialize, ToSchema)]
pub struct Accepted {
    pub workspace_id: Uuid,
}

#[utoipa::path(
    post,
    path = "/api/v1/workspaces/{id}/invites",
    params(("id" = Uuid, Path, description = "Workspace id")),
    request_body = NewInvite,
    responses(
        (status = 201, description = "Created; `invite_url` is the only place the token ever appears", body = CreatedInvite),
        (status = 403, description = "insufficient-role: below admin, or the role is not the caller's to offer"),
        (status = 404, description = "not a member, or no such workspace"),
        (status = 409, description = "already-member"),
        (status = 422, description = "invalid email"),
    ),
)]
pub async fn create(
    State(state): State<AppState>,
    meta: RequestMeta,
    user: CurrentUser,
    ApiPath(id): ApiPath<Uuid>,
    ApiJson(body): ApiJson<NewInvite>,
) -> ApiResult<(
    StatusCode,
    [(header::HeaderName, &'static str); 1],
    Json<CreatedInvite>,
)> {
    let principal = user.principal();
    let mut tx = state.pool.begin().await?;
    let access = authorize(&mut *tx, &principal, id, Role::Admin).await?;
    if !can_assign(access.role, None, body.role) {
        return Err(ApiError::Forbidden("insufficient-role"));
    }
    let email = users::normalize_email(&body.email).map_err(|e| ApiError::Validation(vec![e]))?;

    // Creations in one workspace run one at a time, so the revoke below always sees the other's
    // invite. `no key update` does not block the foreign-key checks of membership inserts.
    sqlx::query!(
        "select id from workspaces where id = $1 for no key update",
        id
    )
    .fetch_one(&mut *tx)
    .await?;
    let is_member = sqlx::query_scalar!(
        r#"select exists(select 1 from memberships m join users u on u.id = m.user_id
                         where m.workspace_id = $1 and u.email = $2) as "exists!""#,
        id,
        email,
    )
    .fetch_one(&mut *tx)
    .await?;
    if is_member {
        return Err(ApiError::Conflict("already-member"));
    }
    sqlx::query!(
        "update invites set revoked_at = now() \
         where workspace_id = $1 and email = $2 and accepted_at is null and revoked_at is null",
        id,
        email,
    )
    .execute(&mut *tx)
    .await?;

    let invite_id = Uuid::now_v7();
    let token = random_token(32);
    let token_hash = sha256(token.as_bytes());
    let expires_at = sqlx::query_scalar!(
        "insert into invites (id, workspace_id, email, role, token_hash, invited_by, expires_at) \
         values ($1, $2, $3, $4, $5, $6, now() + interval '7 days') returning expires_at",
        invite_id,
        id,
        email,
        body.role as Role,
        &token_hash[..],
        user.user_id,
    )
    .fetch_one(&mut *tx)
    .await?;
    audit::record(
        &mut *tx,
        &meta,
        AuditEvent {
            workspace_id: Some(id),
            actor: principal.actor(),
            action: actions::INVITE_CREATED,
            target_type: Some("invite"),
            target_id: Some(invite_id.to_string()),
            metadata: json!({ "email": email, "role": body.role }),
        },
    )
    .await?;
    tx.commit().await?;
    let invite_url = format!(
        "{}/invite/{token}",
        state.config.public_url.as_str().trim_end_matches('/')
    );
    Ok((
        StatusCode::CREATED,
        [(header::CACHE_CONTROL, "no-store")],
        Json(CreatedInvite {
            id: invite_id,
            email,
            role: body.role,
            invite_url,
            expires_at,
        }),
    ))
}

#[utoipa::path(
    get,
    path = "/api/v1/workspaces/{id}/invites",
    params(("id" = Uuid, Path, description = "Workspace id"), PageParams),
    responses(
        (status = 200, description = "Open invites, newest first", body = Page<InviteSummary>),
        (status = 403, description = "insufficient-role"),
        (status = 404, description = "not a member, or no such workspace"),
    ),
)]
pub async fn list(
    State(state): State<AppState>,
    principal: Principal,
    ApiPath(id): ApiPath<Uuid>,
    ApiQuery(page): ApiQuery<PageParams>,
) -> ApiResult<Json<Page<InviteSummary>>> {
    authorize(&state.pool, &principal, id, Role::Admin).await?;
    let limit = page.limit();
    let rows = sqlx::query_as!(
        InviteSummary,
        r#"select id, email, role as "role: Role", invited_by, expires_at, created_at
           from invites
           where workspace_id = $1 and accepted_at is null and revoked_at is null
             and expires_at > now() and ($2::uuid is null or id < $2)
           order by id desc limit $3"#,
        id,
        page.after()?,
        limit + 1,
    )
    .fetch_all(&state.pool)
    .await?;
    Ok(Json(Page::from_rows(rows, limit, |i| i.id)))
}

#[utoipa::path(
    delete,
    path = "/api/v1/workspaces/{id}/invites/{invite_id}",
    params(
        ("id" = Uuid, Path, description = "Workspace id"),
        ("invite_id" = Uuid, Path, description = "Invite id"),
    ),
    responses(
        (status = 204, description = "Revoked"),
        (status = 403, description = "insufficient-role: below admin, or the invite's role is not the caller's to manage"),
        (status = 404, description = "not a member, or no such open invite in this workspace"),
    ),
)]
pub async fn revoke(
    State(state): State<AppState>,
    meta: RequestMeta,
    user: CurrentUser,
    ApiPath((id, invite_id)): ApiPath<(Uuid, Uuid)>,
) -> ApiResult<StatusCode> {
    let principal = user.principal();
    let mut tx = state.pool.begin().await?;
    let access = authorize(&mut *tx, &principal, id, Role::Admin).await?;
    // Locked, so a concurrent accept either lands first (then this is a 404) or waits.
    let invite = sqlx::query!(
        r#"select email, role as "role: Role" from invites
           where id = $1 and workspace_id = $2 and accepted_at is null and revoked_at is null
           for update"#,
        invite_id,
        id,
    )
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(ApiError::NotFound)?;
    if !can_assign(access.role, None, invite.role) {
        return Err(ApiError::Forbidden("insufficient-role"));
    }
    sqlx::query!(
        "update invites set revoked_at = now() where id = $1",
        invite_id
    )
    .execute(&mut *tx)
    .await?;
    audit::record(
        &mut *tx,
        &meta,
        AuditEvent {
            workspace_id: Some(id),
            actor: principal.actor(),
            action: actions::INVITE_REVOKED,
            target_type: Some("invite"),
            target_id: Some(invite_id.to_string()),
            metadata: json!({ "email": invite.email, "role": invite.role }),
        },
    )
    .await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

#[utoipa::path(
    get,
    path = "/api/v1/invites/{token}",
    params(("token" = String, Path, description = "The token from the invite link")),
    responses(
        (status = 200, body = InvitePreview),
        (status = 404, description = "unknown, expired, used or revoked: all the same"),
    ),
)]
pub async fn preview(
    State(state): State<AppState>,
    ApiPath(token): ApiPath<String>,
) -> ApiResult<Json<InvitePreview>> {
    let token_hash = sha256(token.as_bytes());
    let invite = sqlx::query_as!(
        InvitePreview,
        r#"select w.name as workspace_name, i.email, i.role as "role: Role",
                  exists(select 1 from users u where u.email = i.email) as "account_exists!"
           from invites i join workspaces w on w.id = i.workspace_id
           where i.token_hash = $1 and i.accepted_at is null and i.revoked_at is null
             and i.expires_at > now()"#,
        &token_hash[..],
    )
    .fetch_optional(&state.pool)
    .await?
    .ok_or(ApiError::NotFound)?;
    Ok(Json(invite))
}

/// The accept transaction, in order: claim the invite (`accepted_at`), then either find the signed-in
/// user or create the newcomer, create their session, add the membership, audit. All of it is one
/// transaction: any early return drops it and everything rolls back, the claim included.
#[utoipa::path(
    post,
    path = "/api/v1/invites/{token}/accept",
    params(("token" = String, Path, description = "The token from the invite link")),
    request_body = AcceptInvite,
    responses(
        (status = 200, description = "Joined; a newcomer is also signed in (sets the nm_session cookie)", body = Accepted),
        (status = 401, description = "login-required: an account exists for the invited address"),
        (status = 403, description = "invite-email-mismatch: signed in as someone other than the invitee"),
        (status = 404, description = "unknown, expired, used or revoked: all the same"),
        (status = 409, description = "already-member"),
        (status = 422, description = "newcomer's password or display name is invalid"),
    ),
)]
pub async fn accept(
    State(state): State<AppState>,
    meta: RequestMeta,
    jar: CookieJar,
    ApiPath(token): ApiPath<String>,
    ApiJson(body): ApiJson<AcceptInvite>,
) -> ApiResult<(CookieJar, Json<Accepted>)> {
    // Before the transaction, so it does not hold two connections. Only a full session counts.
    let caller = match jar.get(sessions::COOKIE_NAME) {
        Some(c) => sessions::resolve(&state.pool, c.value())
            .await?
            .map(|s| s.user_id),
        None => None,
    };
    let token_hash = sha256(token.as_bytes());
    let mut tx = state.pool.begin().await?;

    // The conditional update is the claim: it locks the row, so a concurrent accept waits here,
    // re-reads the committed row, matches nothing and gets the same 404 as any dead invite.
    let invite = sqlx::query!(
        r#"update invites set accepted_at = now()
           where token_hash = $1 and accepted_at is null and revoked_at is null
             and expires_at > now()
           returning id, workspace_id, email, role as "role: Role""#,
        &token_hash[..],
    )
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(ApiError::NotFound)?;

    let existing = users::find_by_email(&mut *tx, &invite.email).await?;
    let (user_id, new_session) = match (existing, caller) {
        (Some(u), Some(signed_in)) if u.id == signed_in => (u.id, None),
        (Some(_), None) => return Err(ApiError::Unauthorized("login-required")),
        // Signed in as someone else, whether or not the invited address has an account.
        (_, Some(_)) => return Err(ApiError::Forbidden("invite-email-mismatch")),
        (None, None) => {
            let password = body.password.as_deref().ok_or_else(|| {
                ApiError::Validation(vec![FieldError::new("password", "is required")])
            })?;
            let id = users::create_user(
                &mut *tx,
                &invite.email,
                password,
                body.display_name.as_deref(),
                false,
            )
            .await?;
            // As in sign-in: a stale cookie the client presented does not outlive this.
            if let Some(old) = jar.get(sessions::COOKIE_NAME) {
                sessions::delete(&mut *tx, &sessions::hash_cookie_value(old.value())).await?;
            }
            (
                id,
                Some(sessions::create(&mut *tx, id, false, &meta).await?),
            )
        }
    };

    // Never overwrites a role: someone already in the workspace is a conflict, not an upgrade.
    let joined = sqlx::query!(
        "insert into memberships (workspace_id, user_id, role) values ($1, $2, $3) \
         on conflict do nothing",
        invite.workspace_id,
        user_id,
        invite.role as Role,
    )
    .execute(&mut *tx)
    .await?;
    if joined.rows_affected() == 0 {
        return Err(ApiError::Conflict("already-member"));
    }
    audit::record(
        &mut *tx,
        &meta,
        AuditEvent {
            workspace_id: Some(invite.workspace_id),
            actor: Actor::User(user_id),
            action: actions::INVITE_ACCEPTED,
            target_type: Some("invite"),
            target_id: Some(invite.id.to_string()),
            metadata: json!({ "email": invite.email, "role": invite.role }),
        },
    )
    .await?;
    tx.commit().await?;

    // A signed-in user keeps the session they have: replacing it would invalidate the CSRF token
    // their client already holds.
    let jar = match new_session {
        Some(s) => jar.add(sessions::cookie(
            &s.cookie_value,
            &state.config,
            sessions::FULL_MAX_AGE,
        )),
        None => jar,
    };
    Ok((
        jar,
        Json(Accepted {
            workspace_id: invite.workspace_id,
        }),
    ))
}
