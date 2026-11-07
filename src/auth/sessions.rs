// SPDX-License-Identifier: AGPL-3.0-only

//! The session store. A session is a random 32-byte cookie value; only its sha256 is stored
//! (`sessions.id_hash`), so a database read never yields a usable cookie.
//!
//! Lifetimes (the intervals live in the SQL below, next to the columns they test):
//! - full session: absolute 30 days (`expires_at`) and idle 7 days (`last_seen_at`), both
//!   enforced by `resolve` on every authenticated request;
//! - pending-MFA session: 10 minutes (`expires_at`), and `resolve` never returns one.

use axum_extra::extract::cookie::{Cookie, SameSite};
use sqlx::{PgExecutor, PgPool};
use uuid::Uuid;

use crate::{
    config::Config,
    crypto::tokens::{random_token, sha256},
    http::request_meta::{RequestMeta, truncate_user_agent},
};

pub const COOKIE_NAME: &str = "nm_session";

pub struct NewSession {
    pub cookie_value: String,
    pub csrf_token: String,
}

/// A full, unexpired session as `resolve` sees it.
pub struct ActiveSession {
    pub user_id: Uuid,
    pub session_hash: [u8; 32],
    pub csrf_token: String,
}

pub fn hash_cookie_value(value: &str) -> [u8; 32] {
    sha256(value.as_bytes())
}

/// Stores a new session and returns the only copy of its cookie value. A pending-MFA session
/// (`mfa_pending`) expires after 10 minutes; a full one after 30 days.
pub async fn create(
    db: impl PgExecutor<'_>,
    user_id: Uuid,
    mfa_pending: bool,
    meta: &RequestMeta,
) -> sqlx::Result<NewSession> {
    let session = NewSession {
        cookie_value: random_token(32),
        csrf_token: random_token(32),
    };
    let id_hash = hash_cookie_value(&session.cookie_value);
    let user_agent = meta.user_agent.as_deref().map(truncate_user_agent);
    sqlx::query!(
        "insert into sessions (id_hash, user_id, csrf_token, mfa_pending, expires_at, ip, user_agent) \
         values ($1, $2, $3, $4, \
                 now() + case when $4 then interval '10 minutes' else interval '30 days' end, \
                 $5, $6)",
        &id_hash[..],
        user_id,
        session.csrf_token,
        mfa_pending,
        meta.ip as Option<std::net::IpAddr>,
        user_agent,
    )
    .execute(db)
    .await?;
    Ok(session)
}

/// The session behind a cookie value, if it is full (not pending MFA), within its absolute
/// lifetime and not idle for over 7 days. Also refreshes `last_seen_at`.
pub async fn resolve(db: &PgPool, cookie_value: &str) -> sqlx::Result<Option<ActiveSession>> {
    let session_hash = hash_cookie_value(cookie_value);
    let Some(row) = sqlx::query!(
        r#"select user_id, csrf_token, last_seen_at < now() - interval '1 minute' as "stale!"
           from sessions
           where id_hash = $1 and not mfa_pending
             and expires_at > now() and last_seen_at > now() - interval '7 days'"#,
        &session_hash[..],
    )
    .fetch_optional(db)
    .await?
    else {
        return Ok(None);
    };
    // shortcut: last_seen_at is written at most once a minute, so the 7-day idle cut-off is only
    // accurate to about a minute; write on every request if idle-timeout precision ever matters.
    if row.stale {
        sqlx::query!(
            "update sessions set last_seen_at = now() where id_hash = $1",
            &session_hash[..],
        )
        .execute(db)
        .await?;
    }
    Ok(Some(ActiveSession {
        user_id: row.user_id,
        session_hash,
        csrf_token: row.csrf_token,
    }))
}

/// Deletes one session by the hash of its cookie value. Deleting a missing one is not an error.
pub async fn delete(db: impl PgExecutor<'_>, session_hash: &[u8; 32]) -> sqlx::Result<()> {
    sqlx::query!("delete from sessions where id_hash = $1", &session_hash[..])
        .execute(db)
        .await?;
    Ok(())
}

/// Deletes every session of the user, except `except` if given.
pub async fn revoke_all_for_user(
    db: impl PgExecutor<'_>,
    user_id: Uuid,
    except: Option<[u8; 32]>,
) -> sqlx::Result<()> {
    sqlx::query!(
        "delete from sessions where user_id = $1 and ($2::bytea is null or id_hash <> $2)",
        user_id,
        except.as_ref().map(|h| &h[..]),
    )
    .execute(db)
    .await?;
    Ok(())
}

/// `Secure` is dropped only for a plain-http localhost `public_url`, where a browser would
/// otherwise refuse the cookie in development.
fn secure(cfg: &Config) -> bool {
    let local_http = cfg.public_url.scheme() == "http"
        && matches!(cfg.public_url.host_str(), Some("localhost" | "127.0.0.1"));
    !local_http
}

/// The session cookie: HttpOnly, SameSite=Lax, Path=/, Max-Age 30 days.
pub fn cookie(value: &str, cfg: &Config) -> Cookie<'static> {
    let mut c = Cookie::build((COOKIE_NAME, value.to_owned()))
        .http_only(true)
        .same_site(SameSite::Lax)
        .path("/")
        .max_age(cookie::time::Duration::days(30))
        .build();
    if secure(cfg) {
        c.set_secure(true);
    }
    c
}
