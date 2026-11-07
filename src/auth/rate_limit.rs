// SPDX-License-Identifier: AGPL-3.0-only

//! Sign-in lockout: failures per email and per client IP within a sliding window. Counting is
//! keyed on what was submitted, never on whether a user exists, so `check` costs the same for
//! a known and an unknown email.

use std::net::IpAddr;

use sqlx::PgPool;

use crate::error::{ApiError, ApiResult};

const WINDOW_SECS: f64 = 15.0 * 60.0;
const MAX_FAILURES_PER_EMAIL: i64 = 10;
const MAX_FAILURES_PER_IP: i64 = 100;

/// 429 when the email has 10 or more failures, or the IP 100 or more, in the last 15 minutes.
/// `retry_after_secs` is the time until the oldest counted failure leaves the window (the later
/// of the two when both limits are hit).
pub async fn check(db: &PgPool, email: &str, ip: Option<IpAddr>) -> ApiResult<()> {
    let row = sqlx::query!(
        r#"select
             count(*) filter (where email = $1) as "email_failures!",
             count(*) filter (where ip = $2) as "ip_failures!",
             extract(epoch from min(at) filter (where email = $1)
                     + interval '15 minutes' - now())::float8 as email_wait,
             extract(epoch from min(at) filter (where ip = $2)
                     + interval '15 minutes' - now())::float8 as ip_wait
           from login_attempts
           where not succeeded and at > now() - interval '15 minutes'
             and (email = $1 or ip = $2)"#,
        email,
        ip as Option<IpAddr>,
    )
    .fetch_one(db)
    .await?;

    let email_locked = row.email_failures >= MAX_FAILURES_PER_EMAIL;
    let ip_locked = row.ip_failures >= MAX_FAILURES_PER_IP;
    if !email_locked && !ip_locked {
        return Ok(());
    }
    let waits = [
        email_locked.then_some(row.email_wait).flatten(),
        ip_locked.then_some(row.ip_wait).flatten(),
    ];
    let wait = waits.into_iter().flatten().fold(0.0, f64::max);
    Err(ApiError::TooManyRequests {
        retry_after_secs: wait.ceil().clamp(1.0, WINDOW_SECS) as u64,
    })
}

/// Records one sign-in attempt. Only `succeeded = false` rows count toward a lock.
pub async fn record(
    db: &PgPool,
    email: &str,
    ip: Option<IpAddr>,
    succeeded: bool,
) -> sqlx::Result<()> {
    sqlx::query!(
        "insert into login_attempts (email, ip, succeeded) values ($1, $2, $3)",
        email,
        ip as Option<IpAddr>,
        succeeded,
    )
    .execute(db)
    .await?;
    // shortcut: old rows are deleted inline on every attempt; move to a scheduled job when the queue lands (M1).
    sqlx::query!("delete from login_attempts where at < now() - interval '1 day'")
        .execute(db)
        .await?;
    Ok(())
}
