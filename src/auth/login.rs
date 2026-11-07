// SPDX-License-Identifier: AGPL-3.0-only

use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    routing::{get, post},
};
use axum_extra::extract::{CookieJar, cookie::Cookie};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::PgPool;
use utoipa::ToSchema;
use uuid::Uuid;

use super::{CurrentUser, rate_limit, sessions};
use crate::{
    app::AppState,
    audit::{self, Actor, AuditEvent, actions},
    crypto::password::{MAX_PASSWORD_BYTES, burn_verify, validate_password, verify_password},
    error::{ApiError, ApiJson, ApiResult, FieldError},
    http::request_meta::RequestMeta,
    users,
};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/v1/auth/login", post(login))
        .route("/api/v1/auth/logout", post(logout))
        .route("/api/v1/me", get(me))
        .route("/api/v1/me/password", post(change_password))
}

#[derive(Deserialize, ToSchema)]
pub struct LoginRequest {
    pub email: String,
    pub password: String,
}

#[derive(Serialize, ToSchema)]
pub struct LoginResponse {
    /// `ok` once signed in; `mfa_required` when a second factor must follow at
    /// `POST /api/v1/auth/login/totp` (the cookie set is then a 10-minute pending session).
    pub status: &'static str,
    /// The second factors on offer; present only for `mfa_required`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub methods: Option<Vec<&'static str>>,
}

impl LoginResponse {
    pub fn ok() -> LoginResponse {
        LoginResponse {
            status: "ok",
            methods: None,
        }
    }

    fn mfa_required() -> LoginResponse {
        LoginResponse {
            status: "mfa_required",
            methods: Some(vec!["totp"]),
        }
    }
}

#[derive(Serialize, ToSchema)]
pub struct MeResponse {
    pub id: Uuid,
    pub email: String,
    pub display_name: String,
    pub is_instance_admin: bool,
    pub totp_enabled: bool,
    pub passkey_count: u32,
    pub workspaces: Vec<Value>,
    /// Send as `X-CSRF-Token` on unsafe requests.
    pub csrf_token: String,
}

#[derive(Deserialize, ToSchema)]
pub struct ChangePasswordRequest {
    pub current_password: String,
    pub new_password: String,
}

/// Audits a failed sign-in (standalone: nothing else is written) and returns the one response
/// every failure gets. `email` is the normalised address, or `None` if the submitted one was not
/// an email at all; the raw input is never audited, since it is attacker-controlled and may hold
/// a NUL, which Postgres rejects in `jsonb` and would turn this into a 500.
async fn login_failed(
    state: &AppState,
    meta: &RequestMeta,
    email: Option<&str>,
) -> ApiResult<ApiError> {
    audit::record(
        &state.pool,
        meta,
        AuditEvent {
            workspace_id: None,
            actor: Actor::System,
            action: actions::AUTH_LOGIN_FAILED,
            target_type: None,
            target_id: None,
            metadata: json!({ "email": email }),
        },
    )
    .await?;
    Ok(ApiError::UnauthorizedDetail {
        code: "invalid-credentials",
        detail: "Email or password is incorrect",
    })
}

