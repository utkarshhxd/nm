// SPDX-License-Identifier: AGPL-3.0-only

//! TOTP two-factor sign-in and single-use recovery codes.
//!
//! The first half is the pure parts: the code check with its replay rule, and recovery-code
//! generation and hashing. The second half is the five endpoints.
//!
//! - `totp_secret_enc` and `totp_pending_secret_enc` are `Secrets` blobs. The secret is shown
//!   once, in the `setup` response, and is in no log, audit row or later response.
//! - `totp_last_step` is the highest accepted 30-second step. A code is accepted only for a step
//!   above it, and accepting one raises it, so the code and every earlier step are spent.
//! - A recovery code is stored as sha256 of its normalised form and spent by one conditional
//!   `update`, so concurrent uses of the same code cannot both succeed.

use std::time::{SystemTime, UNIX_EPOCH};

use axum::{
    Json, Router,
    extract::State,
    http::{StatusCode, header},
    routing::{delete, post},
};
use axum_extra::extract::CookieJar;
use rand::{TryRng, rngs::SysRng};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::PgExecutor;
use totp_rs::{Algorithm, Builder, Secret, TotpError};
use utoipa::ToSchema;
use uuid::Uuid;

use super::{CurrentUser, login, login::LoginResponse, rate_limit, sessions};
use crate::{
    app::AppState,
    audit::{self, Actor, AuditEvent, actions},
    crypto::tokens::sha256,
    error::{ApiError, ApiJson, ApiResult, FieldError},
    http::request_meta::RequestMeta,
    users,
};

const SECRET_BYTES: usize = 20;
const RECOVERY_CODE_COUNT: usize = 10;
/// 31 characters, with the look-alikes `i`, `l`, `o`, `0` and `1` left out.
const RECOVERY_ALPHABET: &[u8] = b"abcdefghjkmnpqrstuvwxyz23456789";
/// The largest multiple of 31 that fits in a byte; a random byte at or above it is discarded, so
/// `byte % 31` is unbiased.
const RECOVERY_BYTE_LIMIT: u8 = 248;
/// Wrong second-factor submissions a pending session tolerates before it is deleted.
const MAX_MFA_ATTEMPTS: i32 = 5;

/// Sent with anything that carries a secret or recovery codes.
const NO_STORE: [(header::HeaderName, &str); 1] = [(header::CACHE_CONTROL, "no-store")];

fn builder(secret: &[u8]) -> Builder {
    Builder::new()
        .with_algorithm(Algorithm::SHA1)
        .with_digits(6)
        .with_skew(1)
        .with_step_duration(30)
        .with_secret(secret.to_vec())
}

/// The 6-digit code for `secret_b32` (RFC 4648 base32, as shown at setup) at a Unix time.
/// Panics on a malformed secret; meant for tests.
pub fn code_for(secret_b32: &str, at_unix: u64) -> String {
    let secret = Secret::try_from_base32(secret_b32).expect("valid base32 secret");
    let totp = builder(secret.as_bytes())
        .build()
        .expect("RFC-compliant parameters");
    totp.generate(at_unix).to_string()
}

/// The step `code` matches within one step either side of `now_unix`, if that step is above
/// `last_step`; otherwise `None`. The caller must store the returned step (see `claim_step`):
/// that is what makes a code single-use.
///
/// shortcut: `check` reports the lowest matching step, so in the one-in-a-million case where two
/// steps in the window share a code and the lower is at or below `last_step`, a valid code is
/// refused. It fails closed and the next code works; no replay is possible.
pub fn verify(secret: &[u8], code: &str, now_unix: u64, last_step: Option<i64>) -> Option<i64> {
    let totp = builder(secret).build().ok()?;
    let step = i64::try_from(totp.check(code.trim(), now_unix)?).ok()?;
    last_step.is_none_or(|last| step > last).then_some(step)
}

fn new_secret() -> [u8; SECRET_BYTES] {
    let mut secret = [0u8; SECRET_BYTES];
    // Fail closed: a broken OS RNG must never yield a predictable secret.
    SysRng
        .try_fill_bytes(&mut secret)
        .expect("OS RNG unavailable");
    secret
}

/// The `otpauth://` URL an authenticator app reads. Issuer and label may not hold a colon.
fn otpauth_url(secret: &[u8], issuer: &str, email: &str) -> Result<String, TotpError> {
    builder(secret)
        .with_issuer(Some(issuer.replace(':', "")))
        .with_account_name(email.replace(':', ""))
        .build()?
        .to_url()
}

