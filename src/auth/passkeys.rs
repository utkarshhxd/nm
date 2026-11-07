// SPDX-License-Identifier: AGPL-3.0-only

//! Passkeys: registration for a signed-in user, and passwordless sign-in.
//!
//! A passkey sign-in issues a FULL session with no second factor, so it is as sensitive as the
//! password path. The server insists on user verification (`UserVerificationPolicy::Required`), so
//! the one gesture proves possession of the device and a PIN or biometric: that is why TOTP is not
//! asked for even when the user has it enabled.
//!
//! The ceremony state (`auth_challenges.state`) is the server's half of a challenge. It is stored
//! as `jsonb`, consumed by one conditional `delete … returning`, and never returned, logged or
//! audited. Nothing here rate-limits sign-in: a WebAuthn signature cannot be guessed.

use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    routing::{delete, get, post},
};
use axum_extra::extract::CookieJar;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::PgPool;
use url::Url;
use utoipa::ToSchema;
use uuid::Uuid;
use webauthn_rs::{
    Webauthn, WebauthnBuilder,
    prelude::{
        CredentialID, DiscoverableAuthentication, Passkey, PasskeyRegistration,
        PublicKeyCredential, RegisterPublicKeyCredential,
    },
};

use super::{CurrentUser, login::LoginResponse, sessions};
use crate::{
    app::AppState,
    audit::{self, Actor, AuditEvent, actions},
    error::{ApiError, ApiJson, ApiPath, ApiResult},
    http::request_meta::RequestMeta,
    users,
    workspaces::validate_name,
};

const RP_NAME: &str = "NM";

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/v1/me/passkeys", get(list))
        .route("/api/v1/me/passkeys/{id}", delete(remove))
        .route("/api/v1/me/passkeys/register/start", post(register_start))
        .route("/api/v1/me/passkeys/register/finish", post(register_finish))
        // Both are CSRF-exempt by exact path in `http::csrf`: there is no session to hold a token.
        .route("/api/v1/auth/passkey/login/start", post(login_start))
        .route("/api/v1/auth/passkey/login/finish", post(login_finish))
}

/// The relying party, taken from `public_url` and nothing else: `rp_id` is its host (no port, no
/// scheme, no path), the allowed origin is the URL itself, subdomains and other ports are not
/// allowed. A passkey is bound to `rp_id` by the authenticator, so a wrong value either breaks
/// every ceremony or widens who can use the credentials.
///
/// `None` when the host is not a domain name (an IP address): browsers refuse an IP as a
/// relying-party id, so passkeys cannot work there, and refusing to start the server over it
/// would break an otherwise fine `http://127.0.0.1:8080` install.
pub fn relying_party(public_url: &Url) -> anyhow::Result<Option<Webauthn>> {
    let Some(rp_id) = public_url.domain() else {
        tracing::warn!("public_url host is not a domain name; passkeys are disabled");
        return Ok(None);
    };
    Ok(Some(
        WebauthnBuilder::new(rp_id, public_url)?
            .rp_name(RP_NAME)
            .build()?,
    ))
}

fn invalid_passkey() -> ApiError {
    ApiError::UnauthorizedDetail {
        code: "invalid-passkey",
        detail: "Passkey could not be verified",
    }
}

/// A WebAuthn failure is the caller's, not ours: the reason (a bad signature, a wrong origin, a
/// counter that went backwards) goes to the debug log by variant and never to the response, which
/// is the same for every failure.
fn rejected(e: webauthn_rs::prelude::WebauthnError) -> ApiError {
    tracing::debug!(error = ?e, "passkey ceremony rejected");
    invalid_passkey()
}

fn internal(what: &str, e: impl std::fmt::Debug) -> ApiError {
    ApiError::Internal(anyhow::anyhow!("{what}: {e:?}"))
}

/// Writes an in-flight ceremony and returns its id. The expiry is 5 minutes, set by the database
/// clock like every other expiry here.
async fn store_challenge(
    db: &PgPool,
    user_id: Option<Uuid>,
    kind: &'static str,
    state: Value,
) -> sqlx::Result<Uuid> {
    let id = Uuid::now_v7();
    sqlx::query!(
        "insert into auth_challenges (id, user_id, kind, state, expires_at) \
         values ($1, $2, $3, $4, now() + interval '5 minutes')",
        id,
        user_id,
        kind,
        state,
    )
    .execute(db)
    .await?;
    // shortcut: expired challenges are deleted inline on every new one; move to a scheduled job
    // when the queue lands (M1). Best effort: it must not fail the request it rides on. Note it is
    // only as good as the 5-minute window, since an anonymous caller can still add rows to
    // `login/start` as fast as the server answers, and nothing rate-limits that endpoint.
    if let Err(e) = sqlx::query!("delete from auth_challenges where expires_at <= now()")
        .execute(db)
        .await
    {
        tracing::warn!(error = ?e, "auth_challenges cleanup failed");
    }
    Ok(id)
}