#[utoipa::path(
    post,
    path = "/api/v1/auth/login",
    request_body = LoginRequest,
    responses(
        (status = 200, description = "Signed in, or a second factor is required; sets the nm_session cookie", body = LoginResponse),
        (status = 401, description = "invalid-credentials"),
    ),
)]
pub async fn login(
    State(state): State<AppState>,
    meta: RequestMeta,
    jar: CookieJar,
    ApiJson(req): ApiJson<LoginRequest>,
) -> ApiResult<(CookieJar, Json<LoginResponse>)> {
    // Session fixation: whatever session the client presented is gone before a new one exists,
    // whether or not these credentials turn out to be good.
    if let Some(old) = jar.get(sessions::COOKIE_NAME) {
        sessions::delete(&state.pool, &sessions::hash_cookie_value(old.value())).await?;
    }

    // A malformed address cannot belong to a user: it is treated as unknown and never echoed.
    let email = users::normalize_email(&req.email).ok();

    // First, before any write or password work, so a locked email or IP is refused even with the
    // right password and a flood of requests cannot keep writing rows. It is keyed on the
    // submitted email and IP only and never reads `users`, so it costs the same for a known and
    // an unknown email.
    rate_limit::check(&state.pool, email.as_deref(), meta.ip).await?;
    // Stored as a failure now and flipped on success, so guesses already in flight count.
    let attempt = rate_limit::record_attempt(&state.pool, email.as_deref(), meta.ip).await?;

    // Reject an over-length password BEFORE looking the user up. `verify_password` bails out
    // without argon2 work on such input, so branching on whether the user exists first would
    // make a 1025-byte password instant for a known email and ~25ms for an unknown one.
    if req.password.len() > MAX_PASSWORD_BYTES {
        return Err(login_failed(&state, &meta, email.as_deref()).await?);
    }

    let user = match &email {
        Some(email) => users::find_by_email(&state.pool, email).await?,
        None => None,
    };
    // shortcut: argon2 (~25ms) runs on the async worker thread, which is fine at M0 sign-in
    // volumes behind the Task 8 rate limit; move it to spawn_blocking when tail latency matters.
    let verified = match user.as_ref().and_then(|u| u.password_hash.as_deref()) {
        Some(hash) => verify_password(&req.password, hash),
        // Unknown user, or one with no password set: the same argon2 cost as a wrong password.
        None => {
            burn_verify();
            false
        }
    };
    let Some(user) = user.filter(|_| verified) else {
        return Err(login_failed(&state, &meta, email.as_deref()).await?);
    };

    // The password stage is a success whether or not a second factor follows, so it stops counting
    // toward the lock: otherwise every user with TOTP would lock their own email after ten
    // sign-ins in 15 minutes. The second factor has its own budget (5 wrong codes, then
    // `mfa-restart`, which does count as a failure).
    rate_limit::mark_succeeded(&state.pool, attempt).await?;

    if user.totp_enabled {
        // Audited, so a password that was right but never followed by a second factor leaves a
        // trace.
        let mut tx = state.pool.begin().await?;
        let pending = sessions::create(&mut *tx, user.id, true, &meta).await?;
        audit::record(
            &mut *tx,
            &meta,
            AuditEvent {
                workspace_id: None,
                actor: Actor::User(user.id),
                action: actions::AUTH_LOGIN_MFA_REQUIRED,
                target_type: Some("user"),
                target_id: Some(user.id.to_string()),
                metadata: json!({ "email": user.email }),
            },
        )
        .await?;
        tx.commit().await?;
        return Ok((
            jar.add(sessions::cookie(
                &pending.cookie_value,
                &state.config,
                sessions::PENDING_MAX_AGE,
            )),
            Json(LoginResponse::mfa_required()),
        ));
    }

    let mut tx = state.pool.begin().await?;
    let session = sessions::create(&mut *tx, user.id, false, &meta).await?;
    audit::record(
        &mut *tx,
        &meta,
        AuditEvent {
            workspace_id: None,
            actor: Actor::User(user.id),
            action: actions::AUTH_LOGIN_SUCCEEDED,
            target_type: Some("user"),
            target_id: Some(user.id.to_string()),
            metadata: json!({ "email": user.email }),
        },
    )
    .await?;
    tx.commit().await?;

    Ok((
        jar.add(sessions::cookie(
            &session.cookie_value,
            &state.config,
            sessions::FULL_MAX_AGE,
        )),
        Json(LoginResponse::ok()),
    ))
}

