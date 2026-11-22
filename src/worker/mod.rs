// SPDX-License-Identifier: AGPL-3.0-only

//! The work queue's claim, lease, completion and backoff primitives. Task 11's `run` is the tick
//! that calls them.
//!
//! **The claim protocol is not derived here. It is written down, and the authority is
//! `comment on table jobs`** (`migrations/0026_jobs_references_and_consent_guards.sql`), which
//! supersedes the query in `0023_forms_and_jobs.sql` — that one predates `completed_at` and omits
//! it, so a worker written from `0023` alone re-claims every completed row the moment its lease
//! expires, for ever. `0023` cannot be amended: its checksum has been taken. [`CLAIM`] below is
//! that comment's statement, and `tests/worker_claim.rs` reads it clause by clause.
//!
//! Three rules from the same comment that are not visible in a function signature:
//!
//! - **Nothing deletes a `jobs` row.** The hosted signup form's resend throttle has no state of
//!   its own and counts these rows; a worker that deleted on success would turn a public route
//!   into a mailer anybody could aim at any address, with nothing going red. A delete inside the
//!   retention window is refused by the `jobs_throttle_retention` trigger rather than failing
//!   quietly. Success is [`complete`]; giving up is [`fail`]. Neither removes anything.
//! - **References are columns, not payload.** `contact_id` and `list_id` are real columns with
//!   composite foreign keys carrying `workspace_id`, so [`Claimed`] carries them and nothing digs
//!   them out of the JSON.
//! - **The lease is imported, never re-declared.** [`JOB_LEASE`] lives in
//!   [`crate::relay::transport`], where a compile-time assertion (`SESSION_TIMEOUT < JOB_LEASE`)
//!   and a hang test share one value. A second constant meaning "the lease" is how the two drift,
//!   and a tick that outlives its lease has its rows re-claimed and its messages sent twice.
//!
//! `docs/adr/0005` places all four functions here in one lock class: they touch `jobs` rows alone,
//! and `jobs` is a parent of nothing.

pub mod confirmation;

use std::time::Duration;

use chrono::{DateTime, Utc};
use rand::{TryRng, rngs::SysRng};
use sqlx::PgExecutor;
use uuid::Uuid;

pub use crate::relay::transport::JOB_LEASE;

/// Rows one tick claims. Enough that a single worker is not the bottleneck at the signup form's
/// own rate limits, small enough that a crash re-does little (spec §5.2).
pub const BATCH: i64 = 16;

/// How often a worker asks for work when the last tick found none.
///
/// **Task 11's tick must jitter this.** That is an obligation, not a description: nothing in this
/// module applies it. A fleet restarted together — which is what a deploy is — otherwise polls in
/// lockstep for ever, every worker hitting the same claim at the same instant and all but one of
/// them doing a round trip for nothing.
///
/// shortcut: polling puts a one-second floor on latency; upgrade to `LISTEN`/`NOTIFY` when a
/// tick's claim regularly comes back full (spec §5.2).
pub const POLL: Duration = Duration::from_secs(1);

/// First backoff, and the base of the doubling: `30 * 2^(attempts - 1)` seconds.
const BACKOFF_BASE_SECS: i64 = 30;

/// An hour. Past this the doubling stops, so a row that keeps failing is still retried hourly
/// rather than drifting out to a delay nobody would wait for.
const BACKOFF_CAP_SECS: i64 = 3_600;

/// The claim, exactly as `comment on table jobs` states it, plus the `kind` filter spec §5.3 adds.
///
/// `$1` kind, `$2` lease in seconds, `$3` batch size.
///
/// Load-bearing, none of it optional — the comment's own list, with its reasons:
///
/// - `for update skip locked`: without it two pollers queue behind one row instead of taking a
///   row each, and one worker's stall is every worker's stall.
/// - `run_after` advanced in **this** statement. It is the lease as well as the schedule; a claim
///   that leaves `run_after` in the past re-offers the row before the first worker has finished,
///   and the message goes twice.
/// - `completed_at is null` (`0025`): without it a completed row's lease expires and it is claimed
///   again, for ever.
/// - `attempts < max_attempts`: a worker that dies after incrementing `attempts` and before
///   writing `failed_at` otherwise leaves a row retried past its limit for ever.
///
/// `returning` names the columns [`Claimed`] carries rather than the comment's `*`; it is a subset
/// of it, and `tests/worker_claim.rs` reads this string for every clause above.
///
/// `#[doc(hidden)] pub` for that test, the way `relay::bucket::TAKE` is public for
/// `tests/lock_order.rs`.
#[doc(hidden)]
pub const CLAIM: &str = "update jobs
        set attempts = attempts + 1, run_after = now() + make_interval(secs => $2)
      where id in (select id from jobs
                    where kind = $1 and run_after <= now() and failed_at is null
                      and completed_at is null and attempts < max_attempts
                    order by run_after, id
                    for update skip locked
                    limit $3)
  returning id, workspace_id, payload, contact_id, list_id, attempts, max_attempts, created_at";

/// A job this worker holds the lease on. `attempts` is the post-increment count — this claim
/// included — so it is what [`defer`] measures against `max_attempts`.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Claimed {
    pub id: Uuid,
    pub workspace_id: Uuid,
    pub payload: serde_json::Value,
    pub contact_id: Option<Uuid>,
    pub list_id: Option<Uuid>,
    pub attempts: i32,
    pub max_attempts: i32,
    /// When the row was enqueued, not when it was claimed. A `confirmation_email` older than the
    /// token's 14-day TTL carries a link that is already dead and must be failed, not sent.
    pub created_at: DateTime<Utc>,
}

