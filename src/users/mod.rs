// SPDX-License-Identifier: AGPL-3.0-only

use serde_json::json;
use sqlx::{PgConnection, PgExecutor, PgPool};
use uuid::Uuid;

use crate::{
    audit::{self, Actor, AuditEvent, actions},
    auth::sessions,
    crypto::password::{hash_password, validate_password},
    error::{ApiError, ApiResult, FieldError},
    http::request_meta::RequestMeta,
};

/// A `users` row. No `Debug`: `password_hash` must never reach a log line.
pub struct User {
    pub id: Uuid,
    pub email: String,
    pub display_name: String,
    pub password_hash: Option<String>,
    pub is_instance_admin: bool,
    /// Whether a confirmed TOTP secret is set (`totp_enabled_at`).
    pub totp_enabled: bool,
}

/// Trim and lowercase; 3..=254 characters; exactly one `@`; non-empty local part; a domain
/// containing a `.`. Control characters (so NUL, which Postgres rejects in `text` and `jsonb`)
/// and whitespace are rejected outright rather than stripped: such an address is never valid,
/// and silently altering it could point a login at a different account.
pub fn normalize_email(raw: &str) -> Result<String, FieldError> {
    let email = raw.trim().to_lowercase();
    let bad = || FieldError::new("email", "must be a valid email address");
    let len = email.chars().count();
    if !(3..=254).contains(&len) || email.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return Err(bad());
    }
    match email.split_once('@') {
        Some((local, domain))
            if !local.is_empty() && domain.contains('.') && !domain.contains('@') =>
        {
            Ok(email)
        }
        _ => Err(bad()),
    }
}

/// Inserts a user. `409 email-taken` if the (normalised) address exists. The password must be
/// 12..=256 characters (422 on field `password`). Takes an executor so a caller can create the user
/// in the same transaction as other writes; note that a failed insert aborts that transaction.
pub async fn create_user(
    db: impl PgExecutor<'_>,
    email: &str,
    password: &str,
    is_instance_admin: bool,
) -> ApiResult<Uuid> {
    let email = normalize_email(email).map_err(|e| ApiError::Validation(vec![e]))?;
    validate_password(password).map_err(|e| ApiError::Validation(vec![e]))?;
    let hash = hash_password(password)?;
    let id = Uuid::now_v7();
    sqlx::query!(
        "insert into users (id, email, password_hash, is_instance_admin) values ($1, $2, $3, $4)",
        id,
        email,
        hash,
        is_instance_admin,
    )
    .execute(db)
    .await
    .map_err(|e| match e.as_database_error() {
        Some(d) if d.is_unique_violation() => ApiError::Conflict("email-taken"),
        _ => e.into(),
    })?;
    Ok(id)
}

/// Replaces the password hash. 404 if the user does not exist. Does not touch sessions: callers
/// decide which to revoke, in the same transaction.
pub async fn set_password(db: impl PgExecutor<'_>, user_id: Uuid, password: &str) -> ApiResult<()> {
    validate_password(password).map_err(|e| ApiError::Validation(vec![e]))?;
    let hash = hash_password(password)?;
    let done = sqlx::query!(
        "update users set password_hash = $2, updated_at = now() where id = $1",
        user_id,
        hash,
    )
    .execute(db)
    .await?;
    if done.rows_affected() == 0 {
        return Err(ApiError::NotFound);
    }
    Ok(())
}

/// `email` must already be normalised.
pub async fn find_by_email(db: impl PgExecutor<'_>, email: &str) -> sqlx::Result<Option<User>> {
    sqlx::query_as!(
        User,
        r#"select id, email, display_name, password_hash, is_instance_admin,
                  totp_enabled_at is not null as "totp_enabled!"
           from users where email = $1"#,
        email,
    )
    .fetch_optional(db)
    .await
}

pub async fn find_by_id(db: impl PgExecutor<'_>, id: Uuid) -> sqlx::Result<Option<User>> {
    sqlx::query_as!(
        User,
        r#"select id, email, display_name, password_hash, is_instance_admin,
                  totp_enabled_at is not null as "totp_enabled!"
           from users where id = $1"#,
        id,
    )
    .fetch_optional(db)
    .await
}

/// Removes the second factor: the TOTP secrets and every recovery code. Takes the caller's
/// transaction connection because it is two statements that must land together.
pub async fn clear_totp(db: &mut PgConnection, user_id: Uuid) -> sqlx::Result<()> {
    sqlx::query!(
        "update users set totp_secret_enc = null, totp_pending_secret_enc = null, \
         totp_enabled_at = null, totp_last_step = null, updated_at = now() where id = $1",
        user_id,
    )
    .execute(&mut *db)
    .await?;
    sqlx::query!("delete from recovery_codes where user_id = $1", user_id)
        .execute(&mut *db)
        .await?;
    Ok(())
}

/// Operator recovery for a user locked out of their second factor (`nm admin disable-mfa`):
/// clears TOTP and the recovery codes, signs the user out everywhere and audits it as the system.
/// One transaction, so a crash cannot leave MFA cleared with sessions still live, or the other
/// way round. 404 if no user has that email.
pub async fn disable_mfa(db: &PgPool, email: &str) -> ApiResult<()> {
    let email = normalize_email(email).map_err(|e| ApiError::Validation(vec![e]))?;
    let user = find_by_email(db, &email).await?.ok_or(ApiError::NotFound)?;
    let mut tx = db.begin().await?;
    clear_totp(&mut tx, user.id).await?;
    sessions::revoke_all_for_user(&mut *tx, user.id, None).await?;
    audit::record(
        &mut *tx,
        &RequestMeta::default(),
        AuditEvent {
            workspace_id: None,
            actor: Actor::System,
            action: actions::AUTH_TOTP_DISABLED,
            target_type: Some("user"),
            target_id: Some(user.id.to_string()),
            metadata: json!({ "via": "cli" }),
        },
    )
    .await?;
    tx.commit().await?;
    Ok(())
}