#[utoipa::path(
    post,
    path = "/api/v1/auth/logout",
    responses(
        (status = 204, description = "Session ended; cookie cleared"),
        (status = 401, description = "unauthenticated"),
    ),
)]
pub async fn logout(
    State(state): State<AppState>,
    meta: RequestMeta,
    user: CurrentUser,
    jar: CookieJar,
) -> ApiResult<(CookieJar, StatusCode)> {
    let mut tx = state.pool.begin().await?;
    sessions::delete(&mut *tx, &user.session_hash).await?;
    audit::record(
        &mut *tx,
        &meta,
        AuditEvent {
            workspace_id: None,
            actor: Actor::User(user.user_id),
            action: actions::AUTH_LOGOUT,
            target_type: Some("user"),
            target_id: Some(user.user_id.to_string()),
            metadata: json!({}),
        },
    )
    .await?;
    tx.commit().await?;
    // A removal cookie must repeat the Path it was set with.
    let jar = jar.remove(Cookie::build((sessions::COOKIE_NAME, "")).path("/"));
    Ok((jar, StatusCode::NO_CONTENT))
}

#[utoipa::path(
    get,
    path = "/api/v1/me",
    responses(
        (status = 200, body = MeResponse),
        (status = 401, description = "unauthenticated"),
    ),
)]
pub async fn me(State(state): State<AppState>, user: CurrentUser) -> ApiResult<Json<MeResponse>> {
    // Sessions cascade with their user, so a missing row means it was deleted mid-request.
    let u = users::find_by_id(&state.pool, user.user_id)
        .await?
        .ok_or(ApiError::Unauthorized("unauthenticated"))?;
    Ok(Json(MeResponse {
        id: u.id,
        email: u.email,
        display_name: u.display_name,
        is_instance_admin: u.is_instance_admin,
        totp_enabled: u.totp_enabled,
        // Later tasks fill these in.
        passkey_count: 0,
        workspaces: vec![],
        csrf_token: user.csrf_token,
    }))
}

/// Re-checks the signed-in user's password before a sensitive change, returning their row.
/// `401 invalid-credentials` when it is wrong.
pub(super) async fn require_password(
    db: &PgPool,
    user_id: Uuid,
    password: &str,
) -> ApiResult<users::User> {
    let wrong = || ApiError::UnauthorizedDetail {
        code: "invalid-credentials",
        detail: "Current password is incorrect",
    };
    // Over-length first, as in `login`: no lookup and no argon2 for input that cannot match.
    if password.len() > MAX_PASSWORD_BYTES {
        return Err(wrong());
    }
    let user = users::find_by_id(db, user_id)
        .await?
        .ok_or(ApiError::Unauthorized("unauthenticated"))?;
    let matches = match user.password_hash.as_deref() {
        Some(h) => verify_password(password, h),
        None => {
            burn_verify();
            false
        }
    };
    if matches { Ok(user) } else { Err(wrong()) }
}

#[utoipa::path(
    post,
    path = "/api/v1/me/password",
    request_body = ChangePasswordRequest,
    responses(
        (status = 204, description = "Password changed; every other session revoked"),
        (status = 401, description = "invalid-credentials (wrong current password) or unauthenticated"),
        (status = 422, description = "new_password is not 12-256 characters"),
    ),
)]
pub async fn change_password(
    State(state): State<AppState>,
    meta: RequestMeta,
    user: CurrentUser,
    ApiJson(req): ApiJson<ChangePasswordRequest>,
) -> ApiResult<StatusCode> {
    validate_password(&req.new_password)
        .map_err(|e| ApiError::Validation(vec![FieldError::new("new_password", e.message)]))?;

    require_password(&state.pool, user.user_id, &req.current_password).await?;

    // One transaction: a new hash with the old sessions still alive would be the worst outcome.
    let mut tx = state.pool.begin().await?;
    users::set_password(&mut *tx, user.user_id, &req.new_password).await?;
    sessions::revoke_all_for_user(&mut *tx, user.user_id, Some(user.session_hash)).await?;
    audit::record(
        &mut *tx,
        &meta,
        AuditEvent {
            workspace_id: None,
            actor: Actor::User(user.user_id),
            action: actions::AUTH_PASSWORD_CHANGED,
            target_type: Some("user"),
            target_id: Some(user.user_id.to_string()),
            metadata: json!({}),
        },
    )
    .await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}