/// Takes up to `batch` runnable rows of `kind` and holds them for `lease_secs`.
///
/// **One statement.** Pass the pool: nothing here needs a transaction, and a transaction holding
/// claimed rows open across the sends is how a tick ends up outliving its own lease.
pub async fn claim(
    db: impl PgExecutor<'_>,
    kind: &str,
    batch: i64,
    lease_secs: i64,
) -> sqlx::Result<Vec<Claimed>> {
    sqlx::query_as::<_, Claimed>(CLAIM)
        .bind(kind)
        .bind(lease_secs as f64)
        .bind(batch)
        .fetch_all(db)
        .await
}

/// Done. The row is **kept** and marked; see this module's note on the resend throttle.
pub async fn complete(db: impl PgExecutor<'_>, id: Uuid) -> sqlx::Result<()> {
    sqlx::query("update jobs set completed_at = now() where id = $1")
        .bind(id)
        .execute(db)
        .await?;
    Ok(())
}

/// Gives up, with the reason left on the row. `failed_at` is what answers "why did this
/// confirmation never arrive?" months later, so it is never a bare flag.
pub async fn fail(db: impl PgExecutor<'_>, id: Uuid, reason: &str) -> sqlx::Result<()> {
    sqlx::query("update jobs set failed_at = now(), last_error = $2 where id = $1")
        .bind(id)
        .bind(reason)
        .execute(db)
        .await?;
    Ok(())
}

/// A transient outcome: reschedule with backoff, and leave `reason` on the row so an operator can
/// see what keeps going wrong before it finally gives up.
///
/// **Unless this was the last attempt, in which case it [`fail`]s instead.** Once `attempts`
/// reaches `max_attempts` the claim predicate stops offering the row, so nothing else would ever
/// write `failed_at`: deferring it would leave it sitting silently unclaimable, invisible to an
/// operator asking why a message never arrived.
pub async fn defer(db: impl PgExecutor<'_>, j: &Claimed, reason: &str) -> sqlx::Result<()> {
    if j.attempts >= j.max_attempts {
        return fail(db, j.id, reason).await;
    }
    sqlx::query(
        "update jobs set run_after = now() + make_interval(secs => $2), last_error = $3
          where id = $1",
    )
    .bind(j.id)
    .bind(backoff(j.attempts) as f64)
    .bind(reason)
    .execute(db)
    .await?;
    Ok(())
}

/// `30 * 2^(attempts - 1)` seconds, capped at an hour, then jittered into the top half of that
/// range: `[base / 2, base]`.
///
/// Jitter rather than the bare doubling because a relay that refuses one message usually refuses
/// the whole batch, and an exact backoff sends that batch back at the relay in lockstep for ever.
/// The bottom half of the range is kept out of it so the nudge can never make the next attempt
/// sooner than half the delay that was asked for.
///
/// The shift is clamped before it is applied: `attempts` is an `int` the database will happily
/// carry to two billion, and `1 << 2_000_000_000` is not a number.
fn backoff(attempts: i32) -> i64 {
    let shift = u32::try_from(attempts.saturating_sub(1))
        .unwrap_or(0)
        .min(32);
    let base = (BACKOFF_BASE_SECS << shift).min(BACKOFF_CAP_SECS);
    let half = base / 2;
    half + jitter(half)
}

/// A whole number of seconds in `0..=span`.
///
/// The OS CSPRNG because it is the only generator this codebase draws on; the modulo bias of a
/// span under two thousand against 2^64 is unmeasurable, and this is a scheduling nudge, not a
/// secret.
fn jitter(span: i64) -> i64 {
    if span <= 0 {
        return 0;
    }
    let mut buf = [0u8; 8];
    SysRng.try_fill_bytes(&mut buf).expect("OS RNG unavailable");
    (u64::from_le_bytes(buf) % (span as u64 + 1)) as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shape of the curve, without a database. Bounds, because of the jitter.
    #[test]
    fn backoff_doubles_from_thirty_seconds_and_stops_at_an_hour() {
        for (attempts, lo, hi) in [
            (1, 15, 30),
            (2, 30, 60),
            (5, 240, 480),
            // 30 * 2^7 is 3 840, over the cap already.
            (8, 1_800, 3_600),
            (20, 1_800, 3_600),
            (i32::MAX, 1_800, 3_600),
        ] {
            let b = backoff(attempts);
            assert!(
                (lo..=hi).contains(&b),
                "backoff({attempts}) must land in {lo}..={hi}, got {b}"
            );
        }
        // Zero cannot be claimed (`max_attempts > 0` is on the table and a claim increments before
        // it returns), but the shift must not panic if one ever reaches here.
        assert!((15..=30).contains(&backoff(0)));
    }

    /// **The jitter has to actually vary, and no bound above says so.** Every range asserted there
    /// has `base` as its exact upper end and `base / 2` at or below its lower end, so a `backoff`
    /// that returned a bare `base` — or a `jitter` that returned zero — passes all of them.
    ///
    /// What that would cost is not coverage, it is a thundering herd: a relay that refuses one
    /// message usually refuses the whole batch, and an exact backoff sends every job that failed
    /// in one tick back at that relay at the same instant. It is the pile-up the token bucket
    /// exists to prevent, arriving from the other side.
    ///
    /// This is a bound on **variance**, not on timing, so it adds no dependence on elapsed time.
    /// Fifty draws from the 241 values of attempt 5's range: a fixed implementation yields one
    /// distinct value, and the chance of fifty genuine draws colliding is `241^-49`.
    #[test]
    fn backoff_does_not_return_the_same_delay_every_time() {
        let seen: std::collections::HashSet<i64> = (0..50).map(|_| backoff(5)).collect();
        assert!(
            seen.len() > 1,
            "fifty backoffs over a 240-second range came back identical: {seen:?}"
        );
    }
}