fn recovery_code() -> String {
    let mut code = String::with_capacity(11);
    let mut random = [0u8; 32];
    while code.len() < 10 {
        SysRng
            .try_fill_bytes(&mut random)
            .expect("OS RNG unavailable");
        for byte in random.into_iter().filter(|b| *b < RECOVERY_BYTE_LIMIT) {
            if code.len() < 10 {
                code.push(RECOVERY_ALPHABET[usize::from(byte) % RECOVERY_ALPHABET.len()] as char);
            }
        }
    }
    code.insert(5, '-');
    code
}

/// Ten fresh codes shaped `xxxxx-xxxxx`.
pub fn generate_recovery_codes() -> Vec<String> {
    (0..RECOVERY_CODE_COUNT).map(|_| recovery_code()).collect()
}

/// Lowercase, hyphen removed: what is hashed, so `ABCDE-FGHJK` and `abcdefghjk` are one code.
fn recovery_code_hash(code: &str) -> [u8; 32] {
    sha256(code.trim().to_lowercase().replace('-', "").as_bytes())
}

/// Spends a recovery code: `true` if it exists for the user and was unspent. One conditional
/// `update`, so of any number of concurrent calls with the same code exactly one gets `true`
/// (the others wait on the row lock, then no longer match `used_at is null`).
pub async fn consume_recovery_code(
    db: impl PgExecutor<'_>,
    user_id: Uuid,
    code: &str,
) -> sqlx::Result<bool> {
    let hash = recovery_code_hash(code);
    let done = sqlx::query!(
        "update recovery_codes set used_at = now() \
         where user_id = $1 and code_hash = $2 and used_at is null",
        user_id,
        &hash[..],
    )
    .execute(db)
    .await?;
    Ok(done.rows_affected() > 0)
}

/// Raises `totp_last_step` to `step`: `true` if that moved it up, `false` if a concurrent request
/// already spent this step or a later one.
async fn claim_step(db: impl PgExecutor<'_>, user_id: Uuid, step: i64) -> sqlx::Result<bool> {
    let done = sqlx::query!(
        "update users set totp_last_step = $2 \
         where id = $1 and (totp_last_step is null or totp_last_step < $2)",
        user_id,
        step,
    )
    .execute(db)
    .await?;
    Ok(done.rows_affected() == 1)
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time is after the Unix epoch")
        .as_secs()
}

fn decrypt(state: &AppState, blob: &[u8]) -> ApiResult<Vec<u8>> {
    state
        .secrets
        .decrypt(blob)
        .map_err(|e| ApiError::Internal(e.into()))
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/v1/me/totp/setup", post(setup))
        .route("/api/v1/me/totp/confirm", post(confirm))
        .route("/api/v1/me/totp", delete(disable))
        .route("/api/v1/auth/login/totp", post(login_totp))
}

#[derive(Serialize, ToSchema)]
pub struct SetupResponse {
    /// Base32 secret, shown only here.
    pub secret: String,
    pub otpauth_url: String,
}

#[derive(Deserialize, ToSchema)]
pub struct ConfirmRequest {
    pub code: String,
}

#[derive(Serialize, ToSchema)]
pub struct ConfirmResponse {
    /// Shown only here.
    pub recovery_codes: Vec<String>,
}

#[derive(Deserialize, ToSchema)]
pub struct DisableRequest {
    pub password: String,
}

/// Exactly one of the two.
#[derive(Deserialize, ToSchema)]
pub struct SecondFactorRequest {
    pub code: Option<String>,
    pub recovery_code: Option<String>,
}

