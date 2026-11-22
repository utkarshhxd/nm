// SPDX-License-Identifier: AGPL-3.0-only

//! The worker's claim protocol (`nm::worker`), held against the contract in
//! `comment on table jobs` — the authority for it, which supersedes the query in
//! `0023_forms_and_jobs.sql`.
//!
//! The comment names four clauses as load-bearing. Each has a test here that goes red when it is
//! removed:
//!
//! | clause | test |
//! |---|---|
//! | `for update skip locked` | `a_row_another_session_holds_is_stepped_over_not_waited_for` |
//! | `run_after` advanced with the claim | `a_claimed_row_is_not_re_offered_before_its_lease_expires` |
//! | `completed_at is null` | `a_completed_row_is_never_re_claimed` |
//! | `attempts < max_attempts` | `a_row_at_max_attempts_is_not_claimed` |
//!
//! **Every quantity here that depends on elapsed time is asserted as a bound, never as an
//! equality**, and each bound is chosen so the drift it tolerates (tens of seconds) is far larger
//! than any plausible test runtime and far smaller than the defect it hunts. Two tests in
//! `lock_order.rs` were found to be correct only on a slow machine for want of this.

use std::time::Duration;

use nm::worker::{claim, complete, defer, fail};
use sqlx::{Connection, PgConnection, PgPool};
use tokio::{sync::Barrier, time::timeout};
use uuid::Uuid;

/// The one kind M2a drains. `jobs.kind` is deliberately open, so claiming filters on it (§5.3).
const KIND: &str = "confirmation_email";

/// A workspace, a contact and a list. `jobs_contact_fk` and `jobs_list_fk` are composite on
/// `workspace_id`, and `jobs_confirmation_email_shape` refuses a `confirmation_email` row that
/// names neither — so a fixture cannot skip these.
async fn fixture(pool: &PgPool) -> (Uuid, Uuid, Uuid) {
    let ws = Uuid::now_v7();
    sqlx::query("insert into workspaces (id, slug, name) values ($1, 'acme', 'acme')")
        .bind(ws)
        .execute(pool)
        .await
        .expect("workspace");
    let contact = Uuid::now_v7();
    sqlx::query("insert into contacts (id, workspace_id, email) values ($1, $2, 'a@example.com')")
        .bind(contact)
        .bind(ws)
        .execute(pool)
        .await
        .expect("contact");
    let list = Uuid::now_v7();
    sqlx::query("insert into lists (id, workspace_id, name) values ($1, $2, 'news')")
        .bind(list)
        .bind(ws)
        .execute(pool)
        .await
        .expect("list");
    (ws, contact, list)
}

/// One runnable job. `attempts`/`max_attempts` are arguments because three tests turn on exactly
/// where a row sits between them. `run_after` defaults to `now()`, which a later transaction's
/// `now()` is already past.
async fn enqueue(
    pool: &PgPool,
    (ws, contact, list): (Uuid, Uuid, Uuid),
    kind: &str,
    attempts: i32,
    max_attempts: i32,
) -> Uuid {
    let id = Uuid::now_v7();
    sqlx::query(
        "insert into jobs (id, workspace_id, kind, contact_id, list_id, payload, attempts,
                           max_attempts)
         values ($1, $2, $3, $4, $5, '{\"email\": \"a@example.com\", \"token\": \"t\"}'::jsonb,
                 $6, $7)",
    )
    .bind(id)
    .bind(ws)
    .bind(kind)
    .bind(contact)
    .bind(list)
    .bind(attempts)
    .bind(max_attempts)
    .execute(pool)
    .await
    .expect("enqueue");
    id
}

/// What the passage of the lease does to a row, without making a test sleep for it: `run_after`
/// **is** the lease, so putting it back in the past is exactly the state a worker that died
/// mid-tick leaves behind.
async fn expire_the_lease(pool: &PgPool, id: Uuid) {
    sqlx::query("update jobs set run_after = now() - interval '1 second' where id = $1")
        .bind(id)
        .execute(pool)
        .await
        .expect("expire the lease");
}

/// `(attempts, last_error, completed_at is not null, failed_at is not null)`.
async fn state(pool: &PgPool, id: Uuid) -> (i32, Option<String>, bool, bool) {
    sqlx::query_as(
        "select attempts, last_error, completed_at is not null, failed_at is not null
           from jobs where id = $1",
    )
    .bind(id)
    .fetch_one(pool)
    .await
    .expect("read the row")
}

