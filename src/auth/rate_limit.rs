// SPDX-License-Identifier: AGPL-3.0-only

//! Sign-in lockout: failures per email and per client IP within a sliding window. Counting is
//! keyed on what was submitted, never on whether a user exists, so `check` costs the same for
//! a known and an unknown email.
//!
//! An attempt is stored as a failure *before* its password is checked and flipped to a success
//! afterwards, so concurrent guesses cannot all slip past `check` while argon2 runs: the race
//! window is one database round trip, and nothing is locked while hashing.

use std::net::{IpAddr, Ipv6Addr};

use sqlx::PgPool;

use crate::error::{ApiError, ApiResult};

const WINDOW_SECS: f64 = 15.0 * 60.0;
const MAX_FAILURES_PER_EMAIL: i64 = 10;
const MAX_FAILURES_PER_IP: i64 = 100;

/// IPv6 is keyed per /64, the smallest block an ISP or cloud customer is normally given, so one
/// allocation cannot rotate through 2^64 addresses to dodge the per-IP limit. An IPv4-mapped
/// address (`::ffff:a.b.c.d`, as a dual-stack listener or proxy may report) is keyed as the IPv4
/// address it carries: masking it would put every IPv4 client in one `::` bucket.
///
/// **Every per-IP counter in the product keys through this**, sign-in and the public pages alike
/// (`http::rate_limit::Key`'s `From<IpAddr>`). A limiter that counts raw addresses is not a limiter
/// against anyone holding an allocation, which is everyone it exists to stop.
pub(crate) fn bucket_of(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => IpAddr::V4(v4),
            None => IpAddr::V6(Ipv6Addr::from(u128::from(v6) & (!0u128 << 64))),
        },
        v4 => v4,
    }
}

/// [`bucket_of`] for a caller that may have no address at all.
pub(crate) fn bucket(ip: Option<IpAddr>) -> Option<IpAddr> {
    ip.map(bucket_of)
}

/// 429 when the email has 10 or more failures, or the IP 100 or more, in the last 15 minutes.
/// `retry_after_secs` is the time until the oldest counted failure leaves the window (the later
/// of the two when both limits are hit). `email` is `None` when the submitted address did not
/// normalise; only the IP limit applies then.
pub async fn check(db: &PgPool, email: Option<&str>, ip: Option<IpAddr>) -> ApiResult<()> {
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
        bucket(ip) as Option<IpAddr>,
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

/// Stores an attempt as a failure and returns its id; call `mark_succeeded` once the password
/// has checked out. Only failures count toward a lock.
pub async fn record_attempt(
    db: &PgPool,
    email: Option<&str>,
    ip: Option<IpAddr>,
) -> sqlx::Result<i64> {
    let id = sqlx::query_scalar!(
        "insert into login_attempts (email, ip, succeeded) values ($1, $2, false) returning id",
        email,
        bucket(ip) as Option<IpAddr>,
    )
    .fetch_one(db)
    .await?;
    // shortcut: old rows are deleted inline on every attempt; move to a scheduled job when the queue lands (M1).
    // The cleanup must never fail the sign-in it piggybacks on, so its error is only logged.
    if let Err(e) = sqlx::query!("delete from login_attempts where at < now() - interval '1 day'")
        .execute(db)
        .await
    {
        tracing::warn!(error = ?e, "login_attempts cleanup failed");
    }
    Ok(id)
}

/// Flips an attempt stored by `record_attempt` to a success, so it stops counting toward a lock.
pub async fn mark_succeeded(db: &PgPool, id: i64) -> sqlx::Result<()> {
    sqlx::query!(
        "update login_attempts set succeeded = true where id = $1",
        id
    )
    .execute(db)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ipv6_is_masked_to_64_and_ipv4_is_kept() {
        let ip = |s: &str| Some(s.parse::<IpAddr>().unwrap());
        assert_eq!(
            bucket(ip("2001:db8:1:2:aaaa:bbbb:cccc:dddd")),
            ip("2001:db8:1:2::")
        );
        assert_eq!(
            bucket(ip("2001:db8:1:2::1")),
            bucket(ip("2001:db8:1:2:ffff::"))
        );
        assert_ne!(bucket(ip("2001:db8:1:2::1")), bucket(ip("2001:db8:1:3::1")));
        assert_eq!(bucket(ip("9.9.9.9")), ip("9.9.9.9"));
        assert_eq!(bucket(None), None);
    }

    #[test]
    fn ipv4_mapped_addresses_are_keyed_as_their_ipv4_address() {
        let ip = |s: &str| Some(s.parse::<IpAddr>().unwrap());
        assert_eq!(bucket(ip("::ffff:1.2.3.4")), bucket(ip("1.2.3.4")));
        assert_ne!(bucket(ip("::ffff:1.2.3.4")), bucket(ip("::ffff:5.6.7.8")));
    }
}
