// SPDX-License-Identifier: AGPL-3.0-only

//! The relay token bucket (`nm::relay::bucket::take`).
//!
//! **Every quantity here that depends on elapsed time is asserted as a bound, never as an
//! equality.** The refill is `floor(elapsed_seconds * limit / 60)`: with a limit of 60 that is one
//! token a second, so an assertion pinned to an exact count passes on a fast machine and silently
//! stops discriminating on a slow one (two tests in `lock_order.rs` were found to do exactly that).
//! Each test picks numbers so the drift it tolerates is far larger than any plausible test runtime
//! and the defect it hunts is far larger than the drift.

use std::sync::Arc;

use nm::relay::bucket::take;
use sqlx::{Connection, PgConnection, PgPool};
use tokio::sync::Barrier;
use uuid::Uuid;

/// A workspace and one relay. `relay_buckets` has a composite foreign key onto `relays`, so the
/// row has to exist; `take` itself never reads it and is told the limit as an argument.
async fn relay_fixture(pool: &PgPool) -> (Uuid, Uuid) {
    let ws = Uuid::now_v7();
    sqlx::query("insert into workspaces (id, slug, name) values ($1, 'acme', 'acme')")
        .bind(ws)
        .execute(pool)
        .await
        .expect("workspace");
    let relay = Uuid::now_v7();
    sqlx::query(
        "insert into relays (id, workspace_id, name, host, port, security, rate_limit_per_minute)
         values ($1, $2, 'primary', 'smtp.example.com', 587, 'starttls', 60)",
    )
    .bind(relay)
    .bind(ws)
    .execute(pool)
    .await
    .expect("relay");
    (ws, relay)
}

/// Seeds a bucket holding `tokens`, last refilled `ago_seconds` ago.
async fn seed(pool: &PgPool, ws: Uuid, relay: Uuid, tokens: i32, ago_seconds: i32) {
    sqlx::query(
        "insert into relay_buckets (relay_id, workspace_id, tokens, refilled_at)
         values ($1, $2, $3, now() - make_interval(secs => $4))",
    )
    .bind(relay)
    .bind(ws)
    .bind(tokens)
    .bind(f64::from(ago_seconds))
    .execute(pool)
    .await
    .expect("bucket");
}

async fn balance(pool: &PgPool, relay: Uuid) -> Option<i32> {
    sqlx::query_scalar("select tokens from relay_buckets where relay_id = $1")
        .bind(relay)
        .fetch_optional(pool)
        .await
        .expect("read balance")
}

#[sqlx::test]
async fn a_fresh_bucket_allows_up_to_the_limit_and_then_refuses(pool: PgPool) {
    let (ws, relay) = relay_fixture(&pool).await;
    // A limit of 6 a minute refills one token every ten seconds, so the drift this test tolerates
    // is ten seconds of runtime. It takes milliseconds.
    for i in 1..=6 {
        assert!(
            take(&pool, ws, relay, 6, 1).await.expect("take"),
            "take {i} of 6 on a fresh bucket must succeed"
        );
    }
    assert!(
        !take(&pool, ws, relay, 6, 1).await.expect("take"),
        "the seventh take must be refused"
    );
    assert_eq!(
        balance(&pool, relay).await,
        Some(0),
        "a refusal spends nothing"
    );
}

#[sqlx::test]
async fn tokens_refill_with_time(pool: PgPool) {
    let (ws, relay) = relay_fixture(&pool).await;
    // Empty, last refilled 30 seconds ago, limit 60: thirty tokens, plus one more per second the
    // test itself takes. So "exactly 30, not 31" cannot be asserted as written; both ends are
    // bounds instead. 35 must be refused (it needs the test to run five seconds slow) and 30 must
    // be granted (it holds however slow the test is).
    seed(&pool, ws, relay, 0, 30).await;
    assert!(
        !take(&pool, ws, relay, 60, 35).await.expect("take"),
        "30 seconds at 60 a minute is about 30 tokens, not 35"
    );
    // The refusal must not have spent or reset anything: if it had restarted the clock, the grant
    // below would find no elapsed time to refill from.
    assert!(
        take(&pool, ws, relay, 60, 30).await.expect("take"),
        "30 seconds at 60 a minute must allow 30"
    );
    // What is left is whatever accrued in the test since: nought to a few. A refill that counted
    // its elapsed time from the post-update `refilled_at` would have granted nothing above.
    let left = balance(&pool, relay).await.expect("row");
    assert!(
        (0..5).contains(&left),
        "about nothing should be left, found {left}"
    );
}