/// Seconds from now until the row may run again. Read against the database's clock, so it never
/// depends on the test process agreeing with it. It only ever *shrinks* as the test runs, which is
/// why every upper bound below can be exact and every lower bound carries slack.
async fn seconds_until_runnable(pool: &PgPool, id: Uuid) -> f64 {
    sqlx::query_scalar(
        "select extract(epoch from (run_after - now()))::float8 from jobs where id = $1",
    )
    .bind(id)
    .fetch_one(pool)
    .await
    .expect("read run_after")
}

/// **Invariant 1.** The two vectors must total **exactly** one. "At most one" is the vacuous form:
/// it passes on a claim that returns nothing to anybody.
///
/// Each worker gets a connection of its own, taken outside the pool `#[sqlx::test]` hands over, so
/// the two really are two sessions racing and not a queue in front of one.
#[sqlx::test]
async fn two_workers_racing_one_row_claim_it_once(pool: PgPool) {
    let f = fixture(&pool).await;
    enqueue(&pool, f, KIND, 0, 5).await;
    let barrier = std::sync::Arc::new(Barrier::new(2));
    let options = (*pool.connect_options()).clone();
    let mut tasks = Vec::new();
    for _ in 0..2 {
        let (barrier, options) = (barrier.clone(), options.clone());
        tasks.push(tokio::spawn(async move {
            let mut conn = PgConnection::connect_with(&options).await.expect("connect");
            barrier.wait().await;
            claim(&mut conn, KIND, 16, 120).await.expect("claim").len()
        }));
    }
    let mut total = 0;
    for task in tasks {
        total += task.await.expect("task");
    }
    assert_eq!(
        total, 1,
        "one row and two workers must produce exactly one claim between them"
    );
}

/// `for update skip locked`, the first clause the table comment calls load-bearing. Without
/// `skip locked` the claim **waits** for the row another session is holding; with it, the claim
/// steps over the row and comes back at once.
///
/// This is the only clause the race above cannot see: plain `for update` also yields exactly one
/// claim, because the blocked statement re-checks the predicate against the committed row and
/// finds `run_after` in the future. The difference is liveness — one poller stalling behind
/// another for as long as it holds its row — so that is what is asserted. Five seconds is a bound
/// with three orders of margin: the claim returns in milliseconds, and the wedge is held until
/// after the assertion, so a claim that waits cannot finish at all.
#[sqlx::test]
async fn a_row_another_session_holds_is_stepped_over_not_waited_for(pool: PgPool) {
    let f = fixture(&pool).await;
    let id = enqueue(&pool, f, KIND, 0, 5).await;
    let options = (*pool.connect_options()).clone();
    let mut wedge = PgConnection::connect_with(&options).await.expect("connect");
    let mut tx = wedge.begin().await.expect("begin");
    sqlx::query("select id from jobs where id = $1 for update")
        .bind(id)
        .fetch_one(&mut *tx)
        .await
        .expect("hold the row");

    let claimed = timeout(Duration::from_secs(5), claim(&pool, KIND, 16, 120))
        .await
        .expect("a claim must step over a held row, not wait behind it")
        .expect("claim");
    assert!(
        claimed.is_empty(),
        "the only row is held by another session, so there is nothing to claim"
    );
    tx.rollback().await.expect("release the row");
}

/// `run_after` advanced in the claim itself — the second load-bearing clause. A claim that leaves
/// `run_after` in the past re-offers the row before the first worker has finished, and the message
/// goes twice.
#[sqlx::test]
async fn a_claimed_row_is_not_re_offered_before_its_lease_expires(pool: PgPool) {
    let f = fixture(&pool).await;
    let id = enqueue(&pool, f, KIND, 0, 5).await;

    let first = claim(&pool, KIND, 16, 120).await.expect("claim");
    assert_eq!(first.len(), 1, "the row is runnable, so it is claimed");
    assert_eq!(first[0].id, id);
    assert_eq!(
        first[0].attempts, 1,
        "the claim counts the attempt, and it is the post-increment value the caller gets"
    );

    assert!(
        claim(&pool, KIND, 16, 120).await.expect("claim").is_empty(),
        "a row under lease must not be offered again"
    );

    expire_the_lease(&pool, id).await;
    let third = claim(&pool, KIND, 16, 120).await.expect("claim");
    assert_eq!(
        third.len(),
        1,
        "once the lease has expired the row is offered again — that is how a dead worker's rows \
         are recovered"
    );
    assert_eq!(third[0].attempts, 2, "and the second claim counts too");
}