#[utoipa::path(
    post,
    path = "/api/v1/me/totp/setup",
    responses(
        (status = 200, description = "A pending secret, returned once", body = SetupResponse),
        (status = 401, description = "unauthenticated"),
        (status = 409, description = "totp-already-enabled"),
    ),
)]
pub async fn setup(
    State(state): State<AppState>,
    user: CurrentUser,
) -> ApiResult<([(header::HeaderName, &'static str); 1], Json<SetupResponse>)> {
    let u = users::find_by_id(&state.pool, user.user_id)
        .await?
        .ok_or(ApiError::Unauthorized("unauthenticated"))?;
    if u.totp_enabled {
        return Err(ApiError::Conflict("totp-already-enabled"));
    }
    let secret = new_secret();
    let done = sqlx::query!(
        "update users set totp_pending_secret_enc = $2, updated_at = now() \
         where id = $1 and totp_enabled_at is null",
        u.id,
        state.secrets.encrypt(&secret),
    )
    .execute(&state.pool)
    .await?;
    if done.rows_affected() == 0 {
        return Err(ApiError::Conflict("totp-already-enabled"));
    }
    let issuer = state.config.public_url.host_str().unwrap_or("NM");
    let otpauth_url = otpauth_url(&secret, issuer, &u.email)
        .map_err(|e| ApiError::Internal(anyhow::anyhow!("otpauth url: {e:?}")))?;
    Ok((
        NO_STORE,
        Json(SetupResponse {
            secret: Secret::from(secret).to_base32(),
            otpauth_url,
        }),
    ))
}

#[utoipa::path(
    post,
    path = "/api/v1/me/totp/confirm",
    request_body = ConfirmRequest,
    responses(
        (status = 200, description = "TOTP is on; recovery codes returned once", body = ConfirmResponse),
        (status = 401, description = "unauthenticated"),
        (status = 422, description = "code is wrong, or no setup is pending"),
    ),
)]
pub async fn confirm(
    State(state): State<AppState>,
    meta: RequestMeta,
    user: CurrentUser,
    ApiJson(req): ApiJson<ConfirmRequest>,
) -> ApiResult<(
    [(header::HeaderName, &'static str); 1],
    Json<ConfirmResponse>,
)> {
    let invalid = || ApiError::Validation(vec![FieldError::new("code", "code is not valid")]);
    let pending = sqlx::query_scalar!(
        "select totp_pending_secret_enc from users where id = $1",
        user.user_id,
    )
    .fetch_optional(&state.pool)
    .await?
    .flatten()
    .ok_or_else(invalid)?;
    let step =
        verify(&decrypt(&state, &pending)?, &req.code, unix_now(), None).ok_or_else(invalid)?;

    let recovery_codes = generate_recovery_codes();
    let mut tx = state.pool.begin().await?;
    // `totp_last_step` starts at the confirming code's step, so that code cannot also sign in. The
    // pending blob must still be the one just verified, in case `setup` ran again meanwhile.
    let done = sqlx::query!(
        "update users set totp_secret_enc = totp_pending_secret_enc, totp_pending_secret_enc = null, \
         totp_enabled_at = now(), totp_last_step = $2, updated_at = now() \
         where id = $1 and totp_enabled_at is null and totp_pending_secret_enc = $3",
        user.user_id,
        step,
        pending,
    )
    .execute(&mut *tx)
    .await?;
    if done.rows_affected() == 0 {
        return Err(invalid());
    }
    for code in &recovery_codes {
        sqlx::query!(
            "insert into recovery_codes (id, user_id, code_hash) values ($1, $2, $3)",
            Uuid::now_v7(),
            user.user_id,
            &recovery_code_hash(code)[..],
        )
        .execute(&mut *tx)
        .await?;
    }
    audit::record(
        &mut *tx,
        &meta,
        AuditEvent {
            workspace_id: None,
            actor: Actor::User(user.user_id),
            action: actions::AUTH_TOTP_ENABLED,
            target_type: Some("user"),
            target_id: Some(user.user_id.to_string()),
            metadata: json!({}),
        },
    )
    .await?;
    tx.commit().await?;
    Ok((NO_STORE, Json(ConfirmResponse { recovery_codes })))
}

#[utoipa::path(
    delete,
    path = "/api/v1/me/totp",
    request_body = DisableRequest,
    responses(
        (status = 204, description = "TOTP and recovery codes removed"),
        (status = 401, description = "invalid-credentials (wrong password) or unauthenticated"),
        (status = 409, description = "totp-not-enabled"),
    ),
)]
pub async fn disable(
    State(state): State<AppState>,
    meta: RequestMeta,
    user: CurrentUser,
    ApiJson(req): ApiJson<DisableRequest>,
) -> ApiResult<StatusCode> {
    let u = login::require_password(&state.pool, user.user_id, &req.password).await?;
    if !u.totp_enabled {
        return Err(ApiError::Conflict("totp-not-enabled"));
    }
    let mut tx = state.pool.begin().await?;
    users::clear_totp(&mut tx, user.user_id).await?;
    audit::record(
        &mut *tx,
        &meta,
        AuditEvent {
            workspace_id: None,
            actor: Actor::User(user.user_id),
            action: actions::AUTH_TOTP_DISABLED,
            target_type: Some("user"),
            target_id: Some(user.user_id.to_string()),
            metadata: json!({}),
        },
    )
    .await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Ends the pending session and counts the restart as a failed sign-in for the limiter. Without
/// that, a caller who knows the password could sign in again for five more guesses, indefinitely:
/// the password stage is a recorded success, so nothing else would ever add up to a lock.
async fn restart(
    state: &AppState,
    meta: &RequestMeta,
    pending: &sessions::PendingSession,
    user: &users::User,
) -> ApiResult<ApiError> {
    sessions::delete(&state.pool, &pending.session_hash).await?;
    rate_limit::record_attempt(&state.pool, Some(&user.email), meta.ip).await?;
    audit::record(
        &state.pool,
        meta,
        AuditEvent {
            workspace_id: None,
            actor: Actor::System,
            action: actions::AUTH_LOGIN_FAILED,
            target_type: Some("user"),
            target_id: Some(user.id.to_string()),
            metadata: json!({ "email": user.email, "reason": "mfa-restart" }),
        },
    )
    .await?;
    Ok(ApiError::UnauthorizedDetail {
        code: "mfa-restart",
        detail: "Too many wrong codes; sign in again",
    })
}

#[utoipa::path(
    post,
    path = "/api/v1/auth/login/totp",
    request_body = SecondFactorRequest,
    responses(
        (status = 200, description = "Signed in; the pending session is replaced by a full one", body = LoginResponse),
        (status = 401, description = "invalid-code, mfa-restart (5th wrong code), or unauthenticated (no live pending session)"),
        (status = 422, description = "not exactly one of code and recovery_code"),
    ),
)]
pub async fn login_totp(
    State(state): State<AppState>,
    meta: RequestMeta,
    jar: CookieJar,
    ApiJson(req): ApiJson<SecondFactorRequest>,
) -> ApiResult<(CookieJar, Json<LoginResponse>)> {
    let unauthenticated = || ApiError::Unauthorized("unauthenticated");
    let pending = match jar.get(sessions::COOKIE_NAME) {
        Some(c) => sessions::resolve_pending(&state.pool, c.value()).await?,
        None => None,
    }
    .ok_or_else(unauthenticated)?;
    let (code, is_recovery) = match (&req.code, &req.recovery_code) {
        (Some(code), None) => (code, false),
        (None, Some(code)) => (code, true),
        _ => {
            return Err(ApiError::Validation(vec![FieldError::new(
                "code",
                "send exactly one of code and recovery_code",
            )]));
        }
    };

    // Counted before the check, so guesses in flight cannot outnumber the limit.
    let attempts = sessions::record_mfa_attempt(&state.pool, &pending.session_hash)
        .await?
        .ok_or_else(unauthenticated)?;
    let user = users::find_by_id(&state.pool, pending.user_id)
        .await?
        .ok_or_else(unauthenticated)?;
    if attempts > MAX_MFA_ATTEMPTS {
        return Err(restart(&state, &meta, &pending, &user).await?);
    }

    // Everything below that spends a code runs in the transaction that signs the user in, so a
    // code is never burned without a session, or a session issued for an unspent code.
    let mut tx = state.pool.begin().await?;
    let method = if is_recovery {
        consume_recovery_code(&mut *tx, user.id, code)
            .await?
            .then_some("recovery_code")
    } else {
        let row = sqlx::query!(
            "select totp_secret_enc, totp_last_step from users where id = $1",
            user.id,
        )
        .fetch_one(&mut *tx)
        .await?;
        let step = match &row.totp_secret_enc {
            Some(blob) => verify(
                &decrypt(&state, blob)?,
                code,
                unix_now(),
                row.totp_last_step,
            ),
            None => None,
        };
        match step {
            Some(step) if claim_step(&mut *tx, user.id, step).await? => Some("totp"),
            _ => None,
        }
    };
    let Some(method) = method else {
        drop(tx);
        return Err(if attempts >= MAX_MFA_ATTEMPTS {
            restart(&state, &meta, &pending, &user).await?
        } else {
            ApiError::UnauthorizedDetail {
                code: "invalid-code",
                detail: "Code is incorrect",
            }
        });
    };

    // The pending session is deleted, not upgraded: its id was handed out before the second
    // factor was proven.
    sessions::delete(&mut *tx, &pending.session_hash).await?;
    let session = sessions::create(&mut *tx, user.id, false, &meta).await?;
    let event = |action, metadata| AuditEvent {
        workspace_id: None,
        actor: Actor::User(user.id),
        action,
        target_type: Some("user"),
        target_id: Some(user.id.to_string()),
        metadata,
    };
    audit::record(
        &mut *tx,
        &meta,
        event(
            actions::AUTH_LOGIN_SUCCEEDED,
            json!({ "email": user.email, "method": method }),
        ),
    )
    .await?;
    if is_recovery {
        audit::record(
            &mut *tx,
            &meta,
            event(actions::AUTH_RECOVERY_CODE_USED, json!({})),
        )
        .await?;
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 6238 appendix B: ASCII secret "12345678901234567890", SHA-1, last six of the 8 digits.
    const RFC_SECRET_B32: &str = "GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ";
    const RFC_SECRET: &[u8] = b"12345678901234567890";

    #[test]
    fn code_for_matches_the_rfc_6238_vectors() {
        assert_eq!(code_for(RFC_SECRET_B32, 59), "287082");
        assert_eq!(code_for(RFC_SECRET_B32, 1_111_111_109), "081804");
        assert_eq!(code_for(RFC_SECRET_B32, 1_234_567_890), "005924");
    }

    #[test]
    fn verify_accepts_one_step_either_side_and_no_further() {
        let now = 1_700_000_015;
        let step = i64::try_from(now / 30).unwrap();
        let at = |t: u64| code_for(RFC_SECRET_B32, t);
        assert_eq!(verify(RFC_SECRET, &at(now), now, None), Some(step));
        assert_eq!(verify(RFC_SECRET, &at(now - 30), now, None), Some(step - 1));
        assert_eq!(verify(RFC_SECRET, &at(now + 30), now, None), Some(step + 1));
        assert_eq!(verify(RFC_SECRET, &at(now - 60), now, None), None);
        assert_eq!(verify(RFC_SECRET, &at(now + 60), now, None), None);
    }

    #[test]
    fn verify_only_accepts_a_step_above_the_last_one() {
        let now = 1_700_000_015;
        let step = i64::try_from(now / 30).unwrap();
        let code = code_for(RFC_SECRET_B32, now);
        assert_eq!(verify(RFC_SECRET, &code, now, Some(step - 1)), Some(step));
        assert_eq!(verify(RFC_SECRET, &code, now, Some(step)), None);
        assert_eq!(verify(RFC_SECRET, &code, now, Some(step + 1)), None);
        // Spending the current step also spends the previous one.
        let earlier = code_for(RFC_SECRET_B32, now - 30);
        assert_eq!(verify(RFC_SECRET, &earlier, now, Some(step)), None);
    }

    #[test]
    fn verify_rejects_malformed_codes() {
        let now = 1_700_000_015;
        for bad in ["", "12345", "1234567", "abcdef", "12 456", "١٢٣٤٥٦"] {
            assert_eq!(verify(RFC_SECRET, bad, now, None), None, "{bad:?}");
        }
    }

    #[test]
    fn verify_refuses_a_secret_too_short_for_the_rfc() {
        assert_eq!(verify(b"short", "000000", 59, None), None);
    }

    #[test]
    fn recovery_codes_are_ten_distinct_xxxxx_xxxxx() {
        assert_eq!(RECOVERY_ALPHABET.len(), 31);
        assert_eq!(
            usize::from(RECOVERY_BYTE_LIMIT),
            RECOVERY_ALPHABET.len() * 8
        );
        let codes = generate_recovery_codes();
        assert_eq!(codes.len(), 10);
        for c in &codes {
            assert_eq!(c.len(), 11);
            assert_eq!(c.as_bytes()[5], b'-');
            assert!(
                c.bytes()
                    .enumerate()
                    .all(|(i, b)| i == 5 || RECOVERY_ALPHABET.contains(&b)),
                "{c}"
            );
        }
        let mut sorted = codes.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), 10);
    }

    #[test]
    fn recovery_code_hash_ignores_case_hyphen_and_edge_whitespace() {
        let h = recovery_code_hash("abcde-fghjk");
        assert_eq!(h, recovery_code_hash("ABCDE-FGHJK"));
        assert_eq!(h, recovery_code_hash("abcdefghjk"));
        assert_eq!(h, recovery_code_hash(" abcde-fghjk\n"));
        assert_ne!(h, recovery_code_hash("abcde-fghjm"));
    }

    #[test]
    fn otpauth_url_carries_issuer_label_and_secret_and_survives_a_colon() {
        let url = otpauth_url(RFC_SECRET, "nm.example.com", "a:b@example.com").unwrap();
        assert!(url.starts_with("otpauth://totp/nm.example.com:ab%40example.com?secret="));
        assert!(url.contains(RFC_SECRET_B32));
        assert!(url.contains("issuer=nm.example.com"));
        assert!(otpauth_url(RFC_SECRET, "[::1]", "a@example.com").is_ok());
    }
}