/// Spends a challenge and returns its state, or `None` if there is no live one of this kind for
/// this user (unknown, already used, expired, or someone else's all look the same). One
/// conditional `delete … returning`, so two concurrent finishes cannot both receive the state:
/// the second finds no row. A failed finish has spent the challenge as well.
async fn take_challenge(
    db: &PgPool,
    id: Uuid,
    kind: &'static str,
    user_id: Option<Uuid>,
) -> sqlx::Result<Option<Value>> {
    sqlx::query_scalar!(
        "delete from auth_challenges \
         where id = $1 and kind = $2 and user_id is not distinct from $3 and expires_at > now() \
         returning state",
        id,
        kind,
        user_id,
    )
    .fetch_optional(db)
    .await
}

/// The inner `publicKey` object of a WebAuthn options response, in the JSON form
/// `@simplewebauthn/browser` takes as `optionsJSON`.
fn public_key_options(response: &impl Serialize) -> ApiResult<Value> {
    let mut v = serde_json::to_value(response).map_err(|e| internal("serialise options", e))?;
    v.get_mut("publicKey")
        .map(Value::take)
        .ok_or_else(|| internal("options without publicKey", ()))
}

#[derive(Serialize, ToSchema)]
pub struct ChallengeResponse {
    /// Send back with the browser's answer.
    pub challenge_id: Uuid,
    /// The WebAuthn `publicKey` options object, as `@simplewebauthn/browser` expects.
    #[schema(value_type = Object)]
    pub options: Value,
}

#[derive(Deserialize, ToSchema)]
pub struct NewPasskey {
    pub name: String,
}

#[derive(Deserialize, ToSchema)]
pub struct FinishRequest {
    pub challenge_id: Uuid,
    /// The browser's `PublicKeyCredential`, in WebAuthn JSON form.
    #[schema(value_type = Object)]
    pub credential: Value,
}

#[derive(Serialize, ToSchema)]
pub struct CreatedPasskey {
    pub id: Uuid,
    pub name: String,
    pub created_at: DateTime<Utc>,
}

/// A passkey as listed: nothing in it can authenticate.
#[derive(Serialize, ToSchema)]
pub struct PasskeySummary {
    pub id: Uuid,
    pub name: String,
    pub created_at: DateTime<Utc>,
    pub last_used_at: Option<DateTime<Utc>>,
}

/// What a registration challenge keeps: the name the user chose, and the ceremony state.
#[derive(Serialize, Deserialize)]
struct RegistrationChallenge {
    name: String,
    registration: PasskeyRegistration,
}

#[utoipa::path(
    post,
    path = "/api/v1/me/passkeys/register/start",
    request_body = NewPasskey,
    responses(
        (status = 200, description = "Options for navigator.credentials.create; credentials already registered are excluded", body = ChallengeResponse),
        (status = 401, description = "unauthenticated"),
        (status = 403, description = "user-session-required"),
        (status = 409, description = "passkeys-unavailable: public_url host is not a domain name"),
        (status = 422, description = "invalid name"),
    ),
)]
pub async fn register_start(
    State(state): State<AppState>,
    user: CurrentUser,
    ApiJson(req): ApiJson<NewPasskey>,
) -> ApiResult<Json<ChallengeResponse>> {
    let name = validate_name(&req.name).map_err(|e| ApiError::Validation(vec![e]))?;
    let webauthn = state.webauthn()?;
    let u = users::find_by_id(&state.pool, user.user_id)
        .await?
        .ok_or(ApiError::Unauthorized("unauthenticated"))?;
    // The authenticator refuses to enrol a credential it already holds for this account.
    let existing: Vec<CredentialID> = sqlx::query_scalar!(
        "select credential_id from passkeys where user_id = $1",
        user.user_id,
    )
    .fetch_all(&state.pool)
    .await?;
    let display_name = if u.display_name.is_empty() {
        &u.email
    } else {
        &u.display_name
    };
    let (ccr, registration) = webauthn
        .start_passkey_registration(
            u.id,
            &u.email,
            display_name,
            (!existing.is_empty()).then_some(existing),
        )
        .map_err(|e| internal("start registration", e))?;
    let options = public_key_options(&ccr)?;
    let challenge = serde_json::to_value(RegistrationChallenge { name, registration })
        .map_err(|e| internal("serialise registration", e))?;
    let challenge_id =
        store_challenge(&state.pool, Some(u.id), "passkey_register", challenge).await?;
    Ok(Json(ChallengeResponse {
        challenge_id,
        options,
    }))
}