/// The lease the caller asks for is the lease the row gets. Nothing else here pins the *duration*
/// — every other test would pass on a claim that ignored `lease_secs` and hard-coded one — and the
/// duration is `JOB_LEASE`, the one value a second constant would be free to drift from.
///
/// Ten minutes, asserted as a bound: `run_after` is `now() + 600` at the claim and only shrinks
/// afterwards, so the upper end is exact and the lower end carries a minute of slack. The 120 s a
/// hard-coded `JOB_LEASE` would give is nowhere near it.
#[sqlx::test]
async fn the_row_is_held_for_the_lease_the_caller_asked_for(pool: PgPool) {
    let f = fixture(&pool).await;
    let id = enqueue(&pool, f, KIND, 0, 5).await;
    assert_eq!(claim(&pool, KIND, 16, 600).await.expect("claim").len(), 1);
    let left = seconds_until_runnable(&pool, id).await;
    assert!(
        (540.0..=600.0).contains(&left),
        "a ten-minute lease must hold the row for about ten minutes, found {left}"
    );
}

/// `completed_at is null` — the third load-bearing clause, and the defect `0025` describes: a
/// worker written from `0023` alone re-claims every completed row the moment its lease expires,
/// for ever.
///
/// The lease is expired deliberately. Leave it in place and this test passes on a predicate that
/// never mentions `completed_at`, for the wrong reason.
#[sqlx::test]
async fn a_completed_row_is_never_re_claimed(pool: PgPool) {
    let f = fixture(&pool).await;
    let id = enqueue(&pool, f, KIND, 0, 5).await;
    let claimed = claim(&pool, KIND, 16, 120).await.expect("claim");
    assert_eq!(claimed.len(), 1);
    complete(&pool, id).await.expect("complete");
    expire_the_lease(&pool, id).await;
    assert!(
        claim(&pool, KIND, 16, 120).await.expect("claim").is_empty(),
        "a completed row is never claimed again, however long ago its lease ran out"
    );
}

/// `attempts < max_attempts` — the fourth. A worker that dies after incrementing `attempts` and
/// before writing `failed_at` otherwise leaves a row retried past its limit for ever.
///
/// Two rows, so the test cannot pass by claiming nothing: the one below its limit must come back.
#[sqlx::test]
async fn a_row_at_max_attempts_is_not_claimed(pool: PgPool) {
    let f = fixture(&pool).await;
    let spent = enqueue(&pool, f, KIND, 3, 3).await;
    let left = enqueue(&pool, f, KIND, 2, 3).await;
    let claimed = claim(&pool, KIND, 16, 120).await.expect("claim");
    assert_eq!(
        claimed.iter().map(|c| c.id).collect::<Vec<_>>(),
        vec![left],
        "only the row with an attempt left may be claimed; {spent} has spent all of its"
    );
}

/// Spec §5.3: claiming is by `kind`, so a five-million-row campaign never sits in front of a
/// confirmation email. The other row is claimed by its own kind afterwards, which is what stops
/// this passing on a claim that returns nothing.
#[sqlx::test]
async fn claiming_filters_by_kind(pool: PgPool) {
    let f = fixture(&pool).await;
    let ours = enqueue(&pool, f, KIND, 0, 5).await;
    let theirs = enqueue(&pool, f, "campaign_send", 0, 5).await;

    let claimed = claim(&pool, KIND, 16, 120).await.expect("claim");
    assert_eq!(
        claimed.iter().map(|c| c.id).collect::<Vec<_>>(),
        vec![ours],
        "a row of another kind is left alone"
    );
    let other = claim(&pool, "campaign_send", 16, 120).await.expect("claim");
    assert_eq!(
        other.iter().map(|c| c.id).collect::<Vec<_>>(),
        vec![theirs],
        "and it was runnable all along"
    );
}