#[sqlx::test]
async fn refill_is_capped_at_the_limit(pool: PgPool) {
    let (ws, relay) = relay_fixture(&pool).await;
    // An hour of refill is 3600 tokens at this limit; the cap is 60.
    seed(&pool, ws, relay, 10, 3600).await;
    assert!(
        !take(&pool, ws, relay, 60, 61).await.expect("take"),
        "an hour of refill must not allow more than the limit in one take"
    );
    assert!(
        take(&pool, ws, relay, 60, 60).await.expect("take"),
        "a full bucket allows the limit"
    );
    // Uncapped, 3610 - 60 would remain. Capped, nothing does, plus the test's own seconds.
    let left = balance(&pool, relay).await.expect("row");
    assert!(
        (0..5).contains(&left),
        "the balance must have been capped at 60, found {left} left"
    );
}

#[sqlx::test]
async fn a_take_larger_than_the_limit_returns_false_rather_than_raising(pool: PgPool) {
    let (ws, relay) = relay_fixture(&pool).await;
    // First take: the insert arm. Unclamped it inserts 10 - 11 and trips the non-negative check.
    assert!(
        !take(&pool, ws, relay, 10, 11)
            .await
            .expect("must be false, not a 23514"),
        "more than the limit can never be granted"
    );
    assert_eq!(
        balance(&pool, relay).await,
        None,
        "a refused first take creates no bucket"
    );
    // And on an existing bucket, the update arm.
    seed(&pool, ws, relay, 10, 0).await;
    assert!(!take(&pool, ws, relay, 10, 11).await.expect("take"));
    let left = balance(&pool, relay).await.expect("row");
    assert!(left >= 10, "a refused take spends nothing, found {left}");
}

#[sqlx::test]
async fn taking_zero_is_refused(pool: PgPool) {
    let (ws, relay) = relay_fixture(&pool).await;
    assert!(!take(&pool, ws, relay, 60, 0).await.expect("take"));
    assert!(
        !take(&pool, ws, relay, 60, -3).await.expect("take"),
        "a negative take would mint"
    );
    assert_eq!(
        balance(&pool, relay).await,
        None,
        "and neither creates a bucket"
    );
    // No round trip: a closed pool errors on any query, so `Ok(false)` shows none was attempted.
    pool.close().await;
    assert!(
        !take(&pool, ws, relay, 60, 0)
            .await
            .expect("zero must not touch the database")
    );
}

/// Invariant 8. Twenty takers each want 5 of a 50-token bucket that does not exist yet, so the
/// first of them races the others through the insert arm too.
///
/// The grant is asserted to be **exactly** 50. "At most 50" passes on a bucket that refuses
/// everyone. Drift: a refill of 5 more would need the bucket idle for over six seconds at this
/// limit (50 a minute), and every statement here is milliseconds.
#[sqlx::test]
async fn concurrent_takers_never_exceed_the_limit(pool: PgPool) {
    let (ws, relay) = relay_fixture(&pool).await;
    let barrier = Arc::new(Barrier::new(20));
    let options = (*pool.connect_options()).clone();
    let mut tasks = Vec::new();
    for _ in 0..20 {
        let (barrier, options) = (barrier.clone(), options.clone());
        tasks.push(tokio::spawn(async move {
            // Its own connection, so the twenty really are twenty sessions and not a pool queue.
            let mut conn = PgConnection::connect_with(&options).await.expect("connect");
            barrier.wait().await;
            take(&mut conn, ws, relay, 50, 5).await.expect("take")
        }));
    }
    let mut granted = 0;
    for task in tasks {
        if task.await.expect("task") {
            granted += 5;
        }
    }
    assert_eq!(
        granted, 50,
        "twenty takers of 5 against 50 must be granted exactly 50"
    );
    assert_eq!(
        balance(&pool, relay).await,
        Some(0),
        "and the bucket must be empty"
    );
}