#[utoipa::path(
    post,
    path = "/api/v1/me/passkeys/register/finish",
    request_body = FinishRequest,
    responses(
        (status = 201, description = "Registered", body = CreatedPasskey),
        (status = 401, description = "invalid-passkey: unknown, used or expired challenge, or the credential did not verify; or unauthenticated"),
        (status = 403, description = "user-session-required"),
        (status = 409, description = "passkey-already-registered"),
    ),
)]
pub async fn register_finish(
    State(state): State<AppState>,
    meta: RequestMeta,
    user: CurrentUser,
    ApiJson(req): ApiJson<FinishRequest>,
) -> ApiResult<(StatusCode, Json<CreatedPasskey>)> {
    let webauthn = state.webauthn()?;
    // Bound to this user: another user's challenge id is not found, and is not spent either.
    let stored = take_challenge(
        &state.pool,
        req.challenge_id,
        "passkey_register",
        Some(user.user_id),
    )
    .await?
    .ok_or_else(invalid_passkey)?;
    let RegistrationChallenge { name, registration } =
        serde_json::from_value(stored).map_err(|e| internal("stored registration", e))?;
    let credential: RegisterPublicKeyCredential =
        serde_json::from_value(req.credential).map_err(|_| invalid_passkey())?;
    let passkey = webauthn
        .finish_passkey_registration(&credential, &registration)
        .map_err(rejected)?;

    let id = Uuid::now_v7();
    let mut tx = state.pool.begin().await?;
    let created_at = sqlx::query_scalar!(
        "insert into passkeys (id, user_id, credential_id, passkey, name) \
         values ($1, $2, $3, $4, $5) returning created_at",
        id,
        user.user_id,
        passkey.cred_id().as_slice(),
        serde_json::to_value(&passkey).map_err(|e| internal("serialise passkey", e))?,
        name,
    )
    .fetch_one(&mut *tx)
    .await
    .map_err(|e| match e.as_database_error() {
        Some(d) if d.is_unique_violation() => ApiError::Conflict("passkey-already-registered"),
        _ => e.into(),
    })?;
    audit::record(
        &mut *tx,
        &meta,
        AuditEvent {
            workspace_id: None,
            actor: Actor::User(user.user_id),
            action: actions::AUTH_PASSKEY_ADDED,
            target_type: Some("passkey"),
            target_id: Some(id.to_string()),
            // The name is validated: no control characters, at most 100.
            metadata: json!({ "name": name }),
        },
    )
    .await?;
    tx.commit().await?;
    Ok((
        StatusCode::CREATED,
        Json(CreatedPasskey {
            id,
            name,
            created_at,
        }),
    ))
}

#[utoipa::path(
    get,
    path = "/api/v1/me/passkeys",
    responses(
        (status = 200, description = "The caller's passkeys, oldest first", body = Vec<PasskeySummary>),
        (status = 401, description = "unauthenticated"),
        (status = 403, description = "user-session-required"),
    ),
)]
pub async fn list(
    State(state): State<AppState>,
    user: CurrentUser,
) -> ApiResult<Json<Vec<PasskeySummary>>> {
    let rows = sqlx::query_as!(
        PasskeySummary,
        "select id, name, created_at, last_used_at from passkeys \
         where user_id = $1 order by created_at, id",
        user.user_id,
    )
    .fetch_all(&state.pool)
    .await?;
    Ok(Json(rows))
}

