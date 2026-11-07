// SPDX-License-Identifier: AGPL-3.0-only

use sqlx::PgExecutor;
use uuid::Uuid;

use crate::{
    crypto::password::{hash_password, validate_password},
    error::{ApiError, ApiResult, FieldError},
};

/// A `users` row. No `Debug`: `password_hash` must never reach a log line.
pub struct User {
    pub id: Uuid,
    pub email: String,
    pub display_name: String,
    pub password_hash: Option<String>,
    pub is_instance_admin: bool,
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
        "select id, email, display_name, password_hash, is_instance_admin from users where email = $1",
        email,
    )
    .fetch_optional(db)
    .await
}

pub async fn find_by_id(db: impl PgExecutor<'_>, id: Uuid) -> sqlx::Result<Option<User>> {
    sqlx::query_as!(
        User,
        "select id, email, display_name, password_hash, is_instance_admin from users where id = $1",
        id,
    )
    .fetch_optional(db)
    .await
}
