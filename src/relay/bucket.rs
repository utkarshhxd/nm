// SPDX-License-Identifier: AGPL-3.0-only

//! The relay token bucket: how this product keeps to a relay's rate limit across every worker.
//!
//! OCI Email Delivery signals exhaustion with `455` and publishes no `Retry-After`, so once a relay
//! throttles us nothing says when it is safe to try again. This bucket is the only thing between us
//! and a refusal we cannot wait out; a mistake here does not slow sending down, it stops it.
//!
//! **The statement is load-bearing outside this module.** `docs/adr/0005` rules that `relays` is
//! not a lock class, and the ruling was derived against this statement's shape:
//!
//! - the **insert arm** takes `for key share` on the `relays` row and nothing else waitable;
//! - the **update arm** writes `tokens` and `refilled_at` only, neither a foreign-key column, so it
//!   fires no referential trigger and takes nothing on the parent.
//!
//! So `take` stays one statement, never reads or locks `relays` (`limit_per_minute` is a parameter
//! for exactly that reason), and never writes `relay_id` or `workspace_id` on conflict. Change any
//! of that and the ADR's enumeration must be re-run first. `tests/lock_order.rs` holds a
//! reconstruction of this statement as `DEBIT`.

use sqlx::PgExecutor;
use uuid::Uuid;

/// `$1` relay, `$2` workspace, `$3` the relay's limit per minute, `$4` tokens wanted.
///
/// Differs from `lock_order.rs`'s `DEBIT` in one place: the insert arm is `select ... where
/// $3 >= $4` rather than `values (...)`. A first take of more than the limit would otherwise
/// insert `$3 - $4`, which is negative, and `relay_buckets_tokens_non_negative` would raise
/// `23514` where this function promises `false`. With the `where`, no row is proposed, so there is
/// no conflict and no row either: `fetch_optional` sees `None`. Both arms still have the shape the
/// ADR describes.
///
/// The refill is clamped at zero, and that is not decoration. `now()` is the *transaction's* start,
/// not the moment the row is reached: a taker that started a millisecond later but committed first
/// leaves `refilled_at` ahead of this transaction's `now()`, the elapsed time is negative, and an
/// unclamped `floor` of a small negative number is `-1`. Twenty concurrent takers lost a token that
/// way (45 granted of 50), and `DEBIT` in `lock_order.rs` has the same flaw.
///
/// The refill uses the row's `refilled_at` as it was before this statement, because a `set` list
/// reads the old row. `refilled_at = now()` therefore restarts the clock only after the elapsed
/// time was counted.
const TAKE: &str = "insert into relay_buckets (relay_id, workspace_id, tokens, refilled_at)
     select $1, $2, $3::int - $4::int, now() where $3::int >= $4::int
     on conflict (relay_id) do update
        set tokens = least($3::int, relay_buckets.tokens
              + greatest(0, floor(extract(epoch from now() - relay_buckets.refilled_at) * $3::int / 60))::int)
              - $4::int,
            refilled_at = now()
      where least($3::int, relay_buckets.tokens
              + greatest(0, floor(extract(epoch from now() - relay_buckets.refilled_at) * $3::int / 60))::int)
              >= $4::int
     returning tokens";

/// Refills by elapsed time (capped at the relay's per-minute limit), then takes `n`. `false` means
/// not enough tokens, and the caller does not send this tick; a refused take changes nothing.
///
/// Debited once per claimed batch, never per message. `n <= 0` is `false` without a round trip: a
/// batch that thinks it has no messages has nothing to spend tokens on.
pub async fn take(
    db: impl PgExecutor<'_>,
    workspace_id: Uuid,
    relay_id: Uuid,
    limit_per_minute: i32,
    n: i32,
) -> sqlx::Result<bool> {
    if n <= 0 {
        return Ok(false);
    }
    Ok(sqlx::query_scalar::<_, i32>(TAKE)
        .bind(relay_id)
        .bind(workspace_id)
        .bind(limit_per_minute)
        .bind(n)
        .fetch_optional(db)
        .await?
        .is_some())
}