#[utoipa::path(
    delete,
    path = "/api/v1/me/passkeys/{id}",
    params(("id" = Uuid, Path, description = "Passkey id")),
    responses(
        (status = 204, description = "Removed; it can no longer sign in"),
        (status = 401, description = "unauthenticated"),
        (status = 403, description = "user-session-required"),
        (status = 404, description = "no such passkey of the caller's"),
    ),
)]
pub async fn remove(
    State(state): State<AppState>,
    meta: RequestMeta,
    user: CurrentUser,
    ApiPath(id): ApiPath<Uuid>,
) -> ApiResult<StatusCode> {
    let mut tx = state.pool.begin().await?;
    let name = sqlx::query_scalar!(
        "delete from passkeys where id = $1 and user_id = $2 returning name",
        id,
        user.user_id,
    )
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(ApiError::NotFound)?;
    audit::record(
        &mut *tx,
        &meta,
        AuditEvent {
            workspace_id: None,
            actor: Actor::User(user.user_id),
            action: actions::AUTH_PASSKEY_REMOVED,
            target_type: Some("passkey"),
            target_id: Some(id.to_string()),
            metadata: json!({ "name": name }),
        },
    )
    .await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

#[utoipa::path(
    post,
    path = "/api/v1/auth/passkey/login/start",
    responses(
        (status = 200, description = "Options for navigator.credentials.get with conditional mediation", body = ChallengeResponse),
        (status = 409, description = "passkeys-unavailable: public_url host is not a domain name"),
    ),
)]
pub async fn login_start(State(state): State<AppState>) -> ApiResult<Json<ChallengeResponse>> {
    let (rcr, ceremony) = state
        .webauthn()?
        .start_discoverable_authentication()
        .map_err(|e| internal("start authentication", e))?;
    let options = public_key_options(&rcr)?;
    let challenge_id = store_challenge(
        &state.pool,
        None,
        "passkey_login",
        serde_json::to_value(ceremony).map_err(|e| internal("serialise ceremony", e))?,
    )
    .await?;
    Ok(Json(ChallengeResponse {
        challenge_id,
        options,
    }))
}

#[utoipa::path(
    post,
    path = "/api/v1/auth/passkey/login/finish",
    request_body = FinishRequest,
    responses(
        (status = 200, description = "Signed in with a full session; sets the nm_session cookie. No second factor follows", body = LoginResponse),
        (status = 401, description = "invalid-passkey"),
    ),
)]
pub async fn login_finish(
    State(state): State<AppState>,
    meta: RequestMeta,
    jar: CookieJar,
    ApiJson(req): ApiJson<FinishRequest>,
) -> ApiResult<(CookieJar, Json<LoginResponse>)> {
    // As in `login`: a session the client presented is gone before a new one exists.
    if let Some(old) = jar.get(sessions::COOKIE_NAME) {
        sessions::delete(&state.pool, &sessions::hash_cookie_value(old.value())).await?;
    }
    // An instance with no relying party has no passkeys, so nothing can verify.
    let webauthn = state.webauthn().map_err(|_| invalid_passkey())?;

    let stored = take_challenge(&state.pool, req.challenge_id, "passkey_login", None)
        .await?
        .ok_or_else(invalid_passkey)?;
    let ceremony: DiscoverableAuthentication =
        serde_json::from_value(stored).map_err(|e| internal("stored ceremony", e))?;
    let credential: PublicKeyCredential =
        serde_json::from_value(req.credential).map_err(|_| invalid_passkey())?;
    // The user handle the authenticator reports is a claim, not a proof: it only picks which
    // stored credential to verify. The signature is checked against that credential's own key,
    // and the lookup requires the credential to belong to the claimed user.
    let (user_id, credential_id) = webauthn
        .identify_discoverable_authentication(&credential)
        .map_err(rejected)?;

    let mut tx = state.pool.begin().await?;
    // Locked, so two sign-ins with the same credential are checked against one counter in turn.
    let row = sqlx::query!(
        "select id, passkey from passkeys where credential_id = $1 and user_id = $2 for update",
        credential_id,
        user_id,
    )
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(invalid_passkey)?;
    let mut passkey: Passkey =
        serde_json::from_value(row.passkey).map_err(|e| internal("stored passkey", e))?;

    // Rejects a wrong origin, challenge, rp id or signature, missing user verification, and a
    // signature counter that did not advance (a possible cloned authenticator).
    let result = webauthn
        .finish_discoverable_authentication_v2(&credential, ceremony, &passkey)
        .map_err(rejected)?;
    passkey.update_credential(&result);
    sqlx::query!(
        "update passkeys set passkey = $2, last_used_at = now() where id = $1",
        row.id,
        serde_json::to_value(&passkey).map_err(|e| internal("serialise passkey", e))?,
    )
    .execute(&mut *tx)
    .await?;

    let session = sessions::create(&mut *tx, user_id, false, &meta).await?;
    audit::record(
        &mut *tx,
        &meta,
        AuditEvent {
            workspace_id: None,
            actor: Actor::User(user_id),
            action: actions::AUTH_LOGIN_SUCCEEDED,
            target_type: Some("user"),
            target_id: Some(user_id.to_string()),
            metadata: json!({ "method": "passkey" }),
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