/// References are columns, not payload: `contact_id` and `list_id` are real columns with composite
/// foreign keys carrying `workspace_id`, and the claim reads them from there. The payload here
/// carries neither, so a worker that dug them out of the JSON finds nothing.
#[sqlx::test]
async fn a_claimed_row_carries_its_references_from_the_columns(pool: PgPool) {
    let (ws, contact, list) = fixture(&pool).await;
    enqueue(&pool, (ws, contact, list), KIND, 0, 5).await;
    let claimed = claim(&pool, KIND, 16, 120).await.expect("claim");
    let [job] = claimed.as_slice() else {
        panic!("one row")
    };
    assert_eq!(job.workspace_id, ws);
    assert_eq!(job.contact_id, Some(contact));
    assert_eq!(job.list_id, Some(list));
    assert_eq!(job.payload["email"], "a@example.com");
    assert_eq!(job.max_attempts, 5);
}

/// `batch` bounds one tick. Seventeen runnable rows, a batch of sixteen: the tick takes sixteen.
#[sqlx::test]
async fn a_claim_takes_no_more_than_its_batch(pool: PgPool) {
    let f = fixture(&pool).await;
    for _ in 0..17 {
        enqueue(&pool, f, KIND, 0, 5).await;
    }
    assert_eq!(claim(&pool, KIND, 16, 120).await.expect("claim").len(), 16);
    assert_eq!(
        claim(&pool, KIND, 16, 120).await.expect("claim").len(),
        1,
        "and the seventeenth is still there, unclaimed and unleased"
    );
}

/// The decision that is easy to get wrong. On a transient outcome at the last attempt, `defer`
/// must `fail` instead: the claim predicate stops offering the row, so **nothing else would ever
/// write `failed_at`** and it would sit unexplained for ever.
///
/// The last two assertions are what make that true rather than asserted: after the deferral the
/// row is unclaimable, so if `failed_at` were not written here it would be written by nobody.
#[sqlx::test]
async fn defer_at_the_last_attempt_writes_failed_at(pool: PgPool) {
    let f = fixture(&pool).await;
    let id = enqueue(&pool, f, KIND, 2, 3).await;
    let claimed = claim(&pool, KIND, 16, 120).await.expect("claim");
    let [job] = claimed.as_slice() else {
        panic!("one row")
    };
    assert_eq!(
        (job.attempts, job.max_attempts),
        (3, 3),
        "this claim is the last attempt the row has"
    );

    defer(&pool, job, "relay said 421").await.expect("defer");

    let (attempts, last_error, completed, failed) = state(&pool, id).await;
    assert_eq!(attempts, 3);
    assert!(
        failed,
        "a transient failure at the last attempt gives up: it writes failed_at rather than \
         leaving the row silently unclaimable"
    );
    assert_eq!(last_error.as_deref(), Some("relay said 421"));
    assert!(!completed, "and it did not complete");

    expire_the_lease(&pool, id).await;
    assert!(
        claim(&pool, KIND, 16, 120).await.expect("claim").is_empty(),
        "which is the point: nothing claims this row again, so nothing else could explain it"
    );
}

/// A transient outcome with attempts left reschedules and explains itself, and does **not** give
/// up. The row stays claimable once its backoff has passed.
#[sqlx::test]
async fn defer_with_attempts_left_reschedules_rather_than_failing(pool: PgPool) {
    let f = fixture(&pool).await;
    let id = enqueue(&pool, f, KIND, 0, 5).await;
    let claimed = claim(&pool, KIND, 16, 120).await.expect("claim");
    defer(&pool, &claimed[0], "relay said 421")
        .await
        .expect("defer");

    let (attempts, last_error, completed, failed) = state(&pool, id).await;
    assert_eq!(attempts, 1);
    assert!(!failed && !completed, "one bad attempt is not giving up");
    assert_eq!(last_error.as_deref(), Some("relay said 421"));

    expire_the_lease(&pool, id).await;
    assert_eq!(
        claim(&pool, KIND, 16, 120).await.expect("claim").len(),
        1,
        "and it runs again once its backoff has passed"
    );
}

/// Backoff is `30 * 2^(attempts-1)` seconds capped at an hour, with jitter.
///
/// **Bounds, not equalities, and the jitter is only half the reason.** `seconds_until_runnable`
/// measures what is *left* of the delay, so elapsed test time only ever makes it smaller: every
/// upper bound is therefore exact and needs no slack, while every lower bound carries tens of
/// seconds of it. Each bound is also nowhere near the defect it hunts — no backoff at all is 0,
/// a flat backoff is 30 at every attempt, and an uncapped eighth doubling is 3 840 s against a cap
/// of 3 600.
#[sqlx::test]
async fn backoff_grows_with_attempts_and_is_capped_at_an_hour(pool: PgPool) {
    let f = fixture(&pool).await;
    // After the claim these sit at attempts 1, 5 and 20 of a generous limit, so the backoff is
    // 30 s, 480 s and a capped 3 600 s before jitter, and in [15, 30], [240, 480] and
    // [1800, 3600] after it.
    let first = enqueue(&pool, f, KIND, 0, 50).await;
    let fifth = enqueue(&pool, f, KIND, 4, 50).await;
    let twentieth = enqueue(&pool, f, KIND, 19, 50).await;
    for job in claim(&pool, KIND, 16, 120).await.expect("claim") {
        defer(&pool, &job, "relay said 421").await.expect("defer");
    }

    let one = seconds_until_runnable(&pool, first).await;
    assert!(
        (5.0..=30.0).contains(&one),
        "the first backoff is at most 30 s and is not nothing, found {one}"
    );
    let five = seconds_until_runnable(&pool, fifth).await;
    assert!(
        (200.0..=480.0).contains(&five),
        "the fifth attempt backs off far past the first, found {five}"
    );
    let twenty = seconds_until_runnable(&pool, twentieth).await;
    assert!(
        (1_700.0..=3_600.0).contains(&twenty),
        "the twentieth is capped at an hour rather than the 15 728 640 s the formula gives, \
         found {twenty}"
    );
}

/// **Nothing deletes a `jobs` row.** The signup form's resend throttle has no state of its own and
/// counts these rows; a worker that deleted on success would turn a public route into an open
/// mailer with nothing going red.
#[sqlx::test]
async fn nothing_deletes_a_jobs_row(pool: PgPool) {
    let f = fixture(&pool).await;
    let sent = enqueue(&pool, f, KIND, 0, 5).await;
    let given_up = enqueue(&pool, f, KIND, 0, 5).await;
    for job in claim(&pool, KIND, 16, 120).await.expect("claim") {
        if job.id == sent {
            complete(&pool, job.id).await.expect("complete");
        } else {
            fail(&pool, job.id, "relay said 550").await.expect("fail");
        }
    }

    let rows: i64 = sqlx::query_scalar("select count(*) from jobs")
        .fetch_one(&pool)
        .await
        .expect("count");
    assert_eq!(rows, 2, "a drained queue still holds every row it drained");
    assert!(state(&pool, sent).await.2, "the sent one is marked");
    let (_, last_error, _, failed) = state(&pool, given_up).await;
    assert!(failed, "and the one that gave up is marked and explained");
    assert_eq!(last_error.as_deref(), Some("relay said 550"));
}

/// The shipped statement, clause by clause. All four of the clauses the table comment calls
/// load-bearing have behavioural tests above as well; this one exists for the property none of
/// them can see — `run_after` advanced **in the claim** rather than by a follow-up update, whose
/// only observable difference is a window between two statements too narrow to drive from a test.
/// `claim` executes `CLAIM` and nothing else, so what this reads is what ships.
/// `tests/lock_order.rs` holds `relay::bucket::TAKE` the same way and for the same reason.
#[test]
fn the_claim_statement_is_the_one_the_table_comment_states() {
    let s = nm::worker::CLAIM;
    let set = s
        .find("where id in (")
        .expect("a single update with a subselect");
    assert!(
        s[..set].contains("attempts = attempts + 1") && s[..set].contains("run_after = now()"),
        "the lease is advanced in the same statement as the attempt, not by a second update"
    );
    for clause in [
        "run_after <= now()",
        "failed_at is null",
        "completed_at is null",
        "attempts < max_attempts",
        "order by run_after, id",
        "for update skip locked",
    ] {
        assert!(s.contains(clause), "the claim must carry `{clause}`:\n{s}");
    }
    assert!(
        !s.contains("delete"),
        "nothing in the claim protocol deletes a jobs row"
    );
}
